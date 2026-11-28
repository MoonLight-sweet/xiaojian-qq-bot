//! Row mappers: sqlx rows -> store records. Moved out of sqlite.rs
//! verbatim (std-split-sqlite); logic unchanged.

use sqlx::Row;

use crate::error::BotResult;

use crate::store::{
    CompactionJob, GroupMessageRecord, HotMemeRecord, MemberProfileRecord, MemoryChunkRecord,
    MemoryRecord, OutboxRecord, PendingEventRow, PinnedMemoryRecord, StickerRecord,
};

pub(super) fn map_pending(r: sqlx::sqlite::SqliteRow) -> PendingEventRow {
    PendingEventRow {
        id: r.get("id"),
        bot_id: r.get("bot_id"),
        group_id: r.get("group_id"),
        message_id: r.get("message_id"),
        sender_id: r.get("sender_id"),
        received_at: r.get("received_at"),
        normalized_payload: r.get("normalized_payload"),
        image_refs: r.get("image_refs"),
        batch_id: r.get("batch_id"),
    }
}

pub(super) fn map_group_message(r: sqlx::sqlite::SqliteRow) -> GroupMessageRecord {
    GroupMessageRecord {
        id: r.get("id"),
        bot_id: r.get("bot_id"),
        group_id: r.get("group_id"),
        message_id: r.get("message_id"),
        sender_id: r.get("sender_id"),
        display_name: r.get("display_name"),
        speaker_role: r.get("speaker_role"),
        text: r.get("text"),
        has_image: r.get::<i64, _>("has_image") != 0,
        sticker_hash: r.get("sticker_hash"),
        sub_type: r.get("sub_type"),
        image_tags: r.get("image_tags"),
        created_at: r.get("created_at"),
        archived: r.get::<i64, _>("archived") != 0,
    }
}

pub(super) fn map_sticker(r: sqlx::sqlite::SqliteRow) -> StickerRecord {
    StickerRecord {
        hash: r.get("hash"),
        bot_id: r.get("bot_id"),
        group_id: r.get("group_id"),
        file_path: r.get("file_path"),
        kind: r.get("kind"),
        is_self: r.get("is_self"),
        ocr: r.get("ocr"),
        description: r.get("description"),
        scene: r.get("scene"),
        emotion: r.get("emotion"),
        intent: r.get("intent"),
        usage_hint: r.get("usage_hint"),
        search_blob: r.get("search_blob"),
        phash: r.get("phash"),
        last_seen_at: r.get("last_seen_at"),
        seen_count: r.get("seen_count"),
        nsfw: r.get::<i64, _>("nsfw") != 0,
        enabled: r.get::<i64, _>("enabled") != 0,
        kept: r.get("kept"),
        created_at: r.get("created_at"),
    }
}

pub(super) fn map_chunk(r: sqlx::sqlite::SqliteRow) -> MemoryChunkRecord {
    MemoryChunkRecord {
        chunk_id: r.get("chunk_id"),
        bot_id: r.get("bot_id"),
        group_id: r.get("group_id"),
        seq: r.get("seq"),
        from_msg_id: r.get("from_msg_id"),
        to_msg_id: r.get("to_msg_id"),
        summary: r.get("summary"),
        model_id: r.get("model_id"),
        created_at: r.get("created_at"),
    }
}

pub(super) fn map_memory(r: sqlx::sqlite::SqliteRow) -> MemoryRecord {
    MemoryRecord {
        memory_ref: r.get("memory_ref"),
        bot_id: r.get("bot_id"),
        group_id: r.get("group_id"),
        subject_user_id: r.get("subject_user_id"),
        content: r.get("content"),
        kind: r.get("kind"),
        source_message_ids: r.get("source_message_ids"),
        confidence: r.get("confidence"),
        revision: r.get("revision"),
        origin: r.get("origin"),
        archived_at: r.get("archived_at"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

pub(super) fn map_hot_meme(r: sqlx::sqlite::SqliteRow) -> HotMemeRecord {
    HotMemeRecord {
        meme_ref: r.get("meme_ref"),
        bot_id: r.get("bot_id"),
        group_id: r.get("group_id"),
        content: r.get("content"),
        source: r.get("source"),
        confidence: r.get("confidence"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        archived_at: r.get("archived_at"),
    }
}

pub(super) fn map_pinned_memory(r: sqlx::sqlite::SqliteRow) -> PinnedMemoryRecord {
    PinnedMemoryRecord {
        pin_ref: r.get("pin_ref"),
        bot_id: r.get("bot_id"),
        group_id: r.get("group_id"),
        content: r.get("content"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

pub(super) fn map_member_profile(r: sqlx::sqlite::SqliteRow) -> MemberProfileRecord {
    MemberProfileRecord {
        bot_id: r.get("bot_id"),
        group_id: r.get("group_id"),
        user_id: r.get("user_id"),
        display_name: r.get("display_name"),
        identity_tags: r.get("identity_tags"),
        interests: r.get("interests"),
        personality_notes: r.get("personality_notes"),
        recent_status: r.get("recent_status"),
        source_msg_id: r.get("source_msg_id"),
        updated_at: r.get("updated_at"),
    }
}

/// Write one memory audit row inside the caller's transaction.
///
/// Sharing the transaction is what makes the audit complete: a write that committed
/// without its row cannot exist, and a rolled-back write leaves no audit claim.
pub(super) async fn audit_memory(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor_kind: &str,
    action: &str,
    bot_id: &str,
    group_id: &str,
    now: i64,
) -> BotResult<()> {
    sqlx::query(
        "INSERT INTO audit_events (actor_kind, action, bot_id, group_id, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(actor_kind)
    .bind(action)
    .bind(bot_id)
    .bind(group_id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) fn map_job(r: sqlx::sqlite::SqliteRow) -> CompactionJob {
    CompactionJob {
        id: r.get("id"),
        bot_id: r.get("bot_id"),
        group_id: r.get("group_id"),
        source_revision: r.get("source_revision"),
        state: r.get("state"),
        attempt_count: r.get("attempt_count"),
        requested_by: r.get("requested_by"),
        created_at: r.get("created_at"),
        started_at: r.get("started_at"),
        finished_at: r.get("finished_at"),
        error_class: r.get("error_class"),
    }
}

pub(super) fn map_outbox(r: sqlx::sqlite::SqliteRow) -> OutboxRecord {
    OutboxRecord {
        id: r.get("id"),
        bot_id: r.get("bot_id"),
        group_id: r.get("group_id"),
        batch_id: r.get("batch_id"),
        text: r.get("text"),
        kind: r.get("kind"),
        sticker_hash: r.get("sticker_hash"),
        quote_message_id: r.get("quote_message_id"),
        state: r.get("state"),
        message_id: r.get("message_id"),
        part_index: r.get("part_index"),
        part_count: r.get("part_count"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

/// Small unique-ish id. Not a UUID dependency: the store only needs local
/// uniqueness, and collisions inside one SQLite file are practically impossible.
pub(super) fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{nanos:x}{n:04x}")
}
