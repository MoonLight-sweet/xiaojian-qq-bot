//! agy 会话步流的解析器（P4 rev1）。
//!
//! 常驻会话模式下，一轮的“真相”不在 CLI 的 stdout 里，而在 agy 自己的会话 DB
//! （`<home>/.gemini/antigravity-cli/conversations/<id>.db` 的 `steps` 表）里。
//! 本模块把那张表读成一串 [`StepEvent`]，供交付屏障（[`crate::agent::barrier`]）判断
//! 这一轮是否已经完成、哪条文本可以发。
//!
//! 语义逐条移植自参考实现 `p43a/agy_steps_prod.py`。两条来自研究报告的修正必须保留：
//!
//! 1. **未知 step_type 不再无条件硬失败**。带可读文本的未知步才是格式漂移
//!    （[`ParseError::Drift`]），不带的只计数跳过。type 17 是**已知**类别（LS 错误步，
//!    payload 24.3.2 是可读错误信息），它带文本但不是漂移。
//! 2. **步是可变的**：同一个 idx 的 payload 会在生成过程中被原地改写（先 thought，
//!    随后填 text/tool）。因此每次调用都全量重读整张表，事件按 idx 对齐，
//!    **调用方不得缓存增量尾巴**。
//!
//! 已知类型（agy 1.4.7）：
//!
//! | type | 含义 | 正文位置 |
//! |------|------|----------|
//! | 14 | user 输入 | payload 19.2 |
//! | 15 | model 步 | 发言 20.1、thought 20.3、工具调用 20.7（名在 20.7.2） |
//! | 17 | 错误步 | 错误记录 24，信息 24.3.2 |
//! | 101 | 任务通知 | 字符串里以 `<conv>/task-N` 引用被关闭的任务 |
//! | 132 | 工具结果 | 发起了后台任务的带 field 148（148.1 = task id） |
//!
//! 步时间戳：payload field 5 是步元数据，5.1 是 (seconds, nanos) 对，取 float 秒。

use std::collections::BTreeMap;
use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

// ---------------------------------------------------------------------------
// 已注册的步类型

/// 已知的 step_type。落在这之外的类型走漂移启发式。
pub const KNOWN_TYPES: [i64; 5] = [14, 15, 17, 101, 132];

pub const STEP_USER: i64 = 14;
pub const STEP_MODEL: i64 = 15;
pub const STEP_ERROR: i64 = 17;
pub const STEP_TASK_NOTIFICATION: i64 = 101;
pub const STEP_TOOL_RESULT: i64 = 132;

/// 叶子递归的深度上限：超过就当成二进制块，不再往里挖。
pub const MAX_LEAF_DEPTH: usize = 8;
/// 可读文本的可打印字符比例下限。
pub const READABLE_PRINTABLE_RATIO: f64 = 0.95;
/// 可读文本的最短长度。
pub const READABLE_MIN_CHARS: usize = 4;

// ---------------------------------------------------------------------------
// 错误

/// 解析一个会话步流时可能出的两类问题。
///
/// 生产路径对两者的反应不同：`Drift` 说明 agy 的格式已经不可信，该会话必须退役；
/// `Storage` 只说明这一次读不到（文件还没落地、被锁、正在写入），重试即可。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// 步流偏离了所有已知形状 —— 这是真正的格式漂移。
    #[error("drift: {0}")]
    Drift(String),
    /// 会话 DB 读不出来。
    #[error("storage: {0}")]
    Storage(String),
}

impl ParseError {
    /// 是否是格式漂移（调用方据此决定回退，而不是重试）。
    pub fn is_drift(&self) -> bool {
        matches!(self, ParseError::Drift(_))
    }
}

/// 把 Drift 的自由文本归一化成短码，只用于审计后缀与告警证据（不参与判定）。
///
/// 今天的教训（2026-10-27 943 事故）：回退审计若只写一句 drift.fallback，
/// 事后无人能知道是哪种漂移。这里把已知形态收敛成五个短码，未知形态归 other。
pub fn drift_kind(message: &str) -> &'static str {
    if message.contains("unknown step_type") {
        "unknown_type"
    } else if message.contains("no field 20") {
        "no_field20"
    } else if message.contains("no text at 19.2") {
        "no_text"
    } else if message.contains("varint")
        || message.contains("length overrun")
        || message.contains("field 0")
        || message.contains("wire type")
    {
        "wire"
    } else {
        "other"
    }
}

type StepResult<T> = Result<T, ParseError>;

// ---------------------------------------------------------------------------
// protobuf wire walking

