//! Application wiring: startup, the flush loop and graceful shutdown.
//!
//! Nothing here invents policy; it only assembles the modules in the order the design
//! requires, and it refuses to start when a required input is missing. Notably it does
//! **not** fail when an unverified external contract is missing — it starts, reports
//! `production_ready: false` with explicit blockers, and keeps the reply path disabled
//! behind the proxy/agent gates.

pub mod damper;
pub mod insert;
pub mod intake;
pub mod pacing;
pub mod preflight;
pub mod probes;
pub mod relevance;
pub mod resident;
pub mod runtime_settings;
pub mod sticker_gate;
pub mod turn;

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

use crate::admin::auth::{AdminAuth, require_bearer};
use crate::admin::routes::{AdminState, router as admin_router};
use crate::agent::agy::AgyPermissions;
use crate::config::Config;
use crate::error::{BotError, BotResult};
use crate::health::{AgyHealth, HealthRegistry, HealthSnapshot, OneBotHealth, OwnerStatus};
use crate::integrations::aux_gateway::{AuxGatewayConfig, AuxGatewayStatus};
use crate::integrations::aux_llm::AuxLlm;
use crate::integrations::aux_slot::{AgyAuxParams, AuxSlot, build_provider};
use crate::integrations::quota::{QuotaConfig, QuotaService};
use crate::memory::chunker::{FoldOutcome, FoldSkip};
use crate::memory::compaction::{CompactionScheduler, CompactionThresholds, SummaryModel};
use crate::memory::retrieval::MemoryContextBuilder;
use crate::onebot::connection::{Connection, ConnectionHandle};
use crate::pipeline::batcher::BatchLimits;
use crate::pipeline::outbox::Outbox;
use crate::pipeline::scheduler::Scheduler;
use crate::process::mihomo::{Mihomo, MihomoConfig};
use crate::state::memory::MemoryApplier;
use crate::store::SqliteStore;

use runtime_settings::{RuntimeSettings, SettingsOverlay, effective_aux_config};

pub use intake::{Intake, IntakeLimits, IntakeOutcome};
pub use turn::{TurnExecutor, TurnResult};

/// How often the flush tick runs.
pub const TICK_INTERVAL: Duration = Duration::from_millis(500);
/// How often health is refreshed.
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(30);

/// How often to copy OneBot status while it is still disconnected.
///
/// The steady tick is 30s, which is fine once the socket is up. On a restart the
/// socket is up in about a second and the next sample was up to half an interval
/// away, so `production_ready` sat false for 30–60s while the deploy script polled.
pub const HEALTH_CONNECT_INTERVAL: Duration = Duration::from_secs(1);

/// A started bot. Keeping the parts together makes shutdown a single await.
pub struct App {
    pub config: Arc<Config>,
    pub store: Arc<SqliteStore>,
    pub health: Arc<HealthRegistry>,
    pub mihomo: Arc<Mihomo>,
    pub aux: Arc<dyn AuxLlm>,
    /// The swappable provider behind `aux` / `summary_model`: the settings
    /// API replaces the backend inside the slot at runtime.
    pub aux_slot: Arc<AuxSlot>,
    pub summary_model: Arc<dyn crate::memory::compaction::SummaryModel>,
    /// Runtime settings (overlay plus the live handles it steers).
    pub settings: Arc<RuntimeSettings>,
    /// Shared conversation cap slot (-1 = unlimited), flipped live by the
    /// rotation switch.
    pub rotation_limit: Arc<std::sync::atomic::AtomicI64>,
    pub quota: Arc<QuotaService>,
    /// Quota pacing: turns the account snapshot into a tier and per-group caps.
    pub pacing: Arc<crate::app::pacing::PacingEngine>,
    pub compactor: Arc<CompactionScheduler>,
    pub scheduler: Arc<Scheduler>,
    pub connection: ConnectionHandle,
    pub intake: Arc<Intake>,
    pub executor: Arc<TurnExecutor>,
    /// The supervisor paired with `connection`. Taken once by `serve`.
    supervisor: tokio::sync::Mutex<Option<Connection>>,
    admin_token: String,
}

