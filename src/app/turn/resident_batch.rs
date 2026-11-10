//! The resident execution lane for TurnExecutor: run a batch on a live session, take its submission, commit and rotate.
//! Moved out of turn.rs verbatim (std-split-turn-executor); logic unchanged.

use super::*;

impl TurnExecutor {
    /// Fixed header prepended to every inserted batch. The agent must fold the
    /// new messages into the turn it is already answering instead of starting a
    /// second reply, so the instruction is stated before the rendered batch.
    pub(super) const RESIDENT_INSERT_HEADER: &'static str = "## 本轮新增消息（并入当前回答）\n\n以下消息在你生成这一轮回答的过程中到达。把它们并入**同一个**回答，\n不要另起一轮，不要重复已经说过的内容，不要给这些消息填 quote_ref：\n\n";

    /// Header for an insert delivered after an ESC interrupt: the model can
    /// see the interrupt in its TUI, so the header says plainly that the
    /// generation it was in the middle of was cut for these messages, and
    /// the steps it already completed are still in the conversation.
    pub(super) const RESIDENT_INSERT_HEADER_ESC: &'static str = "## 本轮新增消息（生成已被打断）\n\n以下消息在你生成这一轮回答的过程中到达，刚才那段生成已为此中断。\n结合本轮最初的问题与已经完成的内容，把它们并入，给出**一个完整**的回答，\n不要另起一轮，不要重复已经说过的内容，不要给这些消息填 quote_ref：\n\n";

    /// The one execution path: one long-lived TUI per group, one logical turn per
    /// claim.
    ///
    /// It is built out of [`Self::prepare_turn`] and
    /// [`Self::finish_turn_with_coverage`], so validation, memory application, the
    /// freshness gate and the outbox are ordinary code with no second variant. What
    /// is specific to this module is the middle: the prompt goes into a PTY, the
    /// answer is read back from the conversation step stream through the delivery
    /// barrier, and batches that arrive while the turn is in flight may be inserted
    /// into it instead of waiting for a second turn.
    pub(super) async fn run_resident_batch(
        &self,
        job: &BatchJob,
        group: &crate::store::GroupRecord,
        now: i64,
        turn_started: std::time::Instant,
    ) -> TurnResult {
        let manager = &self.resident;

        let prepared = match self.prepare_turn(job, group, now).await {
            Ok(prepared) => prepared,
            Err(result) => return result,
        };

        let call_id = match self
            .store
            .start_call_with_priority(
                &self.bot_id,
                &job.group_id,
                &job.batch_id,
                now,
                job.privileged,
            )
            .await
        {
            Ok(id) => id,
            Err(_) => return TurnResult::AgentFailed,
        };

        // The PTY's cwd is the group project directory: it outlives a single turn,
        // which is the point of a resident session. The per-turn sandbox directory
        // created by `prepare_turn` is still handed to the memory MCP scope below.
        let project_dir = self.project_dir(&job.group_id);
        let session = match manager
            .session(
                &job.group_id,
                &project_dir,
                group.agy_conversation_id.clone(),
            )
            .await
        {
            Ok(session) => session,
            Err(err) => {
                let _ = self
                    .store
                    .finish_call(
                        &call_id,
                        CallState::Failed,
                        Some(ErrorClass::AgentFailed.as_str()),
                        crate::now_unix(),
                    )
                    .await;
                manager
                    .audit(
                        &job.group_id,
                        &format!(
                            "resident.session.blocked.g{}.{}",
                            job.group_id,
                            err.audit_tag()
                        ),
                    )
                    .await;
                let _ = self.store.release_events(&job.event_ids).await;
                return TurnResult::AgentFailed;
            }
        };

        let agent_started = std::time::Instant::now();
        let receipt = match session.deliver(&prepared.prompt).await {
            Ok(receipt) => receipt,
            Err(err) => {
                return self
                    .resident_fail(
                        manager,
                        job,
                        &call_id,
                        &session,
                        err.audit_tag(),
                        TurnResult::AgentFailed,
                    )
                    .await;
            }
        };
        manager.note_deliver_success(&job.group_id).await;
        // The conversation id only becomes known once the first prompt landed.
        let _ = self
            .store
            .set_conversation_id(
                &self.bot_id,
                &job.group_id,
                Some(&receipt.conversation_id),
                now,
            )
            .await;
        // Annex handoff: this batch becomes the next turn's `pN` legend. Recorded
        // here because delivery is the one point both output channels pass through
        // (the text channel reads its answer off the PTY, the MCP channel takes a
        // submission against the same session), and because the conversation id it
        // is keyed by is only known at this instant. A rotation, a retire or a
        // respawn changes that id, which invalidates the entry without a purge.
        self.record_previous_batch(&job.group_id, &receipt.conversation_id, &prepared.messages)
            .await;

        let mut spec = TurnSpec::new(receipt.step_idx);
        let mut coverage = ResidentCoverage::default();
        coverage.batches.push(ResidentDelivery {
            batch_id: job.batch_id.clone(),
            event_ids: job.event_ids.clone(),
            newest: prepared
                .messages
                .iter()
                .map(|m| m.received_at)
                .max()
                .unwrap_or(now),
            privileged: job.privileged,
        });
        let mut inserts = 0u32;
        // Salvaged-delivery hold state (see the Deliver guard below): the
        // last step text a repair was seen on, and whether the hold has
        // already left its audit row for this turn.
        let mut salvage_seen: Option<(i64, String)> = None;
        let mut salvage_hold_audited = false;
        let mut first_turn_nudged = false;
        // MCP channel: when the last insertion landed. A submission older
        // than this is a pre-insertion draft and never commits.
        let mut last_insert_at: i64 = 0;
        // The pending set the last held-insert audit was written for, so a
        // batch that stays held does not re-audit on every poll tick (ESC
        // mode judges on every tick, not just idle-screen ones).
        let mut held_seen: Option<(usize, i64)> = None;
        // A queue waiting behind this turn widens its insert budget for
        // this turn only (see `boosted_limits`): absorbing waiting rows
        // here is cheaper than claiming them as new batches later.
        let waiting = self
            .store
            .pending_count(&self.bot_id, &job.group_id)
            .await
            .unwrap_or(0)
            .max(0) as usize;
        let limits = boosted_limits(self.resident_limits, waiting);
        if limits != self.resident_limits {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!("resident.insert.boosted.g{}.waiting{waiting}", job.group_id),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    now,
                )
                .await;
        }
        let soft_deadline =
            turn_started + std::time::Duration::from_secs(limits.soft_cap_s.max(1) as u64);
        let hard_deadline =
            turn_started + std::time::Duration::from_secs(limits.hard_cap_s.max(1) as u64);

