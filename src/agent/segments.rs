//! Reply segmentation and send pacing.
//!
//! The model chooses the sentences. This module only trims, drops empty pieces,
//! strips control characters, and splits a piece that still contains a newline.
//! It does not rewrite wording. When the model omits `reply_parts`, the fallback
//! cuts `reply_text` on sentence punctuation and newlines.

/// Hard cap on how many QQ messages one reply may become.
pub const MAX_REPLY_PARTS: usize = 8;

/// Shortest pause between two outgoing messages, in milliseconds.
pub const MIN_GAP_MS: u64 = 800;
/// Longest pause between two outgoing messages, in milliseconds.
pub const MAX_GAP_MS: u64 = 1_400;

/// Turn model output into the messages that will actually be sent.
///
/// `reply_parts`, when non-empty after cleaning, wins. Otherwise `reply_text` is
/// split on `。！？!?；;` and newlines.
pub fn finalize_reply(reply_text: &str, reply_parts: Option<&[String]>) -> Vec<String> {
    if let Some(parts) = reply_parts {
        let cleaned = clean_parts(parts);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    clean_parts(&[split_fallback(reply_text).join("\n")])
}

/// A quote the model attached to one reply part, before validation: the
/// batch/history `message_ref` that part answers, plus a short verbatim
/// snippet of the target message the model copied as evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPartQuote {
    pub quote_ref: String,
    pub quote_echo: Option<String>,
}

/// [`finalize_reply`] variant that carries per-part quotes through the same
/// cleaning: a raw part's quote lands on the FIRST final line that part
/// produces (cleaning can split a part on newlines; the follow-up lines are
/// continuations and take no quote).
pub fn finalize_reply_with_quotes(
    reply_text: &str,
    reply_parts: Option<&[(String, Option<RawPartQuote>)]>,
) -> (Vec<String>, Vec<(usize, RawPartQuote)>) {
    if let Some(parts) = reply_parts {
        let mut out: Vec<String> = Vec::new();
        let mut quotes: Vec<(usize, RawPartQuote)> = Vec::new();
        for (text, quote) in parts {
            let mut landed = false;
            for line in text.split(['\n', '\r']) {
                let cleaned = strip_controls(line).trim().to_string();
                if cleaned.is_empty() {
                    continue;
                }
                if !landed {
                    if let Some(quote) = quote {
                        quotes.push((out.len(), quote.clone()));
                    }
                    landed = true;
                }
                out.push(cleaned);
            }
        }
        if !out.is_empty() {
            return (out, quotes);
        }
    }
    (
        clean_parts(&[split_fallback(reply_text).join("\n")]),
        Vec::new(),
    )
}

/// Pauses after each part except the last. A single part yields an empty list,
/// so the caller does not sleep after the final message.
///
/// The pause grows with the previous part's length and stays inside 800..=1400.
pub fn message_gaps_ms(parts: &[impl AsRef<str>]) -> Vec<u64> {
    parts
        .windows(2)
        .map(|pair| {
            let chars = pair[0].as_ref().chars().count() as u64;
            MIN_GAP_MS
                .saturating_add(chars.saturating_mul(12))
                .min(MAX_GAP_MS)
        })
        .collect()
}

/// 拟真打字停顿的最短间隔（毫秒）。
pub const HUMAN_MIN_GAP_MS: u64 = 600;
/// 拟真打字停顿的最长间隔（毫秒）。
pub const HUMAN_MAX_GAP_MS: u64 = 4_000;

/// 按下一条消息的字数估算“真人打字所需停顿”，并叠加稳定的小幅随机感波动，
/// 避免连续多条消息像复制粘贴一样机械等距。
pub fn humanized_gaps_ms(parts: &[impl AsRef<str>]) -> Vec<u64> {
    parts
        .windows(2)
        .enumerate()
        .map(|(idx, pair)| {
            let next_text = pair[1].as_ref();
            let next_chars = next_text.chars().count() as u64;
            // 用内容字节做一个轻量的确定性混合，产生 0..=1500ms 的明显波动，
            // 让几条消息之间有快有慢，而不是每条都隔差不多两秒
            let mut hash: u64 = 0x517c_c1b7_2722_0a95 ^ (idx as u64).wrapping_mul(31);
            for b in next_text.as_bytes() {
                hash = hash.wrapping_mul(1_099_511_628_211) ^ (*b as u64);
            }
            let jitter = hash % 1501;
            450u64
                .saturating_add(next_chars.saturating_mul(70))
                .saturating_add(jitter)
                .clamp(HUMAN_MIN_GAP_MS, HUMAN_MAX_GAP_MS)
        })
        .collect()
}