/// 一个已解码的 protobuf 字段值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WireValue<'a> {
    /// wire type 0
    Varint(u64),
    /// wire type 2
    Bytes(&'a [u8]),
    /// wire type 1 / 5
    Fixed(&'a [u8]),
}

fn read_varint(b: &[u8], i: &mut usize) -> StepResult<u64> {
    let mut shift: u32 = 0;
    let mut val: u64 = 0;
    loop {
        let Some(&x) = b.get(*i) else {
            // Python 版在这里会抛 IndexError，被叶子遍历静默吞掉；对上层的
            // get_msg 则是真错误。
            return Err(ParseError::Drift("varint truncated".to_string()));
        };
        *i += 1;
        val |= u64::from(x & 0x7F).wrapping_shl(shift);
        if x & 0x80 == 0 {
            return Ok(val);
        }
        shift += 7;
        if shift > 63 {
            return Err(ParseError::Drift("varint too long".to_string()));
        }
    }
}

/// 解一个 protobuf 消息缓冲区里的全部字段。
pub fn parse_fields(b: &[u8]) -> StepResult<Vec<(u32, i64, WireValue<'_>)>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let tag = read_varint(b, &mut i)?;
        let field = (tag >> 3) as u32;
        let wt = (tag & 0x07) as i64;
        if field == 0 {
            return Err(ParseError::Drift("field 0".to_string()));
        }
        match wt {
            0 => {
                let v = read_varint(b, &mut i)?;
                out.push((field, wt, WireValue::Varint(v)));
            }
            2 => {
                let len = read_varint(b, &mut i)? as usize;
                let end = i.checked_add(len).filter(|e| *e <= b.len());
                let Some(end) = end else {
                    return Err(ParseError::Drift("length overrun".to_string()));
                };
                out.push((field, wt, WireValue::Bytes(&b[i..end])));
                i = end;
            }
            5 => {
                let end = (i + 4).min(b.len());
                out.push((field, wt, WireValue::Fixed(&b[i..end])));
                i = end;
            }
            1 => {
                let end = (i + 8).min(b.len());
                out.push((field, wt, WireValue::Fixed(&b[i..end])));
                i = end;
            }
            other => {
                return Err(ParseError::Drift(format!("wire type {other}")));
            }
        }
    }
    Ok(out)
}

