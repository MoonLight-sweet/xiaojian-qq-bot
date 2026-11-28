//! Turn preparation for TurnExecutor: batch inputs, sticker offers, image persistence and prompt assembly inputs.
//! Moved out of turn.rs verbatim (std-split-turn-executor); logic unchanged.

use super::*;

impl TurnExecutor {
    /// Per-group directory holding the persistent `AGENTS.md`.
    ///
    /// Agy loads `AGENTS.md` from its own working directory on every run, so the card
    /// lives here rather than being rewritten into a throw-away directory each turn.
    pub fn project_dir(&self, group_id: &str) -> std::path::PathBuf {
        self.agy_projects_dir.join(group_id)
    }
    /// Write every tagged sticker's bytes under `sticker_dir` and cache its row.
    ///
    /// Failures are swallowed on purpose: a sticker that cannot be saved must never cost
    /// the user their reply. Nothing here is logged with a URL or a path.
    ///
    /// Only stickers are cached. A photo or a screenshot is conversation material that
    /// she would never send back as a reaction, and pictures essentially never repeat, so
    /// caching them would cost disk and never save a tagging call.
    ///
    /// Cached rows start unjudged (`kept = 0`): the tagger describes, only she decides
    /// what enters her library.
    /// A1′ pre-selection: score the kept library against this batch and keep
    /// the few stickers that clear the shared match threshold.
    ///
    /// Queries are deliberately short — the vision tags of the batch's own
    /// pictures plus each message's text on its own. One giant query built
    /// from the whole batch would dilute the Dice score of every field to
    /// zero, so each library row takes its best score across all queries.
    /// Scoring is [`crate::memory::stickers::score_text`], the exact function
    /// the `sticker_search` tool uses: one implementation, one threshold.
    pub(super) async fn select_sticker_offers(
        &self,
        tag_signals: &[String],
        messages: &[IncomingMessage],
    ) -> Vec<crate::agent::prompt::StickerOffer> {
        let library = match self
            .store
            .list_library(&self.bot_id, OFFER_SCAN_LIMIT)
            .await
        {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        if library.is_empty() {
            return Vec::new();
        }
        let mut queries: Vec<String> = tag_signals.to_vec();
        let mut spoken: Vec<&str> = tag_signals.iter().map(String::as_str).collect();
        for message in messages {
            let text = message.text.trim();
            if !text.is_empty() {
                queries.push(text.to_string());
                spoken.push(text);
            }
        }
        queries.extend(trigger_queries(&spoken));
        queries.sort();
        queries.dedup();
        queries.truncate(MAX_OFFER_QUERIES);
        if queries.is_empty() {
            return Vec::new();
        }
        // What the group has just seen. Damping a repeat is the whole point here, so a
        // lookup failure falls back to "nothing was sent recently" rather than to a
        // failed turn: a stale offer is a worse menu, not a broken one.
        let group_id = messages
            .first()
            .map(|message| message.group_id.as_str())
            .unwrap_or_default();
        let recent = match group_id.is_empty() {
            true => Vec::new(),
            false => self
                .store
                .recent_sent_stickers(&self.bot_id, group_id, RECENT_SEND_SCAN_LIMIT)
                .await
                .unwrap_or_default(),
        };
        let just_now: Vec<String> = recent.iter().map(|(hash, _, _)| hash.clone()).collect();
        let mut scored: Vec<(f64, &crate::store::StickerRecord)> = library
            .iter()
            .filter_map(|record| {
                let fields = record.text();
                let raw = queries
                    .iter()
                    .map(|query| crate::memory::stickers::score_text(query, &fields))
                    .fold(0.0_f64, f64::max);
                // A5: a repeat is damped (RECENT_SEND_DAMPING). This is a discount,
                // not a ban — on a quiet shelf a strong match still clears
                // `OFFER_MIN_SCORE` after damping, and the shelf answer has to stay
                // usable rather than go empty.
                let best = match just_now.iter().any(|hash| hash == &record.hash) {
                    true => raw * crate::memory::stickers::RECENT_SEND_DAMPING,
                    false => raw,
                };
                (best >= crate::memory::stickers::OFFER_MIN_SCORE).then_some((best, record))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.hash.cmp(&b.1.hash))
        });
        let over_threshold: Vec<String> = scored.iter().map(|(_, row)| row.hash.clone()).collect();
        let mut picked = pick_diverse(&scored);
        // A4 fallback with an A5 vest: when the batch did match something but has room,
        // one proven pick rides along — unless it just went out and a fresher
        // over-threshold row can take the slot. Silently skipped on any lookup error:
        // the shelf answer is never worth failing a turn over.
        if !picked.is_empty() && picked.len() < MAX_STICKER_OFFERS && !group_id.is_empty() {
            let ranked = self.sent_sticker_ranking(group_id).await;
            let taken: Vec<String> = picked.iter().map(|record| record.hash.clone()).collect();
            let just_sent = rounds_just_sent(&recent);
            if let Some(choice) = habit_pick(&ranked, &just_sent, &over_threshold, &taken)
                && let Some(record) = library.iter().find(|row| row.hash == choice)
            {
                picked.push(record);
            }
        }
        picked
            .into_iter()
            .map(|record| crate::agent::prompt::StickerOffer {
                hash: record.hash.clone(),
                short_hash: record.hash.chars().take(8).collect(),
                label: first_label(&[&record.emotion, &record.intent], "表情"),
                usage: first_label(
                    &[
                        &record.description,
                        &record.usage_hint,
                        &record.scene,
                        &record.ocr,
                    ],
                    "",
                ),
            })
            .collect()
    }