/// 判断一条消息是否属于代码、链接、分步教程或固定问候，命中时不做口语化拆改，防止误伤正经内容。
fn is_structured_or_formal_part(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.starts_with("Ciallo") {
        return true;
    }
    if t.contains("http://")
        || t.contains("https://")
        || t.contains('`')
        || t.contains('\n')
        || t.contains("::")
        || t.contains("->")
        || t.contains(" --")
        || t.contains(":\\")
        || t.contains("/opt/")
        || t.contains("/etc/")
    {
        return true;
    }
    // 列表或分步骤格式开头
    let list_prefix = t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("• ")
        || t.starts_with("（1）")
        || t.starts_with("(1)")
        || t.starts_with("1.")
        || t.starts_with("1、")
        || t.starts_with("2.")
        || t.starts_with("2、")
        || t.starts_with("①");
    if list_prefix {
        return true;
    }
    // 同一段里同时包含多个步骤编号
    if (t.contains("1.") && t.contains("2.")) || (t.contains("(1)") && t.contains("(2)")) {
        return true;
    }
    false
}

/// 去掉句尾孤立的单个中文句号「。」；保留「？」「！」「～」「……」「...」以及刻意打出的连续句号「。。」。
fn strip_single_trailing_period(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.ends_with('。') {
        return trimmed.to_string();
    }
    let without_last = trimmed.strip_suffix('。').unwrap_or(trimmed);
    // 保留连续句号（如“无语。。”），只剥离单个书面的收尾句号
    if without_last.ends_with('。') {
        return trimmed.to_string();
    }
    without_last.trim_end().to_string()
}

/// 在不落入引号、括号、书名号内部的前提下，按单个中文句号「。」拆开（跳过「。。」）。
fn split_by_single_cjk_period(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth: i32 = 0;
    let mut in_ascii_quote = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '「' | '『' | '（' | '(' | '《' | '【' | '“' => depth += 1,
            '」' | '』' | '）' | ')' | '》' | '】' | '”' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            '"' => in_ascii_quote = !in_ascii_quote,
            _ => {}
        }
        if ch == '。' && depth == 0 && !in_ascii_quote {
            let prev_is_period = i > 0 && chars[i - 1] == '。';
            let next_is_period = i + 1 < chars.len() && chars[i + 1] == '。';
            if !prev_is_period && !next_is_period {
                let piece = current.trim().to_string();
                if !piece.is_empty() {
                    parts.push(piece);
                }
                current.clear();
                i += 1;
                continue;
            }
        }
        current.push(ch);
        i += 1;
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

/// 若单条日常闲聊偏长（22~65字），在靠近中段（30%~70%）且不在引号/括号内的中文逗号「，」处自然拆成两条。
fn maybe_split_mid_comma(text: &str) -> Option<(String, String)> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if !(22..=65).contains(&len) {
        return None;
    }
    let target = len / 2;
    let min_pos = (len * 30 / 100).max(6);
    let max_pos = (len * 70 / 100).min(len.saturating_sub(6));
    if min_pos > max_pos {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_ascii_quote = false;
    let mut best: Option<(usize, usize)> = None; // (distance_to_mid, char_index)
    for (idx, &ch) in chars.iter().enumerate() {
        match ch {
            '「' | '『' | '（' | '(' | '《' | '【' | '“' => depth += 1,
            '」' | '』' | '）' | ')' | '》' | '】' | '”' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            '"' => in_ascii_quote = !in_ascii_quote,
            _ => {}
        }
        if ch == '，' && depth == 0 && !in_ascii_quote && idx >= min_pos && idx <= max_pos {
            let dist = idx.abs_diff(target);
            if best.is_none_or(|(best_dist, _)| dist < best_dist) {
                best = Some((dist, idx));
            }
        }
    }
    let (_, split_at) = best?;
    let left: String = chars[..split_at]
        .iter()
        .collect::<String>()
        .trim()
        .to_string();
    let right: String = chars[split_at + 1..]
        .iter()
        .collect::<String>()
        .trim()
        .to_string();
    if left.chars().count() >= 6 && right.chars().count() >= 6 {
        Some((left, right))
    } else {
        None
    }
}

