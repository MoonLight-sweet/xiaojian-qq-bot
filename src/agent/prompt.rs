//! Prompt construction.
//!
//! Everything the model sees about the group is aliased: `speaker_ref` (`u1`, `u2`)
//! and `message_ref` (`m1`, `m2`) are batch local. `memory_ref` values are the store
//! keys (`mem_{hex}`) of memories handed to the model, never sequential aliases.
//! Raw QQ ids, `message_id`, `group_id` and the owner's number never enter this
//! module — the type system prevents it because [`PromptBatch`] has no field that
//! could carry them.

use crate::agent::contract::MAX_REPLY_CHARS;
use crate::memory::retrieval::MemoryContext;
use crate::onebot::normalize::SpeakerRole;

/// One message as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMessage {
    /// Batch-local alias, e.g. `m1`.
    pub message_ref: String,
    /// Group-local speaker alias, e.g. `u1`.
    pub speaker_ref: String,
    pub speaker_role: SpeakerRole,
    /// Untrusted QQ card or nickname. It is never an identity.
    pub display_name: String,
    /// Untrusted nickname stored from an earlier model proposal.
    pub known_alias: Option<String>,
    /// Plain text with image placeholders.
    pub text: String,
    /// Number of images attached to this message.
    pub images: usize,
    pub mentions_bot: bool,
    /// Aliases of the members this message @-mentions (never the bot:
    /// `mentions_bot` covers her, and @全体成员 has no alias). Rendered as
    /// the line's `mentions` field so the model can tell a line aimed at
    /// somebody else from one aimed at her.
    pub mentions: Vec<String>,
    /// When the message arrived, Unix seconds. Rendered as a `[HH:MM]` prefix so
    /// the model reads a clock instead of guessing one from the conversation.
    /// `0` means unknown and renders no prefix.
    pub received_at: i64,
}

/// Image descriptions produced by the gate for the current batch.
///
/// The agent never receives image bytes. The gate looks at every picture once and
/// writes the result of that look here as text, so the agent reads a description instead
/// of decoding pixels — cheaper, and it keeps image bytes out of the conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptImages {
    /// One line per image, already ordered and referenced by `mN`.
    pub lines: Vec<String>,
    /// Images that could not be fetched, decoded or tagged.
    pub unavailable: usize,
    /// Hashes of the pictures the model was shown this batch. It is the only set a
    /// sticker op may name, so a model cannot reach the library by guessing.
    pub sticker_hashes: Vec<String>,
    /// Per-stage loss counters for the audit trail. The prompt never sees
    /// these; they exist so an image incident is diagnosable after the fact
    /// instead of collapsing into one opaque `unavailable` number.
    pub stats: ImagePrepStats,
}

/// Where a batch's images were lost, stage by stage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImagePrepStats {
    /// Images that reached the tagger and got a description.
    pub described: usize,
    /// Dropped by the per-batch image cap before any work.
    pub capped: usize,
    /// The segment carried no downloadable URL.
    pub no_url: usize,
    /// The download itself failed.
    pub download: usize,
    /// Decode/resize refused the bytes.
    pub prepare: usize,
    /// The vision-fit re-encode gave up.
    pub fit: usize,
    /// Reached the tagger but no tag came back.
    pub undescribed: usize,
}

impl ImagePrepStats {
    pub fn lost(&self) -> usize {
        self.capped + self.no_url + self.download + self.prepare + self.fit + self.undescribed
    }
}

impl PromptImages {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.unavailable == 0
    }
}

/// One library sticker the server pre-matched to this batch (A1′).
///
/// Selection happens in Rust with the same scoring the `sticker_search` tool
/// uses, so the model only ever sees stickers that already cleared the match
/// threshold — or, on a quiet batch, no section at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StickerOffer {
    /// Full content hash. Server-side only: the adoption audit compares the
    /// hashes she actually sent against the ones she was offered.
    pub hash: String,
    /// Eight-character prefix, rendered as `#xxxxxxxx`.
    pub short_hash: String,
    /// The line's headline label: the emotion tag, or the intent when empty.
    pub label: String,
    /// What the sticker is for: usage hint, or scene/OCR as a fallback.
    pub usage: String,
}

