//! Agent output contract and its second-pass validation.
//!
//! The model is untrusted input. It returns `speaker_ref` / `message_ref` aliases
//! that only mean something inside the batch it was given; **Rust** maps them back
//! to real ids. Anything that cannot be mapped, is out of scope, is too long, or is
//! structurally wrong is rejected and counted — never silently stored.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::salvage::{SalvageLayer, SalvageReport};

/// Maximum characters of a reply the bot will send.
pub const MAX_REPLY_CHARS: usize = 1_000;
/// Maximum characters of one memory fact.
pub const MAX_MEMORY_CONTENT_CHARS: usize = 500;
/// Maximum characters of a topic state string.
/// A topic slot is one short label. Longer text was the model restating its
/// last line (group 112, "萝卜直呼难绷…").
pub const MAX_TOPIC_CHARS: usize = 40;
/// Maximum characters of a mood label.
pub const MAX_MOOD_LABEL_CHARS: usize = 32;
/// Maximum characters of a mood reason.
pub const MAX_MOOD_REASON_CHARS: usize = 200;
/// Maximum memory proposals accepted per batch.
pub const MAX_MEMORY_OPS: usize = 8;
/// Maximum sticker proposals accepted per batch.
pub const MAX_STICKER_OPS: usize = 8;
/// Maximum sticker actions accepted in one turn. Sending is an outbound message,
/// so it is capped harder than keeping: a model that dumps five pictures at once
/// is not replying any more, it is spamming.
pub const MAX_STICKER_SENDS: usize = 3;
/// A sticker hash is `sha256` lowercase hex, so 64 characters and nothing else.
pub const STICKER_HASH_CHARS: usize = 64;
/// Maximum characters of an `ignore` reason, for audit only.
pub const MAX_STICKER_REASON_CHARS: usize = 120;
/// Maximum source refs per memory proposal.
pub const MAX_SOURCE_REFS: usize = 8;

/// Allowed `kind` values for memory facts.
pub const MEMORY_KINDS: [&str; 5] = ["fact", "preference", "topic", "relation", "event"];

/// Raw shape returned by the agent, before validation.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentOutputRaw {
    #[serde(default)]
    pub should_reply: Option<bool>,
    #[serde(default)]
    pub reply_text: Option<String>,
    #[serde(default)]
    pub reply_parts: Option<Vec<ReplyPartRaw>>,
    #[serde(default)]
    pub nicknames: Option<Vec<NicknameRaw>>,
    #[serde(default)]
    pub mood_update: Option<MoodUpdateRaw>,
    #[serde(default)]
    pub topic_state: Option<String>,
    #[serde(default)]
    pub memory_ops: Option<Vec<MemoryOpRaw>>,
    #[serde(default)]
    pub sticker_ops: Option<Vec<StickerOpRaw>>,
    /// Batch-local `message_ref` the reply mainly answers; the first sent
    /// part carries a OneBot `reply` segment pointing at it.
    #[serde(default)]
    pub quote_ref: Option<String>,
    /// Batch-local `speaker_ref` to poke once as part of this turn. At most one
    /// per turn; the turn resolves it to a QQ id inside this batch only.
    #[serde(default)]
    pub poke_ref: Option<String>,
}

/// One reply part as the model may send it: a bare string (the historic
/// shape) or an object carrying the part's own quote. Per-part quotes are
/// how a multi-part reply answers more than one message; the top-level
/// `quote_ref` remains as the single-quote fallback for part one.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ReplyPartRaw {
    Text(String),
    Quoted {
        text: String,
        #[serde(default)]
        quote_ref: Option<String>,
        #[serde(default)]
        quote_echo: Option<String>,
    },
}

