//! Sticker preselection for a turn: trigger queries, diverse picks and
//! habit picks over the group's kept shelf. Moved out of turn.rs verbatim
//! (std-split-turn); logic unchanged.

/// How many pre-matched library stickers one prompt may offer (A1′).
pub(crate) const MAX_STICKER_OFFERS: usize = 3;

/// Cap on the short queries one batch scores the library against (A1′).
pub(crate) const MAX_OFFER_QUERIES: usize = 32;

/// How much of the kept shelf one batch's preselection scans (A1′).
pub(crate) const OFFER_SCAN_LIMIT: i64 = 500;

/// How many recently sent stickers one batch looks at when damping offers (A5).
pub(crate) const RECENT_SEND_SCAN_LIMIT: i64 = 10;

/// How many distinct bot rounds count as "just sent" for the habit slot (A5).
///
/// One round can deliver several parts, so rounds are counted by `batch_id`, not by
/// outbox row.
pub(crate) const RECENT_ROUND_LIMIT: usize = 3;

/// Wording that implies an emotion outranks nothing at all (A2): each hit adds
/// its phrase list to the query set. These never select a sticker directly —
/// the phrases are scored like any other query, so a trigger can only widen what
/// the library is compared against, never overrule the scoring.
pub(crate) const EMOTION_TRIGGERS: &[(&[&str], &str)] = &[
    (&["哈哈哈", "笑死", "笑拉"], "大笑 得意 俏皮"),
    (&["绉号", "外号", "叫错"], "气鼓鼓 假装生气 质问"),
    (&["无语", "服了", "醉了"], "无语 翻白眼"),
    (&["真的假的", "离谱", "震惊"], "震惊 难以置信 求证"),
    (&["谢谢", "谢了"], "开心 感谢"),
    (&["困了", "想睡", "晚安"], "困怔 想睡"),
    (&["吃瓜", "围观", "热闹"], "吃瓜 看热闹"),
    (&["厉害", "牛", "强"], "得意 点赞 认可"),
    (&["呜呜", "抱抱", "难过"], "委屈 安慰"),
    (&["急了", "破防"], "得意 俏皮 调侃"),
];

/// Extra queries implied by a batch's wording (A2), in table order.
pub(crate) fn trigger_queries<S: AsRef<str>>(texts: &[S]) -> Vec<String> {
    let mut out = Vec::new();
    for (needles, phrases) in EMOTION_TRIGGERS {
        let hit = texts.iter().any(|text| {
            let text = text.as_ref();
            needles.iter().any(|needle| text.contains(needle))
        });
        if hit {
            out.push((*phrases).to_string());
        }
    }
    out
}

/// Leading emotion tag: the part before any label separator (A3).
pub(crate) fn emotion_bucket(emotion: &str) -> String {
    emotion
        .split(['、', '，', ','])
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// At most [`MAX_STICKER_OFFERS`] picks, greedy on the already-sorted order but
/// skipping a pick whose leading emotion is already taken, then filling any
/// remaining slots from the leftovers (A3). Sorted input means the highest
/// scores survive whenever diversity and score disagree.
pub(crate) fn pick_diverse<'a>(
    scored: &[(f64, &'a crate::store::StickerRecord)],
) -> Vec<&'a crate::store::StickerRecord> {
    let mut picked: Vec<&crate::store::StickerRecord> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (_, record) in scored {
        if picked.len() == MAX_STICKER_OFFERS {
            break;
        }
        let bucket = emotion_bucket(&record.emotion);
        if seen.contains(&bucket) {
            continue;
        }
        seen.push(bucket);
        picked.push(record);
    }
    for (_, record) in scored {
        if picked.len() == MAX_STICKER_OFFERS {
            break;
        }
        if picked.iter().any(|kept| kept.hash == record.hash) {
            continue;
        }
        picked.push(record);
    }
    picked
}

/// Hashes of what went out in the group's last [`RECENT_ROUND_LIMIT`] bot rounds.
///
/// `recent` arrives newest first. A round is a `batch_id`; once the third new one is
/// seen the walk stops, so a long tail of older sends cannot leak into "just sent".
pub(crate) fn rounds_just_sent(recent: &[(String, String, i64)]) -> Vec<String> {
    let mut batches: Vec<&str> = Vec::new();
    let mut hashes: Vec<String> = Vec::new();
    for (hash, batch, _) in recent {
        if !batches.contains(&batch.as_str()) {
            if batches.len() == RECENT_ROUND_LIMIT {
                break;
            }
            batches.push(batch.as_str());
        }
        if !hashes.iter().any(|seen| seen == hash) {
            hashes.push(hash.clone());
        }
    }
    hashes
}

/// Which library row the habit slot promotes this round (A5).
///
/// Normally the group's most-sent sticker. If that one went out within the last few
/// rounds *and* a different over-threshold row that has not just gone out is
/// available, that row takes the slot for this round instead. With no replacement the
/// proven row keeps it: this is a fallback, and a fallback that can silently vanish is
/// worse than a repeat. `None` means the slot stays unused — already picked, or the
/// group has no send history at all.
pub(crate) fn habit_pick(
    ranked: &[String],
    just_sent: &[String],
    over_threshold: &[String],
    taken: &[String],
) -> Option<String> {
    let proven = ranked.first()?;
    if taken.iter().any(|hash| hash == proven) {
        return None;
    }
    if !just_sent.iter().any(|hash| hash == proven) {
        return Some(proven.clone());
    }
    for candidate in ranked.iter().skip(1) {
        if taken.iter().any(|hash| hash == candidate) {
            continue;
        }
        if just_sent.iter().any(|hash| hash == candidate) {
            continue;
        }
        if !over_threshold.iter().any(|hash| hash == candidate) {
            continue;
        }
        return Some(candidate.clone());
    }
    Some(proven.clone())
}

/// One-line, length-capped summary of a quoted message's logged text.
pub(crate) fn quote_summary(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

/// First non-blank of `candidates`, else `fallback`.
pub(crate) fn first_label(candidates: &[&str], fallback: &str) -> String {
    candidates
        .iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}