/// Everything needed to render a turn.
#[derive(Debug, Clone)]
pub struct PromptBatch {
    pub messages: Vec<PromptMessage>,
    /// Current group mood/topic line, if any.
    pub topic_state: Option<String>,
    pub mood_label: Option<String>,
    /// Retrieved memory context for this group only.
    pub memory_context: MemoryContext,
    /// A digested view of `memory_context` for this batch, produced by the summary model
    /// during the pacing gate's wait. Rendered *instead of* the raw entry list, in the same
    /// position. Pure derivation: it is never stored and never becomes a memory itself.
    /// `None` means the model was slow, absent or malformed — the raw list is used.
    pub digest: Option<String>,
    /// The operator-stored character card read at turn preparation, kept for
    /// the admin record. The card reaches the model through the project
    /// `AGENTS.md` (rendered from this stored card by
    /// [`agents_md_with_persona`]), never as per-turn prompt text.
    pub persona: Option<String>,
    /// Images downloaded and staged by Rust for this batch.
    pub images: PromptImages,
    /// Server-pre-matched library stickers for this batch (A1′). Empty means
    /// the offers section is not rendered at all.
    pub sticker_offers: Vec<StickerOffer>,
    /// One-line hint naming the batch's quote anchor (the earlier message
    /// her reply most likely targets), rendered only when one was detected.
    pub anchor_hint: Option<String>,
    /// Enhanced brief for the first turn of a fresh conversation (rotation
    /// seeding): fold summary + active facts + a raw recent-message window.
    /// `None` on every ordinary continuation turn.
    pub seed_brief: Option<String>,
    /// Continuation-turn annex: a ref table for the previous batch's messages,
    /// whose refs (`p1`, `p2`, ...) stay quotable in this turn even though the
    /// batch itself is gone. Only the number + speaker + truncated text is
    /// rendered — the full messages are already in the model's transcript, and
    /// re-sending them would pay for the same text twice. `None` on the first
    /// turn of a conversation, or when the previous batch left no targets.
    pub annex: Option<String>,
    /// The pending pool flooded and was drained before this batch was
    /// claimed: the render adds a notice that the older messages visible in
    /// context are background, not a queue of questions awaiting answers.
    pub flooded: bool,
    /// Flood stage 4: the drained overflow compressed into a short
    /// background brief, rendered as its own block ahead of the live
    /// batch. `None` unless this batch is the first claim after a drain
    /// (or the summary landed late, after that claim already left).
    pub flood_background: Option<String>,
    /// MCP output channel: this turn's submission token, rendered into the
    /// contract card so the model can present it to `submit_reply`. `None`
    /// on the text channel, where the card renders in its text form.
    pub turn_token: Option<String>,
    /// The clock line at the top of the prompt, produced when the batch was
    /// assembled ([`now_line`]). `None` renders nothing.
    pub now_line: Option<String>,
}

/// Timezone the clock is rendered in: Asia/Shanghai is UTC+8 all year, so a fixed
/// offset is exact and, unlike `Local`, testable regardless of the host's clock.
const CLOCK_OFFSET_SECONDS: i32 = 8 * 3600;

const WEEKDAYS: [&str; 7] = [
    "星期一",
    "星期二",
    "星期三",
    "星期四",
    "星期五",
    "星期六",
    "星期日",
];

/// One instant as seen in Asia/Shanghai, as `(date, weekday index, hour, minute)`.
fn clock_parts(ts: i64) -> Option<(String, usize, u32, u32)> {
    use chrono::Datelike;
    use chrono::Timelike;
    let offset = chrono::FixedOffset::east_opt(CLOCK_OFFSET_SECONDS)?;
    let local = chrono::TimeZone::timestamp_opt(&offset, ts, 0).single()?;
    Some((
        local.format("%Y-%m-%d").to_string(),
        local.weekday().num_days_from_monday() as usize,
        local.hour(),
        local.minute(),
    ))
}