impl ReplyPartRaw {
    /// The part's text plus its raw quote, if it named one.
    pub fn into_quote_pair(self) -> (String, Option<crate::agent::segments::RawPartQuote>) {
        match self {
            ReplyPartRaw::Text(text) => (text, None),
            ReplyPartRaw::Quoted {
                text,
                quote_ref,
                quote_echo,
            } => {
                let quote = quote_ref
                    .map(|r| r.trim().to_string())
                    .filter(|r| !r.is_empty())
                    .map(|r| crate::agent::segments::RawPartQuote {
                        quote_ref: r,
                        quote_echo: quote_echo
                            .map(|e| e.trim().to_string())
                            .filter(|e| !e.is_empty()),
                    });
                (text, quote)
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StickerOpRaw {
    #[serde(default)]
    pub op: Option<String>,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NicknameRaw {
    #[serde(default)]
    pub subject_ref: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MoodUpdateRaw {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryOpRaw {
    /// `add` or `update`.
    #[serde(default)]
    pub op: Option<String>,
    #[serde(default)]
    pub subject_ref: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub source_refs: Option<Vec<String>>,
    #[serde(default)]
    pub confidence: Option<f64>,
    /// Required for `update`: the `memory_ref` alias handed to the model.
    #[serde(default)]
    pub memory_ref: Option<String>,
}

/// A validated mood update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MoodUpdate {
    pub label: String,
    pub reason: String,
}

/// A validated memory proposal. The op still carries only aliases; the caller
/// resolves them against the batch and the current group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryOp {
    pub op: MemoryOpKind,
    pub subject_ref: Option<String>,
    pub content: String,
    pub kind: String,
    pub source_refs: Vec<String>,
    pub confidence: f64,
    pub memory_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOpKind {
    Add,
    Update,
}

/// One validated sticker action.
///
/// The model can only name a hash it was actually shown this turn, so the hash
/// set in [`ValidationScope`] is the whole authority: anything else is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickerOp {
    pub op: StickerOpKind,
    pub hash: String,
    /// Free text kept for audit on `ignore` only.
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StickerOpKind {
    /// Collect into her own sticker library so she can send it later.
    Keep,
    /// Never collect, and do not ask again.
    Ignore,
    /// Send it in this reply.
    Send,
}

impl StickerOpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StickerOpKind::Keep => "keep",
            StickerOpKind::Ignore => "ignore",
            StickerOpKind::Send => "send",
        }
    }
}

/// Fully validated agent output.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutput {
    pub should_reply: bool,
    /// Parts joined for logs and the admin view. Sending uses `reply_parts`.
    pub reply_text: String,
    pub reply_parts: Vec<String>,
    pub nicknames: Vec<NicknameProposal>,
    pub mood_update: Option<MoodUpdate>,
    pub topic_state: Option<String>,
    pub memory_ops: Vec<MemoryOp>,
    /// Sticker actions in the order the model asked for them.
    pub sticker_ops: Vec<StickerOp>,
    /// Validated quote target (`message_ref` inside this batch), if any.
    pub quote_ref: Option<String>,
    /// Per-part quotes that survived validation, aligned to `reply_parts`
    /// by `part_index`. The turn resolves each ref (batch `m…` or history
    /// `h…`) and verifies the echo before anything is attached.
    pub part_quotes: Vec<PartQuote>,
    /// Validated poke target (`speaker_ref` inside this batch), if any. The
    /// turn resolves it to a QQ id and fires one `send_poke`.
    pub poke_ref: Option<String>,
}

/// One validated per-part quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartQuote {
    pub part_index: usize,
    pub quote_ref: String,
    /// Short verbatim snippet of the target message, copied by the model.
    /// The turn checks it against the real message text; a part quote
    /// without a matching echo is not attached.
    pub quote_echo: Option<String>,
}

/// A nickname the model proposed for one speaker in this batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NicknameProposal {
    pub subject_ref: String,
    pub alias: String,
}

