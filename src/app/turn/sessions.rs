//! Resident session management for TurnExecutor: eligibility, sweep, prewarm, canary and retirement.
//! Moved out of turn.rs verbatim (std-split-turn-executor); logic unchanged.

use super::*;

impl TurnExecutor {
    /// Whether the group may run on a resident session right now.
    ///
    /// A pure query. The single entry point for that question — the canary, prewarm
    /// and the panel all go through it — so none of them can drift into its own
    /// interpretation of the respawn backoff. Eligibility says nothing about *when*
    /// the group comes back: the backoff deadline expires on its own, and the next
    /// tick simply reads `false` here until it does.
    ///
    /// There is no per-group switch behind this any more: the resident session is the
    /// only execution path, so the sole gate is the respawn backoff.
    pub async fn resident_eligible(&self, group: &crate::store::GroupRecord, now: i64) -> bool {
        !self.resident.backoff_active(&group.group_id, now).await
    }

    /// Apply a probe decision. `canary` selects which probe to feed.
    pub(super) async fn apply_probe(
        &self,
        manager: &Arc<crate::app::resident::ResidentManager>,
        group_id: &str,
        outcome: RoundOutcome,
        canary: bool,
    ) {
        let action = if canary {
            manager.record_canary(group_id, outcome.contract_ok).await
        } else {
            manager.record_round(group_id, outcome).await
        };
        if let ProbeAction::Rotate(reason) = action {
            manager
                .audit(
                    group_id,
                    &format!("resident.probe.rotate.g{group_id}.{reason}"),
                )
                .await;
            // Forcing the next logical round onto a fresh conversation is a
            // matter of dropping the stored id; the session is closed so the
            // new conversation is what the next spawn resumes.
            let _ = self
                .store
                .set_conversation_id(&self.bot_id, group_id, None, crate::now_unix())
                .await;
            manager.close(group_id).await;
        }
    }

    /// Resident housekeeping, driven by the flush loop: pause reclaim, watchdog,
    /// audit drain. A no-op when no group has a live session.
    pub async fn resident_sweep(&self, now: i64) {
        self.resident.sweep(now).await;
    }

    /// Keep a warm session standing for a resident-eligible group that has
    /// none, so the next batch (or the first batch after a restart) does not
    /// pay the PTY boot before its generation can start. Spawning is the
    /// expensive part and happens off the flush loop; the seed still rides
    /// the first real prompt, exactly as a lazily spawned session's would.
    pub async fn resident_prewarm(
        &self,
        group: &crate::store::GroupRecord,
        now: i64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let manager = Arc::clone(&self.resident);
        if !self.resident_eligible(group, now).await {
            return None;
        }
        // Never birth a session into a proxy outage: a TUI that boots while
        // the network check is red lands on an error screen and stays
        // wedged (the 2026-10-27 restart pattern, twice). Lazy spawns never
        // hit this because the turn path defers on the same gate first.
        if !self.mihomo.ready().await {
            return None;
        }
        // `has_session` stays false for the whole of onboarding. Without the
        // in-flight check the 500ms flush loop forks another PTY every tick.
        if manager.has_session(&group.group_id).await || manager.spawn_in_flight(&group.group_id) {
            return None;
        }
        if self
            .running_turns
            .snapshot()
            .iter()
            .any(|(g, _)| g == &group.group_id)
        {
            return None;
        }
        let project_dir = self.project_dir(&group.group_id);
        let persona = self.store.get_persona().await.ok().flatten();
        let _ = crate::media::SandboxWorkspace::ensure_project_dir(
            &project_dir,
            &crate::agent::prompt::agents_md_with_persona(self.output_channel, persona.as_deref()),
        );
        if let Ok(ws) = crate::media::SandboxWorkspace::open_in(&project_dir) {
            let _ = ws.write_scope(&self.bot_id, &group.group_id);
        }
        let group_id = group.group_id.clone();
        let conversation_id = group.agy_conversation_id.clone();
        Some(tokio::spawn(async move {
            let _ = manager
                .session(&group_id, &project_dir, conversation_id)
                .await;
        }))
    }

