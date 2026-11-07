//! Wire → business normalization.
//!
//! This is the security boundary for untrusted input: group names, nicknames and
//! message text are hostile data. Nothing here forwards a raw QQ id to the agent;
//! ids stay inside Rust and SQLite and the agent only ever sees `speaker_ref` and
//! `message_ref`.

use serde::{Deserialize, Serialize};

use crate::onebot::event::{RawEvent, RawMessage, RawSender};

/// A text/at/image/other segment after normalization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Segment {
    /// Plain text, already unescaped.
    Text { text: String },
    /// The bot itself was mentioned.
    AtBot,
    /// Somebody else was mentioned. The raw QQ from the wire never enters
    /// this type: intake re-reads the target from the event and resolves it
    /// to the member's alias and display name (see `Intake::resolve_mentions`),
    /// so the agent learns *who* was addressed by alias — never by number.
    /// Both fields are `None` between normalize and that resolution.
    AtOther {
        /// Alias (`uN`) of the mentioned member, resolved by intake.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_ref: Option<String>,
        /// Display name of the mentioned member when the group log knows
        /// one. Display text only, not an identity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_name: Option<String>,
    },
    /// An image. `url` is retained only for the media downloader, never logged and
    /// never sent to the agent.
    ///
    /// `sub_type` is QQ's own sticker flag (`1` = animated sticker, `0` = picture). It
    /// arrives from the wire and is the only zero-cost way to tell a sticker from a
    /// photo, so it is kept instead of being re-guessed by the vision model.
    Image {
        url: Option<String>,
        file: Option<String>,
        sub_type: Option<String>,
    },
    /// A voice message. The audio itself is never fetched; the segment
    /// renders as the `[语音]` placeholder (mirroring `[图片]`) so the
    /// agent and the gates can see that somebody spoke, instead of the
    /// message collapsing into empty text.
    Voice,
    /// A segment type we do not model.
    Unsupported { seg: String },
}

/// A normalized inbound group message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingMessage {
    pub bot_id: String,
    pub group_id: String,
    pub message_id: String,
    pub sender_id: String,
    /// Stable per-group alias, assigned by the store (`u1`, `u2`, ...).
    pub speaker_ref: String,
    /// Role derived from a trusted id comparison, never from text.
    pub speaker_role: SpeakerRole,
    /// Sanitized QQ card, or nickname when the card is empty. Not an identity.
    #[serde(default)]
    pub display_name: String,
    /// Nickname previously stored for this member. Not an identity.
    #[serde(default)]
    pub known_alias: Option<String>,
    pub received_at: i64,
    pub segments: Vec<Segment>,
    /// Plain text of the message with image placeholders removed.
    pub text: String,
    /// True when the bot was @-mentioned in this message.
    pub mentions_bot: bool,
    /// The OneBot message id this message quotes (a `reply` segment), when it
    /// quotes one. The quoted *content* is never copied in from the wire; the
    /// turn resolves it against the group log, so a forged quote can at most
    /// point at a message that really exists in this group.
    #[serde(default)]
    pub quote_of: Option<String>,
}

/// Who sent the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerRole {
    /// The configured owner, matched by exact decimal id equality.
    Owner,
    /// Anybody else in the group.
    Member,
    /// This bot, written on successful send so L1 search can find her own lines.
    Bot,
}

impl SpeakerRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SpeakerRole::Owner => "owner",
            SpeakerRole::Member => "member",
            SpeakerRole::Bot => "bot",
        }
    }
}

/// A bot command recognized before enqueueing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `/gemini status`
    Status,
    /// `/gemini pause`
    Pause,
    /// `/gemini resume`
    Resume,
}

impl Command {
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        let rest = trimmed.strip_prefix("/gemini")?;
        let mut parts = rest.split_whitespace();
        match parts.next() {
            None => None,
            Some(word) => match word.to_ascii_lowercase().as_str() {
                "status" => Some(Command::Status),
                "pause" => Some(Command::Pause),
                "resume" => Some(Command::Resume),
                _ => None,
            },
        }
    }
}

