//! Cognitive memory layer 1 & 2: Pinned reminders and Historian member profiles.
//!
//! Inspired by `69gg/Undefined` (3-layer cognitive memory + background Historian
//! 4-section user profiling + joke/fact separation) and `MemPalace` (per-person wings).

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;

use crate::memory::compaction::{SummaryModel, leaks_identifier};
use crate::memory::extract::json_body;
use crate::store::{
    GroupMessageRecord, MemberProfileRecord, MemoryRecord, PinnedMemoryRecord, SqliteStore,
};

/// Maximum characters allowed per profile section.
pub const MAX_SECTION_CHARS: usize = 160;
/// Maximum member profiles refreshed in a single 10-minute background pass.
pub const MAX_PROFILES_PER_PASS: usize = 2;

#[derive(Debug, Deserialize)]
struct ProfileDraft {
    #[serde(default)]
    identity_tags: String,
    #[serde(default)]
    interests: String,
    #[serde(default)]
    personality_notes: String,
    #[serde(default)]
    recent_status: String,
}

fn clamp_section(raw: &str) -> String {
    let cleaned: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if leaks_identifier(&cleaned) {
        return String::new();
    }
    cleaned.chars().take(MAX_SECTION_CHARS).collect()
}

/// Render the Historian prompt for updating one group member's 4-section profile.
pub fn render_profile_prompt(
    display_name: &str,
    existing: Option<&MemberProfileRecord>,
    facts: &[&MemoryRecord],
    recent_lines: &[&GroupMessageRecord],
) -> String {
    let mut out = String::with_capacity(3072);
    out.push_str(
        "你是群聊认知记忆系统的「后台史官」，负责为群友维护一份客观、克制、去冗的四段式人物侧写档案。\n\
         严格遵守【事实与玩笑分离原则】：\n\
         (1) 区分「说过这句话」与「这句话属实」：群友玩梗、反讽、自嘲夸张、角色扮演、复读机跟风、起哄贴标签，绝不能作为真实年龄、职业、学段、身份或长期喜好写入侧写；\n\
         (2) 可靠的非玩笑自述、明确纠正与长期一致的实际行为优先；一句新的玩笑不能覆盖旧的真实事实；\n\
         (3) 宁缺毋滥，没有把握的字段保持原样或留空字符串，禁止脑补猜测；\n\
         (4) 严禁出现任何数字串（QQ号、群号、编号）、CQ码或颜文字模板；\n\
         (5) 每个字段控制在 80 字以内，用简短词组或分号隔开。\n\n\
         只输出一个 JSON 对象，格式固定为：\n\
         {\"identity_tags\": \"身份/属性/常驻特征\", \"interests\": \"常玩游戏/兴趣偏好/常聊话题\", \"personality_notes\": \"说话风格/性格特点/相处印象\", \"recent_status\": \"近期动态/最近在忙的事\"}\n\n",
    );
    out.push_str(&format!("# 目标群友：{display_name}\n\n"));

    if let Some(prev) = existing.filter(|p| p.has_content()) {
        out.push_str("# 当前已有侧写（属实内容保留并精炼，过时内容更新）\n");
        out.push_str(&format!("- 身份标签：{}\n", prev.identity_tags));
        out.push_str(&format!("- 喜好与常聊：{}\n", prev.interests));
        out.push_str(&format!("- 性格与印象：{}\n", prev.personality_notes));
        out.push_str(&format!("- 近期动态：{}\n\n", prev.recent_status));
    }

    if !facts.is_empty() {
        out.push_str("# 已入库的个人事实与偏好记录\n");
        for f in facts.iter().take(12) {
            out.push_str(&format!("- ({}) {}\n", f.kind, f.content));
        }
        out.push('\n');
    }

    out.push_str("# 该群友最近的群内发言（旧→新）\n");
    for line in recent_lines.iter().take(25) {
        let t = line.text.trim();
        if !t.is_empty() {
            out.push_str(&format!("- {t}\n"));
        }
    }
    out
}