    /// Run today's canary for one group, if it is due.
    ///
    /// The canary is a real turn against a real session, but its output is only
    /// inspected: nothing is written to the outbox, nothing enters `group_messages`,
    /// no memory op is applied. It answers "is this session still able to produce a
    /// contract-shaped reply" and nothing else.
    pub async fn resident_canary(&self, group_id: &str, now: i64) {
        let manager = &self.resident;
        let Some(local) = chrono::TimeZone::timestamp_opt(&chrono::Local, now, 0).single() else {
            return;
        };
        let hour = chrono::Timelike::hour(&local);
        let day = now.div_euclid(86_400);
        if !manager.canary_due(group_id, day, hour).await {
            return;
        }
        // A turn in flight owns the session: skip and retry on a later tick inside
        // the window instead of talking over it.
        if self
            .running_turns
            .snapshot()
            .iter()
            .any(|(g, _)| g == group_id)
        {
            return;
        }
        match self.store.get_group(&self.bot_id, group_id).await {
            Ok(Some(group))
                if group.enabled
                    && !group.paused
                    && self.resident_eligible(&group, crate::now_unix()).await => {}
            _ => return,
        };
        manager.canary_started(group_id, day).await;
        let project_dir = self.project_dir(group_id);
        // Scratch session still shares the group's project dir, so plant scope.json
        // next to AGENTS.md before spawn. Env injection pins the real group_id even
        // though the session key is `<group>#canary`.
        let _ = crate::media::SandboxWorkspace::write_project_scope(
            &project_dir,
            &self.bot_id,
            group_id,
        );
        // The canary runs against a scratch session so its question and its answer
        // never enter the group's own transcript, never advance the group's step
        // count toward the 40-step rotation, and never let the token be echoed back
        // by a later real reply (2026-10-27: exactly that leaked into a group).
        //
        // The scratch PTY still shares the group's workspace, so it has to hold the
        // same "a turn owns this group" mark the real turn path uses: prewarm skips
        // any group with a turn in flight, and two `agy` processes rewriting one
        // settings.json would break the queued-messages invariant.
        self.running_turns.register(group_id, crate::now_unix());
        let ok = self.canary_probe(manager, group_id, &project_dir).await;
        self.running_turns.deregister(group_id);
        self.canary_settle(manager, group_id, ok).await;
    }

    /// One canary attempt on a scratch session; `true` when the session answered
    /// with the token. Always closes the scratch session before returning.
    pub(super) async fn canary_probe(
        &self,
        manager: &Arc<crate::app::resident::ResidentManager>,
        group_id: &str,
        project_dir: &std::path::Path,
    ) -> bool {
        let key = crate::app::resident::canary_session_key(group_id);
        // `conversation_id: None` is the isolation: a scratch session can never
        // resume the group's stored conversation, and nothing writes an id back.
        let session = match manager.session(&key, project_dir, None).await {
            Ok(session) => session,
            Err(err) => {
                manager
                    .audit(
                        group_id,
                        &format!("resident.canary.fail.g{group_id}.{}", err.audit_tag()),
                    )
                    .await;
                return false;
            }
        };
        let ok = self.canary_turn(manager, group_id, &session).await;
        // Attribute the scratch session's own audit trail to the real group and
        // drop the session: the probe is over either way.
        manager.close_as(&key, group_id).await;
        ok
    }