/// Reason an event was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Not a message event.
    NotMessage,
    /// Private message: never handled.
    PrivateMessage,
    /// The bot's own message.
    SelfMessage,
    /// Missing a required identifier.
    MissingField,
    /// Group is disabled or paused.
    GroupInactive,
    /// Empty after normalization (e.g. only unsupported segments).
    Empty,
    /// SnowLuma's own status trigger, which it also forwards as a group message.
    SnowLumaCommand,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::NotMessage => "not_message",
            SkipReason::PrivateMessage => "private_message",
            SkipReason::SelfMessage => "self_message",
            SkipReason::MissingField => "missing_field",
            SkipReason::GroupInactive => "group_inactive",
            SkipReason::Empty => "empty",
            SkipReason::SnowLumaCommand => "snowluma_command",
        }
    }
}

/// Result of normalizing one raw event.
#[derive(Debug)]
pub enum Normalized {
    /// A group message to enqueue.
    Message(Box<IncomingMessage>),
    /// An owner command, handled synchronously and never sent to the agent.
    Command {
        group_id: String,
        command: Command,
        speaker_role: SpeakerRole,
    },
    /// Skipped, with a reason for the audit counter.
    Skipped(SkipReason),
}

/// Normalize a raw event.
///
/// `speaker_ref` is passed in because alias allocation needs the store; the caller
/// resolves it before calling. `bot_id` and `owner_qq_id` are trusted config.
pub fn normalize_event(
    event: &RawEvent,
    bot_id: &str,
    owner_qq_id: &str,
    speaker_ref: &str,
    received_at: i64,
) -> Normalized {
    if event.post_type.as_deref() != Some("message") {
        return Normalized::Skipped(SkipReason::NotMessage);
    }
    if event.message_type.as_deref() != Some("group") {
        return Normalized::Skipped(SkipReason::PrivateMessage);
    }

    let group_id = match event.group_id.as_deref().filter(|g| is_decimal_id(g)) {
        Some(g) => g.to_string(),
        None => return Normalized::Skipped(SkipReason::MissingField),
    };
    let user_id = match event.user_id.as_deref().filter(|u| is_decimal_id(u)) {
        Some(u) => u.to_string(),
        None => return Normalized::Skipped(SkipReason::MissingField),
    };
    let message_id = match event.message_id.as_deref().filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => return Normalized::Skipped(SkipReason::MissingField),
    };
    let self_id = event
        .self_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(bot_id);
    if user_id == self_id || user_id == bot_id {
        return Normalized::Skipped(SkipReason::SelfMessage);
    }

    let mut segments = normalize_segments(event, bot_id);
    // A poke delivered as a message segment (some clients send it this way
    // instead of as a notify notice): poking the bot is an address, exactly
    // like an @ — the cheapest "you, come here" there is. Surface it as an
    // AtBot plus a readable marker so the model knows what happened.
    let poked_bot = matches!(&event.message, Some(RawMessage::Segments(raw)) if raw.iter().any(|seg| {
        seg.seg_type == "poke"
            && seg.data.as_ref().and_then(|d| d.qq.as_deref()) == Some(bot_id)
    }));
    if poked_bot {
        if !segments.iter().any(|s| matches!(s, Segment::AtBot)) {
            segments.insert(0, Segment::AtBot);
        }
        if !segments.iter().any(|s| matches!(s, Segment::Text { .. })) {
            segments.push(Segment::Text {
                text: "（戳了戳你）".to_string(),
            });
        }
    }
    let mentions_bot = segments.iter().any(|s| matches!(s, Segment::AtBot));
    let text = plain_text(&segments);

    if segments.is_empty() {
        return Normalized::Skipped(SkipReason::Empty);
    }
    // An @ that carries no text ("@bot" and nothing else) is a real message: it is the
    // cheapest way a human says "you, come here". Judging it by text alone dropped it,
    // so a plain mention never reached the gate at all. The same holds for an @ of
    // another member: "@小鲸鱼" alone is the first half of an exchange, and dropping
    // it orphaned the answer ("叫我干嘛") — the model then had to guess who had been
    // called and guessed itself (2026-11-03, a production group).
    let has_address = segments.iter().any(|s| {
        matches!(
            s,
            Segment::AtBot | Segment::AtOther { .. } | Segment::Image { .. }
        )
    });
    if text.trim().is_empty() && !has_address {
        return Normalized::Skipped(SkipReason::Empty);
    }

    // SnowLuma answers `#sl` itself and, unless its own swallow flag is on, still
    // forwards the frame to us. Dropping the trigger here keeps the bot's own gate
    // independent of that container's configuration.
    if text.trim() == "#sl" {
        return Normalized::Skipped(SkipReason::SnowLumaCommand);
    }

    let speaker_role = if user_id == owner_qq_id {
        SpeakerRole::Owner
    } else {
        SpeakerRole::Member
    };
    let display_name = event
        .sender
        .as_ref()
        .map(|sender| {
            display_name_from(
                sender.card.as_deref().unwrap_or(""),
                sender.nickname.as_deref().unwrap_or(""),
            )
        })
        .unwrap_or_default();

    // Commands are recognized before enqueueing and are always owner-only. A
    // non-owner `/gemini ...` message is ordinary chat: it is neither executed nor
    // forwarded as a command.
    if speaker_role == SpeakerRole::Owner
        && let Some(command) = Command::parse(&text)
    {
        return Normalized::Command {
            group_id,
            command,
            speaker_role,
        };
    }

    Normalized::Message(Box::new(IncomingMessage {
        bot_id: bot_id.to_string(),
        group_id,
        message_id,
        sender_id: user_id,
        speaker_ref: speaker_ref.to_string(),
        speaker_role,
        display_name,
        known_alias: None,
        received_at,
        segments,
        text,
        mentions_bot,
        quote_of: quoted_message_id(event),
    }))
}

