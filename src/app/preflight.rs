//! Cheap pacing gate that runs before the agent.
//!
//! Its only question is *when* a burst of group messages has settled enough to hand to
//! the agent. It never decides whether she replies — that is hers alone.
//! Image tagging also lives here because it shares the Aux client; folding the
//! raw log into chunks is a separate background loop in `app/mod.rs`.
//!
//! It answers with a score, not a verdict: how unfinished the burst looks. Turning that
//! score into an actual waiting time is the caller's job, because timing is arithmetic
//! and arithmetic belongs in code that can be tested, not in a language model.
//!
//! Every failure path (disabled, HTTP error, unparseable JSON, timeout) returns
//! [`BurstScore::Unavailable`], whose hold time is zero, so a broken gate degrades into
//! "hand it over now", never into a message that waits forever.

use std::sync::Arc;

use futures_util::stream::{self, StreamExt};
use serde_json::Value;

use crate::integrations::aux_llm::AuxLlm;
use crate::onebot::normalize::IncomingMessage;

/// Pinned system prompt for the gate. Byte-identical on every call so the upstream
/// prefix cache stays warm; the volatile part only ever lives in the user message.
pub const PREFLIGHT_SYSTEM: &str = "\
You decide when a burst of QQ group chat has settled enough to hand to a reader. \
Return one JSON object and nothing else.\
";

/// How many ordinary pictures (not stickers) one turn may send to the tagger.
pub const MAX_IMAGES_PER_CALL: usize = 4;

/// Stickers have their own budget. They used to share [`MAX_IMAGES_PER_CALL`],
/// so two stickers in front of three photos silently dropped a photo.
pub const MAX_STICKERS_PER_CALL: usize = 4;

/// Models a vision call may use, in rotation. Measured on 2026-10-27 against the
/// live gateway (four real stickers, 512px JPEG, the production prompt, no session
/// header): one `hy3` call with all four took 5.94s, four parallel `hy3` calls
/// took 5.51s, and the same four spread across `hy3` / `hy3-b` / `hy3-c` took
/// 3.58s, all HTTP 200, no 429. Memory and the gate stay on the pinned `hy3`.
pub const VISION_MODELS: &[&str] = &["hy3", "hy3-b", "hy3-c"];

/// How many vision calls may be in flight at once. Four is the burst that was
/// actually measured; a larger batch waits for a free slot instead of stampeding.
pub const VISION_IN_FLIGHT: usize = 4;

/// Per-picture budget. A single spread call landed in 3.0–3.6s; past this the
/// picture is undescribed and the turn proceeds. The shared client timeout is
/// 120s, which is wrong for something sitting on the reply path.
pub const VISION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// Which published vision model serves picture `index` (0-based).
fn vision_model(index: usize) -> &'static str {
    VISION_MODELS[index % VISION_MODELS.len()]
}

/// One picture, one call. The model numbers its only picture `1`; the batch
/// position is what the caller maps by, so the returned tag is renumbered.
fn tag_from_single(raw: &str, index: usize) -> Option<ImageTag> {
    let mut tag = parse_tags(raw).into_iter().next()?;
    tag.index = index;
    Some(tag)
}

/// How many unarchived raw-log rows the gate sees as conversational context.
pub const PREFLIGHT_CONTEXT_MESSAGES: i64 = 20;

/// The longest a burst is ever held back because the gate thinks it is unfinished.
///
/// Twelve seconds is the budget for "someone is mid-thought": long enough to cover the
/// gap between two messages of one idea, short enough that a wrong score costs a pause
/// rather than a stall.
pub const HOLD_MAX_SECONDS: i64 = 6;

/// The shortest time a burst must exist before its score is allowed to release it.
///
/// Played back over the real group log, a pure score gate (floor 0) produced one Agy call
/// **more** than the old fixed window and cut one speaker mid-thought: a burst that scores
/// 0, like a lone `[图片]`, is released on the tick it arrives, so a message sent two
/// seconds later can no longer join it. A two-second floor already restores "same calls,
/// zero cuts" while still cutting the average wait from 4.9 s to 2.9 s — the old five
/// second window was paying three extra seconds for nothing.
pub const MIN_BURST_SECONDS: i64 = 2;