        loop {
            // Watchdog: a dead child or a stalled stream while a turn is in flight
            // is a crash, which takes the same path as any other agent failure.
            if let Err(err) = session.tick(true).await {
                return self
                    .resident_fail(
                        manager,
                        job,
                        &call_id,
                        &session,
                        err.audit_tag(),
                        TurnResult::AgentFailed,
                    )
                    .await;
            }

            // Insertion window: only while the turn is young enough to absorb more
            // messages. Past the soft cap the turn keeps running but stops taking
            // new work, so it converges instead of chasing the conversation.
            if std::time::Instant::now() < soft_deadline {
                match self
                    .try_resident_insert(
                        manager,
                        &session,
                        &mut coverage,
                        &prepared,
                        group,
                        &mut inserts,
                        &mut held_seen,
                        turn_started,
                        limits,
                    )
                    .await
                {
                    Ok(Some(step_idx)) => {
                        spec.accept_insert(step_idx);
                        last_insert_at = crate::now_unix();
                    }
                    Ok(None) => {}
                    Err(result) => {
                        return self
                            .resident_fail(
                                manager,
                                job,
                                &call_id,
                                &session,
                                "interrupt_failed",
                                result,
                            )
                            .await;
                    }
                }
            }

            // The barrier still gate-keeps every delivery, but through the salvage
            // ladder: a document the strict path refuses only because one side
            // section is wrong must not sink a reply whose body is intact. The
            // strict path is tried first inside `validate_salvaged`, so a clean
            // draft is judged exactly as before.
            let validator =
                |raw: &str| contract::validate_salvaged(raw, &prepared.scope).map(|_| ());
            // MCP channel: the barrier judges only whether the model has
            // finished (inserts, background tasks, quiet). There is no text
            // deliverable to validate — the delivery is the recorded
            // submission, judged by `resident_submission_outcome` below.
            let validator_ref: Option<
                &(dyn Fn(&str) -> Result<(), contract::RejectReason> + Send + Sync),
            > = if self.output_channel.is_mcp() {
                None
            } else {
                Some(&validator)
            };
            let evaluated = session
                .evaluate(
                    &spec,
                    crate::now_unix() as f64,
                    limits.quiet_s,
                    validator_ref,
                )
                .await;
            match evaluated {
                // A refusal while the prompt is still absent is a stub. The
                // model rewrites the same step; accepting it deletes the batch.
                Ok((_, Verdict::Deliver { text, .. }))
                    if !self.output_channel.is_mcp()
                        && delivered_text_is_silence(&text, &prepared.scope)
                        && !screen_is_idle_for_insert(&session.screen_text()) => {}
                // A delivery that only passed through the salvage ladder is
                // a repair of bytes that may still be growing: the barrier's
                // validator is the ladder itself, so a half-written step
                // whose first segment is already complete repairs into a
                // fragment and would ship as the whole answer (2026-10-28,
                // group 943: a four-part DDR3 explainer went out as its
                // one-line lead-in). A repair closes the turn only once the
                // step text survives a full poll unchanged, or the screen is
                // back at the prompt — the same terminal evidence the
                // silence and failed arms require. A clean contract is
                // complete by construction and still delivers immediately.
                Ok((_, Verdict::Deliver { idx, text }))
                    if !self.output_channel.is_mcp()
                        && delivery_was_salvaged(&text, &prepared.scope)
                        && !screen_is_idle_for_insert(&session.screen_text())
                        && !salvage_text_stable(&mut salvage_seen, idx, &text) =>
                {
                    if !salvage_hold_audited {
                        salvage_hold_audited = true;
                        manager
                            .audit(
                                &job.group_id,
                                &format!("resident.turn.salvage_hold.g{}.step{idx}", job.group_id),
                            )
                            .await;
                    }
                }
                Ok((parsed, Verdict::Deliver { idx, text })) => {
                    let drafts = list_drafts(&parsed.events, &spec).len() as i64;
                    let steps = parsed.events.len() as i64;
                    // An upstream content-filter refusal leaves its wording
                    // in the step stream or on the PTY screen; the text
                    // channel checks its own text separately below (a valid
                    // contract starts with '{', the refusal never does).
                    // Classified once, shared by both channels.
                    let prompt_rejected =
                        texts_show_prompt_rejection(parsed.events.iter().map(|e| e.text.as_str()))
                            || looks_like_prompt_rejection(&session.screen_text());
                    if self.output_channel.is_mcp() {
                        match self
                            .resident_submission_outcome(
                                manager,
                                job,
                                &call_id,
                                &prepared,
                                steps,
                                drafts,
                                i64::from(inserts),
                                last_insert_at,
                                &coverage,
                                prompt_rejected,
                                first_turn_nudged,
                            )
                            .await
                        {
                            SubmissionOutcome::Wait => continue,
                            SubmissionOutcome::Nudge => {
                                match session
                                    .deliver(first_turn_nudge_prompt(self.output_channel))
                                    .await
                                {
                                    Ok(_) => {
                                        first_turn_nudged = true;
                                        continue;
                                    }
                                    Err(err) => {
                                        return self
                                            .resident_fail(
                                                manager,
                                                job,
                                                &call_id,
                                                &session,
                                                err.audit_tag(),
                                                TurnResult::AgentFailed,
                                            )
                                            .await;
                                    }
                                }
                            }
                            SubmissionOutcome::Done(result) => return result,
                            SubmissionOutcome::Commit(payload) => {
                                return self
                                    .finish_resident_commit(
                                        manager,
                                        job,
                                        &call_id,
                                        payload,
                                        TurnTiming {
                                            turn_started,
                                            agent_started,
                                        },
                                        prepared,
                                        coverage,
                                        steps,
                                        drafts,
                                        i64::from(inserts),
                                    )
                                    .await;
                            }
                        }
                    } else {
                        // One classification per round, reused by the addressed-silence
                        // guard below and by the probe at the end of the arm: the same
                        // text decides both questions, and asking twice risked the two
                        // answers drifting apart.
                        let shape = RoundShape::of(&text, &prepared.scope);
                        // A deep session that answers with nothing but
                        // `should_reply: false` is not a reply. Recording it as
                        // success deletes the batch. Retire the session and answer
                        // this batch once, on a conversation that has no transcript.
                        // A content-filter refusal delivered as the turn's text
                        // (2026-10-28, group 943: the refusal prose reached the
                        // salvage ladder and was booked as a malformed contract):
                        // the prompt never ran, so close it as what it is —
                        // before the refusal-shape checks below can mistake it
                        // for a contract the session chose to write.
                        if !text.trim_start().starts_with('{')
                            && looks_like_prompt_rejection(&text)
                        {
                            let _ = self
                                .store
                                .record_resident_counts(&call_id, steps, i64::from(inserts), drafts)
                                .await;
                            return self
                                .retire_for_content_filter(manager, job, &call_id, &coverage)
                                .await;
                        }
                        if steps >= RESIDENT_STEP_ROTATE_LIMIT && contract::is_bare_refusal(&text) {
                            let _ = self
                                .store
                                .record_resident_counts(&call_id, steps, i64::from(inserts), drafts)
                                .await;
                            manager
                                .audit(
                                    &job.group_id,
                                    &format!("resident.turn.bare_refusal.g{}.step{idx}", job.group_id),
                                )
                                .await;
                            return self
                                .retire_resident(
                                    manager,
                                    job,
                                    &call_id,
                                    "bare_refusal",
                                    ErrorClass::AgentFailed.as_str(),
                                    &coverage,
                                )
                                .await;
                        }
                        // An addressed round may not end on a silence. The batch that
                        // claimed this turn — or one absorbed into it — was aimed at the
                        // bot, so `should_reply: false` here is a dropped question, not a
                        // judgement that the group should be left alone (2026-10-28,
                        // group 943: the owner asked a question, the mention claimed its
                        // own batch, and the turn ended as a salvaged decline). Retire
                        // the session and answer the batch once on a conversation that
                        // has no transcript — the same recovery the bare refusal uses.
                        if shape.addressed_silence(coverage.any_privileged()) {
                            let _ = self
                                .store
                                .record_resident_counts(&call_id, steps, i64::from(inserts), drafts)
                                .await;
                            manager
                                .audit(
                                    &job.group_id,
                                    &format!(
                                        "resident.turn.addressed_silence.g{}.step{idx}",
                                        job.group_id
                                    ),
                                )
                                .await;
                            // The round is recorded as degraded before the session is
                            // retired: it answered nothing it was asked, which is the
                            // quality signal the window exists to catch. It is recorded
                            // here because the retry's own round is not measured, so
                            // nothing else would feed the window.
                            self.apply_probe(
                                manager,
                                &job.group_id,
                                RoundOutcome {
                                    contract_ok: true,
                                    steps,
                                    drafts,
                                    degraded: true,
                                },
                                false,
                            )
                            .await;
                            return self
                                .retire_resident(
                                    manager,
                                    job,
                                    &call_id,
                                    "addressed_silence",
                                    ErrorClass::AgentFailed.as_str(),
                                    &coverage,
                                )
                                .await;
                        }
                        // A logical round completed: this is what the conversation cap
                        // counts, not the number of PTY round-trips.
                        let count = self
                            .store
                            .bump_agy_turn_count(&self.bot_id, &job.group_id, crate::now_unix())
                            .await
                            .unwrap_or(0);
                        let _ = self
                            .store
                            .record_resident_counts(&call_id, steps, i64::from(inserts), drafts)
                            .await;
                        manager
                            .audit(
                                &job.group_id,
                                &format!(
                                    "resident.turn.delivered.g{}.step{idx}.ins{inserts}.drafts{drafts}",
                                    job.group_id
                                ),
                            )
                            .await;

                        let result = self
                            .finish_turn_with_coverage(
                                job,
                                &call_id,
                                &text,
                                TurnTiming {
                                    turn_started,
                                    agent_started,
                                },
                                prepared,
                                coverage,
                            )
                            .await
                            .unwrap_or_else(|early| early);
                        // A send or a chosen silence is a real resident round: it is the
                        // only thing that walks the respawn ladder back down. Nothing
                        // else reaches this point, so nothing can excuse a failing
                        // session.
                        if matches!(result, TurnResult::Replied | TurnResult::Silent) {
                            manager.note_respawn_clean(&job.group_id).await;
                            // A delivered round is also a clean round for the
                            // seed quarantine: ten in a row earn the full seed
                            // back (see SqliteStore::note_seed_clean_round).
                            let _ = self
                                .store
                                .note_seed_clean_round(&self.bot_id, &job.group_id, crate::now_unix())
                                .await;
                        }

                        // Probe: a delivered round is the only kind that feeds the
                        // rolling window; failures are counted by the caller of
                        // `finish_turn_with_coverage` via the BadOutput branch below.
                        let contract_ok = !matches!(result, TurnResult::BadOutput);
                        self.apply_probe(
                            manager,
                            &job.group_id,
                            RoundOutcome {
                                contract_ok,
                                steps,
                                drafts,
                                degraded: shape.degraded(),
                            },
                            false,
                        )
                        .await;

                        // Rotation after a completed logical round: the next turn
                        // starts a fresh conversation and `prepare_turn` seeds it.
                        self.rotate_after_delivered_round(&job.group_id, count, steps)
                            .await;
                        return result;
                    }
                }
                // Same rule for a contract the barrier already failed: the
                // bytes are often a half-written step. Wait for the prompt,
                // or for the hard cap below, before retiring or releasing.
                Ok((_, Verdict::Failed { .. }))
                    if !screen_is_idle_for_insert(&session.screen_text()) => {}
                Ok((parsed, Verdict::Failed { idx, reason })) => {
                    let drafts = list_drafts(&parsed.events, &spec).len() as i64;
                    let steps = parsed.events.len() as i64;
                    let _ = self
                        .store
                        .record_resident_counts(&call_id, steps, i64::from(inserts), drafts)
                        .await;
                    manager
                        .audit(
                            &job.group_id,
                            &format!(
                                "resident.turn.contract_failed.g{}.step{idx}.{reason}",
                                job.group_id
                            ),
                        )
                        .await;
                    // An upstream content-filter refusal is not a contract
                    // failure — the prompt never ran — so it bypasses the
                    // probe record entirely and closes under its own class.
                    if texts_show_prompt_rejection(parsed.events.iter().map(|e| e.text.as_str()))
                        || looks_like_prompt_rejection(&session.screen_text())
                    {
                        return self
                            .retire_for_content_filter(manager, job, &call_id, &coverage)
                            .await;
                    }
                    // First miss of a brand-new session: one reminder, no probe,
                    // no retire. A second miss still retires as before.
                    if failure_retires_session(&reason) && submission_nudge_due(first_turn_nudged) {
                        manager
                            .audit(
                                &job.group_id,
                                &format!("resident.turn.no_submission.nudge.g{}", job.group_id),
                            )
                            .await;
                        match session
                            .deliver(first_turn_nudge_prompt(self.output_channel))
                            .await
                        {
                            Ok(_) => first_turn_nudged = true,
                            Err(err) => {
                                return self
                                    .resident_fail(
                                        manager,
                                        job,
                                        &call_id,
                                        &session,
                                        err.audit_tag(),
                                        TurnResult::AgentFailed,
                                    )
                                    .await;
                            }
                        }
                    } else {
                        self.apply_probe(
                            manager,
                            &job.group_id,
                            RoundOutcome {
                                contract_ok: false,
                                steps,
                                drafts,
                                degraded: false,
                            },
                            false,
                        )
                        .await;
                        // An empty contract is the session giving up, not a
                        // draft we can send. Putting the batch back on the same
                        // transcript just produces the next empty contract.
                        if failure_retires_session(&reason) {
                            let why = if reason.contains("malformed") {
                                "malformed"
                            } else {
                                "empty_reply"
                            };
                            return self
                                .retire_resident(
                                    manager,
                                    job,
                                    &call_id,
                                    why,
                                    ErrorClass::AgentFailed.as_str(),
                                    &coverage,
                                )
                                .await;
                        }
                        // Never fall back to a draft: the round is a failure.
                        return self
                            .resident_fail(
                                manager,
                                job,
                                &call_id,
                                &session,
                                "contract_invalid",
                                TurnResult::BadOutput,
                            )
                            .await;
                    }
                }
                Ok((parsed, Verdict::NotReady { reasons })) => {
                    // MCP channel: a turn that ends without any final agent
                    // text is not stuck — the submission is the output. When
                    // the only unmet barrier condition is the missing text,
                    // the screen is back at the prompt, and a committable
                    // submission exists, the turn is complete.
                    if self.output_channel.is_mcp()
                        && reasons.len() == 1
                        && reasons[0] == "no_agent_text_after_last_user"
                        && screen_is_idle_for_insert(&session.screen_text())
                    {
                        let steps = parsed.events.len() as i64;
                        let drafts = list_drafts(&parsed.events, &spec).len() as i64;
                        match self
                            .resident_submission_outcome(
                                manager,
                                job,
                                &call_id,
                                &prepared,
                                steps,
                                drafts,
                                i64::from(inserts),
                                last_insert_at,
                                &coverage,
                                texts_show_prompt_rejection(
                                    parsed.events.iter().map(|e| e.text.as_str()),
                                ) || looks_like_prompt_rejection(&session.screen_text()),
                                first_turn_nudged,
                            )
                            .await
                        {
                            SubmissionOutcome::Wait => {}
                            SubmissionOutcome::Nudge => {
                                match session
                                    .deliver(first_turn_nudge_prompt(self.output_channel))
                                    .await
                                {
                                    Ok(_) => {
                                        first_turn_nudged = true;
                                        continue;
                                    }
                                    Err(err) => {
                                        return self
                                            .resident_fail(
                                                manager,
                                                job,
                                                &call_id,
                                                &session,
                                                err.audit_tag(),
                                                TurnResult::AgentFailed,
                                            )
                                            .await;
                                    }
                                }
                            }
                            SubmissionOutcome::Done(result) => return result,
                            SubmissionOutcome::Commit(payload) => {
                                return self
                                    .finish_resident_commit(
                                        manager,
                                        job,
                                        &call_id,
                                        payload,
                                        TurnTiming {
                                            turn_started,
                                            agent_started,
                                        },
                                        prepared,
                                        coverage,
                                        steps,
                                        drafts,
                                        i64::from(inserts),
                                    )
                                    .await;
                            }
                        }
                    }
                }
                Err(err) => {
                    // A parse drift is not a transient read failure: the step
                    // stream no longer has a shape we understand, so this session
                    // must be retired. But a single drift sample is
                    // not proof: steps are rewritten in place while agy works,
                    // so one read can catch a step half-formed (2026-10-27,
                    // group 943: one sample cost the group its residency).
                    // Confirm first — re-read the stream twice; only a drift
                    // that survives both re-reads condemns the session.
                    let mut drift_suffix = String::new();
                    if let Some(kind) = err.drift_kind() {
                        manager
                            .audit(
                                &job.group_id,
                                &format!("resident.drift.suspected.g{}.{}", job.group_id, kind),
                            )
                            .await;
                        let mut confirmed = true;
                        for _ in 0..2 {
                            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                            match session.poll().await {
                                Ok(_) => {
                                    confirmed = false;
                                    break;
                                }
                                Err(e) if e.drift_kind().is_some() => {}
                                Err(_) => {
                                    // The failure changed shape: not the
                                    // drift we were confirming. Let the turn
                                    // loop's normal paths judge it.
                                    confirmed = false;
                                    break;
                                }
                            }
                        }
                        if !confirmed {
                            continue;
                        }
                        drift_suffix = format!(".{kind}");
                    }
                    if manager
                        .record_drift(&job.group_id)
                        .await
                        .audit_reason()
                        .is_some()
                    {
                        // Drift is a format change, not a slow degradation: the
                        // verdict is a fresh session, the same as any other probe
                        // rotation. The retirement below arms the respawn backoff,
                        // so a build that keeps rewriting the stream still costs one
                        // spawn per ladder step rather than one per tick.
                        manager
                            .audit(
                                &job.group_id,
                                &format!("resident.drift.rotate.g{}{}", job.group_id, drift_suffix),
                            )
                            .await;
                    }
                    return self
                        .resident_fail(
                            manager,
                            job,
                            &call_id,
                            &session,
                            err.audit_tag(),
                            TurnResult::AgentFailed,
                        )
                        .await;
                }
            }

            // Hard cap: stop the turn the same way an interrupt would, then settle it
            // through the normal failure semantics so the rows are released.
            if std::time::Instant::now() >= hard_deadline {
                let _ = session.interrupt().await;
                manager
                    .audit(
                        &job.group_id,
                        &format!(
                            "resident.turn.hard_cap.g{}.{}s",
                            job.group_id, limits.hard_cap_s
                        ),
                    )
                    .await;
                return self
                    .resident_fail(
                        manager,
                        job,
                        &call_id,
                        &session,
                        "hard_cap",
                        TurnResult::AgentFailed,
                    )
                    .await;
            }

            tokio::time::sleep(std::time::Duration::from_millis(limits.poll_ms.max(50))).await;
        }
    }

    /// Judge the MCP channel's recorded submissions for a resident turn
    /// whose barrier says the model has finished (with or without a final
    /// text). Failure semantics mirror the text path: no submission at all
    /// is this channel's `malformed` — the session retires and the batch goes back
    /// to the pool behind a respawn backoff; a deep bare refusal retires the same
    /// way. A submission that predates the last insertion is a draft: the model was
    /// told to fold the insertion in and submit again, so the turn keeps waiting for
    /// the newer one.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn resident_submission_outcome(
        &self,
        manager: &Arc<crate::app::resident::ResidentManager>,
        job: &BatchJob,
        call_id: &str,
        prepared: &PreparedTurn,
        steps: i64,
        drafts: i64,
        inserts: i64,
        last_insert_at: i64,
        coverage: &ResidentCoverage,
        prompt_rejected: bool,
        already_nudged: bool,
    ) -> SubmissionOutcome {
        let Some(token) = prepared.turn_token.as_deref() else {
            return SubmissionOutcome::Done(TurnResult::AgentFailed);
        };
        let rows = match self.store.submissions_for(token).await {
            Ok(rows) => rows,
            Err(_) => return SubmissionOutcome::Wait,
        };
        let accepted: Vec<&crate::store::TurnSubmission> =
            rows.iter().filter(|r| r.verdict == "accepted").collect();
        let Some(candidate) = accepted.last() else {
            let _ = self
                .store
                .record_resident_counts(call_id, steps, inserts, drafts)
                .await;
            if prompt_rejected {
                // The prompt was refused upstream, so of course nothing was
                // submitted: a content-filter round, not a no-submission
                // failure. No audit under the no_submission key, no probe
                // record — the seed is quarantined and the session retires
                // under the dedicated class.
                let result = self
                    .retire_for_content_filter(manager, job, call_id, coverage)
                    .await;
                return SubmissionOutcome::Done(result);
            }
            if submission_nudge_due(already_nudged) {
                manager
                    .audit(
                        &job.group_id,
                        &format!("resident.turn.no_submission.nudge.g{}", job.group_id),
                    )
                    .await;
                return SubmissionOutcome::Nudge;
            }
            manager
                .audit(
                    &job.group_id,
                    &format!(
                        "resident.turn.no_submission.g{}.calls{}",
                        job.group_id,
                        rows.len()
                    ),
                )
                .await;
            self.apply_probe(
                manager,
                &job.group_id,
                RoundOutcome {
                    contract_ok: false,
                    steps,
                    drafts,
                    degraded: false,
                },
                false,
            )
            .await;
            let retry_ids = coverage.event_ids();
            let result = self
                .retire_resident(
                    manager,
                    job,
                    call_id,
                    "no_submission",
                    ErrorClass::AgentFailed.as_str(),
                    coverage,
                )
                .await;
            // One-shot replay (2026-10-29): this batch was delivered and
            // the session never started, so its events go back unanswered.
            // For an opted-in group, arm the intake to claim exactly those
            // rows as the next job — fresh session, fresh token, ahead of
            // any merge. A retry job never re-arms.
            if retry_arm_due(
                job.retry,
                self.no_submission_retry_groups
                    .iter()
                    .any(|g| g == &job.group_id),
            ) {
                manager.arm_retry(&job.group_id, retry_ids).await;
                manager
                    .audit(
                        &job.group_id,
                        &format!("resident.retry.armed.g{}", job.group_id),
                    )
                    .await;
            }
            return SubmissionOutcome::Done(result);
        };
        // The settle second keeps a just-landed call from being committed
        // while the model is still mid-gesture after it.
        if candidate.created_at < last_insert_at || crate::now_unix() - candidate.created_at < 1 {
            return SubmissionOutcome::Wait;
        }
        if steps >= RESIDENT_STEP_ROTATE_LIMIT && payload_is_bare_refusal(&candidate.payload) {
            let _ = self
                .store
                .record_resident_counts(call_id, steps, inserts, drafts)
                .await;
            manager
                .audit(
                    &job.group_id,
                    &format!("resident.turn.bare_refusal.g{}.mcp", job.group_id),
                )
                .await;
            let result = self
                .retire_resident(
                    manager,
                    job,
                    call_id,
                    "bare_refusal",
                    ErrorClass::AgentFailed.as_str(),
                    coverage,
                )
                .await;
            return SubmissionOutcome::Done(result);
        }
        if rows.len() > 1 {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        "agent.submission.extra.g{}.calls{}",
                        job.group_id,
                        rows.len()
                    ),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    crate::now_unix(),
                )
                .await;
        }
        SubmissionOutcome::Commit(candidate.payload.clone())
    }

    /// Commit an accepted MCP submission through the same finish path a
    /// text-channel barrier delivery uses, with the same round bookkeeping
    /// (turn count, rotation, probe window) as a text delivery.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finish_resident_commit(
        &self,
        manager: &Arc<crate::app::resident::ResidentManager>,
        job: &BatchJob,
        call_id: &str,
        payload: String,
        timing: TurnTiming,
        prepared: PreparedTurn,
        coverage: ResidentCoverage,
        steps: i64,
        drafts: i64,
        inserts: i64,
    ) -> TurnResult {
        let count = self
            .store
            .bump_agy_turn_count(&self.bot_id, &job.group_id, crate::now_unix())
            .await
            .unwrap_or(0);
        let _ = self
            .store
            .record_resident_counts(call_id, steps, inserts, drafts)
            .await;
        manager
            .audit(
                &job.group_id,
                &format!(
                    "resident.turn.delivered.g{}.mcp.drafts{drafts}",
                    job.group_id
                ),
            )
            .await;
        let result = self
            .finish_turn_with_coverage(job, call_id, &payload, timing, prepared, coverage)
            .await
            .unwrap_or_else(|early| early);
        if matches!(result, TurnResult::Replied | TurnResult::Silent) {
            manager.note_respawn_clean(&job.group_id).await;
        }
        let contract_ok = !matches!(result, TurnResult::BadOutput);
        self.apply_probe(
            manager,
            &job.group_id,
            RoundOutcome {
                contract_ok,
                steps,
                drafts,
                degraded: false,
            },
            false,
        )
        .await;
        self.rotate_after_delivered_round(&job.group_id, count, steps)
            .await;
        result
    }

    /// Clear the conversation and audit when a delivered round hits the turn
    /// cap or the step-depth cap. Extracted so the two finish paths cannot
    /// drift, and so the rotation contract can be tested without a PTY.
    pub(super) async fn rotate_after_delivered_round(
        &self,
        group_id: &str,
        count: i64,
        steps: i64,
    ) {
        let step_due = steps >= RESIDENT_STEP_ROTATE_LIMIT;
        if !(rotation_due(
            count,
            super::turn_limit_value(&self.conversation_turn_limit),
        ) || step_due)
        {
            return;
        }
        let now = crate::now_unix();
        let _ = self
            .store
            .set_conversation_id(&self.bot_id, group_id, None, now)
            .await;
        let why = if step_due {
            format!("ops.conversation.rotated.g{group_id}.steps{steps}")
        } else {
            format!("ops.conversation.rotated.g{group_id}.after{count}")
        };
        self.resident.audit(group_id, &why).await;
        self.resident.close(group_id).await;
    }

    /// Decide whether a newly claimed batch may join the in-flight turn, and if so
    /// deliver it into the same conversation.
    ///
    /// Returns the `user` step index on success so the caller can register the
    /// insertion with the barrier — without that registration the barrier would
    /// keep waiting for it forever.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn try_resident_insert(
        &self,
        manager: &Arc<crate::app::resident::ResidentManager>,
        session: &Arc<crate::agent::resident::ResidentSession>,
        coverage: &mut ResidentCoverage,
        prepared: &PreparedTurn,
        group: &crate::store::GroupRecord,
        inserts: &mut u32,
        held_seen: &mut Option<(usize, i64)>,
        turn_started: std::time::Instant,
        limits: ResidentLimits,
    ) -> Result<Option<i64>, TurnResult> {
        let rows = match self
            .store
            .pending_for_group(&self.bot_id, &group.group_id)
            .await
        {
            Ok(rows) if !rows.is_empty() => rows,
            _ => return Ok(None),
        };
        // The model is still writing. Pasting now blocks the loop that reads
        // the answer (15s, then a second try). Leave the rows unclaimed.
        // ESC mode is the exception: it may judge the batch while the turn
        // is still generating, because an accepted insert stops the
        // generation instead of waiting for an idle screen.
        if !limits.insert_esc && !screen_is_idle_for_insert(&session.screen_text()) {
            return Ok(None);
        }
        let now = crate::now_unix();
        let tier = match self.pacing.as_ref() {
            Some(engine) => {
                engine
                    .observe(now, std::slice::from_ref(&group.group_id))
                    .await
                    .tier
            }
            None => Tier::T0,
        };

        let mut mentions_bot = false;
        let mut from_owner = false;
        let mut has_image = false;
        let mut same_speaker = false;
        let mut newest_incoming = i64::MIN;
        let last_speaker = prepared.messages.last().map(|m| m.sender_id.as_str());
        let mut parsed: Vec<IncomingMessage> = Vec::with_capacity(rows.len());
        for row in &rows {
            newest_incoming = newest_incoming.max(row.received_at);
            // One unreadable row keeps the whole batch unclaimed. Claiming the
            // siblings would strand the bad row, and a later turn could not retry it.
            let Ok(message) = serde_json::from_str::<IncomingMessage>(&row.normalized_payload)
            else {
                return Ok(None);
            };
            parsed.push(message);
        }
        for message in &parsed {
            mentions_bot |= message.mentions_bot;
            from_owner |= message.speaker_role == SpeakerRole::Owner;
            has_image |= message
                .segments
                .iter()
                .any(|s| matches!(s, Segment::Image { .. }));
            same_speaker |= last_speaker == Some(message.sender_id.as_str());
        }
        if newest_incoming == i64::MIN {
            return Ok(None);
        }
        let covered_newest = coverage
            .batches
            .iter()
            .map(|b| b.newest)
            .max()
            .unwrap_or(newest_incoming);

        let insert_limits = InsertLimits {
            enabled: group.resident_insert_enabled,
            tier,
            inserts_so_far: *inserts,
            max_inserts: limits.max_inserts,
            turn_age_s: turn_started.elapsed().as_secs() as i64,
            soft_cap_s: limits.soft_cap_s,
        };
        let signals = InsertSignals {
            mentions_bot,
            from_owner,
            has_image,
            same_speaker,
            same_topic: insert::is_continuation(newest_incoming - covered_newest),
        };
        let mut decision = insert::decide(&signals, &insert_limits);
        // Indices (into `rows`/`parsed`) the insert will actually absorb:
        // normally the whole batch; a mixed batch can shed its pictures.
        let mut chosen: Vec<usize> = (0..rows.len()).collect();
        // A batch mixing plain text with picture messages is split at
        // message granularity: the text joins the running turn now, the
        // picture messages stay unclaimed for the next full turn (the
        // only place pictures get described). Holding the whole batch
        // for its pictures held the text hostage too — an owner's
        // question sat out a turn because a sticker rode along in the
        // same batch (2026-11-04, a production group). A message is never
        // split from its own picture; only whole messages move halves.
        let (text_idx, image_idx) = partition_insertable(&parsed);
        if !text_idx.is_empty() && !image_idx.is_empty() {
            let mut sub_mentions = false;
            let mut sub_owner = false;
            let mut sub_same_speaker = false;
            let mut sub_newest = i64::MIN;
            for &idx in &text_idx {
                let message = &parsed[idx];
                sub_mentions |= message.mentions_bot;
                sub_owner |= message.speaker_role == SpeakerRole::Owner;
                sub_same_speaker |= last_speaker == Some(message.sender_id.as_str());
                sub_newest = sub_newest.max(rows[idx].received_at);
            }
            let sub_signals = InsertSignals {
                mentions_bot: sub_mentions,
                from_owner: sub_owner,
                has_image: false,
                same_speaker: sub_same_speaker,
                same_topic: insert::is_continuation(sub_newest - covered_newest),
            };
            let sub_decision = insert::decide(&sub_signals, &insert_limits);
            if sub_decision.is_accept() {
                chosen = text_idx;
                decision = sub_decision;
                // Downstream bookkeeping (claim set, coverage, the
                // privilege flag) describes exactly the absorbed rows.
                mentions_bot = sub_mentions;
                newest_incoming = sub_newest;
            }
        }
        if !decision.is_accept() {
            // Held, not dropped: the rows keep their unclaimed state and the next
            // turn picks them up. Audited once per pending set so a group that
            // never inserts is visible in the trail without one row per poll
            // tick — ESC mode reaches this judgement on every tick.
            let held_key = (rows.len(), newest_incoming);
            if *held_seen != Some(held_key) {
                *held_seen = Some(held_key);
                manager
                    .audit(
                        &group.group_id,
                        &format!(
                            "resident.insert.held.g{}.n{}.{}",
                            group.group_id,
                            rows.len(),
                            decision.reason_str()
                        ),
                    )
                    .await;
            }
            return Ok(None);
        }

        // Text only. `prepare_turn` would re-run vision, memory and sticker
        // selection and stall the turn that is already generating. Pictures
        // never reach this arm: `decide` holds them for the next full turn.
        let event_ids: Vec<i64> = chosen.iter().map(|&idx| rows[idx].id).collect();
        let batch_id = format!("b{}-{}-ins{}", group.group_id, now, *inserts);
        let event_ids = match self.store.claim_batch(&event_ids, &batch_id).await {
            Ok(claimed) if !claimed.is_empty() => claimed,
            _ => return Ok(None),
        };
        let parsed: Vec<_> = rows
            .iter()
            .zip(parsed)
            .filter(|(row, _)| event_ids.contains(&row.id))
            .map(|(_, message)| message)
            .collect();
        let body = render_insert_lines(&parsed);
        if body.is_empty() {
            let _ = self.store.release_events(&event_ids).await;
            return Ok(None);
        }
        // ESC mode with the turn still generating: stop the generation
        // first, then paste into the idle prompt. Interrupting only after
        // the claim and the render means a batch that fails to assemble
        // never costs the running turn its generation. If the interrupt
        // itself fails, ESC has usually already cancelled the TUI without
        // returning the ready marker (2026-10-29: four misses then a 60s
        // stall). Release the insert, close the session, and fail the turn
        // so the next batch gets a fresh PTY — do not paste, and do not
        // wait for the watchdog.
        let mut interrupted = false;
        if insert_needs_interrupt(
            limits.insert_esc,
            screen_is_idle_for_insert(&session.screen_text()),
        ) {
            match session
                .interrupt_generation(std::time::Duration::from_secs(5))
                .await
            {
                Ok(()) => interrupted = true,
                Err(_) => {
                    let _ = self.store.release_events(&event_ids).await;
                    manager
                        .audit(
                            &group.group_id,
                            &format!("resident.insert.interrupt_failed.g{}", group.group_id),
                        )
                        .await;
                    manager.close(&group.group_id).await;
                    return Err(TurnResult::AgentFailed);
                }
            }
        }
        let header = if interrupted {
            Self::RESIDENT_INSERT_HEADER_ESC
        } else {
            Self::RESIDENT_INSERT_HEADER
        };
        let text = format!("{}{}", header, body);
        // One paste, five seconds to confirm, on both paths. The idle
        // path used to get one second on the theory that a missed paste
        // is harmless — the rows are released and the next turn takes
        // the messages. That theory is wrong when the paste *landed* but
        // the conversation step DB had not made it visible within the
        // window: the attempt was scored a miss, the rows were released,
        // the next poll pasted the same batch again, and the copy that
        // finally stayed claimed was answered by the next turn as if it
        // were new (2026-11-03, a production group: one message pasted
        // four times, answered twice). Five seconds matches the
        // post-interrupt window — the TUI has just redrawn, so the user
        // step can land a beat later. The main prompt still uses 15s.
        //
        // Even five seconds lost the race once more (2026-11-04: the
        // same insert pasted twice inside one turn), so confirmation
        // now gets a second look against the same pre-paste floor
        // after an 8s wait (`deliver_insert`) before the rows are
        // released as uninserted.
        let confirm_window = std::time::Duration::from_secs(5);
        let receipt = match session
            .deliver_insert(&text, confirm_window, std::time::Duration::from_secs(8))
            .await
        {
            Ok(receipt) => receipt,
            Err(_) => {
                let _ = self.store.release_events(&event_ids).await;
                return Ok(None);
            }
        };
        *inserts += 1;
        coverage.batches.push(ResidentDelivery {
            batch_id,
            event_ids,
            newest: newest_incoming,
            // Same rule the batcher uses to claim a batch with priority: a row
            // in this batch mentions the bot. `mentions_bot` above is computed
            // over exactly the rows being absorbed, so the absorbed batch
            // carries the same weight as one that claimed its own turn.
            privileged: mentions_bot,
        });
        manager
            .audit(
                &group.group_id,
                &format!(
                    "resident.insert.accepted.g{}.step{}.{}",
                    group.group_id,
                    receipt.step_idx,
                    decision.reason_str()
                ),
            )
            .await;
        if chosen.len() < rows.len() {
            // The picture half stayed behind for the next full turn.
            // Audit it once, under the same dedup key the next poll's
            // hold path computes for it, so the split is visible
            // exactly once in the trail.
            let deferred_newest = rows
                .iter()
                .enumerate()
                .filter(|(idx, _)| !chosen.contains(idx))
                .map(|(_, row)| row.received_at)
                .max()
                .unwrap_or(newest_incoming);
            let held_key = (rows.len() - chosen.len(), deferred_newest);
            if *held_seen != Some(held_key) {
                *held_seen = Some(held_key);
                manager
                    .audit(
                        &group.group_id,
                        &format!(
                            "resident.insert.held.g{}.n{}.{}",
                            group.group_id,
                            held_key.0,
                            insert::HoldReason::ImageDeferred.as_str()
                        ),
                    )
                    .await;
            }
        }
        Ok(Some(receipt.step_idx))
    }
}

