//! Post-answer commit for TurnExecutor: validation, memory ops, gates and the outbox write.
//! Moved out of turn.rs verbatim (std-split-turn-executor); logic unchanged.

use super::*;

impl TurnExecutor {
    /// Everything after the agent produced a contract: validation, memory
    /// ops, the reply gates and the outbox. Extracted verbatim from
    /// [`Self::run_batch`] so the resident path shares one implementation.
    /// A turn that chose not to send. The call stays `succeeded` so it is not a
    /// failure, but `error_class` is `silent` and the audit row is
    /// `agent.silent.g{id}.{why}` so the panel log shows the decision.
    pub(super) async fn note_silence(&self, job: &BatchJob, call_id: &str, why: &str) {
        let now = crate::now_unix();
        let _ = self
            .store
            .finish_call(call_id, CallState::Succeeded, Some("silent"), now)
            .await;
        // An addressed silence is a different event from a chosen one: the batch
        // was aimed at the bot and got nothing back. It gets its own audit action so
        // the rate of dropped questions is queryable, and so a silence that only
        // reached this point after a failed retry is distinguishable from an
        // ambient group being left alone.
        let action = if job.privileged {
            format!("agent.silent.addressed.g{}.{why}", job.group_id)
        } else {
            format!("agent.silent.g{}.{why}", job.group_id)
        };
        let _ = self
            .store
            .audit(
                "system",
                &action,
                Some(&self.bot_id),
                Some(&job.group_id),
                now,
            )
            .await;
        tracing::info!(group_id = %job.group_id, reason = why, addressed = job.privileged, "本轮不回复");
    }