/// How long the gate may take before we stop waiting for it.
///
/// The shared client timeout is 120 s, which is right for compaction but absurd for a
/// pacing decision: a gate that hangs would hold a burst for two minutes while the flush
/// loop is blocked behind it. A gate is a cheap, low-effort query, but measured against
/// the real Aux model its scoring calls take 4.85–9.23 s, so the old 8 s budget
/// cut healthy slow answers into `Unavailable` — the burst then waited the full eight
/// seconds only to be handed over ungated. Fifteen seconds covers the measured range
/// with headroom while staying far below the 120 s client timeout; past that the burst
/// is better served by being handed over.
pub const GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How unfinished a burst looks, as judged by the gate.
///
/// A score, not a verdict. There is deliberately no "silence" variant: the gate has no
/// authority over whether she replies, only over when the burst is complete enough to
/// show her.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurstScore {
    /// 0 means plainly finished, 100 plainly mid-thought.
    Scored {
        more_coming: u8,
        reason: &'static str,
    },
    /// The gate could not answer. Treated as "finished" so the queue never stalls.
    Unavailable,
}

impl BurstScore {
    /// How long this burst still has to wait, in seconds.
    ///
    /// A finished burst needs no wait at all, so the very next tick releases it. That is
    /// where the latency win comes from: a plain question used to sit out the full
    /// settle grace even when the gate already knew it was complete.
    pub fn hold_seconds(self) -> i64 {
        match self {
            BurstScore::Unavailable => 0,
            // Rounded up, so a score of 1 still buys a second rather than nothing.
            BurstScore::Scored { more_coming, .. } => {
                (i64::from(more_coming) * HOLD_MAX_SECONDS + 99) / 100
            }
        }
    }

    /// The score itself, or `None` when the gate failed.
    pub fn more_coming(self) -> Option<u8> {
        match self {
            BurstScore::Scored { more_coming, .. } => Some(more_coming),
            BurstScore::Unavailable => None,
        }
    }

    /// Short reason for the audit trail.
    pub fn reason(self) -> &'static str {
        match self {
            BurstScore::Scored { reason, .. } => reason,
            BurstScore::Unavailable => "unavailable",
        }
    }
}

/// Parsed score, kept separate from IO so it can be unit tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub more_coming: u8,
    pub reason: String,
}

/// Anything that can turn pictures into [`ImageTag`]s.
///
/// One method, deliberately narrow: it is what lets a test drive the whole image
/// path — download, shrink, describe, ingest — without a network call, and it keeps
/// `Preflight` free of a test-only branch.
///
/// `sticker_hashes` on the reply side is derived from the tags, never from the
/// describer, so a describer that answers with canned tags exercises exactly the same
/// code as the real one.
#[async_trait::async_trait]
pub trait ImageDescriber: Send + Sync + 'static {
    async fn describe(&self, bot_id: &str, group_id: &str, images: &[String]) -> Vec<ImageTag>;
}

/// The gate itself.
#[derive(Debug, Clone)]
pub struct Preflight {
    client: Arc<dyn AuxLlm>,
}

#[async_trait::async_trait]
impl ImageDescriber for Preflight {
    async fn describe(&self, bot_id: &str, group_id: &str, images: &[String]) -> Vec<ImageTag> {
        self.describe_images(bot_id, group_id, images).await
    }
}

impl Preflight {
    pub fn new(client: Arc<dyn AuxLlm>) -> Self {
        Self { client }
    }

    /// The sticky session id for one group. Stable across turns on purpose: the
    /// gateway pins one account per session, which is what keeps the provider's
    /// prefix cache from being thrown away on every call.
    pub fn session_id(bot_id: &str, group_id: &str) -> String {
        format!("preflight:{bot_id}:{group_id}")
    }