/// The message id a `reply` segment points at, if this event quotes one.
///
/// The segment itself stays `Unsupported` in the normalized segment list —
/// quoting is metadata about the message, not content — but the target id is
/// kept so the turn can show her what is being replied to.
fn quoted_message_id(event: &RawEvent) -> Option<String> {
    let Some(RawMessage::Segments(raw)) = &event.message else {
        return None;
    };
    raw.iter()
        .filter(|seg| seg.seg_type == "reply")
        .filter_map(|seg| seg.data.as_ref().and_then(|d| d.id.clone()))
        .find(|id| !id.is_empty())
}

/// Card wins over nickname. Control characters are removed and the result is
/// capped at 32 Unicode scalars. The value is display text only.
pub fn display_name_from(card: &str, nickname: &str) -> String {
    let card = sanitize_display_name(card);
    if !card.is_empty() {
        return card;
    }
    sanitize_display_name(nickname)
}

fn sanitize_display_name(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}' | '\u{feff}') {
            continue;
        }
        out.push(ch);
        if out.chars().count() >= 32 {
            break;
        }
    }
    out.trim().to_string()
}

/// Convert raw segments to normalized ones.
///
/// The card and nickname are kept only as [`IncomingMessage::display_name`].
/// They never affect [`SpeakerRole`].
pub fn normalize_segments(event: &RawEvent, bot_id: &str) -> Vec<Segment> {
    let sender_ids: Option<&RawSender> = event.sender.as_ref();
    let _ = sender_ids;
    match &event.message {
        Some(RawMessage::Text(text)) => {
            let text = text.clone();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![Segment::Text { text }]
            }
        }
        Some(RawMessage::Segments(raw)) => raw
            .iter()
            .filter_map(|seg| {
                let kind = seg.seg_type.as_str();
                match kind {
                    "text" => {
                        let text = seg
                            .data
                            .as_ref()
                            .and_then(|d| d.text.clone())
                            .unwrap_or_default();
                        if text.is_empty() {
                            None
                        } else {
                            Some(Segment::Text { text })
                        }
                    }
                    "at" => {
                        let qq = seg
                            .data
                            .as_ref()
                            .and_then(|d| d.qq.clone())
                            .unwrap_or_default();
                        if qq == bot_id {
                            Some(Segment::AtBot)
                        } else if qq == "all" {
                            // @全体成员 has no single target; the name is
                            // the whole content of the mention.
                            Some(Segment::AtOther {
                                target_ref: None,
                                target_name: Some("全体成员".to_string()),
                            })
                        } else {
                            Some(Segment::AtOther {
                                target_ref: None,
                                target_name: None,
                            })
                        }
                    }
                    "image" => Some(Segment::Image {
                        url: seg.data.as_ref().and_then(|d| d.url.clone()),
                        file: seg.data.as_ref().and_then(|d| d.file.clone()),
                        sub_type: seg.data.as_ref().and_then(|d| d.sub_type.clone()),
                    }),
                    "record" => Some(Segment::Voice),
                    "face" => {
                        let id_str = seg.data.as_ref().and_then(|d| d.id.clone()).unwrap_or_default();
                        Some(Segment::Text {
                            text: if id_str.is_empty() {
                                "[表情]".to_string()
                            } else {
                                format!("[表情:{}]", id_str)
                            },
                        })
                    }
                    "mface" => {
                        let summary = seg.data.as_ref().and_then(|d| d.summary.clone()).unwrap_or_default();
                        let url = seg.data.as_ref().and_then(|d| d.url.clone());
                        if !summary.is_empty() {
                            let text = if summary.starts_with('[') && summary.ends_with(']') {
                                summary
                            } else {
                                format!("[{}]", summary)
                            };
                            Some(Segment::Text { text })
                        } else if let Some(url) = url {
                            Some(Segment::Image {
                                url: Some(url),
                                file: seg.data.as_ref().and_then(|d| d.file.clone()),
                                sub_type: Some("1".to_string()),
                            })
                        } else {
                            Some(Segment::Text { text: "[动画表情]".to_string() })
                        }
                    }
                    // Pokes are handled at the event level (see the scan in
                    // `normalize_event`): as a segment they carry no content
                    // of their own.
                    "poke" => None,
                    // `reply`, `forward`, `video`, ... are not
                    // modeled; they are surfaced as unsupported instead of being
                    // silently converted into text.
                    other => Some(Segment::Unsupported {
                        seg: other.to_string(),
                    }),
                }
            })
            .collect(),
        None => Vec::new(),
    }
}