/// The batch header's clock line: `现在：2026-10-28 星期三 21:07`.
pub fn now_line(ts: i64) -> String {
    match clock_parts(ts) {
        Some((date, weekday, hour, minute)) => {
            format!("现在：{date} {} {hour:02}:{minute:02}", WEEKDAYS[weekday])
        }
        None => String::new(),
    }
}

/// The per-message prefix `[21:07] `, or nothing when the timestamp is unknown.
fn clock_prefix(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    match clock_parts(ts) {
        Some((_, _, hour, minute)) => format!("[{hour:02}:{minute:02}] "),
        None => String::new(),
    }
}

/// Compiled-in character card, byte-for-byte. It is the fallback: an operator
/// card stored through the admin API replaces it in the rendered `AGENTS.md`
/// (see [`agents_md_with_persona`]). The output contract appended after the
/// card is compiled-in and can never be replaced from storage.
pub const DEFAULT_PERSONA: &str = include_str!("persona_card.txt");

/// Agy 1.4.7 stops reading an instruction file past this size.
pub const AGENTS_MD_MAX_BYTES: usize = 64_000;

/// The persistent instruction file Agy reads on its own, once per run, from its working
/// directory.
///
/// This is what makes the character card and the output contract *persistent* instead of
/// being paid for as prompt text on every turn: they are written into the group's project
/// directory once (and only rewritten when this string changes), and Agy loads them
/// itself. The prompt then carries only what is genuinely per-batch.
pub fn agents_md() -> String {
    agents_md_for(crate::config::OutputChannel::Text)
}

/// The persistent instruction file for one output channel. The two channel
/// contracts share the persona and every rule; they differ only in how the
/// turn's result is delivered (see [`contract_for`]).
pub fn agents_md_for(channel: crate::config::OutputChannel) -> String {
    agents_md_with_persona(channel, None)
}

/// The persistent instruction file for one output channel, rendered with the
/// operator-stored character card when one exists. The card is the only part
/// storage may replace; the contract is always the compiled-in one. A blank
/// stored card counts as no card. Edits propagate because the caller rewrites
/// the project `AGENTS.md` whenever this string changes.
pub fn agents_md_with_persona(
    channel: crate::config::OutputChannel,
    persona: Option<&str>,
) -> String {
    let card = persona
        .map(str::trim)
        .filter(|card| !card.is_empty())
        .unwrap_or(DEFAULT_PERSONA);
    format!("{card}\n\n{}", contract_for(channel))
}

fn contract_for(channel: crate::config::OutputChannel) -> String {
    match channel {
        crate::config::OutputChannel::Text => OUTPUT_CONTRACT.to_string(),
        crate::config::OutputChannel::Mcp => mcp_contract(),
    }
}

/// The MCP channel's output contract, derived from [`OUTPUT_CONTRACT`] by
/// targeted replacements so the shared rules can never drift apart: only
/// the delivery framing, the tool list in rule 10, and three submission
/// rules appended at the end differ.