    /// Describe pictures so a text-only agent can react without ever receiving
    /// a byte of image data.
    ///
    /// One call per picture, spread across [`VISION_MODELS`], with no session
    /// header. A sticky session would pin every call to one gateway account, so
    /// the parallel requests would queue instead of running. A picture that
    /// times out or comes back unusable is simply missing: the caller reports
    /// it as undescribed and does not invent a caption. There is no second
    /// shot on the reply path.
    ///
    /// `bot_id` and `group_id` stay in the signature because the trait is also
    /// how tests inject a canned describer. The live path does not derive a
    /// session from them.
    pub async fn describe_images(
        &self,
        _bot_id: &str,
        _group_id: &str,
        images: &[String],
    ) -> Vec<ImageTag> {
        if images.is_empty() {
            return Vec::new();
        }
        // When the aux backend is the single serialized `agy` session, fanning
        // out N parallel calls just queues N turns behind one Mutex while all N
        // 25s timers tick simultaneously. Pass the whole batch in one turn
        // instead (AgyAuxClient stages every image as `@path` in one prompt).
        if self.client.mode_name() == "agy" && images.len() > 1 {
            let call = self.client.complete_vision_on(
                VISION_MODELS[0],
                TAG_SYSTEM,
                TAG_PROMPT,
                images,
                None,
            );
            let Ok(Ok(raw)) = tokio::time::timeout(VISION_TIMEOUT, call).await else {
                return Vec::new();
            };
            let mut tags = parse_tags(&raw);
            tags.retain(|tag| tag.index >= 1 && tag.index <= images.len());
            tags.sort_by_key(|tag| tag.index);
            tags.dedup_by_key(|tag| tag.index);
            return tags;
        }
        let client = Arc::clone(&self.client);
        let mut tags: Vec<ImageTag> = stream::iter(images.iter().cloned().enumerate())
            .map(move |(index, url)| {
                let client = Arc::clone(&client);
                async move {
                    let model = vision_model(index);
                    let call = client.complete_vision_on(
                        model,
                        TAG_SYSTEM,
                        TAG_PROMPT,
                        std::slice::from_ref(&url),
                        None,
                    );
                    let Ok(Ok(raw)) = tokio::time::timeout(VISION_TIMEOUT, call).await else {
                        return None;
                    };
                    tag_from_single(&raw, index + 1)
                }
            })
            .buffer_unordered(VISION_IN_FLIGHT)
            .filter_map(|tag| async move { tag })
            .collect()
            .await;
        tags.sort_by_key(|tag| tag.index);
        tags
    }

    /// Ask the gate how unfinished this burst looks.
    ///
    /// `recent` is the tail of the raw log (already oldest-first). Note what is *not*
    /// passed: how long this burst has been accumulating. Telling the model how long it
    /// has waited makes its score creep upward with time, which quietly reinstates a
    /// fixed window with extra steps.
    pub async fn score(
        &self,
        bot_id: &str,
        group_id: &str,
        messages: &[IncomingMessage],
        recent: &[(String, String)],
        image_lines: &[String],
    ) -> BurstScore {
        self.score_with_hint(bot_id, group_id, messages, recent, image_lines, None)
            .await
    }

    /// The same question, plus the conversation-shape line the damper produced.
    ///
    /// The hint only ever describes the window. It cannot make the gate release anything:
    /// the score is still a probability about the text, and the caller still decides.
    pub async fn score_with_hint(
        &self,
        bot_id: &str,
        group_id: &str,
        messages: &[IncomingMessage],
        recent: &[(String, String)],
        image_lines: &[String],
        hint: Option<&str>,
    ) -> BurstScore {
        let prompt = build_prompt_with_hint(messages, recent, image_lines, hint);
        let session = Self::session_id(bot_id, group_id);
        let raw = match tokio::time::timeout(
            GATE_TIMEOUT,
            self.client
                .complete_streaming(PREFLIGHT_SYSTEM, &prompt, Some(&session)),
        )
        .await
        {
            Ok(Ok(raw)) => raw,
            // A broken or slow gate must not stall the queue: hand it over and let her
            // judge. Timeout and upstream error take the same path on purpose.
            Ok(Err(_)) | Err(_) => return BurstScore::Unavailable,
        };
        match parse_verdict(&raw) {
            Some(v) => BurstScore::Scored {
                more_coming: v.more_coming,
                reason: leak_static(&v.reason),
            },
            // A model that cannot follow the schema is not evidence of a pause.
            None => BurstScore::Unavailable,
        }
    }
}

/// Collapse a free-text reason into a small closed set.
///
/// The reason only ever reaches the audit trail, so it must not carry model prose (or
/// anything a model might echo out of the prompt).
fn leak_static(reason: &str) -> &'static str {
    let lowered = reason.to_ascii_lowercase();
    let hit = |needles: &[&str]| needles.iter().any(|n| lowered.contains(n));
    if hit(&[
        "announc",
        "promis",
        "listed",
        "not said yet",
        "预告",
        "还没说",
        "只列",
    ]) {
        "announced_more"
    } else if hit(&[
        "unfinished",
        "incomplete",
        "dangl",
        "mid-",
        "conjunction",
        "未完成",
        "截断",
        "悬空",
    ]) {
        "unfinished_thought"
    } else if hit(&["typing", "filler", "填写"]) {
        "still_typing"
    } else if hit(&["complete", "settled", "finished", "完整", "收尾", "闭合"]) {
        "settled"
    } else {
        "unclear"
    }
}