    /// Deliver the canary prompt and wait for the token. Returns whether it came
    /// back; every failure is audited against the real group id.
    ///
    /// Text channel: the token must appear in the PTY step text (JSON body).
    /// MCP channel: arm a turn, ask for `submit_reply`, and read `turn_submissions`.
    pub(super) async fn canary_turn(
        &self,
        manager: &Arc<crate::app::resident::ResidentManager>,
        group_id: &str,
        session: &Arc<crate::agent::resident::ResidentSession>,
    ) -> bool {
        let mcp = self.output_channel.is_mcp();
        let token = mcp.then(new_turn_token);
        if let Some(token) = token.as_deref() {
            let now = crate::now_unix();
            let ttl = 120 + 30;
            if self
                .store
                .arm_turn(
                    group_id,
                    token,
                    &format!("canary-{group_id}"),
                    now,
                    now + ttl,
                )
                .await
                .is_err()
            {
                manager
                    .audit(group_id, &format!("resident.canary.fail.g{group_id}.arm"))
                    .await;
                return false;
            }
        }
        let prompt = crate::app::probes::canary_text(self.output_channel, token.as_deref());
        let receipt = match session.deliver(&prompt).await {
            Ok(receipt) => receipt,
            Err(err) => {
                if let Some(token) = token.as_deref() {
                    let _ = self.store.disarm_turn(group_id, token).await;
                }
                manager
                    .audit(
                        group_id,
                        &format!("resident.canary.fail.g{group_id}.{}", err.audit_tag()),
                    )
                    .await;
                return false;
            }
        };
        let spec = TurnSpec::new(receipt.step_idx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let outcome = loop {
            if std::time::Instant::now() >= deadline {
                let _ = session.interrupt().await;
                manager
                    .audit(
                        group_id,
                        &format!("resident.canary.fail.g{group_id}.timeout"),
                    )
                    .await;
                break false;
            }
            if let Some(token) = token.as_deref()
                && let Ok(rows) = self.store.submissions_for(token).await
                && rows.iter().any(|row| {
                    row.verdict == "accepted"
                        && row.payload.contains(crate::app::probes::CANARY_TOKEN)
                })
            {
                manager
                    .audit(group_id, &format!("resident.canary.ok.g{group_id}"))
                    .await;
                break true;
            }
            let now_f = crate::now_unix() as f64;
            match session
                .evaluate(&spec, now_f, self.resident_limits.quiet_s, None)
                .await
            {
                Ok((_, Verdict::Deliver { text, .. })) if !mcp => {
                    // Shape + fact only. The reply must be the token it was asked
                    // for; anything else means the session drifted.
                    let ok = text.contains(crate::app::probes::CANARY_TOKEN);
                    manager
                        .audit(
                            group_id,
                            &format!(
                                "resident.canary.{}.g{group_id}",
                                if ok { "ok" } else { "fail" }
                            ),
                        )
                        .await;
                    break ok;
                }
                Ok((_, Verdict::Deliver { .. })) => {
                    // MCP: PTY text is not the delivery channel. Keep waiting for
                    // submit_reply (checked at the top of the loop).
                    tokio::time::sleep(std::time::Duration::from_millis(
                        self.resident_limits.poll_ms.max(50),
                    ))
                    .await;
                }
                Ok((_, Verdict::Failed { .. })) => {
                    manager
                        .audit(
                            group_id,
                            &format!("resident.canary.fail.g{group_id}.contract"),
                        )
                        .await;
                    break false;
                }
                Ok((_, Verdict::NotReady { .. })) => {
                    // A canary has no inserted batches, so nothing can register an
                    // insertion; just keep watching for the deliverable.
                    tokio::time::sleep(std::time::Duration::from_millis(
                        self.resident_limits.poll_ms.max(50),
                    ))
                    .await;
                }
                Err(_) => {
                    let _ = session.interrupt().await;
                    manager
                        .audit(group_id, &format!("resident.canary.fail.g{group_id}.steps"))
                        .await;
                    break false;
                }
            }
        };
        if let Some(token) = token.as_deref() {
            let _ = self.store.disarm_turn(group_id, token).await;
        }
        outcome
    }

    /// Feed one canary result into the probe and act on it.
    pub(super) async fn canary_settle(
        &self,
        manager: &Arc<crate::app::resident::ResidentManager>,
        group_id: &str,
        ok: bool,
    ) {
        self.apply_probe(
            manager,
            group_id,
            RoundOutcome {
                contract_ok: ok,
                steps: 0,
                drafts: 0,
                // A canary is one fixed question with one fixed answer: it either
                // came back or it did not, so there is no repaired or declined
                // middle ground to record.
                degraded: false,
            },
            true,
        )
        .await;
    }

    /// The resident session produced a contract that must not be sent and must
    /// not be fed back into the same transcript. Close it, forget its
    /// conversation, and hand this batch back to the pool.
    ///
    /// There is no fallback lane: the resident path owns the group. The retirement
    /// arms the respawn backoff, the batch's events are released so they are not
    /// stuck behind a dead batch id, and the lane defers until the deadline passes.
    pub(super) async fn retire_resident(
        &self,
        manager: &Arc<crate::app::resident::ResidentManager>,
        job: &BatchJob,
        call_id: &str,
        reason: &str,
        error_class: &str,
        coverage: &ResidentCoverage,
    ) -> TurnResult {
        let now = crate::now_unix();
        let _ = self
            .store
            .finish_call(call_id, CallState::Failed, Some(error_class), now)
            .await;
        let _ = self
            .store
            .set_conversation_id(&self.bot_id, &job.group_id, None, now)
            .await;
        manager.close(&job.group_id).await;
        manager
            .audit(
                &job.group_id,
                &format!("resident.session.retired.g{}.{reason}", job.group_id),
            )
            .await;
        // Every event this turn covered goes back to the pool, not just the batch
        // that opened it: the inserted batches were claimed by a session that no
        // longer exists, and leaving them claimed would strand them behind a batch
        // id nothing will ever release.
        let released = coverage.event_ids();
        let _ = self.store.release_events(&released).await;
        // Consecutive retirements walk the respawn ladder; the deadline is what
        // keeps a group that fails on every spawn from burning one PTY per tick.
        let (tier, until) = manager.note_respawn_failure(&job.group_id, now).await;
        // Tier 0 means the cooldown switch is off and nothing was
        // armed; there is no backoff to put in the audit trail.
        if tier > 0 {
            manager
                .audit(
                    &job.group_id,
                    &format!(
                        "resident.session.backoff.g{}.tier{tier}.until{until}",
                        job.group_id
                    ),
                )
                .await;
        }
        TurnResult::BadOutput
    }

    /// Settle a failed resident turn: close the call, release the batch, and hand
    /// the outcome back to the caller.
    pub(super) async fn resident_fail(
        &self,
        manager: &Arc<crate::app::resident::ResidentManager>,
        job: &BatchJob,
        call_id: &str,
        session: &Arc<crate::agent::resident::ResidentSession>,
        reason: &str,
        result: TurnResult,
    ) -> TurnResult {
        let _ = self
            .store
            .finish_call(
                call_id,
                CallState::Failed,
                Some(ErrorClass::AgentFailed.as_str()),
                crate::now_unix(),
            )
            .await;
        manager
            .audit(
                &job.group_id,
                &format!("resident.turn.failed.g{}.{reason}", job.group_id),
            )
            .await;
        // A crash leaves a process that can never make progress again; a plain
        // failure may be transient and the session is kept.
        if matches!(
            session.state().await,
            crate::agent::resident::SessionState::Dead(_)
                | crate::agent::resident::SessionState::Blocked(_)
        ) {
            manager.close(&job.group_id).await;
        }
        // A session that stops confirming deliveries is wedged in a way its
        // process state cannot show (a TUI parked on an error screen reads
        // as alive). One strike and the session is dropped with its
        // conversation id, so the next turn starts truly fresh instead of
        // feeding batches into the same dead screen — the manual recovery
        // from the 2026-10-27 restart wedge, automated. The threshold was
        // two until 2026-10-30, when step-DB evidence from two 943 failure
        // clusters showed a first-strike session never recovers: the agy
        // cascade stalls mid-flight (or a fresh conversation's first send
        // is silently swallowed inside the language server), later pastes
        // queue unprocessed behind it, and the batch bought by the second
        // strike is always sacrificed. A turn-level failure already means
        // both 30s delivery attempts went unconfirmed, so one strike is
        // evidence enough.
        if reason == "deliver_unconfirmed" && manager.note_deliver_failure(&job.group_id).await >= 1
        {
            manager.close(&job.group_id).await;
            let _ = self
                .store
                .set_conversation_id(&self.bot_id, &job.group_id, None, crate::now_unix())
                .await;
            manager
                .audit(
                    &job.group_id,
                    &format!(
                        "resident.session.dropped.g{}.deliver_unconfirmed",
                        job.group_id
                    ),
                )
                .await;
        }
        let _ = self.store.release_events(&job.event_ids).await;
        result
    }

    /// Remember `messages` as the batch a continuation turn may cite under its
    /// `pN` names.
    ///
    /// The numbering continues from the entry already cached for the same
    /// conversation (`next_ref`), and restarts at 1 only when the cached entry
    /// belongs to a different, dead conversation. A batch with nothing quotable
    /// still replaces the entry — leaving the older one in place would present a
    /// two-turns-old batch as "the previous one" — but keeps the counter, so no
    /// number is ever handed out twice inside one conversation.
    pub(super) async fn record_previous_batch(
        &self,
        group_id: &str,
        conversation_id: &str,
        messages: &[IncomingMessage],
    ) {
        let mut cache = self.previous_batch.lock().await;
        let base = cache
            .get(group_id)
            .filter(|cached| cached.conversation_id == conversation_id)
            .map(|cached| cached.next_ref)
            .unwrap_or(1);
        match PreviousBatch::for_batch(conversation_id, base, messages) {
            Some(entry) => {
                cache.insert(group_id.to_string(), entry);
            }
            None => {
                cache.insert(
                    group_id.to_string(),
                    PreviousBatch {
                        conversation_id: conversation_id.to_string(),
                        table: String::new(),
                        targets: Vec::new(),
                        next_ref: base,
                        own_reply: None,
                    },
                );
            }
        }
    }

    /// Note what the bot herself answered on the turn that opened the
    /// cached previous batch, so the next turn's annex can show it (as an
    /// unnumbered line — her lines are context, never quote targets).
    /// A conversation mismatch makes this a no-op: the note belongs to
    /// the session that produced the batch, and a rotated or retired
    /// session's annex is never read again anyway.
    pub(super) async fn note_previous_reply(
        &self,
        group_id: &str,
        conversation_id: &str,
        text: &str,
    ) {
        let mut cache = self.previous_batch.lock().await;
        if let Some(entry) = cache.get_mut(group_id)
            && entry.conversation_id == conversation_id
        {
            entry.own_reply = Some(text.to_string());
        }
    }
}
