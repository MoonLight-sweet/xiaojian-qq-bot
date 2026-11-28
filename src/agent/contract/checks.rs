//! Per-section contract checks: sticker ops, nicknames, part quotes and memory ops.
//! Moved out of contract.rs verbatim (std-split-contract); logic unchanged.

use super::*;

pub(super) fn clamp_chars(text: String, max: usize, report: &mut SalvageReport) -> String {
    if text.chars().count() <= max {
        return text;
    }
    report.clamped += 1;
    text.chars().take(max).collect()
}

pub(super) fn validate_sticker_ops(
    raw: Vec<StickerOpRaw>,
    _scope: &ValidationScope,
    rejections: &mut Rejections,
) -> Vec<StickerOp> {
    let mut out: Vec<StickerOp> = Vec::new();
    let mut sends = 0usize;
    if raw.len() > MAX_STICKER_OPS {
        rejections.push(RejectReason::TooManyOps);
    }
    for op in raw.into_iter().take(MAX_STICKER_OPS) {
        let kind = match op.op.as_deref() {
            // Keep/ignore belong to the sticker_keep MCP tool, not this JSON.
            Some("keep") | Some("ignore") => {
                rejections.push(RejectReason::Forbidden);
                continue;
            }
            Some("send") => StickerOpKind::Send,
            // Anything else is forbidden: the model cannot delete or rename.
            Some(_) => {
                rejections.push(RejectReason::Forbidden);
                continue;
            }
            None => {
                rejections.push(RejectReason::Malformed);
                continue;
            }
        };
        // `send` may name something already in her library, which the caller
        // resolves against the database — validation only checks the shape here.
        let raw_hash = op.hash.unwrap_or_default().trim().to_ascii_lowercase();
        if !is_sticker_hash(&raw_hash) {
            rejections.push(RejectReason::Malformed);
            continue;
        }
        let hash = raw_hash;
        sends += 1;
        if sends > MAX_STICKER_SENDS {
            rejections.push(RejectReason::TooManyOps);
            continue;
        }
        // The same hash twice is a model slip, not two actions: the first wins.
        if out.iter().any(|existing| existing.hash == hash) {
            continue;
        }
        let reason = op
            .reason
            .unwrap_or_default()
            .trim()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(MAX_STICKER_REASON_CHARS)
            .collect::<String>();
        out.push(StickerOp {
            op: kind,
            hash,
            reason,
        });
    }
    out
}