    /// The group's sent stickers, most-sent first (A4).
    ///
    /// Read straight from the outbox instead of a stored counter so it tracks what
    /// really went out, deliveries included. The whole ranking comes back (capped by
    /// [`OFFER_SCAN_LIMIT`]) because A5 may need the runner-up when the top one just
    /// went out. Empty on any error: a missing habit signal is a weaker menu, not a
    /// broken turn.
    pub(super) async fn sent_sticker_ranking(&self, group_id: &str) -> Vec<String> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT sticker_hash, COUNT(*) AS sends FROM outbox \
             WHERE bot_id = ? AND group_id = ? AND state = 'sent' AND sticker_hash IS NOT NULL \
             GROUP BY sticker_hash ORDER BY sends DESC, sticker_hash ASC LIMIT ?",
        )
        .bind(&self.bot_id)
        .bind(group_id)
        .bind(OFFER_SCAN_LIMIT)
        .fetch_all(self.store.pool())
        .await
        .unwrap_or_default();
        rows.into_iter().map(|(hash, _)| hash).collect()
    }

    pub(super) async fn persist_tagged_images(
        &self,
        group_id: &str,
        tagged: Vec<TaggedImage>,
        now: i64,
    ) {
        if tagged.is_empty() {
            return;
        }
        // The gates and the write live in one place (`app::sticker_gate`). Nothing is
        // decided here: this loop only feeds pictures to the shared door.
        for image in tagged {
            let _ = crate::app::sticker_gate::ingest(
                &self.store,
                &self.bot_id,
                &self.sticker_dir,
                crate::app::sticker_gate::TaggedPicture {
                    group_id,
                    message_ref: &image.message_ref,
                    hash: &image.hash,
                    bytes: &image.bytes,
                    tag: &image.tag,
                    now,
                },
            )
            .await;
        }
    }

    /// Calls this group may still start inside the rolling hour.
    ///
    /// `None` when the fuse is disabled (`hourly_call_budget == 0`). This is
    /// deliberately the same arithmetic the intake's `budget_gate` uses — one
    /// fuse, two checkpoints, never two different fuses.
    pub(super) async fn hourly_budget_remaining(&self, group_id: &str, now: i64) -> Option<i64> {
        if self.hourly_call_budget == 0 {
            return None;
        }
        let calls = self
            .store
            .count_agent_calls_since(
                &self.bot_id,
                group_id,
                now - crate::app::intake::BUDGET_WINDOW_SECONDS,
            )
            .await
            .unwrap_or(0);
        Some((i64::from(self.hourly_call_budget) - calls).max(0))
    }

    /// `received_at` per pending row id, for the fuse audit's age stamp.
    pub(super) async fn pending_ages(
        &self,
        event_ids: &[i64],
    ) -> std::collections::HashMap<i64, i64> {
        let mut out = std::collections::HashMap::new();
        for id in event_ids {
            let row: Option<(i64,)> =
                sqlx::query_as("SELECT received_at FROM pending_events WHERE id = ?")
                    .bind(id)
                    .fetch_optional(self.store.pool())
                    .await
                    .ok()
                    .flatten();
            if let Some((received_at,)) = row {
                out.insert(*id, received_at);
            }
        }
        out
    }

    /// from [`Self::run_batch`] so the resident lane shares one implementation.
    pub(super) async fn prepare_turn(
        &self,
        job: &BatchJob,
        group: &crate::store::GroupRecord,
        now: i64,
    ) -> Result<PreparedTurn, TurnResult> {
        // 3. Load the pending payloads for this batch.
        let payloads = match self.load_payloads(&job.event_ids).await {
            Some(p) if !p.is_empty() => p,
            _ => {
                let _ = self.store.delete_pending(&job.event_ids).await;
                return Err(TurnResult::SkippedInactive);
            }
        };
        let mut messages = messages_from_payloads(&payloads);
        if messages.is_empty() {
            let _ = self.store.delete_pending(&job.event_ids).await;
            return Err(TurnResult::BadOutput);
        }

        // 3.35 轻量识别群内语音与短视频（语音离线转写 + 短视频抽4格关键帧送视觉模型并听音轨）
        enrich_voice_and_video_messages(
            &mut messages,
            &self.connection,
            self.image_describer.as_deref(),
            &self.bot_id,
            &job.group_id,
        )
        .await;

        // 3.4 Tag every image and persist the stickers *before* the reply gate runs.
        // A batch the gate decides not to answer must still contribute pictures to the
        // library: her not wanting to talk does not mean the sticker was unwanted.
        let (image_context, tagged_images) = prepare_image_context(
            self.media_fetcher.as_ref(),
            &messages,
            self.media_max_bytes,
            self.media_max_pixels,
            self.image_describer.as_deref(),
            &self.bot_id,
            &job.group_id,
        )
        .await;
        // Image losses get one audit line per turn that has any: the stages
        // are counted in `stats`, and without this the loss is invisible
        // until a user asks why the bot "saw" a picture wrong.
        if image_context.stats.lost() > 0 {
            let s = &image_context.stats;
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        "image.context.g{}.ok{}.nourl{}.dl{}.prep{}.fit{}.cap{}.und{}",
                        job.group_id,
                        s.described,
                        s.no_url,
                        s.download,
                        s.prepare,
                        s.fit,
                        s.capped,
                        s.undescribed
                    ),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    now,
                )
                .await;
        }
        // Write each picture's description next to its own row. Without this the column
        // stays NULL forever, so `memory_search` sees only the `[图片]` placeholder that
        // normalize.rs left in `text` and can say nothing about what the picture was.
        let mut tags_by_message: std::collections::HashMap<usize, Vec<serde_json::Value>> =
            std::collections::HashMap::new();
        for image in &tagged_images {
            let position = image
                .message_ref
                .strip_prefix('m')
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|n| n.checked_sub(1));
            let Some(position) = position.filter(|p| *p < messages.len()) else {
                continue;
            };
            tags_by_message
                .entry(position)
                .or_default()
                .push(serde_json::json!({
                    "hash": image.hash,
                    "kind": image.tag.kind,
                    "ocr": image.tag.ocr,
                    "description": image.tag.description,
                    "emotion": image.tag.emotion,
                    "self": image.tag.self_ref,
                    "nsfw": image.tag.nsfw,
                    "text": image.tag.render(),
                }));
        }
        for (position, tags) in tags_by_message {
            let json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string());
            let _ = self
                .store
                .set_message_image_tags(
                    &self.bot_id,
                    &job.group_id,
                    &messages[position].message_id,
                    &json,
                )
                .await;
        }
        // A1′ offer signals: what the vision tagger said about this batch's
        // own pictures, in the same vocabulary the library rows are tagged
        // with. Collected before the persist step consumes the vector.
        let offer_signals: Vec<String> = tagged_images
            .iter()
            .map(|image| {
                // `ocr` and `emotion` are short, specific labels; the description is a
                // paragraph and would dilute every Dice score it is used as a query in.
                [image.tag.ocr.as_str(), image.tag.emotion.as_str()]
                    .into_iter()
                    .filter(|s| !s.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty())
            .collect();
        self.persist_tagged_images(&job.group_id, tagged_images, now)
            .await;

        // The pacing gate lives in the scheduler, not here. By the time a batch reaches
        // the executor the timing question is already answered, so this step has nothing
        // to decide and nothing to veto: every batch that arrives gets read.

        // 4. Memory context, current group only.
        let (memories, members) = self.load_memory_inputs(&job.group_id).await;
        let aliases: Vec<(String, String)> = members
            .iter()
            .map(|m| (m.user_id.clone(), m.speaker_ref.clone()))
            .collect();
        let query_text: String = messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let speaker_refs: Vec<String> = {
            let mut refs: Vec<String> = Vec::new();
            for m in &messages {
                if !refs.contains(&m.speaker_ref) {
                    refs.push(m.speaker_ref.clone());
                }
            }
            refs
        };
        let mut context = self.context_builder.build(RetrievalInput {
            memories: &memories,
            query_text: &query_text,
            speaker_refs: &speaker_refs,
            subject_aliases: &aliases,
            budget: self.context_builder.budget,
        });

        // §5.6 digested view. The summary was computed during the pacing gate's wait (in
        // the intake tick, on the same batch text) and left here in a one-shot slot. It is
        // a *derived* view of this batch only: never persisted, never a memory. Missing,
        // late or malformed means the raw list is injected exactly as before, so a slow
        // model can never delay or degrade a turn.
        let mut digest_text: Option<String> = None;
        let mut digest_refs: Vec<String> = Vec::new();
        if let Some(engine) = self.digest.as_ref()
            && let Some(slot) = engine.take(&job.group_id, &query_text)
        {
            // The expansion terms already bought one extra lexical pass; fold its
            // findings into the entry list before the batch is assembled.
            for extra in slot.extra {
                if !context
                    .entries
                    .iter()
                    .any(|e| e.memory_ref == extra.memory_ref)
                {
                    context.entries.push(extra);
                }
            }
            context.empty = context.entries.is_empty();
            digest_refs = slot.digest.refs.clone();
            let action = crate::memory::digest::audit_action(true, context.entries.len(), None);
            let _ = self
                .store
                .audit(
                    "system",
                    &action,
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    crate::now_unix(),
                )
                .await;
            digest_text = Some(slot.digest.render());
        } else if self.digest.is_some() {
            let reason = self
                .digest
                .as_ref()
                .and_then(|engine| engine.last_outcome(&job.group_id));
            let action = crate::memory::digest::audit_action(false, context.entries.len(), reason);
            let _ = self
                .store
                .audit(
                    "system",
                    &action,
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    crate::now_unix(),
                )
                .await;
        }

        let state = self
            .store
            .get_state(&self.bot_id, &job.group_id)
            .await
            .ok()
            .flatten();
        let topic_state = state
            .as_ref()
            .and_then(|s| s.topic_state.clone())
            .filter(|t| TopicState::new(t).is_some());
        let mood_label = state
            .as_ref()
            .and_then(|s| s.mood_json.as_deref())
            .and_then(MoodState::from_json)
            .and_then(|m| m.label_or_none().map(|l| l.to_string()));
        let persona = self.store.get_persona().await.ok().flatten();

        if let Ok(stored) = self
            .store
            .list_speaker_aliases(&self.bot_id, &job.group_id)
            .await
        {
            for message in &mut messages {
                if let Some((_, alias)) = stored
                    .iter()
                    .find(|(user_id, _)| user_id == &message.sender_id)
                {
                    message.known_alias = Some(alias.clone());
                }
            }
        }

        // The card lives in the group's long-lived project directory, not in the per-turn
        // directory: Agy loads `AGENTS.md` from its working directory by itself, so it is
        // written once (and only rewritten when the card changes) instead of being paid
        // for as prompt text on every turn.
        let project_dir = self.project_dir(&job.group_id);
        if crate::media::SandboxWorkspace::ensure_project_dir(
            &project_dir,
            &crate::agent::prompt::agents_md_with_persona(self.output_channel, persona.as_deref()),
        )
        .is_err()
        {
            let _ = self.store.release_events(&job.event_ids).await;
            return Err(TurnResult::AgentFailed);
        }
        let workspace = match SandboxWorkspace::open_in(&project_dir) {
            Ok(workspace) => workspace,
            Err(_) => {
                let _ = self.store.release_events(&job.event_ids).await;
                return Err(TurnResult::AgentFailed);
            }
        };
        // 2026-10-28: the resident PTY cwd and `--add-dir` are the group project
        // directory, not this per-turn sandbox. `scope.json` used to live only
        // here, so `roots/list` never saw it and every MCP tool (including
        // `submit_reply`) failed the scope gate. `write_scope` now also plants
        // the file next to `AGENTS.md`; the turn-dir copy is a harmless leftover.
        // The agent process is already running — this file is for the MCP child.
        if workspace.write_scope(&self.bot_id, &job.group_id).is_err() {
            let _ = self.store.release_events(&job.event_ids).await;
            return Err(TurnResult::AgentFailed);
        }
        // A1′: pre-match the kept library against this batch so the prompt
        // can offer her two or three stickers that already fit, instead of
        // paying for a search round-trip — or a full catalogue — every turn.
        let sticker_offers = self.select_sticker_offers(&offer_signals, &messages).await;
        if !sticker_offers.is_empty() {
            let shorts = sticker_offers
                .iter()
                .map(|o| o.short_hash.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        "sticker.offer.g{}.n{}.h{shorts}",
                        job.group_id,
                        sticker_offers.len()
                    ),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    crate::now_unix(),
                )
                .await;
        }
        // Resolve summaries for quotes pointing outside this batch, from
        // this group's own log only. In-batch targets need no lookup (the
        // prompt names them by ref); ids the log has never seen stay a plain
        // marker — the wire alone never gets to invent quoted content.
        let batch_ids: std::collections::HashSet<&str> =
            messages.iter().map(|m| m.message_id.as_str()).collect();
        let mut quote_summaries: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for message in &messages {
            if let Some(target) = &message.quote_of
                && !batch_ids.contains(target.as_str())
                && !quote_summaries.contains_key(target)
            {
                if let Ok(Some(logged)) = self
                    .store
                    .group_message_text(&self.bot_id, &job.group_id, target)
                    .await
                {
                    quote_summaries.insert(target.clone(), quote_summary(&logged));
                } else {
                    let param = target
                        .parse::<i64>()
                        .map(|n| serde_json::json!({ "message_id": n }))
                        .unwrap_or_else(|_| serde_json::json!({ "message_id": target }));
                    if let Ok(val) = self
                        .connection
                        .call_data("get_msg", param, std::time::Duration::from_millis(1500))
                        .await
                    {
                        if let Some(quoted) = extract_quote_from_onebot_json(&val) {
                            quote_summaries.insert(target.clone(), quote_summary(&quoted));
                        }
                    }
                }
            }
        }
        let (mut prompt_batch, mut scope) = build_prompt_batch(
            &messages,
            context,
            topic_state,
            mood_label,
            persona,
            image_context,
            sticker_offers,
            &quote_summaries,
        );
        prompt_batch.digest = digest_text;
        prompt_batch.flooded = job.flooded;
        prompt_batch.flood_background = job.flood_background.clone();

        // 梗百科与独立热梗记忆预查：检测本批消息中是否存在问梗、黑话或关联热梗，命中则走独立热梗通道注入（与群聊记忆彻底分离）
        let mut meme_sections: Vec<String> = Vec::new();
        for msg in messages.iter().rev() {
            if let Some(query) = crate::memory::memewiki::extract_query(&msg.text) {
                if let Some(meme) =
                    crate::memory::memewiki::search_meme(&self.store.pool, &query).await
                {
                    let _ = self
                        .store
                        .audit(
                            "system",
                            &format!("memewiki.hit.g{}.{}", job.group_id, meme.title),
                            Some(&self.bot_id),
                            Some(&job.group_id),
                            crate::now_unix(),
                        )
                        .await;
                    meme_sections.push(meme.render_context());
                    break;
                }
            }
        }
        if let Ok(hot_memes) = self
            .store
            .list_active_hot_memes(&self.bot_id, &job.group_id, 50)
            .await
        {
            let q_bigrams = crate::memory::retrieval::bigrams(&query_text);
            if !q_bigrams.is_empty() {
                let mut matched_hot: Vec<(f64, &crate::store::HotMemeRecord)> = hot_memes
                    .iter()
                    .filter_map(|hm| {
                        let score = crate::memory::retrieval::overlap_score(
                            &q_bigrams,
                            &crate::memory::retrieval::bigrams(&hm.content),
                        );
                        (score >= 0.08).then_some((score, hm))
                    })
                    .collect();
                matched_hot
                    .sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                matched_hot.truncate(2);
                if !matched_hot.is_empty() {
                    let mut hot_block = String::from("【独立热梗记忆参考】：\n");
                    for (_, hm) in matched_hot {
                        hot_block.push_str(&format!("- [{}] {}\n", hm.source, hm.content));
                    }
                    meme_sections.push(hot_block);
                }
            }
        }
        prompt_batch.meme_wiki_context = if meme_sections.is_empty() {
            None
        } else {
            Some(meme_sections.join("\n"))
        };

        // 认知记忆层（69gg/Undefined 架构）：1毫秒直取置顶备忘录 + 本轮发言群友四段式侧写档案
        let pins = self
            .store
            .list_pinned_memories(&self.bot_id, &job.group_id)
            .await
            .unwrap_or_default();
        let all_profiles = self
            .store
            .list_member_profiles(&self.bot_id, &job.group_id)
            .await
            .unwrap_or_default();
        let mut seen_speakers: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut speaker_profiles: Vec<(String, crate::store::MemberProfileRecord)> = Vec::new();
        for msg in &messages {
            if seen_speakers.insert(msg.sender_id.clone()) {
                if let Some(prof) = all_profiles.iter().find(|p| p.user_id == msg.sender_id) {
                    speaker_profiles.push((msg.speaker_ref.clone(), prof.clone()));
                }
            }
        }
        prompt_batch.cognitive_context =
            crate::memory::profiles::render_cognitive_context(&pins, &speaker_profiles);

        // An update still needs its key to survive validation, so every key the digest
        // exposed is accepted even if the digest's own pass picked it up through an
        // expansion term the batch text alone would not have surfaced.
        for key in digest_refs {
            if !scope.memory_refs.contains(&key) {
                scope.memory_refs.push(key);
            }
        }
        // Rotation seeding: a turn that starts a fresh conversation (the
        // stored id is None — after a rotation, a timeout recycle, a manual
        // reset, or for a brand-new group) gets the enhanced brief so the
        // new session can pick up the thread instead of starting cold.
        // Continuation turns never pay for it.
        let mut history_targets: Vec<HistoryTarget> = Vec::new();
        if group.agy_conversation_id.is_none()
            && let Some((brief, targets)) =
                seed_brief_for(&self.store, &self.bot_id, &job.group_id).await
        {
            scope.history_refs = targets.iter().map(|t| t.history_ref.clone()).collect();
            history_targets = targets;
            prompt_batch.seed_brief = Some(brief);
        }
        // Continuation annex: the previous batch's messages stay quotable under
        // their own `pN` refs, because the session's transcript still shows the
        // `mN` names those messages had last turn and those numbers now belong
        // to different messages. The entry is used only when its conversation
        // id matches this turn's, which is exactly the "still the same session"
        // test — a rotation, a retire or a reseed all leave it stale on purpose.
        if let Some(conversation_id) = group.agy_conversation_id.as_deref() {
            let cached = self
                .previous_batch
                .lock()
                .await
                .get(&job.group_id)
                .filter(|cached| cached.conversation_id == conversation_id)
                .cloned();
            if let Some(cached) = cached {
                scope
                    .history_refs
                    .extend(cached.targets.iter().map(|t| t.history_ref.clone()));
                let mut annex = cached.table;
                if let Some(own) = &cached.own_reply {
                    annex.push_str(&format!("- 你上一轮的回复：{own}\n"));
                }
                prompt_batch.annex = Some(annex);
                history_targets.extend(cached.targets);
            }
        }
        // MCP channel: arm this turn and put its token in the prompt batch,
        // so the contract card the model reads carries the token its
        // submit_reply call must present. Arming replaces any stale arm a
        // crashed turn left behind; the expiry (turn timeout + slack)
        // bounds how long a leftover arm stays presentable, and a dead
        // turn's token never matches a later turn's arm, so the exit paths
        // need no explicit disarm.
        let turn_token = if self.output_channel.is_mcp() {
            let token = new_turn_token();
            let ttl = (self.timeout.as_secs() as i64).max(self.resident_limits.hard_cap_s) + 120;
            match self
                .store
                .arm_turn(&job.group_id, &token, &job.batch_id, now, now + ttl)
                .await
            {
                Ok(previous) => {
                    if previous.is_some() {
                        let _ = self
                            .store
                            .audit(
                                "system",
                                &format!("agent.arm.superseded.g{}", job.group_id),
                                Some(&self.bot_id),
                                Some(&job.group_id),
                                now,
                            )
                            .await;
                    }
                    Some(token)
                }
                Err(_) => {
                    let _ = self.store.release_events(&job.event_ids).await;
                    return Err(TurnResult::AgentFailed);
                }
            }
        } else {
            None
        };
        prompt_batch.turn_token = turn_token.clone();
        // Stamped at assembly time, not when the batch was queued: the model's
        // "现在" should be when it is about to write, and by this point every
        // gate that could still drop the turn has already run.
        prompt_batch.now_line = Some(crate::agent::prompt::now_line(now));
        let prompt = render(&prompt_batch);
        if !crate::agent::prompt::prompt_is_safe(&prompt) {
            // A batch whose own text smuggles CQ markup is dropped rather than forwarded.
            let _ = self.store.delete_pending(&job.event_ids).await;
            return Err(TurnResult::BadOutput);
        }
        Ok(PreparedTurn {
            messages,
            members,
            state,
            prompt_batch,
            scope,
            history_targets,
            workspace,
            prompt,
            turn_token,
            is_first_turn: group.agy_conversation_id.is_none(),
        })
    }
}