impl App {
    /// Build everything except the listeners. Used by `run` and by tests.
    pub async fn build(config: Config) -> BotResult<Self> {
        let config = Arc::new(config);

        // Store + migrations first: everything else needs it.
        let pool = crate::store::migrations::open_and_migrate(&config.db_path).await?;
        let store = Arc::new(SqliteStore::new(pool));

        // Crash recovery: an interrupted call is `unknown`, never replayed.
        let now = crate::now_unix();
        let _ = store.reconcile_calls(now).await;
        // Claims the previous process made but never executed are orphans now:
        // the lanes are empty and no call is running, yet a tagged row is
        // invisible to the flush tick forever. Release them back to the pool
        // and leave an audit row per group, so a restart never silently
        // swallows claimed messages again.
        if let Ok(released) = store.release_orphaned_claims(&config.bot_id).await {
            for (group_id, count) in released {
                if count == 0 {
                    continue;
                }
                let _ = store
                    .audit(
                        "system",
                        &format!("ops.startup.release_claims.g{group_id}.n{count}"),
                        Some(&config.bot_id),
                        Some(&group_id),
                        now,
                    )
                    .await;
            }
        }
        let _ = store.reconcile_jobs(now).await;
        let _ = store.reconcile_outbox(now).await;
        let _ = store.prune_seen(now - 7 * 86_400).await;

        let mihomo = Arc::new(Mihomo::new(
            MihomoConfig {
                bin: config.mihomo_bin.clone(),
                config_path: config.mihomo_config_path.clone(),
                api_port: config.file.mihomo_api_port,
                proxy_port: config.file.mihomo_proxy_port,
                // The controller secret is generated per deployment; absent means the
                // controller is loopback-only and unauthenticated, which the design
                // accepts because it is never published.
                api_secret: None,
            },
            config.mihomo_enabled,
        ));

        // Audit what the operator actually provisioned when the settings file is
        // readable, and fall back to our intent otherwise. The 1.4.7 schema itself is
        // verified (grants are pruned on rewrite, deny is enforced), so the report is
        // about this deployment's grants, not about an unknown file format.
        let permissions = crate::agent::agy::read_settings_permissions(&config.agy_settings_path)
            .unwrap_or_else(AgyPermissions::required);
        let agy_health = AgyHealth::from_audit(permissions.audit(), &config.agy_model);
        // Runtime settings overlay: changes made through the settings API
        // persist next to the database and re-apply here, on top of the
        // file config.
        let settings_path = config
            .db_path
            .parent()
            .map(|dir| dir.join("runtime-settings.json"));
        let settings_overlay = SettingsOverlay::load(settings_path.as_deref());
        let aux_base_config = AuxGatewayConfig::from_config(&config);
        let aux_config = effective_aux_config(&aux_base_config, &settings_overlay);
        let agy_aux_params = AgyAuxParams {
            bin: config.agy_bin.clone(),
            home: config.agy_home.clone(),
            workspace: config.agy_projects_dir.join("__aux__"),
            model: config.agy_model.clone(),
            turn_limit: config.file.conversation_turn_limit.max(1),
        };
        // The aux layer always has exactly one provider (operator's
        // dual-mode design): the gateway when it is enabled, otherwise one
        // dedicated Agy session. There is no third, unprovisioned state.
        // The slot lets the settings API swap the provider at runtime.
        let aux_slot = Arc::new(AuxSlot::new(build_provider(aux_config, &agy_aux_params)));
        let aux = Arc::clone(&aux_slot) as Arc<dyn AuxLlm>;
        let summary_model =
            Arc::clone(&aux_slot) as Arc<dyn crate::memory::compaction::SummaryModel>;

        // Panel quota card: reads the Agy account's Gemini buckets. It talks
        // to Google directly, so it rides the same Mihomo proxy the agent
        // uses; with the proxy disabled it simply tries a direct connection
        // and degrades to empty buckets when that fails.
        let quota = Arc::new(QuotaService::new(QuotaConfig {
            agy_home: config.agy_home.clone(),
            agy_bin: config.agy_bin.clone(),
            proxy_url: config
                .mihomo_enabled
                .then(|| format!("http://127.0.0.1:{}", config.file.mihomo_proxy_port)),
        }));

        let mut snapshot = HealthSnapshot {
            started_at: now,
            now,
            onebot: OneBotHealth::default(),
            proxy: None,
            proxy_enabled: config.mihomo_enabled,
            agy: Some(agy_health),
            aux: Some(aux_slot.status_cached().await),
            owner: OwnerStatus {
                configured: !config.owner_qq_id.is_empty(),
            },
            production_ready: false,
            blockers: Vec::new(),
        };
        snapshot.recompute();
        let health = Arc::new(HealthRegistry::new(snapshot));

        let compactor = Arc::new(CompactionScheduler::new(
            Arc::clone(&store),
            CompactionThresholds {
                daily_limit: i64::from(config.file.compaction_daily_limit),
                ..CompactionThresholds::default()
            },
        ));

        let (connection_supervisor, connection) = Connection::new(
            config.onebot_ws_url.clone(),
            config.onebot_access_token.clone(),
        );

        let webhook = crate::integrations::EventWebhook::from_parts(
            config.event_webhook_url.as_deref(),
            config.event_webhook_secret.clone(),
        );

        let pacing = Arc::new(
            crate::app::pacing::PacingEngine::new(
                Arc::clone(&store),
                Arc::clone(&quota),
                config.bot_id.clone(),
                config.file.hourly_call_budget,
            )
            .with_heavy_groups(config.file.heavy_group_ids.iter().cloned().collect())
            .with_degradation_enabled(
                settings_overlay
                    .quota_degradation_enabled
                    .unwrap_or(config.file.quota_degradation_enabled),
            )
            .with_webhook(webhook.clone()),
        );

        // §5.6: the digested memory view is derived during the pacing gate's wait and
        // handed to the turn that follows. One engine, shared by the intake (which
        // launches it inside the already-existing gate wait) and the executor (which
        // consumes it, or falls back to the raw entry list). The summariser is
        // whichever aux backend the mode selected above.
        let digest = Arc::new(crate::memory::DigestEngine::new(
            Arc::clone(&store),
            config.bot_id.clone(),
            Arc::clone(&summary_model),
            config.file.memory_context_budget,
        ));
        // Created before intake so flush can skip a group whose turn is already
        // eating unclaimed rows via insert.
        // The resident supervisor: the only execution path there is. It owns no
        // process until a group's first turn asks for one, so an idle deployment
        // still starts with zero PTYs.
        let resident = Arc::new(
            crate::app::resident::ResidentManager::new(
                Arc::clone(&store),
                config.bot_id.clone(),
                config.agy_bin.clone(),
                config.agy_home.clone(),
                config.agy_model.clone(),
            )
            .with_deliver_timeout_s(config.file.resident_deliver_timeout_s)
            .with_respawn_backoff_enabled(config.file.respawn_backoff_enabled),
        );

        let running_turns = Arc::new(turn::RunningTurns::new());

        // Group enablement lives in the database, not in the environment: a new group
        // is discovered disabled and an operator switches it on from the panel. There
        // is deliberately no second, static allow-list — two gates for one decision
        // meant a panel toggle could be silently reverted on the next message.
        let intake = Arc::new(
            Intake::new(
                Arc::clone(&store),
                config.bot_id.clone(),
                config.owner_qq_id.clone(),
                IntakeLimits::from_config(&config),
            )
            .with_digest(Arc::clone(&digest))
            .with_pacing(Arc::clone(&pacing))
            .with_aux(Arc::clone(&aux))
            .with_call_names(config.file.bot_call_names.clone())
            .with_resident(Arc::clone(&resident))
            .with_connection(connection.clone())
            .with_running_turns(Arc::clone(&running_turns)),
        );

        // The gate always exists: the aux layer always has a provider
        // (gateway or the agy session), and the gate fails open when the
        // provider errors, whichever one it is.
        let preflight = Some(Arc::new(crate::app::preflight::Preflight::new(Arc::clone(
            &aux,
        ))));

        let resident_limits = turn::ResidentLimits {
            quiet_s: config.file.resident_quiet_s,
            soft_cap_s: config.file.resident_soft_cap_s as i64,
            hard_cap_s: config.file.resident_hard_cap_s as i64,
            max_inserts: config.file.resident_max_inserts,
            poll_ms: config.file.resident_poll_ms,
            deliver_timeout_s: config.file.resident_deliver_timeout_s,
            insert_esc: config.file.resident_insert_esc,
        };

        // Runtime settings: the switches steer live consumers through
        // these shared handles (see runtime_settings.rs).
        let rotation_cap =
            crate::app::turn::conversation_turn_limit(config.file.conversation_turn_limit);
        let rotation_on = settings_overlay
            .session_rotation_enabled
            .unwrap_or(config.file.session_rotation_enabled);
        let rotation_limit =
            crate::app::turn::shared_turn_limit(if rotation_on { rotation_cap } else { None });
        let settings = RuntimeSettings::new(
            settings_path,
            settings_overlay,
            aux_base_config,
            agy_aux_params,
            Arc::clone(&aux_slot),
            Arc::clone(&pacing),
            Arc::clone(&rotation_limit),
            rotation_cap,
            config.file.session_rotation_enabled,
            config.file.quota_degradation_enabled,
        );

        let executor = Arc::new(TurnExecutor {
            running_turns: Arc::clone(&running_turns),
            quote_enabled: config.file.reply_quote_enabled,
            resident,
            resident_limits,
            no_submission_retry_groups: config.file.no_submission_retry_groups.clone(),
            // Shared with the intake so a resident insert decision reads the same tier
            // the claim-time pacing gate used, rather than a second, drifting estimate.
            pacing: Some(Arc::clone(&pacing)),
            webhook,
            // Per-process, per-group annex cache: the previous batch of a resident
            // conversation, so the next turn's model can still point at those
            // messages without their stale `mN` names.
            previous_batch: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            digest: Some(digest.clone()),
            store: Arc::clone(&store),
            connection: connection.clone(),
            outbox: Outbox::new(Arc::clone(&store)),
            memory: MemoryApplier::new(Arc::clone(&store)),
            mihomo: Arc::clone(&mihomo),
            health: Arc::clone(&health),
            bot_id: config.bot_id.clone(),
            owner_qq_id: config.owner_qq_id.clone(),
            model: config.agy_model.clone(),
            timeout: config.agent_timeout(),
            output_channel: config.output_channel,
            conversation_turn_limit: Arc::clone(&rotation_limit),
            context_builder: MemoryContextBuilder::new(config.file.memory_context_budget),
            persona_default: crate::agent::prompt::DEFAULT_PERSONA.to_string(),
            media_fetcher: Arc::new(crate::media::HttpFetcher::new(
                config.media_origins.clone(),
                config.media_max_bytes,
            )),
            media_max_bytes: config.media_max_bytes,
            media_max_pixels: config.media_max_pixels,
            // The image path talks to the same gate, reached through the trait so a
            // test can substitute a canned describer without a network call.
            image_describer: preflight
                .clone()
                .map(|gate| gate as Arc<dyn crate::app::preflight::ImageDescriber>),
            preflight,
            hourly_call_budget: config.file.hourly_call_budget,
            sticker_dir: config.sticker_dir.clone(),
            agy_projects_dir: config.agy_projects_dir.clone(),
            // Loaded once here rather than at send time: a missing file is normal in
            // tests, so it simply contributes nothing.
            secrets: {
                let mut secrets = vec![config.admin_token.clone()];
                if let Ok(key) = std::fs::read_to_string(&config.aux_api_key_file) {
                    secrets.push(key.trim().to_string());
                }
                secrets
                    .into_iter()
                    .filter(|value| !value.is_empty())
                    .collect()
            },
        });

        let scheduler = Arc::new(Scheduler::new(
            config.file.agent_concurrency,
            BatchLimits {
                max_messages: config.file.max_batch_messages,
                max_chars: config.file.max_batch_chars,
                max_images: config.file.max_batch_images,
            },
        ));

        Ok(Self {
            admin_token: config.admin_token.clone(),
            supervisor: tokio::sync::Mutex::new(Some(connection_supervisor)),
            config,
            store,
            health,
            mihomo,
            aux,
            aux_slot,
            summary_model,
            settings,
            rotation_limit,
            quota,
            pacing,
            compactor,
            scheduler,
            connection,
            intake,
            executor,
        })
    }