/// Why an output or one of its proposals was rejected. The string form is stable
/// because it is written into counters and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The model asked for something the format never allows (command, @all, …).
    Forbidden,
    /// Structurally invalid JSON or a wrong type.
    Malformed,
    /// A field exceeded its length limit.
    TooLong,
    /// `should_reply` true but `reply_text` empty.
    EmptyReply,
    /// Unknown `subject_ref` / `source_ref` / `memory_ref` for this batch.
    UnknownRef,
    /// A ref that belongs to another group.
    CrossGroup,
    /// `source_refs` was missing or empty.
    NoSource,
    /// Unknown memory `kind`.
    BadKind,
    /// Confidence outside 0..=1.
    BadConfidence,
    /// `update` without a `memory_ref`.
    MissingTarget,
    /// Too many memory proposals in one batch.
    TooManyOps,
    /// A poke asked for with `should_reply: false` — a poke *is* a reply action.
    PokeWithoutReply,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::Forbidden => "forbidden",
            RejectReason::Malformed => "malformed",
            RejectReason::TooLong => "too_long",
            RejectReason::EmptyReply => "empty_reply",
            RejectReason::UnknownRef => "unknown_ref",
            RejectReason::CrossGroup => "cross_group",
            RejectReason::NoSource => "no_source",
            RejectReason::BadKind => "bad_kind",
            RejectReason::BadConfidence => "bad_confidence",
            RejectReason::MissingTarget => "missing_target",
            RejectReason::TooManyOps => "too_many_ops",
            RejectReason::PokeWithoutReply => "poke_without_reply",
        }
    }
}

/// A deep session's give-up: the only decision in the document is
/// `should_reply: false`, with no text and no sticker. That is not a reply
/// the group can see, and on a long conversation it is how the model ducks
/// the turn. A silence that still carries a sticker is a real answer.
pub fn is_bare_refusal(raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.get("should_reply").and_then(|v| v.as_bool()) != Some(false) {
        return false;
    }
    let parts_empty = match obj.get("reply_parts") {
        None => true,
        Some(serde_json::Value::Array(parts)) => parts.is_empty(),
        Some(serde_json::Value::String(text)) => text.trim().is_empty(),
        Some(_) => false,
    };
    let text_empty = obj
        .get("reply_text")
        .and_then(|v| v.as_str())
        .map(|text| text.trim().is_empty())
        .unwrap_or(true);
    let sticker = obj
        .get("sticker_ops")
        .and_then(|v| v.as_array())
        .is_some_and(|ops| !ops.is_empty());
    parts_empty && text_empty && !sticker
}

/// Rejection counters for one validation pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rejections {
    pub total: usize,
    /// One entry per rejected item, in encounter order.
    pub reasons: Vec<RejectReason>,
}

impl Rejections {
    pub fn push(&mut self, reason: RejectReason) {
        self.total += 1;
        self.reasons.push(reason);
    }