/// Split a parsed pending batch into (insertable now, waits for a full
/// turn) message indices. A message carrying a picture waits whole — its
/// text is never separated from the picture, which is the invariant the
/// image hold exists for — while picture-free messages may join the
/// running turn immediately instead of being held hostage by a picture
/// that happens to share their batch.
fn partition_insertable(parsed: &[IncomingMessage]) -> (Vec<usize>, Vec<usize>) {
    let mut text = Vec::new();
    let mut image = Vec::new();
    for (idx, message) in parsed.iter().enumerate() {
        if message
            .segments
            .iter()
            .any(|s| matches!(s, Segment::Image { .. }))
        {
            image.push(idx);
        } else {
            text.push(idx);
        }
    }
    (text, image)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(segments: Vec<Segment>) -> IncomingMessage {
        IncomingMessage {
            bot_id: "1".into(),
            group_id: "100".into(),
            message_id: "m".into(),
            sender_id: "555".into(),
            speaker_ref: "u1".into(),
            speaker_role: SpeakerRole::Member,
            display_name: String::new(),
            known_alias: None,
            received_at: 1,
            segments,
            text: String::new(),
            mentions_bot: false,
            quote_of: None,
        }
    }

    fn text_seg() -> Segment {
        Segment::Text {
            text: "在吗".into(),
        }
    }

    fn image_seg() -> Segment {
        Segment::Image {
            url: None,
            file: None,
            sub_type: None,
        }
    }

    #[test]
    fn partition_splits_a_mixed_batch_at_message_granularity() {
        let batch = vec![
            msg(vec![text_seg()]),
            msg(vec![image_seg()]),
            msg(vec![text_seg(), image_seg()]),
            msg(vec![text_seg()]),
        ];
        let (text, image) = partition_insertable(&batch);
        assert_eq!(text, vec![0, 3]);
        assert_eq!(image, vec![1, 2]);
    }

    #[test]
    fn partition_keeps_a_text_only_batch_whole() {
        let batch = vec![msg(vec![text_seg()]), msg(vec![Segment::Voice])];
        let (text, image) = partition_insertable(&batch);
        assert_eq!(text, vec![0, 1]);
        assert!(image.is_empty());
    }

    #[test]
    fn partition_of_an_all_picture_batch_inserts_nothing() {
        let batch = vec![msg(vec![image_seg()])];
        let (text, image) = partition_insertable(&batch);
        assert!(text.is_empty());
        assert_eq!(image, vec![0]);
    }
}