    /// The aux row of the health snapshot, from whichever backend the
    /// mode selected: probed gateway status, or the agy session's
    /// liveness.
    async fn aux_status_snapshot(&self) -> AuxGatewayStatus {
        self.aux_slot.status_probed().await
    }
    /// Admin router, ready to serve (bearer auth already applied).
    pub fn admin_router(&self) -> Router {
        let state = Arc::new(AdminState {
            store: Arc::clone(&self.store),
            health: Arc::clone(&self.health),
            compactor: Arc::clone(&self.compactor),
            summary_model: Arc::clone(&self.summary_model),
            settings: Arc::clone(&self.settings),
            quota: Arc::clone(&self.quota),
            pacing: Arc::clone(&self.pacing),
            scheduler: Arc::clone(&self.scheduler),
            bot_id: self.config.bot_id.clone(),
            persona_default: crate::agent::prompt::DEFAULT_PERSONA.to_string(),
            conversation_turn_limit: Arc::clone(&self.rotation_limit),
            resident: Some(Arc::clone(&self.executor.resident)),
        });
        let api = match AdminAuth::new(&self.admin_token) {
            Ok(auth) => admin_router(state)
                .layer(axum::middleware::from_fn_with_state(auth, require_bearer)),
            // A weak token cannot be constructed here because config already validated
            // it; if it somehow fails, serve 401 for everything rather than open.
            Err(_) => admin_router(state).layer(axum::middleware::from_fn(
                |req: axum::extract::Request, _next: axum::middleware::Next| async move {
                    let _ = req;
                    (
                        axum::http::StatusCode::UNAUTHORIZED,
                        axum::Json(serde_json::json!({"error": "unauthorized"})),
                    )
                        .into_response()
                },
            )),
        };
        self.mount_console(api)
    }