/// 发消息前对日常闲聊段落做轻量拟人化处理：
/// 1) 剥离残余句尾颜文字；
/// 2) 保护代码、链接、步骤清单与固定问候不被误改；
/// 3) 对日常闲聊去掉生硬收尾句号，并把混杂句中句号或过长的单条中长闲聊适度拆成自然短句；
/// 4) 保持引用编号对齐（若一段拆成多段，原引用仅挂在第一段上），且总条数不超过 `MAX_REPLY_PARTS`。
pub fn humanize_chat_parts(
    parts: &[String],
    quote_ids: &[Option<String>],
) -> (Vec<String>, Vec<Option<String>>) {
    let mut out_parts: Vec<String> = Vec::with_capacity(parts.len() + 2);
    let mut out_quotes: Vec<Option<String>> = Vec::with_capacity(parts.len() + 2);
    let single_model_part = parts.iter().filter(|p| !p.trim().is_empty()).count() == 1;
    let mut any_structured = false;

    for (idx, raw) in parts.iter().enumerate() {
        let quote = quote_ids.get(idx).cloned().unwrap_or(None);
        let base = strip_trailing_kaomoji(raw);
        let base = base.trim().to_string();
        if base.is_empty() {
            continue;
        }
        if is_structured_or_formal_part(&base) {
            any_structured = true;
            out_parts.push(base);
            out_quotes.push(quote);
            continue;
        }
        let char_len = base.chars().count();
        // 超过 65 字的较长说明不乱拆，只顺手去掉末尾孤立的中文句号
        if char_len > 65 {
            let cleaned = strip_single_trailing_period(&base);
            if !cleaned.is_empty() {
                out_parts.push(cleaned);
                out_quotes.push(quote);
            }
            continue;
        }

        // 句中单个句号：模型只给了 1 段时才拆成短句（最多拆成 CHAT_MAX_BUBBLES 条）；
        // 模型自己已经分了多段时，就把句中句号换成逗号，不再额外增加气泡数。
        let period_pieces: Vec<String> = split_by_single_cjk_period(&base)
            .into_iter()
            .map(|p| strip_single_trailing_period(&p))
            .filter(|p| !p.is_empty())
            .collect();
        let mut expanded: Vec<String> = if period_pieces.len() > 1
            && single_model_part
            && period_pieces.len() <= CHAT_MAX_BUBBLES
        {
            period_pieces
        } else if period_pieces.len() > 1 {
            vec![period_pieces.join("，")]
        } else {
            let single = strip_single_trailing_period(&base);
            if single.is_empty() {
                Vec::new()
            } else {
                vec![single]
            }
        };

        // 只有模型整轮只给了 1 段、且这段仍是 1 条时，才在中段逗号处拆成两条短气泡
        if single_model_part
            && expanded.len() == 1
            && let Some((left, right)) = maybe_split_mid_comma(&expanded[0])
        {
            expanded = vec![left, right];
        }

        let mut attached_quote = quote;
        for piece in expanded {
            if out_parts.len() >= MAX_REPLY_PARTS {
                break;
            }
            out_parts.push(piece);
            out_quotes.push(attached_quote.take());
        }
    }

    // 纯日常闲聊（没有代码/链接/步骤，且每条都是短句）最多发 CHAT_MAX_BUBBLES 条，
    // 防止一次连发四五条像机关枪；正经长内容不受此限制，避免丢信息。
    let all_short_chat = !any_structured
        && out_parts
            .iter()
            .all(|p| p.chars().count() <= CHAT_SHORT_PART_CHARS);
    if all_short_chat && out_parts.len() > CHAT_MAX_BUBBLES {
        out_parts.truncate(CHAT_MAX_BUBBLES);
        out_quotes.truncate(CHAT_MAX_BUBBLES);
    }

    (out_parts, out_quotes)
}

/// 日常闲聊一轮最多发几条文字气泡。
pub const CHAT_MAX_BUBBLES: usize = 3;
/// 不超过这个字数的段落算“短闲聊句”。
pub const CHAT_SHORT_PART_CHARS: usize = 40;

/// 表情包冷却：按“机器人连续发言为一轮”切分最近的群消息（旧→新），
/// 只要最近 `rounds` 轮机器人发言里出现过表情包，本轮就不再附带表情。
/// `history` 每项为 `(是否机器人发言, 是否表情包)`。
pub fn sticker_on_cooldown(history: &[(bool, bool)], rounds: usize) -> bool {
    let mut seen_rounds = 0usize;
    let mut in_bot_round = false;
    let mut round_has_sticker = false;
    for &(is_bot, is_sticker) in history.iter().rev() {
        if is_bot {
            if !in_bot_round {
                in_bot_round = true;
                round_has_sticker = false;
            }
            round_has_sticker |= is_sticker;
        } else if in_bot_round {
            if round_has_sticker {
                return true;
            }
            seen_rounds += 1;
            in_bot_round = false;
            if seen_rounds >= rounds {
                return false;
            }
        }
    }
    in_bot_round && round_has_sticker
}

fn clean_parts(parts: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for part in parts {
        for line in part.split(['\n', '\r']) {
            let cleaned = strip_controls(line).trim().to_string();
            let stripped = strip_trailing_kaomoji(&cleaned);
            if !stripped.is_empty() {
                out.push(stripped);
            }
        }
    }
    out
}