/// Render the full prompt for one batch.
pub fn render(batch: &PromptBatch) -> String {
    let mut out = String::with_capacity(1_024);

    // No character card and no output contract here: both live in AGENTS.md, which Agy
    // reads from the turn cwd (a hardlink of the project's card). Re-sending them as
    // prompt text would pay for ~11 KB of prompt prefix on every single batch.

    // The clock rides at the very top, before the group state: the model has no
    // other way to know what time it is, and an unlabelled "现在" is exactly what
    // makes it invent one (2026-10-28, user-directed).
    if let Some(now) = &batch.now_line
        && !now.trim().is_empty()
    {
        out.push_str(now.trim());
        out.push_str("\n\n");
    }

    out.push_str("# 当前群状态\n");
    match &batch.topic_state {
        Some(topic) if !topic.trim().is_empty() => {
            out.push_str(&format!("话题：{}\n", topic.trim()));
        }
        _ => out.push_str("话题：未记录\n"),
    }
    match &batch.mood_label {
        Some(mood) if !mood.trim().is_empty() => {
            out.push_str(&format!("氛围：{}\n", mood.trim()));
        }
        _ => out.push_str("氛围：未记录\n"),
    }
    out.push('\n');

    if let Some(seed) = &batch.seed_brief {
        out.push_str("# 前情提要（新会话播种）\n");
        out.push_str("这是新会话的第一轮：先读这份提要接上文，再看下面的待处理消息。\n");
        out.push_str(seed);
        out.push_str("\n\n");
    }

    // Continuation annex: the previous batch is still in the transcript above,
    // but its refs died with that turn. This table renames those messages so a
    // reply can still quote one directly instead of pinning the quote on an
    // unrelated new message (a silent misquote).
    if let Some(annex) = &batch.annex
        && !annex.trim().is_empty()
    {
        out.push_str("# 上一轮消息（续接轮的引用编号）\n");
        out.push_str("这些消息在你上下文里更早的位置；编号 [pN] 是它们在本轮唯一可引用的名字。\n");
        out.push_str(
            "答其中某条时，把它的 pN 填进那一段的 quote_ref；本轮的 m 编号是另一批消息，别混。\n",
        );
        out.push_str(annex.trim());
        out.push_str("\n\n");
    }

    out.push_str("# 记忆上下文（只属于当前群）\n");
    match &batch.digest {
        Some(digest) => out.push_str(digest),
        None => out.push_str(&batch.memory_context.render()),
    }
    out.push('\n');
    if !batch.memory_context.empty {
        out.push_str(
            "本列表之外的更早细节，用 call_mcp_tool 调用 memory_search 查；不确定就直说不知道，不要编。\n",
        );
        out.push('\n');
    }

    if batch.flooded {
        out.push_str("# 积压提示\n");
        out.push_str("刚才群里消息积压成灾，系统已经把较早的一批降为背景：你在上下文里看到的那些旧消息只是背景，不用逐条回应，也不用挨个回答里面的问题。只回应下面这批最新消息的话题。\n\n");
    }
    if let Some(background) = &batch.flood_background {
        out.push_str("# 积压背景\n");
        out.push_str("（较早一批积压消息的摘要，只是背景，不用逐条回应。）\n");
        out.push_str(background.trim());
        out.push_str("\n\n");
    }
    out.push_str("# 待处理消息\n");
    if batch.messages.is_empty() {
        out.push_str("（无）\n");
    }
    for run in runs(&batch.messages) {
        // `role` is assigned here. Display text is only a JSON string, so a nickname
        // cannot close the object and invent another role.
        let head = &batch.messages[run.start];
        let members = &batch.messages[run.start..run.start + run.len];
        let mut line = serde_json::json!({
            "message_ref": head.message_ref,
            "speaker_ref": head.speaker_ref,
            "role": head.speaker_role.as_str(),
            "display_name": head.display_name,
            "known_alias": head.known_alias,
            "text": head.text.trim(),
            "mentions_bot": head.mentions_bot,
            "images": head.images,
        });
        // A run merges several messages into one line; the addressees of
        // every merged message stay visible as the union, in order.
        let mut mentions: Vec<&str> = Vec::new();
        for m in members {
            for r in &m.mentions {
                if !mentions.contains(&r.as_str()) {
                    mentions.push(r.as_str());
                }
            }
        }
        if !mentions.is_empty() {
            line["mentions"] = serde_json::json!(mentions);
        }
        // A run is merged into one line to save tokens, but every message id stays
        // visible and in order: `message_ref` + `also` line up one-to-one with the texts
        // in `text`, so naming any single message in `source_refs` still works. Dropping
        // the ids would silently make the middle of a burst unreferenceable.
        if run.len > 1 {
            let rest: Vec<&str> = members[1..]
                .iter()
                .map(|m| m.message_ref.as_str())
                .collect();
            let identical = members.iter().all(|m| m.text.trim() == head.text.trim());
            if identical {
                // Spam: state the count instead of repeating the text.
                line["repeat"] = serde_json::json!(run.len);
            } else {
                let joined = members
                    .iter()
                    .map(|m| m.text.trim())
                    .collect::<Vec<_>>()
                    .join(RUN_SEPARATOR);
                line["text"] = serde_json::json!(joined);
            }
            line["also"] = serde_json::json!(rest);
        }
        // The `[HH:MM]` prefix is a prefix on the line, not a JSON field: it keeps the
        // object shape byte-identical to what the contract documents while still
        // telling the model when each message landed. A merged run carries the
        // arrival time of its first message.
        out.push_str(&clock_prefix(head.received_at));
        out.push_str(&line.to_string());
        out.push('\n');
    }
    let image_count = batch.messages.iter().map(|m| m.images).sum::<usize>();
    if image_count > 0 {
        out.push_str("\n# 图片\n");
        if batch.images.lines.is_empty() {
            out.push_str("这批图片没有拿到可用描述；不可根据占位符推测内容，也不要说它们模糊或看不清，就当没发过。\n");
        } else {
            out.push_str("图片已由前置识别为文字描述，你不需要也无法直接读图：\n");
            for line in &batch.images.lines {
                out.push_str("- ");
                out.push_str(line);
                out.push('\n');
            }
        }
        if batch.images.unavailable > 0 {
            out.push_str(&format!(
                "另有 {} 张图片未能识别：不要评价它们（不要说糊、暗、看不清），不要推断其内容，就当它们不存在。\n",
                batch.images.unavailable
            ));
        }
        if batch.images.lines.iter().any(|l| l.contains("表情 #")) {
            out.push_str(
                "看到带「表情 #xxxxxxxx」的图时，用 call_mcp_tool 调 sticker_keep 决定收不收：\n\
                 verdict 用 self 就是收进你的表情库（以后能发），用 ignore 就是以后不再问你这张。\n\
                 只收你自己喜欢、真的会拿来用的；不置可否就不调，它会先待在缓存里。\n",
            );
        }
    }

    if let Some(hint) = &batch.anchor_hint {
        out.push_str("\n# 引用提示\n");
        out.push_str(hint);
        out.push('\n');
    }

    if !batch.sticker_offers.is_empty() {
        out.push_str("\n# 本轮的表情候选（已挑好）\n");
        out.push_str(
            "这几张是按这批话的气氛从你的表情库里先挑出来的。想发表情就优先从这里拿，用 send 发——跟在你的话后面补一张，或者直接甩一张都行，不用再搜：\n",
        );
        for offer in &batch.sticker_offers {
            out.push_str(&format!(
                "- #{} {}·{}\n",
                offer.short_hash, offer.label, offer.usage
            ));
        }
    }
    out.push('\n');
    // The full contract lives in AGENTS.md, read once when the session
    // opened; a hundred steps later its grip on the exact field shapes has
    // faded, which is where the deep-step contract_invalid cluster lives.
    // This compact card rides every turn so the skeleton is the last thing
    // the model sees before it writes (2026-10-27, user-directed: resend
    // per turn instead of repairing after the fact).
    match &batch.turn_token {
        Some(token) => out.push_str(&contract_card_mcp(token)),
        None => out.push_str(CONTRACT_CARD),
    }
    out.push('\n');

    out
}