    /// Serve the built web console under `/console/` when `WEB_DIST`
    /// points at a populated dist directory. The statics are not secret —
    /// every API call still needs the bearer token — so they sit outside
    /// the auth layer. Without a dist, the listener stays API-only.
    fn mount_console(&self, api: Router) -> Router {
        let Some(dir) = &self.config.web_dist else {
            return api;
        };
        if !dir.join("index.html").is_file() {
            return api;
        }
        api.nest_service("/console", tower_http::services::ServeDir::new(dir))
    }

    /// Spawn the OneBot supervisor, the flush loop, health refresh and the admin server.
    pub async fn serve(self: Arc<Self>) -> BotResult<()> {
        // Quota cache warmup: the first scrape takes tens of seconds, so it
        // starts now rather than on the panel's first poll.
        self.quota.warmup();

        // Orphan reaper: as PID 1 the bot inherits every orphaned
        // descendant; without a sweeper they pile up as zombies in the
        // pids cgroup (see process::reaper).
        crate::process::spawn_reaper();

        // Admin HTTP server.
        let listener = TcpListener::bind(&self.config.admin_bind)
            .await
            .map_err(BotError::Io)?;
        let app = self.admin_router();
        let admin = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // Proxy child process.
        let _ = self.mihomo.start().await;

        // OneBot supervisor (reconnects forever). This is the supervisor created in
        // `build`, so the executor's handle and the socket are the same connection.
        let intake = Arc::clone(&self.intake);
        let supervisor = match self.supervisor.lock().await.take() {
            Some(s) => s,
            None => return Err(BotError::Message("onebot supervisor already taken".into())),
        };
        let onebot_health = Arc::clone(&self.health);
        let watched_connection = self.connection.clone();
        tokio::spawn(async move {
            loop {
                let status = watched_connection.status().await;
                let connected = status.connected;
                let _ = onebot_health
                    .set_onebot(OneBotHealth {
                        connected,
                        generation: status.generation,
                        last_frame_at: status.last_frame_at,
                        last_error: status.last_error.clone(),
                        reconnect_attempt: status.reconnect_attempt,
                    })
                    .await;
                // Fast until the socket is up, then the steady tick. A disconnect
                // is noticed on the next 30s sample; the deploy wait only cares
                // about the first transition to connected.
                let pause = if connected {
                    HEALTH_INTERVAL
                } else {
                    HEALTH_CONNECT_INTERVAL
                };
                tokio::time::sleep(pause).await;
            }
        });
        tokio::spawn(async move {
            supervisor.run(intake).await;
        });

        // Flush loop: claim batches and hand them to per-group lanes.
        let flush_self = Arc::clone(&self);
        tokio::spawn(async move {
            // Groups whose halted lane has already left its audit row in this
            // halt episode; re-armed when the lane accepts work again.
            let mut halted_reported: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            loop {
                tokio::time::sleep(TICK_INTERVAL).await;
                let jobs = flush_self
                    .intake
                    .flush_tick(crate::now_unix(), flush_self.executor.preflight.as_deref())
                    .await;
                for job in jobs {
                    let executor = Arc::clone(&flush_self.executor);
                    let group_id = job.group_id.clone();
                    let accepted = flush_self.scheduler.enqueue(executor, job).await;
                    if accepted {
                        halted_reported.remove(&group_id);
                    } else if halted_reported.insert(group_id.clone()) {
                        // A halted lane accepts nothing and says nothing on its
                        // own: one row per halt episode, so the trail shows why
                        // the group went quiet (2026-10-28, group 900000003
                        // queued for an hour behind a lane nobody could see).
                        let _ = flush_self
                            .store
                            .audit(
                                "system",
                                &format!("scheduler.lane.halted.g{group_id}"),
                                Some(&flush_self.config.bot_id),
                                Some(&group_id),
                                crate::now_unix(),
                            )
                            .await;
                    }
                }
                // Resident housekeeping: pause reclaim, watchdog ticks and the
                // audit drain. A no-op when no group has a live session.
                let sweep_now = crate::now_unix();
                flush_self.executor.resident_sweep(sweep_now).await;
                // Canary: at most one internal turn per group per local day, only
                // inside the 04:00–05:00 window and only when nothing else is in
                // flight. Its output never reaches the outbox or the group log.
                if let Ok(groups) = flush_self
                    .store
                    .list_groups(&flush_self.config.bot_id)
                    .await
                {
                    for group in groups {
                        // Eligibility, not the raw flag: the canary is a real turn
                        // against the account, so a group waiting out a respawn
                        // backoff must not spend one on a session it will not keep.
                        if group.enabled
                            && !group.paused
                            && flush_self
                                .executor
                                .resident_eligible(&group, sweep_now)
                                .await
                        {
                            // Warm pool: a resident group with no session
                            // gets one spawned off-loop, so its next batch
                            // does not pay the PTY boot first.
                            let _ = flush_self
                                .executor
                                .resident_prewarm(&group, sweep_now)
                                .await;
                            flush_self
                                .executor
                                .resident_canary(&group.group_id, sweep_now)
                                .await;
                        }
                    }
                }
            }
        });

        // Periodic health refresh for proxy and Aux.
        let health_self = Arc::clone(&self);
        tokio::spawn(async move {
            loop {
                let _ = health_self.mihomo.start().await;
                // Independent probes. Sequential, the slower one (Aux's
                // 10s budget) held the proxy result out of the snapshot.
                let (proxy, status) = tokio::join!(
                    health_self.mihomo.probe(),
                    health_self.aux_status_snapshot(),
                );
                health_self.health.set_proxy(proxy).await;
                health_self.health.set_aux(status).await;
                tokio::time::sleep(HEALTH_INTERVAL).await;
            }
        });

        // L3 memory compaction is panel-only: the derived summary is not injected into
        // the agent prompt, so an automatic loop would spend Aux tokens for a
        // panel page nobody is looking at.

        // Background log folding: keep the raw log bounded by summarising its oldest
        // tail into `memory_chunks`. Runs before memory compaction so a fold never races
        // the summary that reads `memories`. Never deletes rows.
        let chunk_self = Arc::clone(&self);
        tokio::spawn(async move {
            let consolidator =
                crate::memory::consolidate::Consolidator::new(Arc::clone(&chunk_self.store));
            loop {
                tokio::time::sleep(FOLD_INTERVAL).await;
                // Rebuilt each round so a hot-swapped aux provider's origin
                // and model label apply from the next fold on.
                let chunker = crate::memory::chunker::Chunker::new(Arc::clone(&chunk_self.store))
                    .with_origin(chunk_self.aux_slot.origin())
                    .with_model_label(chunk_self.aux_slot.model_label());
                if !chunk_self.aux.healthy().await {
                    continue;
                }
                let groups = chunk_self
                    .store
                    .list_groups(&chunk_self.config.bot_id)
                    .await
                    .unwrap_or_default();
                for group in groups {
                    let aliases: Vec<(String, String)> = chunk_self
                        .store
                        .list_speaker_aliases(&chunk_self.config.bot_id, &group.group_id)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(user_id, alias)| (alias, user_id))
                        .collect();
                    let model = Arc::clone(&chunk_self.summary_model);
                    let fold_result = chunker
                        .fold_once(
                            &chunk_self.config.bot_id,
                            &group.group_id,
                            &model,
                            aliases.clone(),
                            crate::now_unix(),
                        )
                        .await;
                    audit_fold(
                        &chunk_self.store,
                        &chunk_self.config.bot_id,
                        &group.group_id,
                        &fold_result,
                        crate::now_unix(),
                    )
                    .await;
                    // Rows already folded into a chunk keep no extra value, so the raw
                    // log is trimmed back to a bounded archive after each pass. The
                    // unarchived tail is untouched: it is what the cadence gate reads.
                    let _ = chunk_self
                        .store
                        .trim_archived_messages(
                            &chunk_self.config.bot_id,
                            &group.group_id,
                            ARCHIVED_MESSAGES_KEEP,
                            crate::now_unix(),
                        )
                        .await;
                    // Second-pass memory consolidation (§5.6 forgetting): fold a subject
                    // cluster back down once a group carries enough active rows. Placed
                    // after the fold so a merge never races the extraction that the fold
                    // just performed, and after the trim so a group that is behind on
                    // folding gets its log handled first.
                    let consolidation = consolidator
                        .run_once(
                            &chunk_self.config.bot_id,
                            &group.group_id,
                            model.as_ref(),
                            &aliases,
                            crate::now_unix(),
                        )
                        .await;
                    audit_consolidation(
                        &chunk_self.store,
                        &chunk_self.config.bot_id,
                        &group.group_id,
                        &consolidation,
                        crate::now_unix(),
                    )
                    .await;
                    // Rolling handover note: keeps the live thread (open
                    // questions, promises, running bits) written down outside
                    // any session, so the 35-turn rotation and crash rebuilds
                    // hand over seamlessly. Skips itself when the group has
                    // not moved since the stored note.
                    let handover_outcome = crate::memory::handover::refresh_handover(
                        &chunk_self.store,
                        &chunk_self.config.bot_id,
                        &group.group_id,
                        &model,
                        "hy3",
                        crate::now_unix(),
                    )
                    .await;
                    let _ = chunk_self
                        .store
                        .audit(
                            crate::store::origin::audit_actor(crate::store::origin::HY3),
                            &format!("memory.handover.{handover_outcome}.g{}", group.group_id),
                            Some(&chunk_self.config.bot_id),
                            Some(&group.group_id),
                            crate::now_unix(),
                        )
                        .await;
                }
            }
        });