fn extract_quote_from_onebot_json(val: &serde_json::Value) -> Option<String> {
    let sender_name = val.get("sender").and_then(|s| {
        let card = s.get("card").and_then(|v| v.as_str()).unwrap_or("").trim();
        if !card.is_empty() {
            Some(card.to_string())
        } else {
            let nick = s
                .get("nickname")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !nick.is_empty() {
                Some(nick.to_string())
            } else {
                None
            }
        }
    });

    let mut body = String::new();
    if let Some(arr) = val.get("message").and_then(|m| m.as_array()) {
        for seg in arr {
            let seg_type = seg.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let data = seg.get("data");
            match seg_type {
                "text" => {
                    if let Some(t) = data.and_then(|d| d.get("text")).and_then(|t| t.as_str()) {
                        body.push_str(t);
                    }
                }
                "image" => body.push_str("[图片]"),
                "face" => body.push_str("[表情]"),
                "mface" => body.push_str("[表情包]"),
                "record" => body.push_str("[语音]"),
                "video" => body.push_str("[短视频]"),
                _ => {}
            }
        }
    } else if let Some(raw) = val.get("raw_message").and_then(|r| r.as_str()) {
        body.push_str(raw);
    }

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(sender) = sender_name {
        Some(format!("{}: {}", sender, trimmed))
    } else {
        Some(trimmed.to_string())
    }
}

