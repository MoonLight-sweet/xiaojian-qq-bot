//! Lenient re-reading of model output: tolerant scalar/list readers and per-section salvage fix-ups.
//! Moved out of contract.rs verbatim (std-split-contract); logic unchanged.

use super::*;

/// Read a JSON value as a bool the way a confused model writes one.
pub(super) fn lenient_bool(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_f64() {
            Some(0.0) => Some(false),
            Some(1.0) => Some(true),
            _ => None,
        },
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Read a JSON value as a string, accepting numbers and bools as their text form.
pub(super) fn lenient_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Collect `reply_parts`, dropping non-string entries. Returns the collected parts
/// and whether the field was an array at all.
pub(super) fn lenient_reply_parts(
    value: Option<&Value>,
    report: &mut SalvageReport,
) -> (
    Vec<String>,
    bool,
    Vec<(usize, crate::agent::segments::RawPartQuote)>,
) {
    let Some(Value::Array(items)) = value else {
        return (Vec::new(), false, Vec::new());
    };
    let mut parts = Vec::new();
    let mut quotes = Vec::new();
    for item in items {
        match item {
            Value::String(s) => parts.push(s.clone()),
            Value::Null => {}
            Value::Object(map) => {
                // A per-part-quote object: lift its text and quote so a
                // salvageable document keeps the addressing it carried.
                match map.get("text") {
                    Some(Value::String(s)) => {
                        if let Some(raw_ref) = map
                            .get("quote_ref")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|r| !r.is_empty())
                        {
                            quotes.push((
                                parts.len(),
                                crate::agent::segments::RawPartQuote {
                                    quote_ref: raw_ref.to_string(),
                                    quote_echo: map
                                        .get("quote_echo")
                                        .and_then(Value::as_str)
                                        .map(|e| e.trim().to_string())
                                        .filter(|e| !e.is_empty()),
                                },
                            ));
                        }
                        parts.push(s.clone());
                    }
                    _ => report.note("reply_parts", "malformed"),
                }
            }
            _ => report.note("reply_parts", "malformed"),
        }
    }
    (parts, true, quotes)
}

/// Enforce the reply limits by clamping instead of voiding the answer.
pub(super) fn salvage_reply_parts(parts: Vec<String>, report: &mut SalvageReport) -> Vec<String> {
    let max_parts = crate::agent::segments::MAX_REPLY_PARTS;
    let mut out: Vec<String> = Vec::new();
    let mut budget = MAX_REPLY_CHARS;
    for (idx, part) in parts.into_iter().enumerate() {
        if idx >= max_parts {
            report.note("reply_parts", "too_many");
            continue;
        }
        if contains_forbidden_markup(&part) {
            // The safety rule is not downgraded: a part carrying markup is gone.
            report.note("reply_parts", "forbidden");
            continue;
        }
        let chars = part.chars().count();
        if chars <= budget {
            budget -= chars;
            out.push(part);
        } else if budget > 0 {
            out.push(part.chars().take(budget).collect());
            report.clamped += 1;
            budget = 0;
        } else {
            report.clamped += 1;
        }
    }
    out
}

pub(super) fn salvage_stickers(
    value: Option<&Value>,
    scope: &ValidationScope,
    rejections: &mut Rejections,
    report: &mut SalvageReport,
) -> Vec<StickerOp> {
    let raw = lenient_list(value, "sticker_ops", report, lenient_sticker_raw);
    let before = rejections.total;
    let ops = validate_sticker_ops(raw, scope, rejections);
    absorb(rejections, before, "sticker_ops", report);
    ops
}

pub(super) fn salvage_memory_ops(
    value: Option<&Value>,
    scope: &ValidationScope,
    rejections: &mut Rejections,
    report: &mut SalvageReport,
) -> Vec<MemoryOp> {
    let raw = lenient_list(value, "memory_ops", report, lenient_op_raw);
    let over = raw.len().saturating_sub(MAX_MEMORY_OPS);
    let before = rejections.total;
    let mut out = Vec::new();
    if raw.len() > MAX_MEMORY_OPS {
        rejections.push(RejectReason::TooManyOps);
    }
    for op in raw.into_iter().take(MAX_MEMORY_OPS) {
        if let Some(op) = validate_op(op, scope, rejections) {
            out.push(op);
        }
    }
    absorb(rejections, before, "memory_ops", report);
    if over > 0 {
        // The overflow entries are gone without a per-item reason upstream.
        for _ in 0..over {
            report.note("memory_ops", "too_many_ops");
        }
    }
    out
}