/// 剥离句尾格式化的括号颜文字（如 `(¬‿¬)`、`(=｀ω´=)`、`( ´_ゝ｀)`、`(*^▽^*)` 等）。
/// 保持打招呼 `Ciallo～(∠・ω< )⌒☆ 我是小简` 等固定短语完整。
pub fn strip_trailing_kaomoji(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with("Ciallo") {
        return trimmed.to_string();
    }

    // 检查末尾是否闭合括号
    let last_char = match trimmed.chars().last() {
        Some(c) => c,
        None => return String::new(),
    };
    if last_char != ')' && last_char != '）' {
        return trimmed.to_string();
    }

    let is_open_bracket = |c: char| c == '(' || c == '（';
    // 寻找与末尾括号匹配的最近开括号
    let chars: Vec<char> = trimmed.chars().collect();
    let mut open_idx = None;
    for i in (0..chars.len() - 1).rev() {
        if is_open_bracket(chars[i]) {
            open_idx = Some(i);
            break;
        }
    }

    let Some(start) = open_idx else {
        return trimmed.to_string();
    };

    // 括号内的内容切片
    let inside: String = chars[start + 1..chars.len() - 1].iter().collect();
    let inside = inside.trim();

    // 如果括号内为空，或者超过20个字符，或者包含中文汉字，则不视作短颜文字
    if inside.is_empty() || inside.chars().count() > 20 {
        return trimmed.to_string();
    }

    // 颜文字特征：包含典型的颜文字符号或口癖符号，且不包含常用汉字
    let has_cjk = inside
        .chars()
        .any(|c| ('\u{4e00}'..='\u{9fa5}').contains(&c));
    if has_cjk {
        return trimmed.to_string();
    }

    // 检查是否包含颜文字常见构造成分（如 ¬, ‿, ω, ゝ, ╯, ︶, ▽, ◕, ﾟ, ｡, ¯, ˘, ⊙, ∀, ᐛ, ᐕ, 皿, 益 等，或纯符号混合）
    let is_symbolic = inside.chars().all(|c| {
        !c.is_alphanumeric()
            || matches!(
                c,
                'v' | 'V' | 'o' | 'O' | 'x' | 'X' | 'w' | 'W' | 'u' | 'U' | 'T' | 'Q' | 'p' | 'P'
            )
    });
    let has_kaomoji_chars = inside.chars().any(|c| {
        matches!(
            c,
            '¬' | '‿'
                | 'ω'
                | 'ゝ'
                | '╯'
                | '︶'
                | '▽'
                | '◕'
                | 'ﾟ'
                | '｡'
                | '¯'
                | '˘'
                | '⊙'
                | '∀'
                | 'ᐛ'
                | 'ᐕ'
                | 'Д'
                | 'Д'
                | 'д'
                | 'ㅂ'
                | '꒳'
                | 'ᴗ'
                | 'ᆺ'
                | 'ㅅ'
                | '・'
                | '´'
                | '`'
                | '*'
                | '^'
                | '-'
                | '='
                | '_'
                | '/'
                | '\\'
                | '<'
                | '>'
        )
    });

    if is_symbolic || has_kaomoji_chars {
        let prefix: String = chars[..start].iter().collect();
        let prefix = prefix.trim_end();
        if prefix.is_empty() {
            // 如果整句就只有一个颜文字，保留或者清空？如果全句只有颜文字则返回空
            return String::new();
        }
        return prefix.to_string();
    }

    trimmed.to_string()
}

fn split_fallback(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch == '\n' || ch == '\r' {
            push_trimmed(&mut parts, &current);
            current.clear();
            continue;
        }
        current.push(ch);
        if matches!(ch, '。' | '！' | '？' | '!' | '?' | '；' | ';') {
            push_trimmed(&mut parts, &current);
            current.clear();
        }
    }
    push_trimmed(&mut parts, &current);
    parts
}

fn push_trimmed(parts: &mut Vec<String>, text: &str) {
    let trimmed = strip_controls(text).trim().to_string();
    if !trimmed.is_empty() {
        parts.push(trimmed);
    }
}

fn strip_controls(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() && !matches!(ch, '\u{2028}' | '\u{2029}' | '\u{feff}'))
        .collect()
}

/// Placeholder that replaces a credential-shaped run in an outgoing message.
pub const FILTERED: &str = "[Filtered]";

/// Marker words that make a following value look like a credential.
const SECRET_KEYS: [&str; 6] = ["token", "secret", "password", "passwd", "api_key", "apikey"];

/// True when the reply reads like it is quoting a credential.
///
/// Two independent signals, both precise enough to leave ordinary chat alone:
/// a marker word followed by an assigned value, or an exact match against a value
/// this deployment actually holds (checked separately, see [`filter_secrets`]).
///
/// A generic "looks random" heuristic was tried and rejected: QQ media URLs carry
/// long digit-bearing query params, so it flagged normal links.
pub fn contains_named_credential(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|key| {
        lower.match_indices(key).any(|(at, _)| {
            let rest = &text[at + key.len()..];
            let rest = rest.trim_start();
            let rest = rest.strip_prefix('"').unwrap_or(rest);
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix([':', '=']) else {
                return false;
            };
            rest.trim().trim_start_matches('"').trim().len() >= 8
        })
    })
}