/// Aliases of the members a segment stream @-mentions, in order,
/// deduplicated. The bot herself is not included (`mentions_bot` covers
/// her), and neither is @全体成员 (it has no single target). Empty until
/// intake has resolved the wire targets.
pub fn mention_refs(segments: &[Segment]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for segment in segments {
        if let Segment::AtOther {
            target_ref: Some(r),
            ..
        } = segment
            && !out.contains(r)
        {
            out.push(r.clone());
        }
    }
    out
}

/// Build the plain text used for batching, command detection and length limits.
pub fn plain_text(segments: &[Segment]) -> String {
    let mut out = String::new();
    for seg in segments {
        match seg {
            Segment::Text { text } => out.push_str(text),
            Segment::AtBot => out.push(' '),
            Segment::AtOther {
                target_name,
                target_ref,
            } => {
                // A resolved mention names its addressee (`@小鲸鱼 `) so a
                // reader can tell who a line was aimed at; unresolved
                // (normalize has no store) it stays the legacy blank until
                // intake resolves the target and re-renders.
                match target_name.as_deref().or(target_ref.as_deref()) {
                    Some(name) => {
                        out.push('@');
                        out.push_str(name);
                        out.push(' ');
                    }
                    None => out.push(' '),
                }
            }
            Segment::Image { .. } => out.push_str("[图片]"),
            Segment::Voice => out.push_str("[语音]"),
            Segment::Unsupported { .. } => {}
        }
    }
    out
}

