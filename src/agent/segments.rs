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
    let has_cjk = inside.chars().any(|c| ('\u{4e00}'..='\u{9fa5}').contains(&c));
    if has_cjk {
        return trimmed.to_string();
    }

    // 检查是否包含颜文字常见构造成分（如 ¬, ‿, ω, ゝ, ╯, ︶, ▽, ◕, ﾟ, ｡, ¯, ˘, ⊙, ∀, ᐛ, ᐕ, 皿, 益 等，或纯符号混合）
    let is_symbolic = inside.chars().all(|c| !c.is_alphanumeric() || matches!(c, 'v' | 'V' | 'o' | 'O' | 'x' | 'X' | 'w' | 'W' | 'u' | 'U' | 'T' | 'Q' | 'p' | 'P'));
    let has_kaomoji_chars = inside.chars().any(|c| matches!(c, '¬' | '‿' | 'ω' | 'ゝ' | '╯' | '︶' | '▽' | '◕' | 'ﾟ' | '｡' | '¯' | '˘' | '⊙' | '∀' | 'ᐛ' | 'ᐕ' | 'Д' | 'Д' | 'д' | 'ㅂ' | '꒳' | 'ᴗ' | 'ᆺ' | 'ㅅ' | '・' | '´' | '`' | '*' | '^' | '-' | '=' | '_' | '/' | '\\' | '<' | '>'));

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
}