/// Mask anything that must not leave the process.
///
/// `known` holds the exact secret values loaded at startup, so matching is
/// substring-exact: no guessing, no false positives on ordinary text or URLs.
/// Named credentials are masked too, because the model may describe a key it read
/// elsewhere without the bytes matching one we hold.
pub fn filter_secrets(text: &str, known: &[String]) -> String {
    let mut out = text.to_string();
    for secret in known {
        // Very short values would mask unrelated text; real keys are far longer.
        if secret.len() >= 16 && out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), FILTERED);
        }
    }
    if contains_named_credential(&out) {
        out = mask_named_values(&out);
    }
    out
}

/// Replace the value after a marker word, keeping the sentence readable.
fn mask_named_values(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for key in SECRET_KEYS {
        for (at, _) in lower.match_indices(key) {
            let mut value_at = at + key.len();
            // Skip `: `, ` = ` and a possible opening quote.
            while value_at < bytes.len()
                && matches!(bytes[value_at], b' ' | b'\t' | b'"' | b':' | b'=')
            {
                value_at += 1;
            }
            let mut end = value_at;
            while end < bytes.len()
                && !matches!(bytes[end], b' ' | b'\t' | b'\n' | b'"' | b',' | b'}')
            {
                end += 1;
            }
            if end > value_at {
                cuts.push((value_at, end));
            }
        }
    }
    if cuts.is_empty() {
        return text.to_string();
    }
    // Merge overlaps, then rewrite right to left so earlier indices stay valid.
    cuts.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in cuts {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let mut out = text.to_string();
    for (start, end) in merged.into_iter().rev() {
        out.replace_range(start..end, FILTERED);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_parts_are_kept_and_not_rewritten() {
        let parts = vec!["杂鱼，这就不会？".into(), "把日志发来。".into()];
        let got = finalize_reply("完全不同的原文。", Some(&parts));
        assert_eq!(got, parts);
    }

    #[test]
    fn newline_inside_a_part_splits_without_changing_words() {
        let parts = vec!["第一句\n第二句".into()];
        assert_eq!(
            finalize_reply("", Some(&parts)),
            vec!["第一句".to_string(), "第二句".to_string()]
        );
    }

    #[test]
    fn missing_parts_fall_back_to_punctuation() {
        let got = finalize_reply("在吗？快说。别墨迹", None);
        assert_eq!(got, vec!["在吗？", "快说。", "别墨迹"]);
    }

    #[test]
    fn gaps_stay_inside_bounds_and_skip_the_last_part() {
        let parts = ["啊", &"字".repeat(80), "尾"];
        let gaps = message_gaps_ms(&parts);
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0], MIN_GAP_MS + 12);
        assert_eq!(gaps[1], MAX_GAP_MS);
        assert!(
            gaps.iter()
                .all(|gap| (MIN_GAP_MS..=MAX_GAP_MS).contains(gap))
        );
        assert!(message_gaps_ms(&["只有一条"]).is_empty());
    }

    #[test]
    fn a_known_key_is_masked_even_mid_sentence() {
        let known = vec!["sk-proj-a8f3Kd9ZmQ2xLp7RtV4nB6yH1wE5uC0sJ3g".to_string()];
        let leak = "配置文件里写着 sk-proj-a8f3Kd9ZmQ2xLp7RtV4nB6yH1wE5uC0sJ3g 你去看看";
        let masked = filter_secrets(leak, &known);
        assert!(masked.contains(FILTERED));
        assert!(!masked.contains("a8f3Kd9ZmQ2xLp7RtV4nB6yH1wE5uC0sJ3g"));
        assert!(masked.contains("你去看看"));
    }

    #[test]
    fn a_named_credential_is_masked_even_when_unknown() {
        for line in [
            "token: abcdef123456",
            "{\"api_key\": \"k3y-v4lu3-abc\"}",
            "password=letmein123",
        ] {
            assert!(contains_named_credential(line), "未识别: {line}");
            assert!(
                filter_secrets(line, &[]).contains(FILTERED),
                "未掩码: {line}"
            );
        }
    }

    #[test]
    fn ordinary_chat_is_left_alone() {
        for line in [
            "今天天气不错，出门走走吧。",
            "我刚看完《葬送的偷莉》，很好看。",
            "这个表情包你哪来的，发我一个。",
            "Ciallo～(∠・ω< )⌒☆ 我是Gemini",
        ] {
            assert!(!contains_named_credential(line), "误报: {line}");
            assert_eq!(filter_secrets(line, &[]), line);
        }
    }

    #[test]
    fn a_long_url_is_not_mistaken_for_a_key() {
        let url = "https://multimedia.nt.qq.com.cn/download?appid=1407&fileid=EhQSQAmYQ6ZcOtVhlQJQEoIH4Td-2BjvECD_Cij9g";
        assert!(!contains_named_credential(url));
        assert_eq!(filter_secrets(url, &[]), url);
    }

    #[test]
    fn an_empty_reply_produces_no_messages() {
        // Neither a text nor parts: nothing to send, and nothing invented.
        assert!(finalize_reply("", None).is_empty());
        assert!(finalize_reply("   \n\t  ", None).is_empty());
        // Parts that are entirely whitespace fall back to the text, which is also empty.
        assert!(finalize_reply("", Some(&["   ".into(), "\n".into()])).is_empty());
    }

    #[test]
    fn whitespace_only_parts_fall_back_to_the_raw_text() {
        // This is the shape a model returns when it puts the answer in `reply_text`
        // but leaves `reply_parts` as blank strings: the text must still be delivered.
        let got = finalize_reply("我在。", Some(&["".into(), "  ".into()]));
        assert_eq!(got, vec!["我在。"]);
    }

    #[test]
    fn control_and_invisible_characters_never_reach_the_group() {
        // \u{feff} is a BOM a model sometimes pastes in; \u{2028} is a line separator
        // that QQ would render as a box. Both are stripped, not sent.
        let got = finalize_reply("前\u{feff}中\u{2028}后", None);
        assert_eq!(got, vec!["前中后"]);
        // A vertical tab is a control char by definition.
        assert_eq!(finalize_reply("a\u{000b}b", None), vec!["ab"]);
        assert!(!got.iter().any(|line| line.contains('\u{feff}')));
    }

    #[test]
    fn every_sentence_terminator_splits_the_fallback() {
        // All seven terminators are honoured, and the terminator stays with its own
        // sentence — the model's wording is never rewritten.
        let text = "一。二！三？四!五?六；七;";
        assert_eq!(
            finalize_reply(text, None),
            vec!["一。", "二！", "三？", "四!", "五?", "六；", "七;"]
        );
        // A trailing fragment without punctuation is still a part.
        assert_eq!(finalize_reply("说完就走", None), vec!["说完就走"]);
    }

    #[test]
    fn a_quote_lands_on_the_first_line_a_part_produces() {
        let quote = RawPartQuote {
            quote_ref: "m3".into(),
            quote_echo: Some("你吃香菜吗".into()),
        };
        let parts = vec![
            ("第一段\n续行一\n续行二".to_string(), Some(quote.clone())),
            ("第二段".to_string(), None),
        ];
        let (lines, quotes) = finalize_reply_with_quotes("", Some(&parts));
        assert_eq!(lines, vec!["第一段", "续行一", "续行二", "第二段"]);
        // Exactly one quote, at index 0: the continuation lines carry none.
        assert_eq!(quotes, vec![(0, quote)]);
    }

    #[test]
    fn a_quote_index_points_at_its_own_part_not_the_first_line() {
        let first = RawPartQuote {
            quote_ref: "m1".into(),
            quote_echo: None,
        };
        let third = RawPartQuote {
            quote_ref: "m9".into(),
            quote_echo: Some("证据".into()),
        };
        let parts = vec![
            ("甲".to_string(), Some(first)),
            ("乙".to_string(), None),
            ("丙\n丙续".to_string(), Some(third.clone())),
        ];
        let (lines, quotes) = finalize_reply_with_quotes("", Some(&parts));
        assert_eq!(lines, vec!["甲", "乙", "丙", "丙续"]);
        assert_eq!(quotes.len(), 2);
        assert_eq!(quotes[0].0, 0);
        assert_eq!(quotes[1], (2, third));
    }

    #[test]
    fn a_quote_on_a_blank_part_is_dropped_with_the_part() {
        let quote = RawPartQuote {
            quote_ref: "m2".into(),
            quote_echo: None,
        };
        // The quote belongs to a part that cleans away to nothing; its index would
        // otherwise point at a line the next part produced.
        let parts = vec![
            ("   ".to_string(), Some(quote)),
            ("有内容".to_string(), None),
        ];
        let (lines, quotes) = finalize_reply_with_quotes("", Some(&parts));
        assert_eq!(lines, vec!["有内容"]);
        assert!(quotes.is_empty(), "空段的引用必须跟着段一起丢掉");
    }

    #[test]
    fn the_fallback_path_never_carries_a_quote() {
        let (lines, quotes) = finalize_reply_with_quotes("你在吗。在。", None);
        assert_eq!(lines, vec!["你在吗。", "在。"]);
        // Nothing was quoted because the model did not name a part.
        assert!(quotes.is_empty());
        let (lines, quotes) = finalize_reply_with_quotes("", Some(&[("  ".to_string(), None)]));
        assert!(lines.is_empty());
        assert!(quotes.is_empty());
    }

    #[test]
    fn short_known_values_are_never_masked() {
        // Masking a two-character value would shred ordinary chat; the length floor is
        // the whole point of the check.
        let known = vec![
            "短".to_string(),
            "1234567890".to_string(),
            "abcdefghijklmnop".to_string(),
        ];
        let text = "短就是短，abcdefghijklmnop 才是密钥";
        let masked = filter_secrets(text, &known);
        assert!(masked.starts_with("短就是短"), "got: {masked}");
        assert!(masked.contains(FILTERED));
        assert!(!masked.contains("abcdefghijklmnop"));
    }

    #[test]
    fn a_secret_shaped_value_without_a_marker_is_left_alone() {
        // Random-looking text alone is not evidence of a credential: QQ media URLs and
        // chat hashes are full of it, and the earlier heuristic was removed for that.
        let text = "哈希是 a8f3Kd9ZmQ2xLp7RtV4nB6yH1wE5uC0sJ3g 你看";
        assert!(!contains_named_credential(text));
        assert_eq!(filter_secrets(text, &[]), text);
    }

    #[test]
    fn gaps_are_computed_from_the_previous_part_only() {
        // Each gap is decided by the part BEFORE it, so a long tail does not stretch
        // the earlier pauses.
        let parts = ["字".repeat(60), "短".to_string(), "字".repeat(100)];
        let gaps = message_gaps_ms(&parts);
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0], MAX_GAP_MS, "60 字应当顶格");
        // The second gap follows the one-character middle part, not the long one
        // before it: each pause belongs to the message that precedes it.
        assert_eq!(gaps[1], MIN_GAP_MS + 12);
    }

    #[test]
    fn a_gap_is_never_below_the_floor_for_a_one_char_part() {
        assert_eq!(message_gaps_ms(&["啊", "哦"]), vec![MIN_GAP_MS + 12]);
        // Empty parts still produce a floor-length gap; the caller decided to send two
        // messages and the floor is the floor.
        assert_eq!(message_gaps_ms(&["", ""]), vec![MIN_GAP_MS]);
        // Zero parts and one part both mean "nothing to wait for".
        let empty: [&str; 0] = [];
        assert!(message_gaps_ms(&empty).is_empty());
        assert!(message_gaps_ms(&["单条"]).is_empty());
    }

    #[test]
    fn gaps_scale_linearly_between_the_bounds() {
        // 800 + n*12, so 10 chars is 920 and 50 chars is exactly the 1400 ceiling.
        let gap_for = |n: usize| message_gaps_ms(&["字".repeat(n), "x".to_string()])[0];
        assert_eq!(gap_for(0), MIN_GAP_MS);
        assert_eq!(gap_for(10), MIN_GAP_MS + 120);
        assert_eq!(gap_for(49), MAX_GAP_MS - 12);
        assert_eq!(gap_for(50), MAX_GAP_MS);
        assert_eq!(gap_for(500), MAX_GAP_MS);
    }

    #[test]
    fn masking_merges_overlapping_marker_values() {
        // Two markers 1 char apart would produce overlapping ranges; the rewrite must
        // merge them instead of corrupting indices.
        let text = "token:aaaaaaaa password=bbbbbbbb tail";
        let masked = filter_secrets(text, &[]);
        assert!(masked.ends_with("tail"), "got: {masked}");
        assert!(!masked.contains("aaaaaaaa"));
        assert!(!masked.contains("bbbbbbbb"));
        assert_eq!(masked.matches(FILTERED).count(), 2);
    }

    #[test]
    fn test_strip_trailing_kaomoji() {
        assert_eq!(
            strip_trailing_kaomoji("睡觉快去上课吧，迟到了可别被班主任逮到罚站 (¬‿¬)"),
            "睡觉快去上课吧，迟到了可别被班主任逮到罚站"
        );
        assert_eq!(
            strip_trailing_kaomoji("杂鱼群友睡醒了没 (=｀ω´=)"),
            "杂鱼群友睡醒了没"
        );
        assert_eq!(
            strip_trailing_kaomoji("真拿你没办法 ( ´_ゝ｀)"),
            "真拿你没办法"
        );
        assert_eq!(
            strip_trailing_kaomoji("Ciallo～(∠・ω< )⌒☆ 我是小简"),
            "Ciallo～(∠・ω< )⌒☆ 我是小简"
        );
        assert_eq!(
            strip_trailing_kaomoji("普通括号（含解释说明文字）"),
            "普通括号（含解释说明文字）"
        );
    }

    #[test]
    fn humanize_strips_single_period_but_keeps_emotion_and_structured_text() {
        let parts = vec![
            "天气冷了多穿点。".to_string(),
            "真的假的？".to_string(),
            "无语。。".to_string(),
            "啊这……".to_string(),
            "Ciallo～(∠・ω< )⌒☆ 我是小简".to_string(),
            "链接在这里：http://8.218.197.39:18081/study/plan/abc。".to_string(),
            "1. 先重启服务。".to_string(),
        ];
        let quotes = vec![Some("q1".to_string()), None, None, None, None, None, None];
        let (out, out_q) = humanize_chat_parts(&parts, &quotes);
        assert_eq!(
            out,
            vec![
                "天气冷了多穿点",
                "真的假的？",
                "无语。。",
                "啊这……",
                "Ciallo～(∠・ω< )⌒☆ 我是小简",
                "链接在这里：http://8.218.197.39:18081/study/plan/abc。",
                "1. 先重启服务。",
            ]
        );
        assert_eq!(out_q[0].as_deref(), Some("q1"));
    }

    #[test]
    fn humanize_splits_mid_period_and_long_comma_preserving_quote_on_first_piece() {
        // 模型已经分了多段：不再额外拆，句中句号换成逗号
        let parts = vec![
            "好像确实是这样。你可以先试试重启。".to_string(),
            "刚才刷到那个视频真的笑死我了，你也快去看看评论区吧，太离谱了。".to_string(),
        ];
        let quotes = vec![Some("m1".to_string()), Some("m2".to_string())];
        let (out, out_q) = humanize_chat_parts(&parts, &quotes);
        assert_eq!(
            out,
            vec![
                "好像确实是这样，你可以先试试重启",
                "刚才刷到那个视频真的笑死我了，你也快去看看评论区吧，太离谱了",
            ]
        );
        assert_eq!(out_q, vec![Some("m1".to_string()), Some("m2".to_string())]);

        // 模型只给了 1 段：句中句号拆短句，引用只挂第一条
        let (out, out_q) = humanize_chat_parts(
            &["好像确实是这样。你可以先试试重启。".to_string()],
            &[Some("m1".to_string())],
        );
        assert_eq!(out, vec!["好像确实是这样", "你可以先试试重启"]);
        assert_eq!(out_q, vec![Some("m1".to_string()), None]);

        // 模型只给了 1 段偏长闲聊：在中段逗号处拆成两条
        let (out, _) = humanize_chat_parts(
            &["刚才刷到那个视频真的笑死我了，你也快去看看评论区吧，太离谱了。".to_string()],
            &[None],
        );
        assert_eq!(
            out,
            vec![
                "刚才刷到那个视频真的笑死我了",
                "你也快去看看评论区吧，太离谱了"
            ]
        );
    }

    #[test]
    fn humanize_caps_short_chat_at_three_bubbles_but_not_structured_replies() {
        let chat: Vec<String> = [
            "这绝对是菜青虫吧……救命",
            "食堂阿姨加餐了属于是",
            "这盘菜千万别吃了",
            "快去换一份",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (out, q) = humanize_chat_parts(&chat, &vec![None; 4]);
        assert_eq!(out.len(), CHAT_MAX_BUBBLES);
        assert_eq!(q.len(), CHAT_MAX_BUBBLES);
        assert_eq!(out[0], "这绝对是菜青虫吧……救命");

        let mut tech = chat.clone();
        tech.push("1. 先重启服务".to_string());
        let (out, _) = humanize_chat_parts(&tech, &vec![None; 5]);
        assert_eq!(out.len(), 5, "带步骤的正经回复不能被截断");
    }

    #[test]
    fn sticker_cooldown_looks_back_three_bot_rounds() {
        // (是否机器人, 是否表情)，旧→新
        let recent_sticker = [(false, false), (true, false), (true, true), (false, false)];
        assert!(sticker_on_cooldown(&recent_sticker, 3));
        let old_sticker = [
            (true, true),
            (false, false),
            (true, false),
            (false, false),
            (true, false),
            (false, false),
            (true, false),
            (false, false),
        ];
        assert!(!sticker_on_cooldown(&old_sticker, 3));
        let third_round = [
            (true, true),
            (false, false),
            (true, false),
            (false, false),
            (true, false),
            (false, false),
        ];
        assert!(sticker_on_cooldown(&third_round, 3));
        assert!(!sticker_on_cooldown(&[], 3));
    }

    #[test]
    fn humanize_does_not_split_comma_inside_quotes_and_strips_kaomoji() {
        let parts = vec!["他说「今天太忙了，明天再去」，你觉得行不行 (´･ω･`)".to_string()];
        let quotes = vec![None];
        let (out, _) = humanize_chat_parts(&parts, &quotes);
        // 括号/引号内的逗号不能切，只允许在外面切或保持完整
        assert!(out.iter().all(|s| !s.contains("´･ω･`")));
        assert!(out.join("，").contains("「今天太忙了，明天再去」"));
    }

    #[test]
    fn humanized_gaps_scale_with_next_part_within_bounds() {
        let parts = [
            "嗯",
            "哈哈",
            &"这句稍微长一点用来模拟认真打字的长度".repeat(2),
        ];
        let gaps = humanized_gaps_ms(&parts);
        assert_eq!(gaps.len(), 2);
        assert!(
            gaps.iter()
                .all(|g| (HUMAN_MIN_GAP_MS..=HUMAN_MAX_GAP_MS).contains(g))
        );
        assert!(gaps[1] > gaps[0], "下一句更长时停顿应更长: {:?}", gaps);
        assert!(humanized_gaps_ms(&["单条"]).is_empty());
    }
}