/// A sticker hash is `sha256` hex: up to 64 lowercase characters.
pub fn is_sticker_hash(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= STICKER_HASH_CHARS
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn validate_nicknames(
    raw: Vec<NicknameRaw>,
    scope: &ValidationScope,
    rejections: &mut Rejections,
) -> Vec<NicknameProposal> {
    let mut out = Vec::new();
    for nick in raw.into_iter().take(8) {
        let Some(subject_ref) = nick.subject_ref.filter(|s| !s.is_empty()) else {
            rejections.push(RejectReason::UnknownRef);
            continue;
        };
        if !scope.speaker_refs.iter().any(|r| r == &subject_ref) {
            rejections.push(RejectReason::UnknownRef);
            continue;
        }
        let alias = sanitize_alias(nick.alias.unwrap_or_default());
        if alias.is_empty() {
            continue;
        }
        if alias.chars().count() > 16 {
            rejections.push(RejectReason::TooLong);
            continue;
        }
        out.push(NicknameProposal { subject_ref, alias });
    }
    out
}

pub(super) fn sanitize_alias(raw: String) -> String {
    raw.chars()
        .filter(|ch| !ch.is_control() && !matches!(ch, '\u{2028}' | '\u{2029}' | '\u{feff}'))
        .take(16)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Validate per-part quotes against the scope: the ref must have been
/// shown this turn (batch `m…` or history `h…`) and the echo must stay a
/// snippet, not a paragraph. Bad entries drop softly, like a bad top-level
/// `quote_ref`.
pub(super) fn validate_part_quotes(
    raw: Vec<(usize, crate::agent::segments::RawPartQuote)>,
    scope: &ValidationScope,
    rejections: &mut Rejections,
) -> Vec<PartQuote> {
    let mut out = Vec::new();
    for (part_index, quote) in raw {
        let trimmed = quote.quote_ref.trim().to_string();
        let known = scope.message_refs.iter().any(|r| r == &trimmed)
            || scope.history_refs.iter().any(|r| r == &trimmed);
        if !known {
            rejections.push(RejectReason::UnknownRef);
            continue;
        }
        if quote
            .quote_echo
            .as_deref()
            .is_some_and(|e| e.chars().count() > 24)
        {
            rejections.push(RejectReason::Malformed);
            continue;
        }
        out.push(PartQuote {
            part_index,
            quote_ref: trimmed,
            quote_echo: quote.quote_echo,
        });
    }
    out
}

pub(super) fn finish(
    output: AgentOutput,
    rejections: Rejections,
) -> Result<Validated, RejectReason> {
    Ok(Validated { output, rejections })
}

impl AgentOutput {
    pub(super) fn silent() -> Self {
        Self {
            should_reply: false,
            reply_text: String::new(),
            reply_parts: Vec::new(),
            nicknames: Vec::new(),
            mood_update: None,
            topic_state: None,
            memory_ops: Vec::new(),
            sticker_ops: Vec::new(),
            quote_ref: None,
            part_quotes: Vec::new(),
            poke_ref: None,
        }
    }
}

pub(super) fn validate_op(
    op: MemoryOpRaw,
    scope: &ValidationScope,
    rejections: &mut Rejections,
) -> Option<MemoryOp> {
    let kind_op = match op.op.as_deref() {
        Some("add") => MemoryOpKind::Add,
        Some("update") => MemoryOpKind::Update,
        // `delete` and anything else is forbidden: only the panel may delete.
        Some(_) => {
            rejections.push(RejectReason::Forbidden);
            return None;
        }
        None => {
            rejections.push(RejectReason::Malformed);
            return None;
        }
    };

    let content = op.content.unwrap_or_default().trim().to_string();
    if content.is_empty() {
        rejections.push(RejectReason::Malformed);
        return None;
    }
    if content.chars().count() > MAX_MEMORY_CONTENT_CHARS {
        rejections.push(RejectReason::TooLong);
        return None;
    }
    if contains_forbidden_markup(&content) {
        rejections.push(RejectReason::Forbidden);
        return None;
    }

    let kind = op.kind.unwrap_or_default().trim().to_string();
    if !MEMORY_KINDS.contains(&kind.as_str()) {
        rejections.push(RejectReason::BadKind);
        return None;
    }

    let confidence = match op.confidence {
        Some(c) if (0.0..=1.0).contains(&c) && c.is_finite() => c,
        Some(_) => {
            rejections.push(RejectReason::BadConfidence);
            return None;
        }
        // A missing confidence is treated as low confidence rather than trusted.
        None => 0.3,
    };

    let source_refs = op.source_refs.unwrap_or_default();
    if source_refs.is_empty() {
        rejections.push(RejectReason::NoSource);
        return None;
    }
    if source_refs.len() > MAX_SOURCE_REFS {
        rejections.push(RejectReason::TooLong);
        return None;
    }
    for r in &source_refs {
        if !scope.message_refs.iter().any(|m| m == r) {
            // A ref this batch never contained: either invented or cross-group.
            rejections.push(if looks_like_raw_id(r) {
                RejectReason::CrossGroup
            } else {
                RejectReason::UnknownRef
            });
            return None;
        }
    }

    let subject_ref = match op.subject_ref {
        None => None,
        Some(s) => {
            let s = s.trim().to_string();
            if s.is_empty() {
                None
            } else if !scope.speaker_refs.iter().any(|r| r == &s) {
                rejections.push(if looks_like_raw_id(&s) {
                    RejectReason::CrossGroup
                } else {
                    RejectReason::UnknownRef
                });
                return None;
            } else {
                Some(s)
            }
        }
    };

    let memory_ref = match kind_op {
        MemoryOpKind::Add => {
            // A caller-supplied ref for a new fact would let the model choose ids.
            if op.memory_ref.is_some() {
                rejections.push(RejectReason::Forbidden);
                return None;
            }
            None
        }
        MemoryOpKind::Update => match op.memory_ref {
            None => {
                rejections.push(RejectReason::MissingTarget);
                return None;
            }
            Some(r) => match normalize_memory_ref(&r) {
                MemoryRefKind::Store(normalized) => Some(normalized),
                MemoryRefKind::RawId => {
                    rejections.push(RejectReason::CrossGroup);
                    return None;
                }
                MemoryRefKind::Unknown => {
                    rejections.push(RejectReason::UnknownRef);
                    return None;
                }
            },
        },
    };

    Some(MemoryOp {
        op: kind_op,
        subject_ref,
        content,
        kind,
        source_refs,
        confidence,
        memory_ref,
    })
}

/// Reject markup the bot must never relay: CQ codes, mass mentions, and `<@id>`
/// style directives that the OneBot implementation might interpret.
pub fn contains_forbidden_markup(text: &str) -> bool {
    const FORBIDDEN: [&str; 8] = [
        "[CQ:",
        "[cq:",
        "@全体成员",
        "@all",
        "@All",
        "@ALL",
        "<@",
        "[at:",
    ];
    FORBIDDEN.iter().any(|needle| text.contains(needle))
}

/// A raw QQ id (all digits, 5..=20 long) appearing where an alias belongs means the
/// model tried to reach across the boundary.
pub(super) fn looks_like_raw_id(candidate: &str) -> bool {
    candidate.len() >= 5 && candidate.len() <= 20 && candidate.bytes().all(|b| b.is_ascii_digit())
}

/// How an update `memory_ref` parsed.
enum MemoryRefKind {
    /// A store key (`mem_{hex}`), optionally prefixed with `mem:`.
    Store(String),
    /// Looks like a QQ number.
    RawId,
    /// Anything else (`u1`, `chunk:N`, `log:N`, `mem_other`, …).
    Unknown,
}

/// Strip an optional `mem:` prefix (old MCP labels) and accept only `mem_{hex}`.
pub(super) fn normalize_memory_ref(raw: &str) -> MemoryRefKind {
    let trimmed = raw.trim();
    let stripped = trimmed.strip_prefix("mem:").unwrap_or(trimmed);
    if looks_like_raw_id(stripped) {
        return MemoryRefKind::RawId;
    }
    let Some(rest) = stripped.strip_prefix("mem_") else {
        return MemoryRefKind::Unknown;
    };
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return MemoryRefKind::Unknown;
    }
    MemoryRefKind::Store(format!("mem_{}", rest.to_ascii_lowercase()))
}