/// Total plain-text length for batching.
pub fn text_len(segments: &[Segment]) -> usize {
    plain_text(segments).chars().count()
}

fn is_decimal_id(raw: &str) -> bool {
    !raw.is_empty() && raw.len() <= 20 && raw.bytes().all(|b| b.is_ascii_digit())
}

/// Escape text for a OneBot text message so a group member cannot inject markup.
///
/// OneBot v11 uses CQ codes in the string message format. The bot always sends the
/// array format, but `&`, `[` and `]` are still escaped defensively so that a
/// downstream implementation that re-serializes to string cannot be fooled.
pub fn escape_cq(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '[' => out.push_str("&#91;"),
            ']' => out.push_str("&#93;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: &str = "999";
    const OWNER: &str = "555";

    fn event(json: &str) -> RawEvent {
        serde_json::from_str(json).expect("解析事件")
    }

    #[test]
    fn skips_events_that_are_not_group_chat() {
        let cases = [
            (
                r#"{"post_type":"message","message_type":"private","user_id":"1","message_id":"1","message":"hi"}"#,
                SkipReason::PrivateMessage,
            ),
            (
                r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"999","message_id":"1","message":"hi","self_id":"999"}"#,
                SkipReason::SelfMessage,
            ),
            (
                r#"{"post_type":"notice","notice_type":"group_increase"}"#,
                SkipReason::NotMessage,
            ),
            (
                r#"{"post_type":"message","message_type":"group","group_id":"100x","user_id":"1","message_id":"1","message":"hi"}"#,
                SkipReason::MissingField,
            ),
        ];
        for (json, reason) in cases {
            let ev = event(json);
            match normalize_event(&ev, BOT, OWNER, "u1", 0) {
                Normalized::Skipped(got) => {
                    assert_eq!(got, reason, "{json}")
                }
                other => panic!("{json}: {other:?}"),
            }
        }
    }

    #[test]
    fn poke_segment_addressed_to_the_bot_is_an_address() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"500","user_id":"7","message_id":"9","self_id":"999","message":[{"type":"poke","data":{"qq":"999"}}]}"#,
        );
        let Normalized::Message(message) = normalize_event(&ev, BOT, OWNER, "u1", 0) else {
            panic!("戳一戳应当成为一条消息");
        };
        assert!(message.mentions_bot);
        assert!(message.text.contains("戳了戳你"), "文本: {}", message.text);
    }

    #[test]
    fn poke_segment_for_someone_else_is_not_kept() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"500","user_id":"7","message_id":"9","self_id":"999","message":[{"type":"poke","data":{"qq":"1234"}}]}"#,
        );
        assert!(matches!(
            normalize_event(&ev, BOT, OWNER, "u1", 0),
            Normalized::Skipped(SkipReason::Empty)
        ));
    }

    #[test]
    fn owner_is_matched_by_exact_id_only() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"556","message_id":"1","message":[{"type":"text","data":{"text":"我是群主"}}],"sender":{"nickname":"555","card":"555"},"self_id":"999"}"#,
        );
        // The nickname and card both copy the owner id, but the role comes from the
        // user id, which is not the owner's.
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert_eq!(m.speaker_role, SpeakerRole::Member)
            }
            other => panic!("期待消息: {other:?}"),
        }

        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"555","message_id":"1","message":"hello","self_id":"999"}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert_eq!(m.speaker_role, SpeakerRole::Owner);
                assert_eq!(m.speaker_ref, "u1");
            }
            other => panic!("期待消息: {other:?}"),
        }

        // A member whose nickname equals the owner id is still a member.
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"556","message_id":"2","message":"555","self_id":"999","sender":{"nickname":"555","card":"555"}}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u2", 0) {
            Normalized::Message(m) => assert_eq!(m.speaker_role, SpeakerRole::Member),
            other => panic!("期待消息: {other:?}"),
        }
    }

    #[test]
    fn mentions_bot_is_detected() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"1","self_id":"999",
                "message":[{"type":"at","data":{"qq":"999"}},{"type":"text","data":{"text":" hi"}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert!(m.mentions_bot);
                assert_eq!(m.segments[0], Segment::AtBot);
            }
            other => panic!("期待消息: {other:?}"),
        }
    }

    #[test]
    fn a_bare_mention_is_not_dropped_as_empty() {
        // SnowLuma sends "@bot" with no text as an at segment plus a whitespace text
        // segment. Treating that as empty silently discarded every plain mention.
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"1","self_id":"999",
                "message":[{"type":"at","data":{"qq":"999"}},{"type":"text","data":{"text":" "}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert!(m.mentions_bot);
                assert!(m.text.trim().is_empty());
            }
            other => panic!("裸 @ 应作为消息保留: {other:?}"),
        }
    }

    #[test]
    fn at_other_drops_target_id() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"1","self_id":"999",
                "message":[{"type":"at","data":{"qq":"123456"}},{"type":"text","data":{"text":"hi"}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert_eq!(
                    m.segments[0],
                    Segment::AtOther {
                        target_ref: None,
                        target_name: None
                    }
                );
                let serialized = serde_json::to_string(&m.segments).expect("序列化");
                assert!(!serialized.contains("123456"), "@他人不得泄漏其 QQ 号");
            }
            other => panic!("期待消息: {other:?}"),
        }
    }

    #[test]
    fn a_bare_mention_of_someone_else_is_not_dropped() {
        // "@小鲸鱼" with no text at all: the first half of an exchange.
        // Dropping it left "叫我干嘛" without an antecedent (2026-11-03).
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"1","self_id":"999",
                "message":[{"type":"at","data":{"qq":"123456"}},{"type":"text","data":{"text":" "}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert!(!m.mentions_bot);
            }
            other => panic!("裸 @他人应作为消息保留: {other:?}"),
        }
    }

    #[test]
    fn resolved_mention_renders_its_name_in_plain_text() {
        let segments = vec![
            Segment::AtOther {
                target_ref: Some("u5".to_string()),
                target_name: Some("小鲸鱼".to_string()),
            },
            Segment::Text {
                text: " 你咋还活着".to_string(),
            },
        ];
        assert_eq!(plain_text(&segments), "@小鲸鱼  你咋还活着");
        assert_eq!(mention_refs(&segments), vec!["u5".to_string()]);
        // Unresolved keeps the legacy blank.
        let unresolved = vec![Segment::AtOther {
            target_ref: None,
            target_name: None,
        }];
        assert_eq!(plain_text(&unresolved), " ");
        assert!(mention_refs(&unresolved).is_empty());
    }

    #[test]
    fn at_all_renders_as_everyone_without_a_target() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"1","self_id":"999",
                "message":[{"type":"at","data":{"qq":"all"}},{"type":"text","data":{"text":"开饭了"}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert_eq!(
                    m.segments[0],
                    Segment::AtOther {
                        target_ref: None,
                        target_name: Some("全体成员".to_string())
                    }
                );
                assert!(m.text.starts_with("@全体成员"), "文本: {}", m.text);
            }
            other => panic!("期待消息: {other:?}"),
        }
    }

    #[test]
    fn unsupported_segment_is_preserved_as_kind() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"1","self_id":"999",
                "message":[{"type":"face","data":{"id":"1"}},{"type":"text","data":{"text":"hi"}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert_eq!(m.segments[0], Segment::Unsupported { seg: "face".into() });
                assert_eq!(m.text, "hi");
            }
            other => panic!("期待消息: {other:?}"),
        }
    }

    #[test]
    fn image_only_message_is_kept() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"1","self_id":"999",
                "message":[{"type":"image","data":{"file":"a.png","url":"https://media.example.com/a.png"}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert_eq!(m.text, "[图片]");
                assert!(matches!(m.segments[0], Segment::Image { .. }));
            }
            other => panic!("期待消息: {other:?}"),
        }
    }

    #[test]
    fn voice_only_message_is_kept() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"1","self_id":"999",
                "message":[{"type":"record","data":{"file":"a.amr","url":"https://media.example.com/a.amr"}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert_eq!(m.text, "[语音]");
                assert_eq!(m.segments[0], Segment::Voice);
            }
            other => panic!("期待消息: {other:?}"),
        }
    }

    #[test]
    fn owner_commands_are_recognized() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"555","message_id":"1","message":"/gemini pause","self_id":"999"}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Command {
                command,
                speaker_role,
                ..
            } => {
                assert_eq!(command, Command::Pause);
                assert_eq!(speaker_role, SpeakerRole::Owner);
            }
            other => panic!("期待命令: {other:?}"),
        }
    }

    #[test]
    fn non_owner_cannot_issue_commands() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"556","message_id":"1","message":"/gemini pause","self_id":"999"}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u2", 0) {
            Normalized::Message(m) => assert_eq!(m.text, "/gemini pause"),
            other => panic!("非主人命令应当作为普通消息: {other:?}"),
        }
    }

    #[test]
    fn command_parsing_is_strict() {
        assert_eq!(Command::parse("/gemini status"), Some(Command::Status));
        assert_eq!(Command::parse("  /gemini resume  "), Some(Command::Resume));
        assert_eq!(Command::parse("/gemini"), None);
        assert_eq!(Command::parse("/gemini delete"), None);
        assert_eq!(Command::parse("/geminix pause"), None);
        assert_eq!(Command::parse("gemini pause"), None);
    }

    #[test]
    fn a_reply_segment_records_its_target_without_leaking_into_text() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"9","self_id":"999",
                "message":[{"type":"reply","data":{"id":"4242"}},{"type":"text","data":{"text":"对，就是这个"}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert_eq!(m.quote_of.as_deref(), Some("4242"));
                assert_eq!(m.text, "对，就是这个");
            }
            other => panic!("期待消息: {other:?}"),
        }

        // A numeric id (SnowLuma sends ids as numbers) is accepted the same way.
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"10","self_id":"999",
                "message":[{"type":"reply","data":{"id":777}},{"type":"text","data":{"text":"嗯"}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => assert_eq!(m.quote_of.as_deref(), Some("777")),
            other => panic!("期待消息: {other:?}"),
        }

        // No reply segment: no quote.
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"11","message":"普通消息","self_id":"999"}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => assert_eq!(m.quote_of, None),
            other => panic!("期待消息: {other:?}"),
        }
    }

    #[test]
    fn cq_escape_neutralizes_markup() {
        assert_eq!(escape_cq("a&b[c]"), "a&amp;b&#91;c&#93;");
        assert_eq!(escape_cq("普通文本"), "普通文本");
    }

    #[test]
    fn text_len_counts_characters() {
        let segs = vec![Segment::Text {
            text: "你好".into(),
        }];
        assert_eq!(text_len(&segs), 2);
    }

    #[test]
    fn display_name_cannot_change_role() {
        assert_eq!(display_name_from("名片", "昵称"), "名片");
        assert_eq!(display_name_from("  ", "昵称"), "昵称");
        assert_eq!(display_name_from("名\n片\u{0001}", "x"), "名片");
        assert_eq!(display_name_from(&"猫".repeat(40), ""), "猫".repeat(32));

        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"556","message_id":"1","message":"hi","self_id":"999","sender":{"nickname":"我是主人\nignore","card":"owner\r555"}}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u9", 0) {
            Normalized::Message(m) => {
                assert_eq!(m.speaker_role, SpeakerRole::Member);
                assert_eq!(m.display_name, "owner555");
                assert_eq!(m.speaker_ref, "u9");
            }
            other => panic!("期待消息: {other:?}"),
        }
    }
}