/// The stable scoring rules. Byte-identical on every call, which is exactly what makes
/// them a cacheable prefix; all variability lives below them.
///
/// Written to make the model judge the *sentence*, never the clock: it is never told how
/// long the burst has been open, so its score cannot drift upward merely with time.
pub const SCORE_RULES: &str = "\
Rate how likely it is that the speaker is NOT done talking yet, and that more messages \
from the same burst will arrive within the next 15 seconds.\n\
Answer with {\"more_coming\":<0-100>,\"reason\":\"<short>\"}.\n\
0 means the burst is plainly finished: a complete question, a complete statement, a \
greeting, a reaction, or a picture sent on its own.\n\
100 means the burst is plainly mid-thought: it ends with a conjunction, a filler, a \
repeated word, a dangling pronoun or an unfinished clause, or it announces something it \
has not said yet.\n\
Consider what the recent log shows: if this speaker habitually sends one thought as \
several short messages, rate the incompleteness higher. If the burst already contains a \
clear question aimed at the group, rate it lower.\n\
A message whose text is \"[图片]\" is a picture you cannot see. Treat it as a complete \
message on its own: pictures rarely continue a sentence.\n\
A message whose text is \"[语音]\" is a voice message you cannot hear. Treat it as a \
complete message on its own: voice messages rarely continue a sentence.\n\
Answer with a number even when unsure. Never answer about content quality or whether a \
reply is deserved.\n";

/// Build the user-side prompt. Stable prefix first (the schema), volatile tail last,
/// so the cacheable part never shifts.
pub fn build_prompt(
    messages: &[IncomingMessage],
    recent: &[(String, String)],
    image_lines: &[String],
) -> String {
    build_prompt_with_hint(messages, recent, image_lines, None)
}