    /// Commit a resident turn: a turn is finished once, by the resident path, with
    /// the coverage window that lists every batch delivered into it.
    ///
    /// `coverage` is `TurnCoverage` for an unattended single-batch turn (the batch's
    /// own messages are the window) and `ResidentCoverage` for a resident turn, where
    /// several batches were delivered into one turn: every one of them has to be
    /// covered by the freshness gate, the oldest first, or the reply is dropped as
    /// stale.
    pub(super) async fn finish_turn_with_coverage<C: FreshnessCoverage>(
        &self,
        job: &BatchJob,
        call_id: &str,
        contract_json: &str,
        timing: TurnTiming,
        prepared: PreparedTurn,
        coverage: C,
    ) -> Result<TurnResult, TurnResult> {
        let TurnTiming {
            turn_started,
            agent_started,
        } = timing;
        let PreparedTurn {
            messages,
            members,
            state,
            prompt_batch,
            scope,
            history_targets,
            workspace: _,
            prompt: _,
            turn_token: _,
            is_first_turn: _,
        } = prepared;
        // The rows this turn consumes if it succeeds. A turn covers every batch it
        // delivered, so the gate and the final delete work off the union rather than
        // off the first batch.
        let covered = coverage.event_ids(job);
        let coverage_label = coverage.label();
        // 6. Validate the contract.
        // Step timing for the post-agent paths (the execution black box the
        // latency study flagged): prepare = everything before the agent call
        // (proxy probe, payload load, image tagging, prompt assembly),
        // agent = the Agy subprocess itself, send = outbox delivery.
        let prepare_ms = agent_started.duration_since(turn_started).as_millis();
        let agent_ms = agent_started.elapsed().as_millis();
        let log_timing = |result: &str, send_ms: u128| {
            tracing::info!(
                group_id = %job.group_id,
                batch_id = %job.batch_id,
                prepare_ms = prepare_ms,
                agent_ms = agent_ms,
                send_ms = send_ms,
                result = result,
                "turn timing"
            );
        };
        let (validated, salvage_report) = match contract::validate_salvaged(contract_json, &scope) {
            Ok(pair) => pair,
            Err(reason) => {
                let _ = self
                    .store
                    .finish_call(
                        call_id,
                        CallState::Failed,
                        Some(ErrorClass::AgentBadOutput.as_str()),
                        crate::now_unix(),
                    )
                    .await;
                let _ = self
                    .store
                    .audit(
                        "system",
                        &format!("agent.rejected.{}", reason.as_str()),
                        Some(&self.bot_id),
                        Some(&job.group_id),
                        crate::now_unix(),
                    )
                    .await;
                // A SUCCESS body that was not a contract object was handed here
                // for salvage. Salvage failed, so the preview is that body, and
                // the session that produced it is not resumed.
                if !contract_json.trim_start().starts_with('{') {
                    let now = crate::now_unix();
                    let _ = self
                        .store
                        .set_conversation_id(&self.bot_id, &job.group_id, None, now)
                        .await;
                    self.audit_bad_output(
                        &job.group_id,
                        reason.as_str(),
                        &crate::agent::agy::output_preview(contract_json, 80),
                    )
                    .await;
                }
                // Accepted nothing, so the batch produced no reply; drop the bodies.
                let _ = self.store.delete_pending(&covered).await;
                return Err(TurnResult::BadOutput);
            }
        };
        let _ = self
            .store
            .finish_call(call_id, CallState::Succeeded, None, crate::now_unix())
            .await;
        // B3: soft rejections used to vanish into a counter nobody read.
        // One audit row per call keeps the rate visible in the panel.
        // A salvaged turn says so on its own row: how much had to be recovered
        // (which layer) and what it cost (sections dropped, values clamped). A
        // clean turn stays silent, so the rescued minority is what shows up.
        if salvage_report.layer != SalvageLayer::None {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        "agent.salvaged.g{}.{}",
                        job.group_id,
                        salvage_report.audit_suffix()
                    ),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    crate::now_unix(),
                )
                .await;
            tracing::info!(
                group_id = %job.group_id,
                batch_id = %job.batch_id,
                salvage = %salvage_report.detail(),
                "agent output salvaged"
            );
        }
        if validated.rejections.total > 0 {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        "agent.rejections.g{}.n{}",
                        job.group_id, validated.rejections.total
                    ),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    crate::now_unix(),
                )
                .await;
        }
        // A quote_ref the validator dropped (aimed outside this batch) is
        // audited on its own: the reply still goes out plain, but the
        // observation week needs the rate at which she aims at messages she
        // may not quote. The raw value only survives in the contract JSON,
        // so it is read back from there.
        let raw_contract = serde_json::from_str::<serde_json::Value>(contract_json)
            .ok()
            .or_else(|| {
                salvage::candidates(contract_json)
                    .first()
                    .map(|c| c.value.clone())
            });
        let raw_quote_ref = raw_contract
            .as_ref()
            .and_then(|value| value.get("quote_ref"))
            .and_then(serde_json::Value::as_str)
            .map(|raw| raw.trim().to_string())
            .filter(|raw| !raw.is_empty());
        // Remember the aim existed: a turn whose quote_ref was dropped for
        // aiming outside the batch must not have a quote re-attached by the
        // anchor fallback below — she did aim, just elsewhere, and pinning
        // the batch anchor onto that reply is how a misquote is manufactured.
        let quote_aimed_outside = raw_quote_ref.is_some() && validated.output.quote_ref.is_none();
        if quote_aimed_outside {
            let raw_ref = raw_quote_ref.as_deref().unwrap_or_default();
            let _ = self
                .store
                .audit(
                    "system",
                    &format!("reply.quote.dropped.g{}.ref{raw_ref}", job.group_id),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    crate::now_unix(),
                )
                .await;
        }

        // 7. Apply memory ops (accepted ones only) and mood/topic updates.
        let resolve = |alias: &str| {
            members
                .iter()
                .find(|m| m.speaker_ref == alias)
                .map(|m| m.user_id.clone())
        };
        // `source_refs` are batch aliases, and the batch is gone after this turn. They
        // are resolved to the real QQ message ids so the audit trail stays followable.
        let resolve_source = |alias: &str| {
            alias
                .strip_prefix('m')
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| messages.get(i))
                .map(|m| m.message_id.clone())
        };
        if let Ok(summary) = self
            .memory
            .apply_all(
                &self.bot_id,
                &job.group_id,
                &validated.output.memory_ops,
                resolve,
                resolve_source,
                crate::now_unix(),
            )
            .await
        {
            // Archived refusals are expected (the model still holds a retired ref in
            // context); warn so a runaway rate shows up instead of happening silently.
            if summary.archived > 0 || summary.failed > 0 {
                tracing::info!(
                    group_id = %job.group_id,
                    added = summary.added,
                    updated = summary.updated,
                    out_of_scope = summary.out_of_scope,
                    archived = summary.archived,
                    failed = summary.failed,
                    "记忆写入完成（含被拒条目）"
                );
            }
        }

        let new_mood = validated
            .output
            .mood_update
            .as_ref()
            .and_then(|m| MoodState::new(&m.label, &m.reason, crate::now_unix()));
        let new_topic = validated
            .output
            .topic_state
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .and_then(TopicState::new);
        if new_mood.is_some() || new_topic.is_some() {
            let mood_json = new_mood
                .map(|m| m.to_json())
                .or_else(|| state.as_ref().and_then(|s| s.mood_json.clone()));
            let topic = new_topic
                .map(|t| t.0)
                .or_else(|| state.as_ref().and_then(|s| s.topic_state.clone()));
            let _ = self
                .store
                .put_state(
                    &self.bot_id,
                    &job.group_id,
                    mood_json.as_deref(),
                    topic.as_deref(),
                    crate::now_unix(),
                )
                .await;
        }

        // Nicknames are stored even when this turn does not send a reply.
        for nick in &validated.output.nicknames {
            if let Some(user_id) = messages
                .iter()
                .find(|message| message.speaker_ref == nick.subject_ref)
                .map(|message| message.sender_id.clone())
            {
                let _ = self
                    .store
                    .upsert_speaker_alias(
                        &self.bot_id,
                        &job.group_id,
                        &user_id,
                        &nick.alias,
                        crate::now_unix(),
                    )
                    .await;
            }
        }

        // JSON sticker_ops is send-only. Library membership is MCP `sticker_keep`.
        let mut sticker_sends: Vec<String> = Vec::new();
        for op in &validated.output.sticker_ops {
            match op.op {
                contract::StickerOpKind::Keep | contract::StickerOpKind::Ignore => {}
                contract::StickerOpKind::Send => {
                    // A send may name something already on her shelf, which is not in
                    // this batch's hash set. Resolve it against the library, and only
                    // accept a row she actually kept: an unkept cache entry is not
                    // hers to send yet.
                    let resolved = self
                        .store
                        .find_sticker_by_prefix(&self.bot_id, &op.hash)
                        .await
                        .ok()
                        .flatten();
                    let Some(hash) = resolved else {
                        // B3: still silent toward the group, but no longer
                        // invisible — a dropped send leaves an audit row.
                        let short: String =
                            op.hash.trim_start_matches('#').chars().take(8).collect();
                        let _ = self
                            .store
                            .audit(
                                "system",
                                &format!("sticker.send.dropped.h{short}"),
                                Some(&self.bot_id),
                                Some(&job.group_id),
                                crate::now_unix(),
                            )
                            .await;
                        continue;
                    };
                    match self.store.sticker_by_hash(&self.bot_id, &hash).await {
                        Ok(Some(record)) if record.kept == 1 && record.enabled => {
                            sticker_sends.push(hash);
                        }
                        // Not kept, disabled, or vanished: drop it silently toward
                        // the group (talking about internals there helps nobody),
                        // but audit it like the unresolved case above.
                        _ => {
                            let short: String = hash.chars().take(8).collect();
                            let _ = self
                                .store
                                .audit(
                                    "system",
                                    &format!("sticker.send.dropped.h{short}"),
                                    Some(&self.bot_id),
                                    Some(&job.group_id),
                                    crate::now_unix(),
                                )
                                .await;
                            continue;
                        }
                    }
                }
            }
        }

        if !validated.output.should_reply {
            let _ = self.store.delete_pending(&covered).await;
            self.note_silence(job, call_id, "declined").await;
            log_timing("silent", 0);
            return Err(TurnResult::Silent);
        }

        let parts = validated.output.reply_parts.clone();
        // A sticker-only reply is legitimate: she may answer with a picture and no
        // words. Only an entirely empty reply is silent.
        if parts.is_empty() && sticker_sends.is_empty() {
            let _ = self.store.delete_pending(&covered).await;
            self.note_silence(job, call_id, "empty").await;
            log_timing("silent", 0);
            return Err(TurnResult::Silent);
        }

        // Mask credentials before the text reaches the outbox. Doing it here rather
        // than at send time keeps the stored row already safe.
        let parts: Vec<String> = parts
            .into_iter()
            .map(|part| crate::agent::segments::filter_secrets(&part, &self.secrets))
            .collect();

        // Resolve the validated quote target to a real message id: an `mN`
        // against this batch, a `pN` against the previous batch's annex (the
        // seed window's `hN` is a first-turn channel and its own refs are
        // already carried by `history_targets` on that turn). Validation
        // already proved the ref is one this turn was shown, so this cannot
        // point at another group; the kill switch and a resolution miss both
        // degrade to a plain send (audited, never an error).
        let quote_ref = validated.output.quote_ref.clone();
        let resolve_history = |reference: &str| {
            history_targets
                .iter()
                .find(|t| t.history_ref == reference)
                .map(|t| t.message_id.clone())
        };
        let mut quote_target: Option<String> = if self.quote_enabled {
            quote_ref
                .as_ref()
                .and_then(|r| match resolve_source(r) {
                    Some(id) => Some(id),
                    None => resolve_history(r),
                })
                .filter(|id| !id.starts_with("proactive-"))
        } else {
            None
        };
        // Whether the attached target names a message the transcript of *this*
        // turn showed under its own `mN`; a previous-batch target does not, and
        // the final gate below treats the two differently.
        let quote_target_in_batch = quote_target
            .as_ref()
            .is_some_and(|id| messages.iter().any(|m| &m.message_id == id));
        if quote_ref.is_some() && quote_target.is_none() {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        "reply.quote.dropped.g{}.{}",
                        job.group_id,
                        if self.quote_enabled {
                            "unresolved"
                        } else {
                            "disabled"
                        }
                    ),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    crate::now_unix(),
                )
                .await;
        }

        // Send-time freshness gate (「宁可不回复也不要错位」): re-check the
        // batch's timeliness at the last moment before anything is
        // enqueued. The agent may have spent a minute or more thinking; if
        // the batch's newest message is already old, or the group has
        // clearly moved on without it, the whole reply is discarded — the
        // batch is consumed exactly like a silent turn, never requeued.
        let send_now = crate::now_unix();
        let batch_newest = coverage.newest(&messages).unwrap_or(send_now);
        let batch_age = send_now - batch_newest;
        let newer_count = self
            .store
            .count_group_messages_after(&self.bot_id, &job.group_id, batch_newest as f64)
            .await
            .unwrap_or(0);
        if send_gate_drop(batch_age, newer_count) {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        // The kind suffix keeps a drop's action distinguishable in the
                        // trail without changing the action's shape.
                        "sendgate.drop.stale.g{}.age{batch_age}.new{newer_count}.{coverage_label}",
                        job.group_id,
                    ),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    send_now,
                )
                .await;
            let _ = self.store.delete_pending(&covered).await;
            log_timing("dropped_stale", 0);
            return Err(TurnResult::DroppedStale);
        }

        // A resident turn may have delivered several batches; the reply only speaks
        // for the ones it actually covered. A batch that arrived after the union's
        // newest message and was never delivered means the group has moved on past
        // what this reply answers, so the reply is dropped exactly like a stale one.
        if let Some((uncovered_batch, uncovered_newest)) = coverage.uncovered(batch_newest) {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        "sendgate.drop.uncovered.g{}.{coverage_label}.batch{uncovered_batch}.new{}",
                        job.group_id, uncovered_newest
                    ),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    send_now,
                )
                .await;
            let _ = self.store.delete_pending(&covered).await;
            log_timing("dropped_uncovered", 0);
            return Err(TurnResult::DroppedStale);
        }

        // Quote fallback: she reliably leaves quote_ref empty (0 of 7 parts
        // on the incident night), so when the batch has a clear anchor and
        // that anchor has scrolled out of the group's latest messages,
        // attach the quote system-side. An anchor that cannot be verified
        // in this group's log makes the reply undeliverable rather than
        // unanchored. Her own quote_ref always wins (handled above).
        let mut auto_quote_newer: Option<i64> = None;
        if quote_target.is_none() && self.quote_enabled && !quote_aimed_outside {
            let anchor_items: Vec<AnchorItem> = messages
                .iter()
                .enumerate()
                .map(|(i, payload)| AnchorItem {
                    message_ref: format!("m{}", i + 1),
                    message_id: payload.message_id.clone(),
                    sender_id: payload.sender_id.clone(),
                    image_count: payload
                        .segments
                        .iter()
                        .filter(|seg| matches!(seg, Segment::Image { .. }))
                        .count(),
                    quote_of: payload.quote_of.clone(),
                    received_at: Some(payload.received_at),
                })
                .collect();
            if let Some(anchor) = batch_anchor(&anchor_items) {
                let resolved = match &anchor {
                    BatchAnchor::Quoted { target_id, .. } => self
                        .store
                        .group_message_created_at(&self.bot_id, &job.group_id, target_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|ts| (target_id.clone(), ts)),
                    BatchAnchor::Image {
                        message_id,
                        received_at,
                        ..
                    } => received_at.map(|ts| (message_id.clone(), ts as f64)),
                };
                match resolved {
                    Some((anchor_id, anchor_ts)) => {
                        let anchor_newer = self
                            .store
                            .count_group_messages_after(&self.bot_id, &job.group_id, anchor_ts)
                            .await
                            .unwrap_or(0);
                        if anchor_newer > QUOTE_ANCHOR_MAX_NEWER {
                            quote_target = Some(anchor_id);
                            auto_quote_newer = Some(anchor_newer);
                        }
                    }
                    None => {
                        // The auto-quote is a display nicety, not the reply's
                        // addressing: an anchor that cannot be verified (the
                        // quoted message predates our records or was never
                        // stored) must not kill an otherwise good reply. Send
                        // it plain, and leave an audit trail of the degrade.
                        let _ = self
                            .store
                            .audit(
                                "system",
                                &format!(
                                    "reply.quote.fallback_plain.g{}.unverifiable",
                                    job.group_id
                                ),
                                Some(&self.bot_id),
                                Some(&job.group_id),
                                send_now,
                            )
                            .await;
                    }
                }
            }
        }
        // Final quote validation (「宁可不回复也不要错位」, quote edition):
        // a resolved target — hers or the fallback's — is attached only if
        // it survives the drop rules. A quote that names a previous-batch
        // message (`pN`) is deliberate and verifiable: its ref exists only in
        // the annex table of this very turn, and the message it names is not in
        // `messages` at all, so the batch-shaped rules below cannot judge it —
        // they would fail it as `not_in_batch` and leave the answer pointing at
        // nothing. It takes the addressee rule only (an answer addressed to the
        // 主人 cannot quote somebody else). A quote that names the fallback
        // anchor points outside the batch by design and only takes the
        // hot-thread rule.
        // The hot-thread rule needs the gap since the bot's previous send; a first
        // send in the group has no gap, which reads as "not a hot thread".
        let batch_first_at = messages.iter().map(|m| m.received_at).min();
        let prev_sent_gap = match batch_first_at {
            Some(first) => self
                .store
                .last_outbox_sent_at(&self.bot_id, &job.group_id, first)
                .await
                .ok()
                .flatten()
                .map(|prev| first - prev),
            None => None,
        };
        let seeded = prompt_batch.seed_brief.is_some();
        if let Some(target_id) = quote_target.clone() {
            let addresses_owner = parts.first().is_some_and(|p| segment_addresses_owner(p));
            let drop_reason = if quote_ref.is_some() && !quote_target_in_batch {
                // Previous-batch target: the ref was shown in this turn's annex,
                // so the only content contradiction left is addressing.
                let target_is_owner = history_targets
                    .iter()
                    .find(|t| t.message_id == target_id)
                    .is_some_and(|t| t.speaker_role == "owner");
                if addresses_owner && !target_is_owner {
                    Some("addressee_mismatch")
                } else {
                    None
                }
            } else if quote_ref.is_some() {
                match messages.iter().find(|m| m.message_id == target_id) {
                    None => Some("not_in_batch"),
                    Some(target) => {
                        messages
                            .iter()
                            .max_by_key(|m| m.received_at)
                            .and_then(|newest| {
                                quote_drop_reason(
                                    &target.sender_id,
                                    target.message_id == newest.message_id,
                                    target.speaker_role == SpeakerRole::Owner,
                                    &newest.sender_id,
                                    true,
                                    prev_sent_gap,
                                    addresses_owner,
                                    seeded,
                                )
                            })
                    }
                }
            } else if seeded {
                Some("cold_seed")
            } else if prev_sent_gap.is_some_and(|gap| gap < QUOTE_HOT_THREAD_SECONDS) {
                Some("hot_thread")
            } else {
                None
            };
            if let Some(reason) = drop_reason {
                let _ = self
                    .store
                    .audit(
                        "system",
                        &format!("reply.quote.dropped.g{}.{}", job.group_id, reason),
                        Some(&self.bot_id),
                        Some(&job.group_id),
                        crate::now_unix(),
                    )
                    .await;
                // A previous-batch target that failed its gate gets a second line
                // under its own prefix: `reply.quote.dropped` cannot tell the
                // observation week apart a batch quote that lost its echo from an
                // annex quote that never resolved, and the annex is new enough
                // that its drop rate has to be readable on its own.
                if !quote_target_in_batch && let Some(named) = &quote_ref {
                    let _ = self
                        .store
                        .audit(
                            "system",
                            &format!(
                                "reply.quote.history_dropped.g{}.ref{}.{}",
                                job.group_id, named, reason
                            ),
                            Some(&self.bot_id),
                            Some(&job.group_id),
                            crate::now_unix(),
                        )
                        .await;
                }
                quote_target = None;
                auto_quote_newer = None;
            }
        }

        if let Some(newer) = auto_quote_newer {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!("reply.quote.auto_attached.g{}.new{newer}", job.group_id),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    send_now,
                )
                .await;
        }

        // Per-part quotes — the repair path. Each part that named a ref is
        // resolved on its own (batch refs against this batch, history refs
        // against the seed window the model was shown) and must prove the
        // addressing with its echo before anything is attached; the quote
        // gates still apply per part. A part whose quote fails simply
        // sends without one.
        let mut part_quote_ids: Vec<Option<String>> = vec![None; parts.len()];
        if self.quote_enabled {
            for part_quote in &validated.output.part_quotes {
                let idx = part_quote.part_index;
                let Some(part_text) = parts.get(idx) else {
                    continue;
                };
                let addresses_owner = segment_addresses_owner(part_text);
                let resolved: Option<(String, Option<&'static str>)> = if let Some(num) =
                    part_quote.quote_ref.strip_prefix('m')
                {
                    num.parse::<usize>()
                        .ok()
                        .and_then(|n| messages.get(n.wrapping_sub(1)))
                        .map(|target| {
                            // An echo-verified per-part quote has proven
                            // its addressing, so the two lag heuristics
                            // (not_latest / hot_thread — approximations
                            // from before echo existed) no longer apply
                            // to it (owner, 2026-10-27: guards off /
                            // sensitivity down). What still applies: the
                            // seeded-turn rule and the addressee rule,
                            // which are content contradictions an echo
                            // cannot cure. The legacy single-quote path
                            // below keeps the full gate set.
                            let gate = quote_drop_reason(
                                &target.sender_id,
                                true,
                                target.speaker_role == SpeakerRole::Owner,
                                &target.sender_id,
                                false,
                                None,
                                addresses_owner,
                                seeded,
                            );
                            (
                                target.message_id.clone(),
                                quote_echo_check(part_quote.quote_echo.as_deref(), &target.text)
                                    .err()
                                    .or(gate),
                            )
                        })
                } else if part_quote.quote_ref.starts_with('h')
                    || part_quote.quote_ref.starts_with(ANNEX_REF_PREFIX)
                {
                    history_targets
                        .iter()
                        .find(|t| t.history_ref == part_quote.quote_ref)
                        .map(|target| {
                            let gate = quote_drop_reason(
                                &target.sender_id,
                                true,
                                target.speaker_role == "owner",
                                &target.sender_id,
                                false,
                                None,
                                addresses_owner,
                                false,
                            );
                            (
                                target.message_id.clone(),
                                quote_echo_check(part_quote.quote_echo.as_deref(), &target.text)
                                    .err()
                                    .or(gate),
                            )
                        })
                } else {
                    None
                };
                match resolved {
                    Some((message_id, None)) if !message_id.starts_with("proactive-") => {
                        part_quote_ids[idx] = Some(message_id);
                        let _ = self
                            .store
                            .audit(
                                "system",
                                &format!(
                                    "reply.quote.attached.g{}.ref{}",
                                    job.group_id, part_quote.quote_ref
                                ),
                                Some(&self.bot_id),
                                Some(&job.group_id),
                                crate::now_unix(),
                            )
                            .await;
                    }
                    Some((_, None)) => {}
                    Some((_, Some(reason))) => {
                        let _ = self
                            .store
                            .audit(
                                "system",
                                &format!("reply.quote.dropped.g{}.{}", job.group_id, reason),
                                Some(&self.bot_id),
                                Some(&job.group_id),
                                crate::now_unix(),
                            )
                            .await;
                    }
                    None => {
                        let _ = self
                            .store
                            .audit(
                                "system",
                                &format!("reply.quote.dropped.g{}.unresolved", job.group_id),
                                Some(&self.bot_id),
                                Some(&job.group_id),
                                crate::now_unix(),
                            )
                            .await;
                    }
                }
            }
        }

        // Replay guard: drop any segment that repeats what she already
        // sent in this group inside the window — the seed-echo failure,
        // where a fresh session reads its seed window as a script and
        // recites her own recent lines at an unrelated message. Segments
        // accepted earlier in this same reply join the comparison set, so
        // a turn cannot repeat itself either.
        let priors = self
            .store
            .recent_outbox_texts(
                &self.bot_id,
                &job.group_id,
                send_now - REPLAY_WINDOW_SECONDS,
            )
            .await
            .unwrap_or_default();
        let mut prior_norms: Vec<String> = priors.iter().map(|t| normalize_for_replay(t)).collect();
        let mut kept_parts: Vec<String> = Vec::with_capacity(parts.len());
        let mut kept_quote_ids: Vec<Option<String>> = Vec::with_capacity(parts.len());
        let mut replay_blocked = 0usize;
        for (index, part) in parts.iter().enumerate() {
            if let Some(sim) = replay_match(part, &prior_norms) {
                replay_blocked += 1;
                let _ = self
                    .store
                    .audit(
                        "system",
                        &format!(
                            "reply.replay_blocked.g{}.sim{}",
                            job.group_id,
                            replay_sim_bucket(sim)
                        ),
                        Some(&self.bot_id),
                        Some(&job.group_id),
                        crate::now_unix(),
                    )
                    .await;
                // The blocked text itself, so a block can be reviewed
                // after the fact instead of guessed at.
                let _ = self
                    .store
                    .audit(
                        "system",
                        &format!(
                            "reply.replay_blocked.g{}.text.{}",
                            job.group_id,
                            replay_preview(part)
                        ),
                        Some(&self.bot_id),
                        Some(&job.group_id),
                        crate::now_unix(),
                    )
                    .await;
                continue;
            }
            prior_norms.push(normalize_for_replay(part));
            kept_parts.push(part.clone());
            kept_quote_ids.push(part_quote_ids.get(index).cloned().unwrap_or(None));
        }
        let parts = kept_parts;
        if replay_blocked > 0 && parts.is_empty() && sticker_sends.is_empty() {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!("reply.replay_blocked.g{}.all", job.group_id),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    crate::now_unix(),
                )
                .await;
            let _ = self.store.delete_pending(&covered).await;
            log_timing("replay_blocked", 0);
            return Err(TurnResult::DroppedStale);
        }

        // The legacy single quote (top-level quote_ref / anchor fallback,
        // already gated above) fills the first part only when no per-part
        // quote survived there — per-part addressing is the more specific
        // signal and wins its own part. A sticker-only reply hands the
        // legacy target to the sticker, as before.
        let mut final_quote_ids = kept_quote_ids;
        if let Some(target) = &quote_target
            && final_quote_ids.first().is_some_and(|q| q.is_none())
        {
            final_quote_ids[0] = Some(target.clone());
        }
        let part_quote_refs: Vec<Option<&str>> = if final_quote_ids.is_empty() {
            vec![quote_target.as_deref()]
        } else {
            final_quote_ids.iter().map(|q| q.as_deref()).collect()
        };

        let ids = match self
            .outbox
            .enqueue_mixed(
                &self.bot_id,
                &job.group_id,
                &job.batch_id,
                &parts,
                &sticker_sends,
                &part_quote_refs,
                crate::now_unix(),
            )
            .await
        {
            Ok(ids) => ids,
            Err(_) => {
                // Without an outbox row the reply would be lost silently; keep the
                // batch so the failure is visible and retryable.
                let _ = self
                    .store
                    .seal_pending_outbox(
                        &self.bot_id,
                        &job.group_id,
                        &job.batch_id,
                        crate::now_unix(),
                    )
                    .await;
                let _ = self.store.release_events(&job.event_ids).await;
                return Err(TurnResult::AgentFailed);
            }
        };
        if let Some(named) = &quote_ref
            && quote_target.is_some()
        {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!("reply.quote.attached.g{}.ref{named}", job.group_id),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    crate::now_unix(),
                )
                .await;
            // The history channel's hits get a line of their own as well.
            // `ref{named}` already spells `pN`/`hN`, so this is not needed to
            // tell them apart today — it exists so the observation week can count
            // history-quote usage with one prefix match instead of parsing ref
            // spellings, and so the hit and drop counters sit under parallel
            // names (`reply.quote.history_attached` / `..._dropped`). It covers
            // both history windows: the annex of a continuation turn and the
            // seed window of a fresh one.
            if !quote_target_in_batch {
                let _ = self
                    .store
                    .audit(
                        "system",
                        &format!("reply.quote.history_attached.g{}.ref{named}", job.group_id),
                        Some(&self.bot_id),
                        Some(&job.group_id),
                        crate::now_unix(),
                    )
                    .await;
            }
        }
        // A poke rides with the reply that carries it: fired only after the outbox
        // accepted the turn, so a reply that never made it out never pokes anyone.
        // Fire-and-forget — a failed poke is an audit row, never a reason to
        // unwind a reply the group already has coming.
        if let Some(ref target) = validated.output.poke_ref {
            self.send_poke(&messages, target, &job.group_id).await;
        }

        // B3 adoption check: how many of the sends were stickers the server
        // offered this batch. Together with `sticker.offer.*` this is the
        // A1′ hit rate the observation week judges the feature by.
        if !prompt_batch.sticker_offers.is_empty() {
            let adopted = sticker_sends
                .iter()
                .filter(|sent| prompt_batch.sticker_offers.iter().any(|o| &o.hash == *sent))
                .count();
            if adopted > 0 {
                let _ = self
                    .store
                    .audit(
                        "system",
                        &format!("sticker.offer_adopted.g{}.n{adopted}", job.group_id),
                        Some(&self.bot_id),
                        Some(&job.group_id),
                        crate::now_unix(),
                    )
                    .await;
            }
        }
        // Only now may the message bodies go away.
        let _ = self.store.delete_pending(&covered).await;

        // 9. Deliver one part at a time. A later failure seals the rest.
        let gaps = crate::agent::segments::message_gaps_ms(&parts);
        let send_started = std::time::Instant::now();
        self.deliver_parts(&ids, &gaps, &job.group_id, &job.batch_id)
            .await;
        // The next turn's annex lists the batch she answered but not her
        // answer; without it she cannot tell a correction of that answer
        // ("没叫你") from a fresh tease, because her own replies never
        // appear in any prompt. Note the reply against the conversation
        // it was produced in — a rotated session makes the note a no-op.
        if let Ok(Some(group)) = self.store.get_group(&self.bot_id, &job.group_id).await
            && let Some(conversation_id) = group.agy_conversation_id.as_deref()
        {
            let own: String = parts.join("\n").chars().take(300).collect();
            if !own.trim().is_empty() {
                self.note_previous_reply(&job.group_id, conversation_id, &own)
                    .await;
            }
        }
        log_timing("replied", send_started.elapsed().as_millis());
        Ok(TurnResult::Replied)
    }
}