/// 取第一个 wire type 2 的同号字段。
pub fn get_msg(b: &[u8], field_no: u32) -> StepResult<Option<&[u8]>> {
    for (f, wt, v) in parse_fields(b)? {
        if f == field_no
            && wt == 2
            && let WireValue::Bytes(bytes) = v
        {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

/// 取第一个 wire type 0 的同号字段。
pub fn get_varint(b: &[u8], field_no: u32) -> StepResult<Option<u64>> {
    for (f, wt, v) in parse_fields(b)? {
        if f == field_no
            && wt == 0
            && let WireValue::Varint(n) = v
        {
            return Ok(Some(n));
        }
    }
    Ok(None)
}

/// `get_msg` + UTF-8 解码；解码失败视为“没有”（与参考实现一致）。
pub fn get_str(b: &[u8], field_no: u32) -> StepResult<Option<String>> {
    let Some(raw) = get_msg(b, field_no)? else {
        return Ok(None);
    };
    Ok(std::str::from_utf8(raw).ok().map(str::to_string))
}

fn opt_msg(b: Option<&[u8]>, field_no: u32) -> StepResult<Option<&[u8]>> {
    match b {
        None => Ok(None),
        Some(b) => get_msg(b, field_no),
    }
}

fn opt_str(b: Option<&[u8]>, field_no: u32) -> StepResult<Option<String>> {
    match b {
        None => Ok(None),
        Some(b) => get_str(b, field_no),
    }
}

// ---------------------------------------------------------------------------
// 可读文本启发式（未知类型的漂移判据）

fn is_uuid(t: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let mut parts = t.split('-');
    for expected in groups {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != expected || !part.bytes().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

fn is_hex(t: &str) -> bool {
    !t.is_empty() && t.bytes().all(|c| c.is_ascii_hexdigit())
}

/// 这段字符串像人写的字（散文 / JSON / 消息），还是像标识符或二进制块？
pub fn is_readable_text(s: &str) -> bool {
    let total = s.chars().count();
    if total < READABLE_MIN_CHARS {
        return false;
    }
    // Python 的 str.isprintable() 排除全部控制字符与格式字符；这里用 Rust 侧的
    // 近似：控制字符不算可打印，换行与制表符按参考实现额外放行。
    let printable = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .count();
    if (printable as f64) / (total as f64) < READABLE_PRINTABLE_RATIO {
        return false;
    }
    let t = s.trim();
    if is_uuid(t) {
        return false;
    }
    if t.chars().count() >= 8 && is_hex(t) {
        return false;
    }
    // 标识符与键名里没有空格、换行或 CJK；真正的文本有。
    s.contains(' ') || s.contains('\n') || s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

fn collect_leaf_bytes<'a>(b: &'a [u8], depth: usize, out: &mut Vec<&'a [u8]>) {
    if depth > MAX_LEAF_DEPTH {
        return;
    }
    // 解不开的缓冲当成叶子块，静默跳过：这一层只是启发式的输入。
    let Ok(fields) = parse_fields(b) else {
        return;
    };
    for (_, wt, v) in fields {
        if wt == 2
            && let WireValue::Bytes(bytes) = v
        {
            out.push(bytes);
            collect_leaf_bytes(bytes, depth + 1, out);
        }
    }
}

/// payload 里所有可读文本（任意深度）。
pub fn payload_readable_texts(payload: &[u8]) -> Vec<String> {
    let mut leaves = Vec::new();
    collect_leaf_bytes(payload, 0, &mut leaves);
    let mut out = Vec::new();
    for leaf in leaves {
        if let Ok(s) = std::str::from_utf8(leaf)
            && is_readable_text(s)
        {
            out.push(s.to_string());
        }
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_word_or_dash(c: char) -> bool {
    is_word_char(c) || c == '-'
}

/// task 通知 payload 里引用的全部任务 id。
///
/// 形态是 `[\w-]{8,}/task-\d+`（会话 id 里可能带连字符，所以 `[\w-]` 而不是 `\w`）。
/// 仓库没有 regex 依赖，这里手写扫描，语义与参考实现一致：从左到右不重叠匹配，
/// 前缀取紧邻 `/task-` 之前的最长 `[\w-]` 串，且长度至少 8。
pub fn notification_task_ids(payload: &[u8]) -> std::collections::BTreeSet<String> {
    let mut ids = std::collections::BTreeSet::new();
    let mut leaves = Vec::new();
    collect_leaf_bytes(payload, 0, &mut leaves);
    for leaf in leaves {
        let Ok(s) = std::str::from_utf8(leaf) else {
            continue;
        };
        ids.extend(scan_task_ids(s));
    }
    ids
}

/// 在一个字符串里扫出全部 `[\w-]{8,}/task-\d+`。
fn scan_task_ids(s: &str) -> Vec<String> {
    const MARKER: &str = "/task-";
    let chars: Vec<char> = s.chars().collect();
    let marker: Vec<char> = MARKER.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + marker.len() <= chars.len() {
        if chars[i..i + marker.len()] == marker[..] {
            // 后缀：贪婪吃数字。
            let mut j = i + marker.len();
            let digits_start = j;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_start {
                // 前缀：向左吃 `[\w-]`。
                let mut k = i;
                while k > 0 && is_word_or_dash(chars[k - 1]) {
                    k -= 1;
                }
                if i - k >= 8 {
                    out.push(chars[k..j].iter().collect());
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// 事件

/// 一条步事件。
#[derive(Debug, Clone, PartialEq)]
pub struct StepEvent {
    pub idx: i64,
    /// user | agent_text | thought | tool_call | task_notification |
    /// tool_result | error | model_empty | other:<type>
    pub kind: String,
    pub text: String,
    /// tool_call 事件的工具名。
    pub tool: String,
    /// tool_result 事件发起后台任务时的任务 id；task_notification 事件上是它关闭的
    /// 任务 id 集合（逗号分隔，排序后拼接）。
    pub task_id: String,
    /// 步时间戳（payload 元数据 5.1），float 秒。
    pub ts: Option<f64>,
    pub raw_type: i64,
}

impl StepEvent {
    fn new(idx: i64, kind: &str, raw_type: i64) -> Self {
        Self {
            idx,
            kind: kind.to_string(),
            text: String::new(),
            tool: String::new(),
            task_id: String::new(),
            ts: None,
            raw_type,
        }
    }

    pub fn is_kind(&self, kind: &str) -> bool {
        self.kind == kind
    }
}

/// 一次全量解析的结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParseResult {
    /// 按 idx 升序的事件，反映每个 idx 的最新 payload。
    pub events: Vec<StepEvent>,
    /// 被跳过的类型计数：`other:<type>` 与 error(17) 步都计在这里。
    pub other_counts: BTreeMap<i64, u32>,
}

impl ParseResult {
    pub fn by_idx(&self, idx: i64) -> Option<&StepEvent> {
        self.events.iter().find(|e| e.idx == idx)
    }
}

fn step_ts(payload: &[u8]) -> StepResult<Option<f64>> {
    let meta = get_msg(payload, 5)?;
    let ts_msg = opt_msg(meta, 1)?;
    let Some(ts_msg) = ts_msg else {
        return Ok(None);
    };
    let Some(sec) = get_varint(ts_msg, 1)? else {
        return Ok(None);
    };
    let nanos = get_varint(ts_msg, 2)?.unwrap_or(0);
    Ok(Some(sec as f64 + nanos as f64 / 1e9))
}

fn classify(
    idx: i64,
    stype: i64,
    payload: &[u8],
    other_counts: &mut BTreeMap<i64, u32>,
) -> StepResult<StepEvent> {
    let ts = step_ts(payload)?;
    let mut ev = match stype {
        STEP_USER => {
            let body = get_msg(payload, 19)?;
            let Some(text) = opt_str(body, 2)? else {
                return Err(ParseError::Drift(format!(
                    "user step {idx}: no text at 19.2"
                )));
            };
            let mut ev = StepEvent::new(idx, "user", stype);
            ev.text = text.replace('\0', "");
            ev
        }
        STEP_MODEL => {
            let Some(body) = get_msg(payload, 20)? else {
                return Err(ParseError::Drift(format!("model step {idx}: no field 20")));
            };
            let tool = get_msg(body, 7)?;
            let tool_name = opt_str(tool, 2)?.filter(|s| !s.is_empty());
            let text = opt_str(Some(body), 1)?.filter(|s| !s.is_empty());
            if let Some(name) = tool_name {
                let mut ev = StepEvent::new(idx, "tool_call", stype);
                ev.tool = name;
                ev.text = text.unwrap_or_default();
                ev
            } else if let Some(text) = text {
                let mut ev = StepEvent::new(idx, "agent_text", stype);
                ev.text = text;
                ev
            } else if let Some(thought) = opt_str(Some(body), 3)?.filter(|s| !s.is_empty()) {
                let mut ev = StepEvent::new(idx, "thought", stype);
                ev.text = thought;
                ev
            } else {
                StepEvent::new(idx, "model_empty", stype)
            }
        }
        STEP_ERROR => {
            // LS 错误步：错误记录在 field 24，可读信息在 24.3.2。
            // 它带文本，但**不是**漂移 —— 所以计入 other_counts 后照常产出事件。
            *other_counts.entry(stype).or_insert(0) += 1;
            let rec = get_msg(payload, 24)?;
            let sub = opt_msg(rec, 3)?;
            let err = opt_str(sub, 2)?;
            let mut ev = StepEvent::new(idx, "error", stype);
            ev.text = err.unwrap_or_default();
            ev
        }
        STEP_TASK_NOTIFICATION => StepEvent::new(idx, "task_notification", stype),
        STEP_TOOL_RESULT => {
            let mut ev = StepEvent::new(idx, "tool_result", stype);
            let task = get_msg(payload, 148)?;
            if let Some(task) = task {
                ev.task_id = get_str(task, 1)?.unwrap_or_default();
            }
            ev
        }
        other => {
            // 未知类型：带可读文本就是漂移，否则只计数。
            let texts = payload_readable_texts(payload);
            if let Some(first) = texts.first() {
                let head: String = first.chars().take(80).collect();
                return Err(ParseError::Drift(format!(
                    "unknown step_type {other} at idx {idx} carries readable text: {head:?}"
                )));
            }
            *other_counts.entry(other).or_insert(0) += 1;
            StepEvent::new(idx, &format!("other:{other}"), other)
        }
    };
    ev.ts = ts;
    Ok(ev)
}

/// 全量重读一个会话 DB。
///
/// 只读打开（独立连接，不走 bot 自己的 store 连接池：这张库是 agy 的，我们只能读）。
/// 返回的事件按 idx 有序，且反映每个 idx 的最新 payload —— 步会被原地改写，调用方
/// 必须反复调用本函数，不能缓存增量尾巴。
pub async fn parse_steps(db_path: &Path) -> StepResult<ParseResult> {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .read_only(true)
        .create_if_missing(false);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|e| ParseError::Storage(e.to_string()))?;
    let rows: Vec<(i64, i64, Option<Vec<u8>>)> =
        match sqlx::query_as("SELECT idx, step_type, step_payload FROM steps ORDER BY idx")
            .fetch_all(&pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                pool.close().await;
                return Err(ParseError::Storage(e.to_string()));
            }
        };
    pool.close().await;

    let mut raw_payloads: BTreeMap<i64, Vec<u8>> = BTreeMap::new();
    let mut events = Vec::with_capacity(rows.len());
    let mut other_counts = BTreeMap::new();
    for (idx, stype, payload) in rows {
        let payload = payload.unwrap_or_default();
        raw_payloads.insert(idx, payload.clone());
        events.push(classify(idx, stype, &payload, &mut other_counts)?);
    }
    // 第二遍：给 101 通知填上它关闭的任务 id（必须等全量分类跑完才知道引用集合）。
    for ev in &mut events {
        if ev.kind == "task_notification"
            && let Some(payload) = raw_payloads.get(&ev.idx)
        {
            let ids: Vec<String> = notification_task_ids(payload).into_iter().collect();
            ev.task_id = ids.join(",");
        }
    }
    Ok(ParseResult {
        events,
        other_counts,
    })
}

// ---------------------------------------------------------------------------
// 测试

#[cfg(test)]
mod tests;
