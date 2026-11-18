//! Long-term memory persistence for SqliteStore: memory rows, group summaries and handovers.
//! Moved out of sqlite.rs verbatim (std-split-sqlite); logic unchanged.

use super::*;

impl SqliteStore {
    pub async fn list_memories(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<MemoryRecord>> {
        self.list_active_memories(bot_id, group_id, limit).await
    }

    /// Active (non-archived) memories, newest revision first.
    ///
    /// Every read path that feeds a model or the retrieval ranker goes through this,
    /// so a retired row can never leak back into a prompt or a search result.
    pub async fn list_active_memories(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<MemoryRecord>> {
        let rows = sqlx::query(
            "SELECT memory_ref, bot_id, group_id, subject_user_id, content, kind,
                    source_message_ids, confidence, revision, origin, archived_at,
                    created_at, updated_at
             FROM memories WHERE bot_id = ? AND group_id = ? AND archived_at IS NULL AND kind != 'meme'
             ORDER BY updated_at DESC, memory_ref DESC LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_memory).collect())
    }

    /// One page of all memories (archived included), newest revision first.
    ///
    /// This is the operator view: retirement is visible, but every other caller
    /// wants [`Self::list_active_memories`].
    pub async fn list_memories_page(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
        offset: i64,
    ) -> BotResult<Vec<MemoryRecord>> {
        let rows = sqlx::query(
            "SELECT memory_ref, bot_id, group_id, subject_user_id, content, kind,
                    source_message_ids, confidence, revision, origin, archived_at,
                    created_at, updated_at
             FROM memories WHERE bot_id = ? AND group_id = ? AND kind != 'meme'
             ORDER BY (archived_at IS NOT NULL) ASC, updated_at DESC, memory_ref DESC LIMIT ? OFFSET ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_memory).collect())
    }

    /// Total memory rows in a group, so the panel can page without guessing.
    pub async fn count_memories(&self, bot_id: &str, group_id: &str) -> BotResult<i64> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memories WHERE bot_id = ? AND group_id = ? AND kind != 'meme'",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Active (non-archived) hot meme entries for a group.
    pub async fn list_active_hot_memes(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<HotMemeRecord>> {
        let rows = sqlx::query(
            "SELECT meme_ref, bot_id, group_id, content, source, confidence,
                    created_at, updated_at, archived_at
             FROM hot_memes WHERE bot_id = ? AND group_id = ? AND archived_at IS NULL
             ORDER BY updated_at DESC, meme_ref DESC LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_hot_meme).collect())
    }