        // Proactive daytime chat scheduler: between 09:00 and 23:00 (UTC+8), randomly
        // every 1–3 hours when a group is quiet, trigger an autonomous topic / Bilibili
        // share turn. Pauses after 2 consecutive unanswered proactive turns until a
        // group member speaks again.
        let proactive_self = Arc::clone(&self);
        tokio::spawn(async move {
            struct ProactiveGroupState {
                next_due_at: i64,
                unanswered_count: u32,
                last_seen_user_msg_id: i64,
            }
            let mut states: std::collections::HashMap<String, ProactiveGroupState> =
                std::collections::HashMap::new();
            let mut seed_counter: u64 = 1;
            let mut random_delay = || -> i64 {
                seed_counter = seed_counter.wrapping_add(1);
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                let mixed = (nanos ^ seed_counter.wrapping_mul(0x9E37_79B9_7F4A_7C15))
                    .wrapping_mul(0xBF58_476D_1CE4_E5B9);
                3600 + (mixed % 7201) as i64
            };

            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let now = crate::now_unix();
                let bj_hour = ((now + 8 * 3600).rem_euclid(86400)) / 3600;
                if !(9..23).contains(&bj_hour) {
                    continue;
                }
                let groups = proactive_self
                    .store
                    .list_groups(&proactive_self.config.bot_id)
                    .await
                    .unwrap_or_default();
                for group in groups {
                    if !group.enabled || group.paused {
                        continue;
                    }
                    let recent = proactive_self
                        .store
                        .recent_group_messages(&proactive_self.config.bot_id, &group.group_id, 20)
                        .await
                        .unwrap_or_default();
                    let last_user_msg = recent.iter().rev().find(|m| m.speaker_role != "bot");
                    let last_user_id = last_user_msg.map(|m| m.id).unwrap_or(0);
                    let last_user_ts = last_user_msg.map(|m| m.created_at).unwrap_or(now);

                    let delay = random_delay();
                    let state = states.entry(group.group_id.clone()).or_insert_with(|| {
                        ProactiveGroupState {
                            next_due_at: now + delay,
                            unanswered_count: 0,
                            last_seen_user_msg_id: last_user_id,
                        }
                    });

                    if last_user_id > state.last_seen_user_msg_id {
                        state.last_seen_user_msg_id = last_user_id;
                        state.unanswered_count = 0;
                        let next_candidate = last_user_ts + delay;
                        if state.next_due_at < next_candidate {
                            state.next_due_at = next_candidate;
                        }
                    }

                    if state.unanswered_count >= 2 || now < state.next_due_at {
                        continue;
                    }

                    if proactive_self
                        .executor
                        .running_turns
                        .snapshot()
                        .iter()
                        .any(|(g, _)| g == &group.group_id)
                    {
                        continue;
                    }
                    let has_pending = proactive_self
                        .store
                        .pending_count(&proactive_self.config.bot_id, &group.group_id)
                        .await
                        .map(|n| n > 0)
                        .unwrap_or(true);
                    if has_pending {
                        continue;
                    }

                    let speaker_ref = proactive_self
                        .store
                        .ensure_member(
                            &proactive_self.config.bot_id,
                            &group.group_id,
                            &proactive_self.config.owner_qq_id,
                            now,
                        )
                        .await
                        .unwrap_or_else(|_| "u1".to_string());
                    let msg_id = format!("proactive-{}-{now}", group.group_id);
                    let text = "【系统内部主动话题触发（群友不可见，切勿引用m1，切勿提起系统或主人）】现在是白天闲聊时间，群里已经安静有一会儿了。请你用温和友善、治愈贴心的语气主动在群里开启一个自然轻松的话题，关心一下大家，或者先用 call_mcp_tool 调用 bilibili_browse(mode=\"popular\") 或 bilibili_browse(mode=\"comments\") 刷一下B站热门视频和评论区，顺手把有趣的表情包收进你的表情库并用 sticker_ops 分享给群友；如果很有感触也可以顺手调 qzone_publish 发条温暖的日常空间说说。注意：直接对群里大家说话，不要叫主人，quote_ref 必须留空！".to_string();
                    let synthetic = crate::onebot::normalize::IncomingMessage {
                        bot_id: proactive_self.config.bot_id.clone(),
                        group_id: group.group_id.clone(),
                        message_id: msg_id.clone(),
                        sender_id: proactive_self.config.owner_qq_id.clone(),
                        speaker_ref,
                        speaker_role: crate::onebot::normalize::SpeakerRole::Owner,
                        display_name: "系统定时触发".to_string(),
                        known_alias: None,
                        received_at: now,
                        segments: vec![crate::onebot::normalize::Segment::Text {
                            text: text.clone(),
                        }],
                        text,
                        mentions_bot: true,
                        quote_of: None,
                    };
                    if let Ok(payload) = serde_json::to_string(&synthetic) {
                        if let Ok(true) = proactive_self
                            .store
                            .insert_pending(
                                &proactive_self.config.bot_id,
                                &group.group_id,
                                &msg_id,
                                &proactive_self.config.owner_qq_id,
                                now,
                                &payload,
                                "[]",
                            )
                            .await
                        {
                            state.unanswered_count += 1;
                            state.next_due_at = now + random_delay();
                            let _ = proactive_self
                                .store
                                .audit(
                                    "system",
                                    &format!(
                                        "proactive.chat.triggered.g{}.try{}",
                                        group.group_id, state.unanswered_count
                                    ),
                                    Some(&proactive_self.config.bot_id),
                                    Some(&group.group_id),
                                    now,
                                )
                                .await;
                        }
                    }
                }
            }
        });

        admin
            .await
            .map_err(|e| BotError::Message(format!("admin server join: {e}")))?;
        Ok(())
    }
}