pub(super) fn salvage_nicknames(
    value: Option<&Value>,
    scope: &ValidationScope,
    rejections: &mut Rejections,
    report: &mut SalvageReport,
) -> Vec<NicknameProposal> {
    let raw = lenient_list(value, "nicknames", report, lenient_nickname_raw);
    let before = rejections.total;
    let out = validate_nicknames(raw, scope, rejections);
    absorb(rejections, before, "nicknames", report);
    out
}

/// Mirror the rejections added by one section into the salvage report.
pub(super) fn absorb(
    rejections: &Rejections,
    from: usize,
    section: &'static str,
    report: &mut SalvageReport,
) {
    for reason in &rejections.reasons[from..] {
        report.note(section, reason.as_str());
    }
}

/// Read an array field into typed raw items, dropping entries that are not objects.
pub(super) fn lenient_list<T>(
    value: Option<&Value>,
    section: &'static str,
    report: &mut SalvageReport,
    read: fn(&Value, &mut SalvageReport) -> T,
) -> Vec<T> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        if item.is_object() {
            out.push(read(item, report));
        } else {
            report.note(section, "malformed");
        }
    }
    out
}

pub(super) fn lenient_op_raw(value: &Value, report: &mut SalvageReport) -> MemoryOpRaw {
    let confidence = value.get("confidence").and_then(Value::as_f64).map(|c| {
        if !c.is_finite() {
            report.clamped += 1;
            0.3
        } else if (0.0..=1.0).contains(&c) {
            c
        } else {
            // Out of range is a slip, not a forbidden action: clamp it.
            report.clamped += 1;
            c.clamp(0.0, 1.0)
        }
    });
    MemoryOpRaw {
        op: lenient_string(value.get("op")),
        subject_ref: lenient_string(value.get("subject_ref")),
        content: lenient_string(value.get("content")),
        kind: lenient_string(value.get("kind")),
        source_refs: match value.get("source_refs") {
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .filter_map(|item| lenient_string(Some(item)))
                    .collect(),
            ),
            _ => None,
        },
        confidence,
        memory_ref: lenient_string(value.get("memory_ref")),
    }
}

pub(super) fn lenient_sticker_raw(value: &Value, _report: &mut SalvageReport) -> StickerOpRaw {
    StickerOpRaw {
        op: lenient_string(value.get("op")),
        hash: lenient_string(value.get("hash")),
        reason: lenient_string(value.get("reason")),
    }
}

pub(super) fn lenient_nickname_raw(value: &Value, _report: &mut SalvageReport) -> NicknameRaw {
    NicknameRaw {
        subject_ref: lenient_string(value.get("subject_ref")),
        alias: lenient_string(value.get("alias")),
    }
}

pub(super) fn salvage_mood(
    value: Option<&Value>,
    report: &mut SalvageReport,
) -> Option<MoodUpdate> {
    let value = value.filter(|v| v.is_object())?;
    let label = lenient_string(value.get("label"))
        .unwrap_or_default()
        .trim()
        .to_string();
    if label.is_empty() {
        report.note("mood_update", "malformed");
        return None;
    }
    let reason = lenient_string(value.get("reason"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let label = clamp_chars(label, MAX_MOOD_LABEL_CHARS, report);
    let reason = clamp_chars(reason, MAX_MOOD_REASON_CHARS, report);
    Some(MoodUpdate { label, reason })
}

pub(super) fn salvage_topic(value: Option<&Value>, report: &mut SalvageReport) -> Option<String> {
    let topic = lenient_string(value)?.trim().to_string();
    if topic.is_empty() {
        return None;
    }
    Some(clamp_chars(topic, MAX_TOPIC_CHARS, report))
}

pub(super) fn salvage_quote_ref(
    value: Option<&Value>,
    scope: &ValidationScope,
    report: &mut SalvageReport,
) -> Option<String> {
    let trimmed = lenient_string(value)?.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    if scope.message_refs.iter().any(|r| r == &trimmed) {
        Some(trimmed)
    } else {
        // A quote pointing outside this batch is dropped, never fatal.
        report.note("quote_ref", "unknown_ref");
        None
    }
}

/// Salvage sibling of the `poke_ref` branch in [`validate_value`]. Same
/// semantics: silence anywhere in the document drops the poke instead of
/// taking the reply with it, and an unknown ref is a drop too. Unlike a
/// quote, a poke is an action against a person, so the drop is recorded.
pub(super) fn salvage_poke_ref(
    value: Option<&Value>,
    scope: &ValidationScope,
    should_reply: bool,
    report: &mut SalvageReport,
) -> Option<String> {
    let trimmed = lenient_string(value)?.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    if !should_reply {
        report.note("poke_ref", "poke_without_reply");
        return None;
    }
    if scope.speaker_refs.iter().any(|r| r == &trimmed) {
        Some(trimmed)
    } else {
        report.note("poke_ref", "unknown_ref");
        None
    }
}