/// 轻量识别本批群消息中的直接语音(`Segment::Voice`)与直接短视频(`Segment::Video`)：
/// - 语音：通过 NapCat `get_record(out_format="wav")` + 本地静态 `ffmpeg` + `sherpa-onnx` SenseVoice 离线转写文字与情绪；
/// - 短视频：通过下载/NapCat `get_file` + `ffmpeg` 抽取 2x2 四宫格关键帧（交现有前置视觉模型看懂画面与字幕）并提取音轨离线转写台词。
async fn enrich_voice_and_video_messages(
    messages: &mut [IncomingMessage],
    connection: &crate::onebot::connection::ConnectionHandle,
    image_describer: Option<&dyn crate::app::preflight::ImageDescriber>,
    bot_id: &str,
    group_id: &str,
) {
    let script_path = "/opt/gemini-qq-bot/scripts/media_light_recognize.py";
    if !std::path::Path::new(script_path).is_file() {
        return;
    }

    for msg in messages.iter_mut() {
        let mut voice_items: Vec<(Option<String>, Option<String>)> = Vec::new();
        let mut video_items: Vec<(Option<String>, Option<String>)> = Vec::new();
        for seg in &msg.segments {
            match seg {
                Segment::Voice { url, file } => voice_items.push((url.clone(), file.clone())),
                Segment::Video { url, file } => video_items.push((url.clone(), file.clone())),
                _ => {}
            }
        }

        // 1. 轻量听语音
        for (url_opt, file_opt) in voice_items.into_iter().take(2) {
            let mut resolved_file = file_opt.clone().unwrap_or_default();
            let mut resolved_url = url_opt.clone().unwrap_or_default();

            // 优先让 NapCat 协议端内置 SILK 解码器直接转为 wav
            if let Some(ref f) = file_opt
                && !f.is_empty()
                && let Ok(data) = connection
                    .call_data(
                        "get_record",
                        serde_json::json!({"file": f, "out_format": "wav"}),
                        std::time::Duration::from_secs(12),
                    )
                    .await
            {
                if let Some(p) = data.get("file").and_then(|v| v.as_str())
                    && !p.is_empty()
                {
                    resolved_file = p.to_string();
                }
                if let Some(u) = data.get("url").and_then(|v| v.as_str())
                    && !u.is_empty()
                {
                    resolved_url = u.to_string();
                }
            }

            if resolved_file.is_empty() && resolved_url.is_empty() {
                continue;
            }

            let out = tokio::process::Command::new("python3")
                .arg(script_path)
                .arg("voice")
                .arg("--file")
                .arg(&resolved_file)
                .arg("--url")
                .arg(&resolved_url)
                .output()
                .await;

            if let Ok(output) = out
                && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
                && val.get("ok").and_then(|v| v.as_bool()) == Some(true)
            {
                let dur = val.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let text = val
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                let emo = val
                    .get("emotion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                let emo_cn = match emo {
                    "HAPPY" => "·语气开心",
                    "ANGRY" => "·语气生气",
                    "SAD" => "·语气难过",
                    "FEARFUL" => "·语气紧张",
                    "SURPRISED" => "·语气惊讶",
                    _ => "",
                };
                let replacement = if !text.is_empty() {
                    format!("[语音({dur:.1}秒{emo_cn}): 「{text}」]")
                } else {
                    format!("[语音({dur:.1}秒): (未识别到清晰人声)]")
                };
                if msg.text.contains("[语音]") {
                    msg.text = msg.text.replacen("[语音]", &replacement, 1);
                } else {
                    msg.text.push(' ');
                    msg.text.push_str(&replacement);
                }
            }
        }

        // 2. 轻量看+听短视频（抽4格关键帧送视觉模型 + 离线听音轨）
        for (idx, (url_opt, file_opt)) in video_items.into_iter().take(2).enumerate() {
            let mut resolved_file = file_opt.clone().unwrap_or_default();
            let mut resolved_url = url_opt.clone().unwrap_or_default();

            if resolved_url.is_empty()
                && let Some(ref f) = file_opt
                && !f.is_empty()
                && let Ok(data) = connection
                    .call_data(
                        "get_file",
                        serde_json::json!({"file": f}),
                        std::time::Duration::from_secs(15),
                    )
                    .await
            {
                if let Some(p) = data.get("file").and_then(|v| v.as_str())
                    && !p.is_empty()
                {
                    resolved_file = p.to_string();
                }
                if let Some(u) = data.get("url").and_then(|v| v.as_str())
                    && !u.is_empty()
                {
                    resolved_url = u.to_string();
                }
            }

            if resolved_file.is_empty() && resolved_url.is_empty() {
                continue;
            }

            let grid_path = format!("/tmp/qq_vid_grid_{}_{}.jpg", msg.message_id, idx);
            let out = tokio::process::Command::new("python3")
                .arg(script_path)
                .arg("video")
                .arg("--file")
                .arg(&resolved_file)
                .arg("--url")
                .arg(&resolved_url)
                .arg("--out-grid")
                .arg(&grid_path)
                .output()
                .await;

            if let Ok(output) = out
                && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
                && val.get("ok").and_then(|v| v.as_bool()) == Some(true)
            {
                let dur = val.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let transcript = val
                    .get("transcript")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let gp = val
                    .get("grid_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();

                let mut visual_desc = String::new();
                if !gp.is_empty()
                    && let Some(describer) = image_describer
                    && let Ok(bytes) = tokio::fs::read(gp).await
                    && !bytes.is_empty()
                {
                    let b64 =
                        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
                    let data_urls = vec![format!("data:image/jpeg;base64,{b64}")];
                    let tags = describer.describe(bot_id, group_id, &data_urls).await;
                    if let Some(tag) = tags.into_iter().next() {
                        let mut parts = Vec::new();
                        if !tag.description.trim().is_empty() {
                            parts.push(tag.description.trim().to_string());
                        }
                        if !tag.ocr.trim().is_empty() {
                            parts.push(format!("画面文字: {}", tag.ocr.trim()));
                        }
                        visual_desc = parts.join("；");
                    }
                }
                let _ = tokio::fs::remove_file(&grid_path).await;

                let mut summary_parts = Vec::new();
                if !visual_desc.is_empty() {
                    summary_parts.push(format!("画面(4格关键帧): {visual_desc}"));
                }
                if !transcript.is_empty() {
                    summary_parts.push(format!("音频转写: 「{transcript}」"));
                }
                let replacement = if summary_parts.is_empty() {
                    format!("[短视频({dur:.1}秒): 已接收，画面与人声均较模糊]")
                } else {
                    format!("[短视频({dur:.1}秒) | {}]", summary_parts.join(" | "))
                };
                if msg.text.contains("[短视频]") {
                    msg.text = msg.text.replacen("[短视频]", &replacement, 1);
                } else {
                    msg.text.push(' ');
                    msg.text.push_str(&replacement);
                }
            } else {
                let _ = tokio::fs::remove_file(&grid_path).await;
            }
        }
    }
}