/// Compact per-turn echo of [`OUTPUT_CONTRACT`]'s skeleton. Deliberately a
/// reminder, not a second contract: the rules live in AGENTS.md, this only
/// pins the field shapes and the easiest rules to lose at depth — the JSON
/// skeleton, and the send-a-sticker default that the persona card alone
/// did not make stick (2026-10-27, user-directed).
pub const CONTRACT_CARD: &str = "# 合约速记（照此输出）\n\
只输出一个 JSON 对象，无解释、无前缀、无代码块：\n\
{\"should_reply\":true,\"reply_parts\":[\"一句话一段\",{\"text\":\"答他的那段\",\"quote_ref\":\"m2\",\"quote_echo\":\"原文片段\"}],\"quote_ref\":\"m2\",\"mood_update\":{\"label\":\"情绪词\",\"reason\":\"为什么\"},\"topic_state\":\"当前话题一句话\",\"memory_ops\":[],\"sticker_ops\":[{\"op\":\"send\",\"hash\":\"a1b2c3d4\"}]}\n\
- 不想说：{\"should_reply\":false}，其余字段可省。\n\
- 闲聊轮次默认带一张表情：sticker_ops 里 send 本轮候选中的一张，跟在话后面发；正经答题、气氛严肃、或上一轮刚发过才免。只发一张，不要连发。\n\
- reply_parts 最多 8 段、合计 ≤1000 字、纯文本无 CQ 码；逐段引用才写对象形，quote_echo 逐字摘自目标原文（≤8 字）。\n\
- ref 只用本批出现过的 m/h 编号、u 别名与 mem_ 记忆键，不编造；memory_ops 只用于纠错，update 必须带 memory_ref。\n\
- 时间以本轮标注为准：提示词顶部的「现在」是本轮组批时刻，消息行首的 [HH:MM] 是那条消息的到达时刻。不要自己推算或编造时间、日期、星期。\n\
- 可以戳一下：poke_ref 填本批某个 speaker_ref，系统会替你戳他一下（打招呼、催他回话、开个玩笑）。一轮最多一个，should_reply 必须为 true；想纯戳不说话就 reply_parts 留空、只带 poke_ref 和贴纸。别连戳，也别拿来骚扰人。";