/// The same prompt, plus one optional line describing the *shape* of the conversation.
///
/// The hint tells the gate what a window of ten minutes looks like — one person, this
/// many rounds — and nothing else: not what to answer, not that anything is being held,
/// and not how long this burst has waited (`prompt_never_tells_the_gate_how_long_it_waited`
/// still has to hold). Its authority is unchanged; a hint is context, not a verdict.
pub fn build_prompt_with_hint(
    messages: &[IncomingMessage],
    recent: &[(String, String)],
    image_lines: &[String],
    hint: Option<&str>,
) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(SCORE_RULES);
    out.push('\n');
    if !recent.is_empty() {
        out.push_str("Recent group log:\n");
        for (name, text) in recent {
            out.push_str("- ");
            out.push_str(name);
            out.push_str(": ");
            out.push_str(text);
            out.push('\n');
        }
        out.push('\n');
    }
    if let Some(hint) = hint
        && !hint.trim().is_empty()
    {
        out.push_str(hint.trim());
        out.push('\n');
        out.push('\n');
    }
    out.push_str(&format!("{} message(s) in this burst.\n", messages.len()));
    out.push_str("Burst messages:\n");
    for message in messages {
        out.push_str("- ");
        if !message.display_name.is_empty() {
            out.push_str(&message.display_name);
        } else {
            out.push_str(&message.speaker_ref);
        }
        out.push_str(": ");
        let text = message.text.trim();
        if text.is_empty() {
            out.push_str("(no text)");
        } else {
            out.push_str(text);
        }
        if message.mentions_bot {
            out.push_str(" [mentions the bot]");
        }
        out.push('\n');
    }
    if !image_lines.is_empty() {
        out.push_str("\nWhat the pictures contain (you cannot see them; this is already ");
        out.push_str("recognised text):\n");
        for line in image_lines {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Parse the gate's JSON. Accepts a bare object or one wrapped in a fenced block,
/// because models occasionally decorate JSON despite being told not to.
pub fn parse_verdict(raw: &str) -> Option<Verdict> {
    let trimmed = raw.trim();
    let candidate = if trimmed.starts_with('{') {
        trimmed
    } else {
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        if end <= start {
            return None;
        }
        &trimmed[start..=end]
    };
    let value: Value = serde_json::from_str(candidate).ok()?;
    // The schema says `more_coming`; models occasionally rename it. Accepting the aliases
    // costs nothing and stops a purely cosmetic deviation from reading as "gate broken".
    let raw_score = ["more_coming", "score", "continuation"]
        .iter()
        .find_map(|key| value.get(*key))?;
    let number = raw_score.as_f64().or_else(|| {
        // Some answers arrive as a quoted number.
        raw_score
            .as_str()
            .and_then(|s| s.trim().parse::<f64>().ok())
    })?;
    if !number.is_finite() {
        return None;
    }
    let more_coming = number.round().clamp(0.0, 100.0) as u8;
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(Verdict {
        more_coming,
        reason,
    })
}

/// Pinned system prompt for the tagger. Byte-identical on every call so the upstream
/// prefix cache stays warm; the volatile part (the pictures) sits in the user message.
pub const TAG_SYSTEM: &str = "\
You look at pictures posted in a QQ group chat and describe them so a text-only \
assistant can react without seeing them. Return one JSON object and nothing else.\n\
\n\
The assistant you are describing for has this fixed appearance. Use it to recognise \
her, and only her:\n\
long wavy hair graded violet-to-blue; four-pointed star hair ornaments; cat ears with \
pink inner lining; heterochromatic eyes, one gold and one pink-violet; navy short top \
with detached white sleeves; ruffled hem; dangling star chains; a cat tail; floating \
prismatic star sparkles around her.\n\
";

/// The tagging questions, kept stable so only the images themselves vary.
pub const TAG_PROMPT: &str = "\
For EACH image, in the order given, answer these fields.\n\
kind: sticker if it is a reaction image, meme or emoji pack picture someone would send \
for its tone; photo if it is a real-world photograph; screenshot if it is a capture of a \
screen; art if it is drawn or rendered artwork.\n\
ocr: the text visible in the picture, as close to what it says as you can read. Empty \
string if there is no text. Do not translate it.\n\
description: a rich Simplified Chinese description of up to 150 characters. For stickers (kind=sticker), \
name the character, action, expression, and crucially explain the underlying meme, humor or chat irony. \
For photos/screenshots (kind=photo/screenshot), detail the real-world scene, main subjects, their status, \
screen contents (code/error/questions), providing complete visual context for conversation.\n\
emotion: the feeling it conveys, one or two Simplified Chinese words.\n\
self: compare the character against the described appearance above. self if it is her: \
matching hair, cat ears, and one gold plus one pink-violet eye are enough to decide, even \
if the outfit, art style or expression differs. self_series if it is recognisably her \
character drawn in a clearly different design or outfit. other if it is anyone else. A \
character who merely has purple hair, or merely has cat ears, is other - it takes the \
specific combination, not one trait.\n\
nsfw: true only if the picture is sexually explicit or graphically violent.\n\
Answer as {\"images\":[{\"index\":1,\"kind\":\"\",\"ocr\":\"\",\"description\":\"\",\
\"emotion\":\"\",\"self\":\"\",\"nsfw\":false}]}.\n\
Never invent text that is not in the picture.\n\
Some images are a horizontal filmstrip of several frames side by side: that is one \\\nanimated sticker sampled over time, frames in playback order left to right. Treat it \\\nas a single animated sticker: describe the motion or change across the frames, and \\\nlet `description` name the action, not one frozen pose.\n";

/// One image the gate should describe before the agent ever sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageTag {
    /// 1-based position in the call, so a partial answer can still be mapped back.
    pub index: usize,
    pub kind: String,
    pub ocr: String,
    /// One short caption (≤30 chars): subject, defining look, expression or
    /// action, art style if distinctive. Replaces the old `scene` + `intent` +
    /// `usage_hint` triple, which used to cost three separate answers.
    pub description: String,
    pub emotion: String,
    /// `self` / `self_series` / `other`.
    pub self_ref: String,
    pub nsfw: bool,
}

impl ImageTag {
    /// Render the tag as the one line the agent will see.
    ///
    /// Deliberately flat: the agent needs the content and the tone, not a schema. Empty
    /// fields are omitted so a bare photo does not turn into a row of blanks.
    pub fn render(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let kind = if self.kind.is_empty() {
            "image"
        } else {
            self.kind.as_str()
        };
        parts.push(kind.to_string());
        if !self.ocr.trim().is_empty() {
            parts.push(format!("文字「{}」", self.ocr.trim()));
        }
        if !self.description.trim().is_empty() {
            parts.push(self.description.trim().to_string());
        }
        if !self.emotion.trim().is_empty() {
            parts.push(format!("情绪{}", self.emotion.trim()));
        }
        if self.self_ref == "self" || self.self_ref == "self_series" {
            parts.push(format!("主体{}", self.self_ref));
        }
        if self.nsfw {
            parts.push("敏感内容".to_string());
        }
        parts.join("｜")
    }
}

/// Parse the tagger's JSON.
///
/// Tolerant of a fenced block and of missing fields, because a partial description is
/// still better than none; an unusable payload yields an empty list so the caller can
/// report the pictures as untagged rather than invent content.
pub fn parse_tags(raw: &str) -> Vec<ImageTag> {
    let trimmed = raw.trim();
    let candidate = if trimmed.starts_with('{') {
        trimmed
    } else {
        match (trimmed.find('{'), trimmed.rfind('}')) {
            (Some(start), Some(end)) if end > start => &trimmed[start..=end],
            _ => return Vec::new(),
        }
    };
    let Ok(value) = serde_json::from_str::<Value>(candidate) else {
        return Vec::new();
    };
    let Some(items) = value.get("images").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for (position, item) in items.iter().enumerate() {
        let text = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        out.push(ImageTag {
            index: item
                .get("index")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(position + 1),
            kind: text("kind"),
            ocr: text("ocr"),
            description: text("description"),
            emotion: text("emotion"),
            self_ref: text("self"),
            nsfw: item.get("nsfw").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    out
}

/// QQ's own sticker flag wins over the vision model's guess.
///
/// `sub_type` is what the sender actually picked in the client: `1` is an animated
/// sticker, `0` a normal picture. The tagger only gets to decide when the flag is
/// missing (older clients, or a segment that never had one) — trusting the model
/// instead loses 240px stickers that it calls photos.
pub fn resolve_kind(sub_type: Option<&str>, model_kind: &str) -> String {
    match sub_type {
        Some("1") => "sticker".to_string(),
        Some("0") => "photo".to_string(),
        Some(other) => format!("image:{other}"),
        None => model_kind.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(name: &str, text: &str, mentions: bool) -> IncomingMessage {
        IncomingMessage {
            bot_id: "9999".into(),
            group_id: "500".into(),
            message_id: "1".into(),
            sender_id: "7".into(),
            speaker_ref: "u1".into(),
            speaker_role: crate::onebot::normalize::SpeakerRole::Member,
            display_name: name.into(),
            known_alias: None,
            received_at: 1,
            segments: Vec::new(),
            text: text.into(),
            mentions_bot: mentions,
            quote_of: None,
        }
    }

    #[test]
    fn parse_verdict_covers_shapes_clamps_and_malformed() {
        let v = parse_verdict(r#"{"more_coming":95,"reason":"ends with a dangling pronoun"}"#)
            .expect("应解析");
        assert_eq!(v.more_coming, 95);
        assert_eq!(v.reason, "ends with a dangling pronoun");

        let fenced = "```json\n{\"more_coming\": 0, \"reason\": \"settled\"}\n```";
        assert_eq!(parse_verdict(fenced).expect("应解析").more_coming, 0);

        assert_eq!(
            parse_verdict(r#"{"score":88,"reason":"x"}"#)
                .expect("应为别名")
                .more_coming,
            88
        );
        assert_eq!(
            parse_verdict(r#"{"continuation":"40"}"#)
                .expect("应为字符串数字")
                .more_coming,
            40
        );
        assert_eq!(
            parse_verdict(r#"{"more_coming":140}"#)
                .expect("应解析")
                .more_coming,
            100
        );
        assert_eq!(
            parse_verdict(r#"{"more_coming":-7}"#)
                .expect("应解析")
                .more_coming,
            0
        );

        // A model that cannot follow the schema must not freeze the queue: the caller
        // treats Unavailable as zero hold time.
        assert!(parse_verdict("我拒绝回答").is_none());
        assert!(parse_verdict(r#"{"more_coming":"maybe"}"#).is_none());
        // The retired binary schema is not accepted either, so a stale prompt cannot
        // smuggle the old contract back in.
        assert!(parse_verdict(r#"{"hand_over":false,"reason":"nope"}"#).is_none());
        assert_eq!(BurstScore::Unavailable.hold_seconds(), 0);
    }

    #[test]
    fn hold_time_maps_the_score_onto_seconds() {
        // A finished burst waits for nothing: the next tick releases it.
        assert_eq!(hold_seconds(0), 0);
        // Anything non-zero still buys a second, so "1" cannot mean "release now".
        assert_eq!(hold_seconds(1), 1);
        assert_eq!(hold_seconds(50), 3);
        // A half-sentence waits the full budget.
        assert_eq!(hold_seconds(95), 6);
        assert_eq!(hold_seconds(100), HOLD_MAX_SECONDS);
    }

    fn hold_seconds(more_coming: u8) -> i64 {
        BurstScore::Scored {
            more_coming,
            reason: "test",
        }
        .hold_seconds()
    }

    #[test]
    fn wait_reasons_are_reduced_to_a_fixed_set() {
        assert_eq!(
            leak_static("ends with a dangling pronoun"),
            "unfinished_thought"
        );
        assert_eq!(leak_static("unfinished clause"), "unfinished_thought");
        assert_eq!(
            leak_static("announces three things but listed one"),
            "announced_more"
        );
        assert_eq!(leak_static("a complete question"), "settled");
        assert_eq!(leak_static("随便编的字符串"), "unclear");
    }

    #[test]
    fn the_gate_has_no_power_to_silence_her() {
        // A score can only ever delay. There is no arm that means "do not show her".
        let scored = BurstScore::Scored {
            more_coming: 100,
            reason: "unfinished_thought",
        };
        assert_eq!(scored.hold_seconds(), HOLD_MAX_SECONDS);
        assert_eq!(scored.more_coming(), Some(100));
        assert_eq!(scored.reason(), "unfinished_thought");
        assert_eq!(BurstScore::Unavailable.reason(), "unavailable");
        assert_eq!(BurstScore::Unavailable.more_coming(), None);
    }

    #[test]
    fn prompt_puts_messages_after_the_stable_schema() {
        let prompt = build_prompt(&[message("tester", "在吗", true)], &[], &[]);
        let schema_at = prompt.find("Rate how likely").expect("schema");
        let body_at = prompt.find("tester").expect("正文");
        assert!(schema_at < body_at);
        assert!(prompt.contains("[mentions the bot]"));
        assert!(!prompt.contains("What the pictures contain"));
    }

    /// The gate must never be told how long the burst has waited: that number makes the
    /// score creep upward with time and quietly reinstates a fixed window.
    #[test]
    fn prompt_never_tells_the_gate_how_long_it_waited() {
        let prompt = build_prompt(&[message("tester", "主人你", false)], &[], &[]);
        assert!(!prompt.contains("Unanswered burst so far"));
        assert!(!prompt.contains("waiting"));
        assert!(!prompt.contains(" so far:"));
    }

    /// A picture reaches the gate as the literal text "[图片]", so the rules have to say
    /// what that means or a picture-only burst reads as an unfinished sentence.
    #[test]
    fn prompt_explains_that_a_picture_is_a_complete_message() {
        let prompt = build_prompt(&[message("tester", "[图片]", false)], &[], &[]);
        assert!(prompt.contains("a picture you cannot see"));
    }

    /// A voice message reaches the gate as the literal text "[语音]", same
    /// placeholder problem as a picture: without a rule it reads as an
    /// unfinished sentence.
    #[test]
    fn prompt_explains_that_a_voice_message_is_a_complete_message() {
        let prompt = build_prompt(&[message("tester", "[语音]", false)], &[], &[]);
        assert!(prompt.contains("a voice message you cannot hear"));
    }

    /// The screen has no eyes. A picture reaches it as the literal text "[图片]", so the
    /// recognised description has to come along or every picture batch is declined.
    #[test]
    fn prompt_carries_the_recognised_picture_text() {
        let lines = vec!["m1 的第1张图：photo｜画面一只白猫趴在键盘上".to_string()];
        let prompt = build_prompt(&[message("tester", "[图片]", false)], &[], &lines);
        assert!(prompt.contains("What the pictures contain"));
        assert!(prompt.contains("白猫趴在键盘上"));
    }

    #[test]
    fn vision_calls_rotate_across_the_three_published_models() {
        assert_eq!(vision_model(0), "hy3");
        assert_eq!(vision_model(1), "hy3-b");
        assert_eq!(vision_model(2), "hy3-c");
        assert_eq!(vision_model(3), "hy3");
    }

    #[test]
    fn a_single_picture_answer_is_renumbered_to_its_batch_position() {
        let raw = r#"{"images":[{"index":1,"kind":"sticker","ocr":"喵","description":"白猫","emotion":"得意","self":"other","nsfw":false}]}"#;
        let tag = tag_from_single(raw, 3).expect("应有标签");
        assert_eq!(tag.index, 3);
        assert_eq!(tag.ocr, "喵");
    }

    #[test]
    fn session_id_is_stable_per_group() {
        let a = Preflight::session_id("2000000001", "900000002");
        let b = Preflight::session_id("2000000001", "900000002");
        let c = Preflight::session_id("2000000001", "895586129");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, "preflight:2000000001:900000002");
        assert_eq!(a.matches(':').count(), 2);
    }

    #[test]
    fn prompt_carries_aliased_recent_without_raw_ids() {
        let prompt = build_prompt(
            &[message("tester", "在吗", false)],
            &[("u1".into(), "昨天聊过咖啡".into())],
            &[],
        );
        assert!(prompt.contains("Recent group log:"));
        assert!(prompt.contains("u1: 昨天聊过咖啡"));
        assert!(!prompt.contains("123456789"));
        assert!(!prompt.contains("900000002"));
    }

    /// The damper's structure line rides in the prompt when there is one, and it says
    /// nothing about how long anything waited — the same rule as every other hint.
    #[test]
    fn prompt_carries_the_structure_hint_when_given_one() {
        let hint = "结构提示：近10分钟发言集中度 85%，主要来自同一人，本机器人已连续回复 7 轮，当前降温等级 L1。";
        let prompt =
            build_prompt_with_hint(&[message("tester", "在吗", false)], &[], &[], Some(hint));
        assert!(prompt.contains(hint), "prompt: {prompt}");
        // Still after the stable schema, still before the burst body.
        let schema_at = prompt.find("Rate how likely").expect("规则");
        let hint_at = prompt.find("集中度").expect("提示");
        let body_at = prompt.find("tester").expect("正文");
        assert!(schema_at < hint_at);
        assert!(hint_at < body_at);
        // The hint must not leak a wait duration back into the gate.
        assert!(!prompt.contains("Unanswered burst so far"));
        assert!(!prompt.contains("waiting"));
    }

    /// No hint, no blank line: the prompt must be byte-identical to the pre-damper one
    /// when there is nothing to say, or every cached prefix shifts for nothing.
    #[test]
    fn prompt_without_a_hint_is_unchanged() {
        let messages = [message("tester", "在吗", true)];
        let recent = [("u1".to_string(), "昨天聊过咖啡".to_string())];
        let images = ["m1 的第1张图：photo｜白猫".to_string()];
        assert_eq!(
            build_prompt(&messages, &recent, &images),
            build_prompt_with_hint(&messages, &recent, &images, None)
        );
        assert_eq!(
            build_prompt(&messages, &recent, &images),
            build_prompt_with_hint(&messages, &recent, &images, Some("   "))
        );
    }

    #[test]
    fn qq_sub_type_decides_kind_over_the_model() {
        // 1 is a sticker, 0 a picture: the sender's choice beats the tagger's guess.
        assert_eq!(resolve_kind(Some("1"), "photo"), "sticker");
        assert_eq!(resolve_kind(Some("0"), "sticker"), "photo");
        // An unexpected flag is surfaced verbatim rather than silently normalised.
        assert_eq!(resolve_kind(Some("7"), "sticker"), "image:7");
        // No flag at all: the model is the only witness, so it keeps the call.
        assert_eq!(resolve_kind(None, "sticker"), "sticker");
        assert_eq!(resolve_kind(None, "screenshot"), "screenshot");
    }

    fn tag(kind: &str, self_ref: &str) -> ImageTag {
        ImageTag {
            index: 1,
            kind: kind.into(),
            ocr: String::new(),
            description: "一只白猫趴在键盘上".into(),
            emotion: String::new(),
            self_ref: self_ref.into(),
            nsfw: false,
        }
    }

    /// `self` is not a sticker-only field. A photo that happens to be her must say so,
    /// otherwise the agent reads a picture of herself as a stranger.
    #[test]
    fn self_is_reported_for_photos_as_well_as_stickers() {
        assert!(tag("photo", "self").render().contains("主体self"));
        assert!(tag("sticker", "self").render().contains("主体self"));
        assert!(
            tag("art", "self_series")
                .render()
                .contains("主体self_series")
        );
        // Someone else's picture must stay silent rather than claim a subject.
        assert!(!tag("photo", "other").render().contains("主体"));
    }

    /// A photo has no library handle: it is conversation material, so the rendered line
    /// must not invite `sticker_keep` on something that can never be kept.
    #[test]
    fn a_photo_line_carries_no_sticker_handle() {
        let line = tag("photo", "self").render();
        assert!(!line.contains('#'));
        assert!(line.starts_with("photo"));
    }
}