    /// Stable `reason:count` pairs for logging and metrics.
    pub fn summary(&self) -> Vec<(&'static str, usize)> {
        let mut out: Vec<(&'static str, usize)> = Vec::new();
        for reason in &self.reasons {
            match out.iter_mut().find(|(name, _)| *name == reason.as_str()) {
                Some(entry) => entry.1 += 1,
                None => out.push((reason.as_str(), 1)),
            }
        }
        out
    }
}

/// Context the validator needs in order to resolve aliases. Built by the caller from
/// the current batch and the current group only.
#[derive(Debug, Clone, Default)]
pub struct ValidationScope {
    /// Speaker refs present in this batch.
    pub speaker_refs: Vec<String>,
    /// Message refs present in this batch.
    pub message_refs: Vec<String>,
    /// `memory_ref` values the model was shown for this group.
    pub memory_refs: Vec<String>,
    /// History refs (`h1`, `h2`, ...) shown to the model this turn — either
    /// the seed-brief window of a fresh conversation, or the annex of the
    /// previous batch on a resident continuation turn. Quotes may point here
    /// as well as at `message_refs`; the caller resolves them against the
    /// same window it rendered, and a ref from an expired window is a soft
    /// drop like any other unknown ref.
    pub history_refs: Vec<String>,
    /// Sticker hashes shown to the model in this batch. A sticker op naming
    /// anything else is dropped, which is what keeps a confused model from
    /// sending a picture out of the library by guessing.
    pub sticker_hashes: Vec<String>,
}

/// Result of a validation pass.
#[derive(Debug, Clone)]
pub struct Validated {
    pub output: AgentOutput,
    pub rejections: Rejections,
}

/// Parse and validate an agent JSON document.
///
/// Returns `Err` only when the whole document is unusable; individual bad memory
/// proposals are dropped and counted in [`Validated::rejections`].
pub fn validate(raw: &str, scope: &ValidationScope) -> Result<Validated, RejectReason> {
    let value: Value = serde_json::from_str(raw).map_err(|_| RejectReason::Malformed)?;
    validate_value(&value, scope)
}

/// Validate an already parsed JSON value.
pub fn validate_value(value: &Value, scope: &ValidationScope) -> Result<Validated, RejectReason> {
    if !value.is_object() {
        return Err(RejectReason::Malformed);
    }
    let raw: AgentOutputRaw =
        serde_json::from_value(value.clone()).map_err(|_| RejectReason::Malformed)?;

    let mut rejections = Rejections::default();

    let should_reply = raw.should_reply.unwrap_or(false);
    let reply_raw = raw.reply_text.unwrap_or_default();
    let raw_parts: Option<Vec<(String, Option<crate::agent::segments::RawPartQuote>)>> =
        raw.reply_parts.map(|parts| {
            parts
                .into_iter()
                .map(ReplyPartRaw::into_quote_pair)
                .collect()
        });
    let (reply_parts, raw_part_quotes) =
        crate::agent::segments::finalize_reply_with_quotes(&reply_raw, raw_parts.as_deref());
    let reply_text = reply_parts.join("\n");
    let reply_chars: usize = reply_parts.iter().map(|part| part.chars().count()).sum();

    if reply_parts.len() > crate::agent::segments::MAX_REPLY_PARTS || reply_chars > MAX_REPLY_CHARS
    {
        rejections.push(RejectReason::TooLong);
        return finish(AgentOutput::silent(), rejections);
    }

    if reply_parts
        .iter()
        .any(|part| contains_forbidden_markup(part))
    {
        rejections.push(RejectReason::Forbidden);
        return finish(AgentOutput::silent(), rejections);
    }

    let sticker_ops = validate_sticker_ops(
        raw.sticker_ops.clone().unwrap_or_default(),
        scope,
        &mut rejections,
    );

    // A sticker-only reply is legitimate: she may answer with a picture and no
    // words. Only when there is neither text nor a sticker send is the reply
    // empty. Where the send lands in the message order is the turn's business.
    let wants_sticker_send = sticker_ops.iter().any(|op| op.op == StickerOpKind::Send);

    // The bot never sends an empty reply merely because the model said to reply.
    if should_reply && reply_parts.is_empty() && !wants_sticker_send {
        rejections.push(RejectReason::EmptyReply);
    }
    let should_reply = should_reply && (!reply_parts.is_empty() || wants_sticker_send);

    let mood_update = match raw.mood_update {
        None => None,
        Some(m) => {
            let label = m.label.unwrap_or_default().trim().to_string();
            let reason = m.reason.unwrap_or_default().trim().to_string();
            if label.is_empty() {
                rejections.push(RejectReason::Malformed);
                None
            } else if label.chars().count() > MAX_MOOD_LABEL_CHARS
                || reason.chars().count() > MAX_MOOD_REASON_CHARS
            {
                rejections.push(RejectReason::TooLong);
                None
            } else {
                Some(MoodUpdate { label, reason })
            }
        }
    };

    let topic_state = match raw.topic_state {
        None => None,
        Some(t) => {
            let t = t.trim().to_string();
            if t.is_empty() {
                None
            } else if t.chars().count() > MAX_TOPIC_CHARS {
                rejections.push(RejectReason::TooLong);
                None
            } else {
                Some(t)
            }
        }
    };

    let mut memory_ops = Vec::new();
    let raw_ops = raw.memory_ops.unwrap_or_default();
    if raw_ops.len() > MAX_MEMORY_OPS {
        rejections.push(RejectReason::TooManyOps);
    }
    for op in raw_ops.into_iter().take(MAX_MEMORY_OPS) {
        if let Some(op) = validate_op(op, scope, &mut rejections) {
            memory_ops.push(op);
        }
    }

    let nicknames = validate_nicknames(raw.nicknames.unwrap_or_default(), scope, &mut rejections);

    let part_quotes = validate_part_quotes(raw_part_quotes, scope, &mut rejections);

    // A quote may only point at something this turn actually showed the
    // model: a message of this batch (`m…`) or an annex history entry
    // (`h…`, the previous batch of a resident conversation, or the seed
    // window on a fresh one). Both resolve to a real message id of this
    // group; anything else is dropped (softly — the reply itself still
    // goes out, just without the quote attached).
    let quote_ref = match raw.quote_ref {
        None => None,
        Some(raw_ref) => {
            let trimmed = raw_ref.trim().to_string();
            if trimmed.is_empty() {
                None
            } else if scope.message_refs.iter().any(|r| r == &trimmed)
                || scope.history_refs.iter().any(|r| r == &trimmed)
            {
                Some(trimmed)
            } else {
                rejections.push(RejectReason::UnknownRef);
                None
            }
        }
    };

    // A poke is a reply action, so it only survives when the turn is actually
    // speaking; a silenced turn that carried a poke would poke with no answer
    // behind it. An unknown speaker_ref is the same soft drop a bad quote_ref
    // gets: the reply still goes out, just without the poke.
    let poke_ref = match raw.poke_ref {
        None => None,
        Some(raw_ref) => {
            let trimmed = raw_ref.trim().to_string();
            if trimmed.is_empty() {
                None
            } else if !should_reply {
                rejections.push(RejectReason::PokeWithoutReply);
                None
            } else if scope.speaker_refs.iter().any(|r| r == &trimmed) {
                Some(trimmed)
            } else {
                rejections.push(RejectReason::UnknownRef);
                None
            }
        }
    };

    finish(
        AgentOutput {
            should_reply,
            reply_text,
            reply_parts,
            nicknames,
            mood_update,
            topic_state,
            memory_ops,
            sticker_ops,
            quote_ref,
            part_quotes,
            poke_ref,
        },
        rejections,
    )
}

/// Salvaged validation: the same per-section rules as [`validate_value`], but
/// applied section by section so one bad item cannot take the answer with it.
///
/// This is a *bypass*, not a relaxation: [`validate_value`] keeps its exact
/// all-or-nothing semantics, and this function is only reached after it failed.
/// What differs, and only inside this function's scope:
///
/// * scalars are read leniently (`should_reply` may be `0/1/"true"`, a number may
///   stand in for a string), because a float where an int belongs is a model slip,
///   not a forbidden action;
/// * a reply part carrying forbidden markup is dropped instead of voiding the
///   reply — but if that leaves nothing to say, the whole document is still
///   refused as `EmptyReply`, so markup never reaches the wire;
/// * over-long sections are clamped on a character boundary and counted;
/// * an unknown `quote_ref` is dropped without touching the reply.
///
/// `layer` records how the document was obtained ([`SalvageLayer`]); every drop
/// lands in the returned [`SalvageReport`] so the turn can audit what it gave up.
pub fn validate_with_salvage(
    value: &Value,
    scope: &ValidationScope,
    layer: SalvageLayer,
) -> Result<(Validated, SalvageReport), RejectReason> {
    if !value.is_object() {
        return Err(RejectReason::Malformed);
    }
    let mut rejections = Rejections::default();
    let mut report = SalvageReport::new(layer);

    let should_reply = lenient_bool(value.get("should_reply")).unwrap_or(false);

    let reply_raw = lenient_string(value.get("reply_text")).unwrap_or_default();
    let (raw_parts, had_parts, raw_part_quotes) =
        lenient_reply_parts(value.get("reply_parts"), &mut report);
    let paired: Vec<(String, Option<crate::agent::segments::RawPartQuote>)> = raw_parts
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let quote = raw_part_quotes
                .iter()
                .find(|(i, _)| *i == index)
                .map(|(_, q)| q.clone());
            (text.clone(), quote)
        })
        .collect();
    let (finalized_parts, finalized_quotes) = crate::agent::segments::finalize_reply_with_quotes(
        &reply_raw,
        if had_parts { Some(&paired) } else { None },
    );
    let pre_salvage = finalized_parts.clone();
    let reply_parts = salvage_reply_parts(finalized_parts, &mut report);
    // Per-part quotes survive salvage only when the parts came through
    // untouched (same count, same text); any clamp or drop breaks the
    // index alignment and the quotes go with it.
    let part_quotes = if reply_parts == pre_salvage {
        validate_part_quotes(finalized_quotes, scope, &mut rejections)
    } else {
        Vec::new()
    };

    let sticker_ops = salvage_stickers(
        value.get("sticker_ops"),
        scope,
        &mut rejections,
        &mut report,
    );
    let wants_sticker_send = sticker_ops.iter().any(|op| op.op == StickerOpKind::Send);

    if should_reply && reply_parts.is_empty() && !wants_sticker_send {
        // Everything that made it through was dropped, so there is nothing to
        // send: refuse the document instead of inventing a silent turn.
        return Err(RejectReason::EmptyReply);
    }
    let should_reply = should_reply && (!reply_parts.is_empty() || wants_sticker_send);
    let reply_text = reply_parts.join("\n");

    let mood_update = salvage_mood(value.get("mood_update"), &mut report);
    let topic_state = salvage_topic(value.get("topic_state"), &mut report);
    let memory_ops =
        salvage_memory_ops(value.get("memory_ops"), scope, &mut rejections, &mut report);
    let nicknames = salvage_nicknames(value.get("nicknames"), scope, &mut rejections, &mut report);
    let quote_ref = salvage_quote_ref(value.get("quote_ref"), scope, &mut report);
    let poke_ref = salvage_poke_ref(value.get("poke_ref"), scope, should_reply, &mut report);

    finish(
        AgentOutput {
            should_reply,
            reply_text,
            reply_parts,
            nicknames,
            mood_update,
            topic_state,
            memory_ops,
            sticker_ops,
            quote_ref,
            part_quotes,
            poke_ref,
        },
        rejections,
    )
    .map(|validated| (validated, report))
}