use axum::Router;
use axum::response::IntoResponse;

/// How often the raw-log folder runs.
const FOLD_INTERVAL: Duration = Duration::from_secs(600);

/// Record one group's fold attempt in the audit log.
///
/// A success and a skip/failure each leave exactly one row per cycle: without
/// this, a group stuck retrying the same window every [`FOLD_INTERVAL`] was
/// invisible (the loop used to discard the result). `Ok(None)` means the tail
/// is below the trigger — nothing was attempted, so nothing is recorded.
async fn audit_fold(
    store: &SqliteStore,
    bot_id: &str,
    group_id: &str,
    result: &Result<Option<FoldOutcome>, FoldSkip>,
    now: i64,
) {
    let action = match result {
        Ok(Some(outcome)) => {
            // Extraction is a side branch of the fold, so it gets its own row instead of
            // bloating the fold action string: a fold that succeeded while extraction
            // found nothing must still look like a plain success.
            if outcome.extracted.created > 0
                || outcome.extracted.duplicates > 0
                || outcome.extracted.rejected > 0
            {
                let extract_action =
                    crate::memory::extract::audit_action(group_id, &outcome.extracted);
                let _ = store
                    .audit(
                        crate::store::origin::audit_actor(crate::store::origin::HY3),
                        &extract_action,
                        Some(bot_id),
                        Some(group_id),
                        now,
                    )
                    .await;
            } else if outcome.extracted.status
                != Some(crate::memory::extract::CandidateParseStatus::Parsed)
            {
                // A reply without a usable candidate block is not worth an audit row on
                // every cycle (the fold row already records the success), but a model
                // that stopped emitting the block must be visible somewhere.
                tracing::info!(
                    group_id,
                    status = outcome
                        .extracted
                        .status
                        .map(|s| s.as_str())
                        .unwrap_or("none"),
                    "记忆提取跳过：候选段缺失或损坏"
                );
            }
            format!("memory.fold.ok.g{group_id}.n{}", outcome.folded_messages)
        }
        Ok(None) => return,
        Err(skip) => format!("memory.fold.skip.g{group_id}.{}", skip.audit_tag()),
    };
    let _ = store
        .audit("system", &action, Some(bot_id), Some(group_id), now)
        .await;
}

