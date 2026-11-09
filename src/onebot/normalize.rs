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
                            } else if let Ok(id) = id_str.parse::<u32>() {
                                if let Some(name) = qq_face_name(id) {
                                    format!("[{}]", name)
                                } else {
                                    format!("[表情:{}]", id_str)
                                }
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


/// Resolve QQ yellow face emoji ID to human-readable label.
pub fn qq_face_name(id: u32) -> Option<&'static str> {
    match id {
        0 => Some("惊讶"),
        1 => Some("撇嘴"),
        2 => Some("色"),
        3 => Some("发呆"),
        4 => Some("得意"),
        5 => Some("流泪"),
        6 => Some("害羞"),
        7 => Some("闭嘴"),
        8 => Some("睡"),
        9 => Some("大哭"),
        10 => Some("尴尬"),
        11 => Some("发怒"),
        12 => Some("调皮"),
        13 => Some("呲牙"),
        14 => Some("微笑"),
        15 => Some("难过"),
        16 => Some("酷"),
        17 => Some("冷汗"),
        18 => Some("抓狂"),
        19 => Some("吐"),
        20 => Some("偷笑"),
        21 => Some("可爱"),
        22 => Some("白眼"),
        23 => Some("傲慢"),
        24 => Some("饥饿"),
        25 => Some("困"),
        26 => Some("惊恐"),
        27 => Some("流汗"),
        28 => Some("憨笑"),
        29 => Some("大兵"),
        30 => Some("奋斗"),
        31 => Some("咒骂"),
        32 => Some("疑问"),
        33 => Some("嘘"),
        34 => Some("晕"),
        35 => Some("折磨"),
        36 => Some("衰"),
        37 => Some("骷髅"),
        38 => Some("敲打"),
        39 => Some("再见"),
        41 => Some("发抖"),
        42 => Some("爱情"),
        43 => Some("跳跳"),
        46 => Some("猪头"),
        49 => Some("拥抱"),
        53 => Some("蛋糕"),
        54 => Some("闪电"),
        55 => Some("炸弹"),
        56 => Some("刀"),
        57 => Some("足球"),
        59 => Some("便便"),
        60 => Some("咖啡"),
        61 => Some("饭"),
        63 => Some("玫瑰"),
        64 => Some("凋谢"),
        66 => Some("爱心"),
        67 => Some("心碎"),
        69 => Some("礼物"),
        74 => Some("太阳"),
        75 => Some("月亮"),
        76 => Some("赞"),
        77 => Some("踩"),
        78 => Some("握手"),
        79 => Some("胜利"),
        85 => Some("飞吻"),
        86 => Some("怄火"),
        89 => Some("西瓜"),
        96 => Some("冷汗"),
        97 => Some("擦汗"),
        98 => Some("抠鼻"),
        99 => Some("鼓掌"),
        100 => Some("糗大了"),
        101 => Some("坏笑"),
        102 => Some("左哼哼"),
        103 => Some("右哼哼"),
        104 => Some("哈欠"),
        105 => Some("鄙视"),
        106 => Some("委屈"),
        107 => Some("快哭了"),
        108 => Some("阴险"),
        109 => Some("亲亲"),
        110 => Some("吓"),
        111 => Some("可怜"),
        112 => Some("菜刀"),
        113 => Some("啤酒"),
        114 => Some("篮球"),
        115 => Some("乒乓"),
        116 => Some("示爱"),
        117 => Some("瓢虫"),
        118 => Some("抱拳"),
        119 => Some("勾引"),
        120 => Some("拳头"),
        121 => Some("差劲"),
        122 => Some("爱你"),
        123 => Some("NO"),
        124 => Some("OK"),
        125 => Some("转圈"),
        129 => Some("挥手"),
        130 => Some("激动"),
        131 => Some("街舞"),
        132 => Some("献吻"),
        133 => Some("左太极"),
        134 => Some("右太极"),
        136 => Some("双喜"),
        137 => Some("鞭炮"),
        138 => Some("灯笼"),
        140 => Some("K歌"),
        144 => Some("喝彩"),
        145 => Some("祈祷"),
        146 => Some("爆筋"),
        147 => Some("棒棒糖"),
        148 => Some("喝奶"),
        151 => Some("飞机"),
        158 => Some("钞票"),
        168 => Some("泪奔"),
        169 => Some("无奈"),
        171 => Some("托腮"),
        172 => Some("卖萌"),
        173 => Some("揉脸"),
        174 => Some("拍手"),
        175 => Some("赞"),
        176 => Some("摊手"),
        177 => Some("头秃"),
        178 => Some("滑稽"),
        179 => Some("汪汪"),
        180 => Some("叹气"),
        181 => Some("抱抱"),
        182 => Some("强笑"),
        183 => Some("摸头"),
        184 => Some("蹭蹭"),
        185 => Some("哼"),
        187 => Some("思考"),
        188 => Some("绝望"),
        190 => Some("脑壳痛"),
        192 => Some("捂脸"),
        193 => Some("辣眼睛"),
        194 => Some("吐血"),
        197 => Some("敬礼"),
        198 => Some("狂笑"),
        199 => Some("面无表情"),
        200 => Some("沧桑"),
        201 => Some("汗"),
        202 => Some("滚"),
        203 => Some("涩"),
        204 => Some("嘿哈"),
        205 => Some("捂脸"),
        206 => Some("奸笑"),
        207 => Some("机智"),
        208 => Some("皱眉"),
        209 => Some("耶"),
        210 => Some("鬼脸"),
        211 => Some("合十"),
        212 => Some("加油"),
        214 => Some("庆祝"),
        216 => Some("击掌"),
        217 => Some("抱拳"),
        218 => Some("托腮"),
        219 => Some("汪汪"),
        222 => Some("摸头"),
        226 => Some("期待"),
        227 => Some("无语"),
        228 => Some("拜托"),
        230 => Some("痛哭"),
        231 => Some("震惊"),
        233 => Some("幽灵"),
        234 => Some("爆米花"),
        240 => Some("摸锦鲤"),
        241 => Some("期待"),
        242 => Some("拜托"),
        243 => Some("嗷呜"),
        244 => Some("狂吃"),
        245 => Some("酸了"),
        246 => Some("绝望"),
        247 => Some("无奈"),
        260 => Some("恶魔"),
        261 => Some("嗷呜"),
        262 => Some("菜汪"),
        263 => Some("汪汪"),
        264 => Some("喵喵"),
        265 => Some("抱抱"),
        266 => Some("摊手"),
        267 => Some("叹气"),
        268 => Some("强笑"),
        269 => Some("脑壳痛"),
        270 => Some("沧桑"),
        271 => Some("痛苦"),
        272 => Some("辣眼睛"),
        273 => Some("敬礼"),
        277 => Some("捂脸"),
        278 => Some("嘿哈"),
        279 => Some("奸笑"),
        280 => Some("机智"),
        281 => Some("皱眉"),
        282 => Some("耶"),
        283 => Some("胖揍"),
        284 => Some("抱头"),
        285 => Some("击掌"),
        286 => Some("害怕"),
        287 => Some("跪了"),
        288 => Some("摸头"),
        289 => Some("蹭蹭"),
        290 => Some("瞪眼"),
        291 => Some("汗颜"),
        292 => Some("苦笑"),
        293 => Some("偷看"),
        294 => Some("疑惑"),
        295 => Some("狂汗"),
        296 => Some("叹气"),
        297 => Some("失望"),
        298 => Some("恐惧"),
        299 => Some("疑问"),
        300 => Some("擦汗"),
        301 => Some("捂脸"),
        302 => Some("破涕为笑"),
        303 => Some("苦笑"),
        304 => Some("翻白眼"),
        305 => Some("哇"),
        306 => Some("害羞"),
        307 => Some("呲牙"),
        308 => Some("傲慢"),
        309 => Some("困"),
        310 => Some("咒骂"),
        311 => Some("嘘"),
        312 => Some("抓狂"),
        313 => Some("吐"),
        314 => Some("偷笑"),
        315 => Some("愉快"),
        316 => Some("白眼"),
        317 => Some("傲慢"),
        318 => Some("困"),
        319 => Some("惊恐"),
        320 => Some("流汗"),
        321 => Some("憨笑"),
        322 => Some("悠闲"),
        323 => Some("奋斗"),
        324 => Some("咒骂"),
        325 => Some("疑问"),
        326 => Some("嘘"),
        327 => Some("晕"),
        328 => Some("折磨"),
        329 => Some("衰"),
        330 => Some("骷髅"),
        331 => Some("敲打"),
        332 => Some("再见"),
        333 => Some("擦汗"),
        334 => Some("抠鼻"),
        335 => Some("鼓掌"),
        336 => Some("糗大了"),
        337 => Some("坏笑"),
        338 => Some("左哼哼"),
        339 => Some("右哼哼"),
        340 => Some("哈欠"),
        341 => Some("鄙视"),
        342 => Some("委屈"),
        343 => Some("快哭了"),
        344 => Some("阴险"),
        345 => Some("亲亲"),
        346 => Some("吓"),
        347 => Some("可怜"),
        348 => Some("菜刀"),
        349 => Some("西瓜"),
        350 => Some("啤酒"),
        351 => Some("咖啡"),
        352 => Some("猪头"),
        353 => Some("玫瑰"),
        354 => Some("凋谢"),
        355 => Some("示爱"),
        356 => Some("爱心"),
        357 => Some("心碎"),
        358 => Some("蛋糕"),
        359 => Some("炸弹"),
        360 => Some("便便"),
        361 => Some("月亮"),
        362 => Some("太阳"),
        363 => Some("拥抱"),
        364 => Some("强"),
        365 => Some("弱"),
        366 => Some("握手"),
        367 => Some("胜利"),
        368 => Some("抱拳"),
        369 => Some("勾引"),
        370 => Some("拳头"),
        371 => Some("差劲"),
        372 => Some("爱你"),
        373 => Some("NO"),
        374 => Some("OK"),
        _ => None,
    }
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
                "message":[{"type":"custom_unsupported","data":{}},{"type":"text","data":{"text":"hi"}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert_eq!(m.segments[0], Segment::Unsupported { seg: "custom_unsupported".into() });
                assert_eq!(m.text, "hi");
            }
            other => panic!("预期消息: {other:?}"),
        }
    }

    #[test]
    fn face_segment_resolves_to_readable_name() {
        let ev = event(
            r#"{"post_type":"message","message_type":"group","group_id":"100","user_id":"1","message_id":"1","self_id":"999",
                "message":[{"type":"face","data":{"id":"178"}},{"type":"face","data":{"id":"14"}}]}"#,
        );
        match normalize_event(&ev, BOT, OWNER, "u1", 0) {
            Normalized::Message(m) => {
                assert_eq!(m.segments[0], Segment::Text { text: "[滑稽]".into() });
                assert_eq!(m.segments[1], Segment::Text { text: "[微笑]".into() });
                assert_eq!(m.text, "[滑稽][微笑]");
            }
            other => panic!("预期消息: {other:?}"),
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