/// One merged run of consecutive messages from the same speaker with the same text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    start: usize,
    len: usize,
}

/// Longest run that is still merged. Above this the burst is left as separate lines so a
/// wall of spam stays readable and its ids are never hidden behind one summary line.
const MAX_MERGED_RUN: usize = 20;

/// Group consecutive identical messages from one speaker.
///
/// Only exact repeats next to each other merge, and only when the speaker and the text
/// both match, so normal back-and-forth is untouched. Images are never merged: two
/// pictures are two different pictures even when the caption repeats.
fn runs(messages: &[PromptMessage]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        let cur = &messages[i];
        let mut len = 1;
        if can_start_run(cur) {
            while i + len < messages.len() {
                let next = &messages[i + len];
                if !can_join_run(cur, next) || len >= MAX_MERGED_RUN {
                    break;
                }
                len += 1;
            }
        }
        out.push(Run { start: i, len });
        i += len;
    }
    out
}

/// A run only starts on a plain text message: images are never merged because two
/// pictures are two different pictures, and a mention is a property of one message.
fn can_start_run(m: &PromptMessage) -> bool {
    m.images == 0 && !m.text.trim().is_empty()
}

/// One speaker talking twice in a row is one utterance split by the send key, so the two
/// lines become one. `role`, `display_name` and `mentions_bot` all have to match, which
/// means merging never changes who said it or whether the bot was addressed.
fn can_join_run(cur: &PromptMessage, next: &PromptMessage) -> bool {
    next.speaker_ref == cur.speaker_ref
        && next.speaker_role == cur.speaker_role
        && next.display_name == cur.display_name
        && next.known_alias == cur.known_alias
        && next.mentions_bot == cur.mentions_bot
        && can_start_run(next)
}

/// Separator between the merged lines of one speaker. Rendered as a visible break so the
/// model can tell where one send ended and the next began.
const RUN_SEPARATOR: &str = " ／ ";

/// Character budget check for a rendered prompt, so a pathological batch cannot be
/// sent. Returns the length in characters.
pub fn rendered_len(prompt: &str) -> usize {
    prompt.chars().count()
}

/// Hard ceiling for a rendered prompt (the model context is far larger, but the bot
/// refuses to send an obviously runaway prompt).
pub const MAX_PROMPT_CHARS: usize = 24_000;

/// Whether a rendered prompt is within limits and contains no forbidden markup.
pub fn prompt_is_safe(prompt: &str) -> bool {
    rendered_len(prompt) <= MAX_PROMPT_CHARS && !prompt.contains("[CQ:")
}

/// A compact reminder of the reply length cap, used by tests and by the admin page.
pub fn reply_cap_note() -> String {
    format!("回复不超过 {MAX_REPLY_CHARS} 字")
}

mod cards;

use self::cards::{OUTPUT_CONTRACT, contract_card_mcp, mcp_contract};

#[cfg(test)]
mod tests;