/// How many already-folded rows stay in the raw log as an archive. Their content is
/// already inside a chunk summary, so keeping the originals is only for debugging.
const ARCHIVED_MESSAGES_KEEP: i64 = 1_000;

/// Record one group's consolidation attempt in the audit log.
///
/// Only rounds that could have done something leave a row: below the threshold and
/// "nothing to merge" are the steady state and would otherwise fill the log with one row
/// per group per cycle. A failure or a malformed reply does get a row — that is the case
/// an operator needs to see, because it silently means "nothing was forgotten".
async fn audit_consolidation(
    store: &SqliteStore,
    bot_id: &str,
    group_id: &str,
    report: &crate::memory::consolidate::ConsolidationReport,
    now: i64,
) {
    use crate::memory::consolidate::ConsolidationStatus;
    let worth_recording = matches!(
        report.status,
        Some(ConsolidationStatus::Applied)
            | Some(ConsolidationStatus::Failed)
            | Some(ConsolidationStatus::Malformed)
    );
    if !worth_recording {
        return;
    }
    let _ = store
        .audit(
            crate::store::origin::audit_actor(crate::store::origin::HY3),
            &report.audit_action(group_id),
            Some(bot_id),
            Some(group_id),
            now,
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_recovers_interrupted_state_and_reports_blockers() {
        let dir = tempfile::tempdir().expect("tmp");
        let file = crate::config::FileConfig {
            bot_id: "9999".into(),
            ..crate::config::FileConfig::default()
        };
        let config = crate::config::Config::build_from_parts(file, |key| match key {
            "ONEBOT_WS_URL" => Some("ws://127.0.0.1:1/".into()),
            "OWNER_QQ_ID" => Some("10001".into()),
            "ADMIN_TOKEN" => Some("0123456789abcdef0123456789abcdef".into()),
            "GEMINI_DB_PATH" => Some(dir.path().join("bot.sqlite3").display().to_string()),
            "MIHOMO_ENABLED" => Some("0".into()),
            _ => None,
        })
        .expect("配置应可用");

        let app = App::build(config).await.expect("应启动");
        let snap = app.health.snapshot().await;
        // Blockers are reported, not hidden. The Agy contracts are now verified against
        // the real CLI, so what remains is the live state this test never establishes.
        assert!(!snap.production_ready);
        assert!(snap.blockers.iter().any(|b| b.contains("OneBot")));
        // Dual-mode contract: with no gateway configured the aux layer is
        // served by the in-process Agy session, so "no Aux" is never a
        // blocker. (The pre-dual-mode assertion expected the disabled
        // gateway itself to block.)
        assert!(!snap.blockers.iter().any(|b| b.contains("Aux")));
        // Owner presence is reported without the value.
        assert!(snap.owner.configured);
    }

    async fn fold_store() -> (SqliteStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("bot.sqlite3");
        let pool = crate::store::migrations::open_and_migrate(&path)
            .await
            .expect("迁移");
        (SqliteStore::new(pool), dir)
    }

    #[tokio::test]
    async fn fold_ok_leaves_one_audit_row_with_count() {
        let (store, _dir) = fold_store().await;
        let outcome = FoldOutcome {
            chunk_id: "chunk_500_1".into(),
            seq: 1,
            folded_messages: 200,
            summary: "摘要".into(),
            extracted: crate::memory::extract::ExtractionReport::default(),
        };
        audit_fold(&store, "9999", "500", &Ok(Some(outcome)), 1_000).await;
        let rows = store.recent_audit(10).await.expect("审计");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "memory.fold.ok.g500.n200");
        assert_eq!(rows[0].actor_kind, "system");
        assert_eq!(rows[0].group_id.as_deref(), Some("500"));
    }

    #[tokio::test]
    async fn fold_skip_leaves_reasoned_row_and_below_trigger_leaves_none() {
        let (store, _dir) = fold_store().await;
        audit_fold(&store, "9999", "500", &Err(FoldSkip::ModelError), 1_000).await;
        audit_fold(
            &store,
            "9999",
            "500",
            &Err(FoldSkip::BadSummary("parse")),
            1_001,
        )
        .await;
        audit_fold(&store, "9999", "500", &Ok(None), 1_002).await;
        let rows = store.recent_audit(10).await.expect("审计");
        assert_eq!(rows.len(), 2);
        let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
        assert!(actions.contains(&"memory.fold.skip.g500.model"));
        assert!(actions.contains(&"memory.fold.skip.g500.bad_summary.parse"));
    }
}