    /// One page of hot meme entries for the panel, active ones first.
    pub async fn list_hot_memes_page(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
        offset: i64,
    ) -> BotResult<Vec<HotMemeRecord>> {
        let rows = sqlx::query(
            "SELECT meme_ref, bot_id, group_id, content, source, confidence,
                    created_at, updated_at, archived_at
             FROM hot_memes WHERE bot_id = ? AND group_id = ? AND archived_at IS NULL
             ORDER BY updated_at DESC, meme_ref DESC LIMIT ? OFFSET ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_hot_meme).collect())
    }

    /// Total active hot meme rows in a group.
    pub async fn count_hot_memes(&self, bot_id: &str, group_id: &str) -> BotResult<i64> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM hot_memes WHERE bot_id = ? AND group_id = ? AND archived_at IS NULL",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Delete one hot meme entry from the operator panel.
    pub async fn delete_hot_meme(
        &self,
        bot_id: &str,
        group_id: &str,
        meme_ref: &str,
    ) -> BotResult<bool> {
        let res =
            sqlx::query("DELETE FROM hot_memes WHERE bot_id = ? AND group_id = ? AND meme_ref = ?")
                .bind(bot_id)
                .bind(group_id)
                .bind(meme_ref)
                .execute(&self.pool)
                .await?;
        Ok(res.rows_affected() > 0)
    }

    /// List all pinned memories for a group, newest first.
    pub async fn list_pinned_memories(
        &self,
        bot_id: &str,
        group_id: &str,
    ) -> BotResult<Vec<PinnedMemoryRecord>> {
        let rows = sqlx::query(
            "SELECT pin_ref, bot_id, group_id, content, created_at, updated_at
             FROM pinned_memories WHERE bot_id = ? AND group_id = ?
             ORDER BY updated_at DESC, pin_ref DESC",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_pinned_memory).collect())
    }

    /// Insert a new pinned memory entry.
    pub async fn insert_pinned_memory(
        &self,
        bot_id: &str,
        group_id: &str,
        content: &str,
        now: i64,
    ) -> BotResult<String> {
        let pin_ref = format!("pin_{}", uuid_like());
        sqlx::query(
            "INSERT INTO pinned_memories (pin_ref, bot_id, group_id, content, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&pin_ref)
        .bind(bot_id)
        .bind(group_id)
        .bind(content)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(pin_ref)
    }

    /// Delete a pinned memory entry.
    pub async fn delete_pinned_memory(
        &self,
        bot_id: &str,
        group_id: &str,
        pin_ref: &str,
    ) -> BotResult<bool> {
        let res = sqlx::query(
            "DELETE FROM pinned_memories WHERE bot_id = ? AND group_id = ? AND pin_ref = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(pin_ref)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// List all member profiles in a group, ordered by most recently updated.
    pub async fn list_member_profiles(
        &self,
        bot_id: &str,
        group_id: &str,
    ) -> BotResult<Vec<MemberProfileRecord>> {
        let rows = sqlx::query(
            "SELECT bot_id, group_id, user_id, display_name, identity_tags,
                    interests, personality_notes, recent_status, source_msg_id, updated_at
             FROM member_profiles WHERE bot_id = ? AND group_id = ?
             ORDER BY updated_at DESC, user_id ASC",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_member_profile).collect())
    }

    /// Fetch one member's profile by `user_id`.
    pub async fn get_member_profile(
        &self,
        bot_id: &str,
        group_id: &str,
        user_id: &str,
    ) -> BotResult<Option<MemberProfileRecord>> {
        let row = sqlx::query(
            "SELECT bot_id, group_id, user_id, display_name, identity_tags,
                    interests, personality_notes, recent_status, source_msg_id, updated_at
             FROM member_profiles WHERE bot_id = ? AND group_id = ? AND user_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(map_member_profile))
    }

    /// Upsert a member's 4-section profile record.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_member_profile(
        &self,
        bot_id: &str,
        group_id: &str,
        user_id: &str,
        display_name: &str,
        identity_tags: &str,
        interests: &str,
        personality_notes: &str,
        recent_status: &str,
        source_msg_id: i64,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "INSERT INTO member_profiles (
                bot_id, group_id, user_id, display_name, identity_tags,
                interests, personality_notes, recent_status, source_msg_id, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(bot_id, group_id, user_id) DO UPDATE SET
                display_name = CASE WHEN excluded.display_name != '' THEN excluded.display_name ELSE member_profiles.display_name END,
                identity_tags = excluded.identity_tags,
                interests = excluded.interests,
                personality_notes = excluded.personality_notes,
                recent_status = excluded.recent_status,
                source_msg_id = CASE WHEN excluded.source_msg_id > member_profiles.source_msg_id THEN excluded.source_msg_id ELSE member_profiles.source_msg_id END,
                updated_at = excluded.updated_at",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(user_id)
        .bind(display_name)
        .bind(identity_tags)
        .bind(interests)
        .bind(personality_notes)
        .bind(recent_status)
        .bind(source_msg_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete one member's profile record.
    pub async fn delete_member_profile(
        &self,
        bot_id: &str,
        group_id: &str,
        user_id: &str,
    ) -> BotResult<bool> {
        let res = sqlx::query(
            "DELETE FROM member_profiles WHERE bot_id = ? AND group_id = ? AND user_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Total sticker rows matching the `kept` filter, so the panel can page them.
    pub async fn count_stickers(&self, bot_id: &str, kept: Option<i64>) -> BotResult<i64> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM stickers WHERE bot_id = ? AND (? IS NULL OR kept = ?)",
        )
        .bind(bot_id)
        .bind(kept)
        .bind(kept)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Total audit rows, newest-first pages need a bound.
    pub async fn count_audit(&self) -> BotResult<i64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Total model call rows for one bot.
    pub async fn count_calls(&self, bot_id: &str) -> BotResult<i64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_calls WHERE bot_id = ?")
            .bind(bot_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    pub async fn memory_stats(&self, bot_id: &str, group_id: &str) -> BotResult<(i64, i64)> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS n, COALESCE(SUM(LENGTH(content)), 0) AS chars
             FROM memories WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_one(&self.pool)
        .await?;
        Ok((row.get("n"), row.get("chars")))
    }

    pub async fn memory_by_ref(
        &self,
        bot_id: &str,
        group_id: &str,
        memory_ref: &str,
    ) -> BotResult<Option<MemoryRecord>> {
        let row = sqlx::query(
            "SELECT memory_ref, bot_id, group_id, subject_user_id, content, kind,
                    source_message_ids, confidence, revision, origin, archived_at,
                    created_at, updated_at
             FROM memories WHERE bot_id = ? AND group_id = ? AND memory_ref = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(memory_ref)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(map_memory))
    }

    /// Insert a memory, generating its reference and bumping the group's revision.
    ///
    /// `origin` is the writer (`agy` / `panel` / `hy3`). The row and its audit event
    /// commit in the same transaction, so a memory that exists always has a matching
    /// `memory.create` row naming who wrote it.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_memory_with_origin(
        &self,
        bot_id: &str,
        group_id: &str,
        subject_user_id: Option<&str>,
        content: &str,
        kind: &str,
        source_message_ids: &str,
        confidence: f64,
        origin: &str,
        now: i64,
    ) -> BotResult<String> {
        let mut tx = self.pool.begin().await?;
        let memory_ref = format!("mem_{}", uuid_like());
        sqlx::query(
            "INSERT INTO memories
                (memory_ref, bot_id, group_id, subject_user_id, content, kind,
                 source_message_ids, confidence, revision, origin, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?)",
        )
        .bind(&memory_ref)
        .bind(bot_id)
        .bind(group_id)
        .bind(subject_user_id)
        .bind(content)
        .bind(kind)
        .bind(source_message_ids)
        .bind(confidence)
        .bind(origin)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        audit_memory(
            &mut tx,
            crate::store::origin::audit_actor(origin),
            "memory.create",
            bot_id,
            group_id,
            now,
        )
        .await?;
        bump_revision(&mut tx, bot_id, group_id, now).await?;
        tx.commit().await?;
        Ok(memory_ref)
    }

    /// Insert a memory written by the per-turn agent contract.
    ///
    /// Kept as the default entry point so pre-existing callers keep their signature.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_memory(
        &self,
        bot_id: &str,
        group_id: &str,
        subject_user_id: Option<&str>,
        content: &str,
        kind: &str,
        source_message_ids: &str,
        confidence: f64,
        now: i64,
    ) -> BotResult<String> {
        self.insert_memory_with_origin(
            bot_id,
            group_id,
            subject_user_id,
            content,
            kind,
            source_message_ids,
            confidence,
            crate::store::origin::AGY,
            now,
        )
        .await
    }

    /// Update a memory's content and confidence.
    ///
    /// An archived row is refused with [`BotError::MemoryArchived`]: retirement is
    /// meant to be one-way for writers, and silently reviving the row would undo it.
    pub async fn update_memory(
        &self,
        bot_id: &str,
        group_id: &str,
        memory_ref: &str,
        content: &str,
        confidence: f64,
        now: i64,
    ) -> BotResult<bool> {
        self.update_memory_with_origin(bot_id, group_id, memory_ref, content, confidence, None, now)
            .await
    }

    /// Update a memory and optionally take over its `origin`.
    ///
    /// `origin = Some(writer)` means the writer now endorses the row: an agent-driven
    /// correction of an `hy3` extraction flips the row to `agy`, so provenance keeps
    /// naming whoever last shaped the content instead of whoever first created it.
    /// `None` leaves the column alone (the panel and internal callers).
    #[allow(clippy::too_many_arguments)]
    pub async fn update_memory_with_origin(
        &self,
        bot_id: &str,
        group_id: &str,
        memory_ref: &str,
        content: &str,
        confidence: f64,
        origin: Option<&str>,
        now: i64,
    ) -> BotResult<bool> {
        let mut tx = self.pool.begin().await?;
        let archived: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT archived_at FROM memories
             WHERE bot_id = ? AND group_id = ? AND memory_ref = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(memory_ref)
        .fetch_optional(&mut *tx)
        .await?;
        match archived {
            Some(Some(_)) => {
                tx.rollback().await?;
                return Err(crate::error::BotError::MemoryArchived(
                    memory_ref.to_string(),
                ));
            }
            // Unknown ref: not an archive problem, just a miss. The caller decides.
            None => {
                tx.rollback().await?;
                return Ok(false);
            }
            Some(None) => {}
        }
        let origin_before: String = sqlx::query_scalar(
            "SELECT origin FROM memories
             WHERE bot_id = ? AND group_id = ? AND memory_ref = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(memory_ref)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE memories SET content = ?, confidence = ?, revision = revision + 1, updated_at = ?
             WHERE bot_id = ? AND group_id = ? AND memory_ref = ? AND archived_at IS NULL",
        )
        .bind(content)
        .bind(confidence)
        .bind(now)
        .bind(bot_id)
        .bind(group_id)
        .bind(memory_ref)
        .execute(&mut *tx)
        .await?;
        if let Some(writer) = origin {
            sqlx::query(
                "UPDATE memories SET origin = ?
                 WHERE bot_id = ? AND group_id = ? AND memory_ref = ?",
            )
            .bind(writer)
            .bind(bot_id)
            .bind(group_id)
            .bind(memory_ref)
            .execute(&mut *tx)
            .await?;
        }
        // The audit actor is the writer that just touched the row, not its original
        // provenance: a correction logged under `hy3_extract` would hide the fact that
        // the agent endorsed it.
        let actor = origin.unwrap_or(origin_before.as_str());
        audit_memory(
            &mut tx,
            crate::store::origin::audit_actor(actor),
            "memory.update",
            bot_id,
            group_id,
            now,
        )
        .await?;
        bump_revision(&mut tx, bot_id, group_id, now).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Operator edit from the panel: content, kind, and subject, marked `panel`.
    ///
    /// `confidence = None` keeps the stored value. An empty subject clears it.
    /// An archived row stays archived.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_memory_panel(
        &self,
        bot_id: &str,
        group_id: &str,
        memory_ref: &str,
        content: &str,
        kind: &str,
        subject_user_id: Option<&str>,
        confidence: Option<f64>,
        now: i64,
    ) -> BotResult<bool> {
        let mut tx = self.pool.begin().await?;
        let archived: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT archived_at FROM memories
             WHERE bot_id = ? AND group_id = ? AND memory_ref = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(memory_ref)
        .fetch_optional(&mut *tx)
        .await?;
        match archived {
            Some(Some(_)) => {
                tx.rollback().await?;
                return Err(crate::error::BotError::MemoryArchived(
                    memory_ref.to_string(),
                ));
            }
            None => {
                tx.rollback().await?;
                return Ok(false);
            }
            Some(None) => {}
        }
        sqlx::query(
            "UPDATE memories
             SET content = ?, kind = ?, subject_user_id = ?,
                 confidence = COALESCE(?, confidence),
                 origin = ?, revision = revision + 1, updated_at = ?
             WHERE bot_id = ? AND group_id = ? AND memory_ref = ? AND archived_at IS NULL",
        )
        .bind(content)
        .bind(kind)
        .bind(subject_user_id)
        .bind(confidence)
        .bind(crate::store::origin::PANEL)
        .bind(now)
        .bind(bot_id)
        .bind(group_id)
        .bind(memory_ref)
        .execute(&mut *tx)
        .await?;
        audit_memory(
            &mut tx,
            crate::store::origin::audit_actor(crate::store::origin::PANEL),
            "memory.update",
            bot_id,
            group_id,
            now,
        )
        .await?;
        bump_revision(&mut tx, bot_id, group_id, now).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Hard delete. Only an operator may do this; the model has no delete path.
    pub async fn delete_memory(
        &self,
        bot_id: &str,
        group_id: &str,
        memory_ref: &str,
        now: i64,
    ) -> BotResult<bool> {
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query(
            "DELETE FROM memories WHERE bot_id = ? AND group_id = ? AND memory_ref = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(memory_ref)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        bump_revision(&mut tx, bot_id, group_id, now).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Soft-retire one memory: set `archived_at` and nothing else.
    ///
    /// The row stays in the table — the panel can still show it and audit still resolves
    /// it — but retrieval skips it, so the fact leaves the agent's view without the
    /// message log it was derived from being lost. One-way for writers: an archived row
    /// refuses [`SqliteStore::update_memory`].
    ///
    /// `task_id` names the consolidation pass that ordered the retirement; audit has no
    /// task column, so it rides in the action string as `memory.archive.task<id>` and the
    /// prefix stays greppable as `memory.archive`.
    ///
    /// Returns `false` when the row does not exist or is already archived.
    pub async fn archive_memory(
        &self,
        bot_id: &str,
        group_id: &str,
        memory_ref: &str,
        task_id: &str,
        now: i64,
    ) -> BotResult<bool> {
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query(
            "UPDATE memories SET archived_at = ?, revision = revision + 1
             WHERE bot_id = ? AND group_id = ? AND memory_ref = ? AND archived_at IS NULL",
        )
        .bind(now)
        .bind(bot_id)
        .bind(group_id)
        .bind(memory_ref)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        audit_memory(
            &mut tx,
            crate::store::origin::audit_actor(crate::store::origin::HY3),
            &format!("memory.archive.task{task_id}"),
            bot_id,
            group_id,
            now,
        )
        .await?;
        bump_revision(&mut tx, bot_id, group_id, now).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn memory_revision(&self, bot_id: &str, group_id: &str) -> BotResult<i64> {
        let rev: Option<i64> = sqlx::query_scalar(
            "SELECT memory_revision FROM groups WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(rev.unwrap_or(0))
    }

    // --------------------------------------------------------------- summary

    pub async fn get_summary(
        &self,
        bot_id: &str,
        group_id: &str,
    ) -> BotResult<Option<SummaryRecord>> {
        let row = sqlx::query(
            "SELECT bot_id, group_id, summary_text, source_revision, model_id, updated_at
             FROM group_memory_summaries WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SummaryRecord {
            bot_id: r.get("bot_id"),
            group_id: r.get("group_id"),
            summary_text: r.get("summary_text"),
            source_revision: r.get("source_revision"),
            model_id: r.get("model_id"),
            updated_at: r.get("updated_at"),
        }))
    }

    pub async fn put_summary(
        &self,
        bot_id: &str,
        group_id: &str,
        text: &str,
        source_revision: i64,
        model_id: &str,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "INSERT INTO group_memory_summaries
                (bot_id, group_id, summary_text, source_revision, model_id, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (bot_id, group_id) DO UPDATE SET
                summary_text = excluded.summary_text,
                source_revision = excluded.source_revision,
                model_id = excluded.model_id,
                updated_at = excluded.updated_at",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(text)
        .bind(source_revision)
        .bind(model_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------- handover

    pub async fn get_handover(
        &self,
        bot_id: &str,
        group_id: &str,
    ) -> BotResult<Option<HandoverRecord>> {
        let row = sqlx::query(
            "SELECT bot_id, group_id, note_text, source_msg_id, model_id, updated_at
             FROM group_handover_notes WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| HandoverRecord {
            bot_id: r.get("bot_id"),
            group_id: r.get("group_id"),
            note_text: r.get("note_text"),
            source_msg_id: r.get("source_msg_id"),
            model_id: r.get("model_id"),
            updated_at: r.get("updated_at"),
        }))
    }

    pub async fn put_handover(
        &self,
        bot_id: &str,
        group_id: &str,
        note_text: &str,
        source_msg_id: i64,
        model_id: &str,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "INSERT INTO group_handover_notes
                (bot_id, group_id, note_text, source_msg_id, model_id, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (bot_id, group_id) DO UPDATE SET
                note_text = excluded.note_text,
                source_msg_id = excluded.source_msg_id,
                model_id = excluded.model_id,
                updated_at = excluded.updated_at",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(note_text)
        .bind(source_msg_id)
        .bind(model_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

async fn bump_revision(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    bot_id: &str,
    group_id: &str,
    now: i64,
) -> BotResult<()> {
    sqlx::query(
        "UPDATE groups SET memory_revision = memory_revision + 1, updated_at = ?
         WHERE bot_id = ? AND group_id = ?",
    )
    .bind(now)
    .bind(bot_id)
    .bind(group_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