/// Background Historian pass: refresh up to [`MAX_PROFILES_PER_PASS`] active speakers' profiles.
pub async fn refresh_member_profiles(
    store: &SqliteStore,
    bot_id: &str,
    group_id: &str,
    model: &Arc<dyn SummaryModel>,
    now: i64,
) -> usize {
    let Ok(window) = store.recent_group_messages(bot_id, group_id, 100).await else {
        return 0;
    };
    if window.is_empty() {
        return 0;
    }

    let existing_list = store
        .list_member_profiles(bot_id, group_id)
        .await
        .unwrap_or_default();
    let existing_map: HashMap<String, MemberProfileRecord> = existing_list
        .into_iter()
        .map(|p| (p.user_id.clone(), p))
        .collect();

    let memories = store
        .list_active_memories(bot_id, group_id, 200)
        .await
        .unwrap_or_default();
    let aliases: HashMap<String, String> = store
        .list_speaker_aliases(bot_id, group_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    // Group recent messages by human sender_id.
    let mut by_sender: HashMap<String, Vec<&GroupMessageRecord>> = HashMap::new();
    for msg in &window {
        if msg.speaker_role == "bot" || msg.sender_id.is_empty() {
            continue;
        }
        by_sender
            .entry(msg.sender_id.clone())
            .or_default()
            .push(msg);
    }

    let mut candidates: Vec<(String, i64, Vec<&GroupMessageRecord>)> = by_sender
        .into_iter()
        .filter_map(|(uid, lines)| {
            let max_id = lines.iter().map(|m| m.id).max().unwrap_or(0);
            let prev_msg_id = existing_map.get(&uid).map(|p| p.source_msg_id).unwrap_or(0);
            let user_facts_count = memories
                .iter()
                .filter(|m| m.subject_user_id.as_deref() == Some(uid.as_str()))
                .count();
            if max_id > prev_msg_id && (lines.len() >= 3 || user_facts_count > 0) {
                Some((uid, max_id, lines))
            } else {
                None
            }
        })
        .collect();

    // Refresh the most active speakers with new messages first.
    candidates.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then_with(|| b.1.cmp(&a.1)));
    candidates.truncate(MAX_PROFILES_PER_PASS);

    let mut updated_count = 0usize;
    for (uid, max_msg_id, lines) in candidates {
        let prev = existing_map.get(&uid);
        let display_name = aliases
            .get(&uid)
            .cloned()
            .or_else(|| {
                lines
                    .iter()
                    .rev()
                    .find_map(|m| m.display_name.clone().filter(|s| !s.trim().is_empty()))
            })
            .or_else(|| {
                prev.map(|p| p.display_name.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "群友".to_string());

        let user_facts: Vec<&MemoryRecord> = memories
            .iter()
            .filter(|m| m.subject_user_id.as_deref() == Some(uid.as_str()))
            .collect();

        let prompt = render_profile_prompt(&display_name, prev, &user_facts, &lines);
        let Ok(raw) = model.summarise(prompt).await else {
            continue;
        };
        let Some(body) = json_body(&raw) else {
            continue;
        };
        let Ok(draft) = serde_json::from_value::<ProfileDraft>(body) else {
            continue;
        };

        let pick = |new_val: &str, old_val: Option<&str>| -> String {
            let clamped = clamp_section(new_val);
            if !clamped.is_empty() {
                clamped
            } else {
                old_val.unwrap_or_default().to_string()
            }
        };

        let identity = pick(&draft.identity_tags, prev.map(|p| p.identity_tags.as_str()));
        let interests = pick(&draft.interests, prev.map(|p| p.interests.as_str()));
        let personality = pick(
            &draft.personality_notes,
            prev.map(|p| p.personality_notes.as_str()),
        );
        let recent = pick(&draft.recent_status, prev.map(|p| p.recent_status.as_str()));

        if identity.is_empty()
            && interests.is_empty()
            && personality.is_empty()
            && recent.is_empty()
        {
            continue;
        }

        if store
            .upsert_member_profile(
                bot_id,
                group_id,
                &uid,
                &display_name,
                &identity,
                &interests,
                &personality,
                &recent,
                max_msg_id,
                now,
            )
            .await
            .is_ok()
        {
            updated_count += 1;
        }
    }

    updated_count
}

/// Render Layer 1 (pinned memories) and Layer 2 (active speakers' 4-section profiles)
/// for zero-latency injection into the turn prompt.
pub fn render_cognitive_context(
    pins: &[PinnedMemoryRecord],
    speaker_profiles: &[(String, MemberProfileRecord)],
) -> Option<String> {
    let mut out = String::new();
    if !pins.is_empty() {
        out.push_str("【置顶备忘录（永久约束与提醒）】：\n");
        for pin in pins.iter().take(10) {
            out.push_str(&format!("- [{}] {}\n", pin.pin_ref, pin.content.trim()));
        }
    }
    let non_empty_profiles: Vec<&(String, MemberProfileRecord)> = speaker_profiles
        .iter()
        .filter(|(_, p)| p.has_content())
        .collect();
    if !non_empty_profiles.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("【本轮发言群友侧写档案（只读背景参考，切勿生硬背诵）】：\n");
        for (speaker_ref, p) in non_empty_profiles {
            let name_part = if p.display_name.trim().is_empty() {
                speaker_ref.clone()
            } else {
                format!("{speaker_ref}（{}）", p.display_name.trim())
            };
            let mut parts: Vec<String> = Vec::new();
            if !p.identity_tags.trim().is_empty() {
                parts.push(format!("身份：{}", p.identity_tags.trim()));
            }
            if !p.interests.trim().is_empty() {
                parts.push(format!("喜好：{}", p.interests.trim()));
            }
            if !p.personality_notes.trim().is_empty() {
                parts.push(format!("性格印象：{}", p.personality_notes.trim()));
            }
            if !p.recent_status.trim().is_empty() {
                parts.push(format!("近期动态：{}", p.recent_status.trim()));
            }
            out.push_str(&format!("- [{}] {}\n", name_part, parts.join(" ｜ ")));
        }
    }
    if out.is_empty() { None } else { Some(out) }
}