/// Full salvage ladder in front of one raw buffer: strict, then L1/L2 span +
/// repair candidates, then L4 plain text.
///
/// The strict result is returned untouched (`layer` stays `None`), so a clean
/// output is byte-for-byte the same turn it always was.
pub fn validate_salvaged(
    raw: &str,
    scope: &ValidationScope,
) -> Result<(Validated, SalvageReport), RejectReason> {
    if let Ok(validated) = validate(raw, scope) {
        return Ok((validated, SalvageReport::new(SalvageLayer::None)));
    }
    let mut last = RejectReason::Malformed;
    for candidate in crate::agent::salvage::candidates(raw) {
        match validate_with_salvage(&candidate.value, scope, candidate.layer) {
            Ok(found) => return Ok(found),
            Err(reason) => last = reason,
        }
    }
    if let Some(text) = crate::agent::salvage::plain_text_reply(raw) {
        // Route the text through the same section rules so the reply limits and
        // the forbidden-markup screen still apply to a recovered plain answer.
        let doc = serde_json::json!({ "should_reply": true, "reply_text": text });
        if let Ok(found) = validate_with_salvage(&doc, scope, SalvageLayer::Text) {
            return Ok(found);
        }
    }
    Err(last)
}

mod checks;

pub use self::checks::{contains_forbidden_markup, is_sticker_hash};
mod lenient;

use self::checks::{
    clamp_chars, finish, looks_like_raw_id, normalize_memory_ref, sanitize_alias,
    validate_nicknames, validate_op, validate_part_quotes, validate_sticker_ops,
};
use self::lenient::{
    absorb, lenient_bool, lenient_list, lenient_nickname_raw, lenient_op_raw, lenient_reply_parts,
    lenient_sticker_raw, lenient_string, salvage_memory_ops, salvage_mood, salvage_nicknames,
    salvage_poke_ref, salvage_quote_ref, salvage_reply_parts, salvage_stickers, salvage_topic,
};

#[cfg(test)]
mod tests;
