//! SQLite implementation of the persistence layer.
//!
//! All identifiers (bot/group/user/message) are stored as decimal `TEXT` so JSON
//! number precision issues can never rewrite an id. Message bodies live only in
//! `pending_events` and are deleted once a batch succeeds.

use sqlx::{Row, SqlitePool};

use crate::error::{BotError, BotResult};
use crate::store::{
    AgentCall, ArmedTurn, AuditEvent, CallState, CompactionJob, GroupMessageRecord, GroupRecord,
    GroupStateRecord, HandoverRecord, HotMemeRecord, JobState, MemberRecord, MemoryChunkRecord,
    MemoryRecord, OutboxRecord, OutboxState, PendingEventRow, SeedQuarantine, StickerRecord,
    SummaryRecord, TurnSubmission,
};

/// Every sticker column in one place so the four read paths cannot drift apart.
const STICKER_COLUMNS: &str = "SELECT hash, bot_id, group_id, file_path, kind, is_self, ocr,\n            description, scene, emotion, intent, usage_hint, search_blob, phash,\n            last_seen_at, seen_count, nsfw, enabled, kept, created_at\n     FROM stickers WHERE hash = ? AND bot_id = ?";

/// Thin wrapper over the SQLite pool.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    pub(crate) pool: SqlitePool,
}

mod audit;
mod calls;
mod continuity;
mod groups;
mod jobs;
mod maps;
mod memory;
mod messages;
mod outbox;
mod pending;
mod stickers;
mod submit;

use self::maps::{
    audit_memory, map_chunk, map_group_message, map_hot_meme, map_job, map_memory, map_outbox,
    map_pending, map_sticker, uuid_like,
};

#[cfg(test)]
mod tests;
