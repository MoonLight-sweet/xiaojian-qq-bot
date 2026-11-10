//! Rolling handover notes: the shift-change note that makes the 35-turn
//! conversation rotation (and resident rebuilds after a crash) seamless.
//!
//! The seed brief already carries the latest chunk summary, the active facts
//! and the last 40 raw messages. What it cannot carry is the *live thread*:
//! who is mid-argument about what, which question is still hanging, what the
//! bot just promised. That state lives only in the outgoing session's context
//! and dies with it. This module distils it, in the background, from the
//! group's recent messages into a short second-person note addressed to the
//! bot itself, refreshed by the fold loop whenever the group has moved on.
//! The seed brief leads with it.
//!
//! Derived state like the digest: if the model fails or the note fails
//! validation, the old note stays — a stale handover still beats none, and
//! the raw window in the seed covers the freshest turns anyway.

use std::sync::Arc;

use crate::memory::compaction::{SummaryModel, leaks_identifier};
use crate::memory::extract::json_body;
use crate::store::{GroupMessageRecord, SqliteStore};

/// How many of the group's latest messages the note is distilled from.
pub const HANDOVER_WINDOW_MESSAGES: i64 = 100;
/// Bounds on one note: long enough for the live threads, short enough to
/// stay a note and not a second chunk summary.
pub const MIN_HANDOVER_CHARS: usize = 10;
pub const MAX_HANDOVER_CHARS: usize = 600;

/// Render the handover prompt. The bot's own lines are labelled 你 (it is the
/// reader), humans by display name — the fold prompt's identity lesson
/// applies here too: an unlabelled bot line becomes "a member said…".
pub fn render_handover_prompt(previous: Option<&str>, window: &[GroupMessageRecord]) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str(
        "你在给「下一个你」写一张交接班纸条。群聊机器人每隔一段时间会换一个全新会话，\
         纸条是你留给接班的自己看的，用第二人称（「你」）写。\n\
         只写接班后立刻用得上的活线头：\n\
         - 正在进行的话题：谁在和谁聊什么、聊到哪一步、什么立场还没收尾；\n\
         - 还没被回答的问题：谁问的、问的什么；\n\
         - 答应过的事、约好的事（时间、事由）；\n\
         - 正在玩的梗、正在用的称呼和玩笑规矩；\n\
         - 当前气氛（一句话）。\n\
         规则：\n\
         - 只写消息里真实出现过的内容，不要推测、不要评价、不要写长期事实（那些另有记忆库）；\n\
         - 提到群友用消息里标注的名字；标注「你」的就是你自己（机器人 Gemini），别把自己写成群友；\n\
         - 不写任何数字 id、群号、QQ 号；\n\
         - 不写固定颜文字、标点符号习惯或句式模板（严禁把颜文字或句末口癖作为规则写入纸条）；\n\
         - 不超过 400 字，像纸条一样短句分行也没关系。\n\
         只输出一个 JSON 对象：{\"handover\": \"<纸条正文>\"}。\n\n",
    );
    if let Some(previous) = previous.filter(|text| !text.trim().is_empty()) {
        out.push_str("# 上一张纸条（仍然成立的线头保留，已经聊完的删掉）\n");
        out.push_str(previous.trim());
        out.push_str("\n\n");
    }
    out.push_str("# 最近的群聊（旧→新）\n");
    for message in window {
        let who = if message.speaker_role == "bot" {
            "你".to_string()
        } else {
            message
                .display_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "群友".to_string())
        };
        let mut text = message.text.trim().to_string();
        if text.is_empty() && message.has_image {
            text = "[图片]".to_string();
        }
        let text: String = text.chars().take(120).collect();
        out.push_str(&format!("- {who}：{text}\n"));
    }
    out
}

/// Pull the note out of the model's JSON, tolerating fenced output. Same
/// discipline as chunks: bounded length, no leaked identifiers.
pub fn parse_handover(raw: &str) -> Option<String> {
    let value = json_body(raw)?;
    let note = value
        .get("handover")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| {
            let chars = s.chars().count();
            (MIN_HANDOVER_CHARS..=MAX_HANDOVER_CHARS).contains(&chars)
        })?;
    if leaks_identifier(&note) {
        return None;
    }
    Some(note)
}

/// Refresh one group's handover note when the group has moved on since the
/// stored note was written. Returns a short outcome word for the audit
/// trail; the caller audits it.
pub async fn refresh_handover(
    store: &Arc<SqliteStore>,
    bot_id: &str,
    group_id: &str,
    model: &Arc<dyn SummaryModel>,
    model_id: &str,
    now: i64,
) -> &'static str {
    let window = match store
        .recent_group_messages(bot_id, group_id, HANDOVER_WINDOW_MESSAGES)
        .await
    {
        Ok(window) => window,
        Err(_) => return "store",
    };
    let Some(last) = window.last() else {
        return "empty";
    };
    let existing = store.get_handover(bot_id, group_id).await.ok().flatten();
    if let Some(existing) = &existing
        && existing.source_msg_id >= last.id
    {
        return "fresh";
    }
    let prompt =
        render_handover_prompt(existing.as_ref().map(|row| row.note_text.as_str()), &window);
    let raw = match model.summarise(prompt).await {
        Ok(raw) => raw,
        Err(_) => return "model",
    };
    let Some(note) = parse_handover(&raw) else {
        return "parse";
    };
    match store
        .put_handover(bot_id, group_id, &note, last.id, model_id, now)
        .await
    {
        Ok(()) => "ok",
        Err(_) => "store",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: i64, role: &str, name: &str, text: &str) -> GroupMessageRecord {
        GroupMessageRecord {
            id,
            bot_id: "9999".into(),
            group_id: "500".into(),
            message_id: format!("m{id}"),
            sender_id: if role == "bot" { "9999" } else { "42" }.into(),
            display_name: Some(name.into()),
            speaker_role: role.into(),
            text: text.into(),
            has_image: false,
            sticker_hash: None,
            sub_type: None,
            image_tags: None,
            created_at: id,
            archived: false,
        }
    }

    #[test]
    fn prompt_labels_the_bot_as_you_and_humans_by_name() {
        let window = vec![
            record(1, "member", "大肥鱼", "这图啥意思"),
            record(2, "bot", "Gemini", "这是鲸落啦"),
        ];
        let prompt = render_handover_prompt(Some("旧纸条：还在聊鲸鱼"), &window);
        assert!(prompt.contains("- 你：这是鲸落啦"), "got: {prompt}");
        assert!(prompt.contains("- 大肥鱼：这图啥意思"), "got: {prompt}");
        assert!(prompt.contains("# 上一张纸条"), "got: {prompt}");
        assert!(prompt.contains("旧纸条：还在聊鲸鱼"), "got: {prompt}");
    }

    #[test]
    fn parse_accepts_a_wellformed_note() {
        let raw = r#"{"handover": "大肥鱼还在问鲸落的事，你答应给他找张大图。"}"#;
        assert_eq!(
            parse_handover(raw).as_deref(),
            Some("大肥鱼还在问鲸落的事，你答应给他找张大图。")
        );
    }

    #[test]
    fn parse_rejects_leaks_garbage_and_out_of_bounds() {
        assert!(parse_handover("not json").is_none());
        assert!(parse_handover(r#"{"handover": "太短"}"#).is_none());
        let leak = r#"{"handover": "群 123456789 里有人在问问题呢"}"#;
        assert!(parse_handover(leak).is_none());
        let long = format!(
            r#"{{"handover": "{}"}}"#,
            "长".repeat(MAX_HANDOVER_CHARS + 1)
        );
        assert!(parse_handover(&long).is_none());
    }
}
