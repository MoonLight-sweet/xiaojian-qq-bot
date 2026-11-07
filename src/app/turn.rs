//! The turn executor: one batch, end to end.
//!
//! This is the only place where the pieces meet, so the ordering of side effects is the
//! design:
//!
//! 1. re-check the group is still enabled and unpaused (it may have been toggled while
//!    the batch was queueing),
//! 2. check the proxy gate — if it is not ready the batch stays pending and the lane
//!    defers, it is never dropped,
//! 3. build the memory context from this group only,
//! 4. deliver the prompt into the group's resident session and read the answer back
//!    from its step stream,
//! 5. on a session failure, mark the call `unknown`/failed, close the session and let
//!    the respawn backoff decide when the group comes back; there is no other lane,
//! 6. validate the agent JSON, apply the accepted memory ops, persist the reply in the
//!    outbox, then delete the pending rows *after* the outbox write,
//! 7. deliver from the outbox; an ambiguous send becomes `unknown` and is never retried
//!    automatically.

use std::sync::Arc;

use crate::agent::barrier::{TurnSpec, Verdict, list_drafts};
use crate::agent::contract::{self, ValidationScope};
use crate::agent::prompt::{PromptBatch, PromptImages, PromptMessage, render};
use crate::agent::salvage::{self, SalvageLayer};
use crate::app::insert::{self, InsertLimits, InsertSignals};
use crate::app::pacing::Tier;
use crate::app::preflight::Preflight;
use crate::app::probes::{ProbeAction, RoundOutcome};
use crate::error::ErrorClass;
use crate::health::HealthRegistry;
use crate::media::{ImageFetcher, SandboxWorkspace};
use crate::memory::retrieval::{MemoryContextBuilder, RetrievalInput};
use crate::onebot::connection::ConnectionHandle;
use crate::onebot::normalize::{IncomingMessage, Segment, SpeakerRole};
use crate::pipeline::outbox::{DeliveryResult, Outbox};
use crate::pipeline::scheduler::{BatchExecutor, BatchJob, ExecOutcome};
use crate::process::mihomo::Mihomo;
use crate::state::memory::MemoryApplier;
use crate::state::mood::{MoodState, TopicState};
use crate::store::{CallState, SqliteStore};

/// Everything the executor needs. Constructed once at startup.
pub struct TurnExecutor {
    pub store: Arc<SqliteStore>,
    pub connection: ConnectionHandle,
    pub outbox: Outbox,
    pub memory: MemoryApplier,
    pub mihomo: Arc<Mihomo>,
    pub health: Arc<HealthRegistry>,
    pub bot_id: String,
    pub owner_qq_id: String,
    pub model: String,
    /// Wall-clock basis for the MCP `arm_turn` TTL: an armed turn must outlive the
    /// turn it belongs to, so this is its floor (see the `arm_turn` call site).
    pub timeout: std::time::Duration,
    /// Output channel for agent turns (MCP `submit_reply` vs final text).
    pub output_channel: crate::config::OutputChannel,
    /// Effective Agy conversation cap as a shared slot (-1 = unlimited),
    /// so the settings API can flip rotation live. An unlimited cap keeps
    /// the session forever: Agy compacts its own transcript, so recycling
    /// is an opt-in safety net.
    pub conversation_turn_limit: Arc<std::sync::atomic::AtomicI64>,
    /// Optional outbound event webhook (turn outcomes are reported).
    pub webhook: Option<Arc<crate::integrations::EventWebhook>>,
    pub context_builder: MemoryContextBuilder,
    pub persona_default: String,
    pub media_fetcher: Arc<dyn ImageFetcher>,
    pub media_max_bytes: u64,
    pub media_max_pixels: u64,
    /// The cheap gate in front of the agent. `None` disables it entirely, which is
    /// how tests that do not care about screening run.
    ///
    /// The image path only asks it one thing (`describe`), through
    /// [`crate::app::preflight::ImageDescriber`], so that path can be exercised without
    /// a network call; the pacing side still needs the concrete type for its scores.
    pub preflight: Option<Arc<Preflight>>,
    /// What describes pictures for the agent.
    ///
    /// In production this is the same `Preflight` as above, reached through the
    /// [`crate::app::preflight::ImageDescriber`] trait so the image path can be driven
    /// without a network call. `None` means "no describer": pictures are then simply
    /// unavailable to the prompt, which is the old disabled-gate behaviour.
    pub image_describer: Option<Arc<dyn crate::app::preflight::ImageDescriber>>,
    /// Per-group cap on agent calls started per rolling hour; 0 disables it.
    ///
    /// The same fuse the intake checks before it claims a batch, re-checked here at
    /// the point where the call actually starts. Claiming and starting are not the
    /// same instant: a batch can queue behind others and be re-claimed after a
    /// deferral, and the count the claim saw says nothing about the calls started
    /// since.
    pub hourly_call_budget: u32,
    /// Where sticker bytes are written. Empty in tests that never touch the library.
    pub sticker_dir: std::path::PathBuf,
    /// Long-lived root for per-group Agy project directories.
    ///
    /// Each group gets `<agy_projects_dir>/<group_id>` holding its `AGENTS.md`. Agy reads
    /// that file from its own working directory on every run, so the card persists across
    /// turns instead of being rewritten into a throw-away directory each time.
    pub agy_projects_dir: std::path::PathBuf,
    /// Exact credential values loaded at startup. The last gate before anything is
    /// queued for delivery masks these, because a turn that read a file it should not
    /// have would otherwise leak the value straight into a chat message.
    pub secrets: Vec<String>,
    /// In-flight agent calls, keyed by group. The resident canary reads it to avoid
    /// talking over a turn that is already using the session.
    pub running_turns: Arc<RunningTurns>,
    /// The summary model's digested view of the memory context (§5.6). `None` when
    /// Aux is disabled: every turn then injects the raw entry list, exactly as
    /// before the feature existed.
    pub digest: Option<Arc<crate::memory::DigestEngine>>,
    /// Kill switch for outbound quotes (config `reply_quote_enabled`). When
    /// off, a validated `quote_ref` is dropped (audited) and replies go out
    /// as plain text, exactly as before the feature existed.
    pub quote_enabled: bool,
    /// Groups opted into the no-submission replay (config
    /// `no_submission_retry_groups`); empty disables the replay.
    pub no_submission_retry_groups: Vec<String>,
    /// P4 resident sessions (one PTY-backed Agy TUI per group). Always present: the
    /// resident session is the only execution path, so a deployment without one does
    /// not run at all. A group whose session cannot start stays silent (audited) and
    /// waits for its respawn backoff — there is no fallback lane behind it.
    pub resident: Arc<crate::app::resident::ResidentManager>,
    /// P4 resident knobs, copied out of `bot.toml` at construction time.
    pub resident_limits: ResidentLimits,
    /// The same pacing engine the intake uses. The resident path re-reads the tier
    /// per insert decision, because an insert is a policy decision made *after* the
    /// batch was claimed, while the claim-time gate has already run.
    pub pacing: Option<Arc<crate::app::pacing::PacingEngine>>,
    /// The previous batch per group, for the continuation turn's annex.
    ///
    /// Keyed by group, holding the conversation the messages were delivered
    /// into (see [`PreviousBatch`]): a turn only uses an entry whose
    /// conversation id still matches its own, so a rotation or a retire
    /// invalidates the numbering without any explicit purge.
    pub previous_batch: Arc<tokio::sync::Mutex<std::collections::HashMap<String, PreviousBatch>>>,
}

/// A resident conversation whose step stream grows past this is rotated
/// even if its turn count has not reached the cap: step depth, not turn
/// count, is what tracks generation latency and session decay.
/// Deep sessions were still answering at step 50–65 with either
/// `contract_invalid.empty_reply` or a 27-byte `{"should_reply": false}`
/// that the turn recorded as success and then deleted. 180 never fired.
/// 40 is past a normal conversation and before that decay.
const RESIDENT_STEP_ROTATE_LIMIT: i64 = 40;

/// Tuning for the resident path. Defaults match the brief and the `FileConfig`
/// defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResidentLimits {
    /// How long the step stream must be quiet before a turn may deliver.
    pub quiet_s: f64,
    /// After this turn age no new inserts are accepted (soft cap).
    pub soft_cap_s: i64,
    /// At this turn age the turn is interrupted and closed out as a failure.
    pub hard_cap_s: i64,
    /// Upper bound on how many batches may be inserted into one turn.
    pub max_inserts: u32,
    /// Barrier re-parse cadence for an in-flight turn.
    pub poll_ms: u64,
    /// How long a delivered prompt may take to show up as a user step.
    pub deliver_timeout_s: u64,
    /// Insert delivery mode: `true` stops the in-flight generation with
    /// ESC before pasting an accepted insert (see `FileConfig::
    /// resident_insert_esc`); `false` keeps the idle-screen paste.
    pub insert_esc: bool,
}

impl Default for ResidentLimits {
    fn default() -> Self {
        Self {
            quiet_s: 5.0,
            soft_cap_s: 45,
            hard_cap_s: 90,
            max_inserts: 3,
            poll_ms: 500,
            deliver_timeout_s: 15,
            insert_esc: false,
        }
    }
}

/// Waiting rows at or past this count mean the group is outrunning the
/// turn pipeline: batches queue up faster than turns finish.
pub const INSERT_BOOST_PENDING: usize = 10;
/// Insert seats added under boost (see [`boosted_limits`]).
pub const INSERT_BOOST_MAX_INSERTS: u32 = 6;
/// Soft cap under boost, seconds.
pub const INSERT_BOOST_SOFT_CAP_S: i64 = 60;

/// Widen one turn's insert budget when a backlog is waiting behind it.
///
/// An insert rides a call that is already paid for; a new batch does not.
/// When a group is hot, letting the in-flight turn absorb more of the
/// waiting rows is strictly cheaper than ending the turn and claiming the
/// same rows as fresh batches — that is the throughput half of the flood
/// fix. Only the insert side widens: handing batches over earlier would
/// start *more* turns and burn the hourly budget faster, which is the
/// opposite of the intent. Configured limits above the boost values win
/// (the boost is a floor, never a cap).
pub fn boosted_limits(base: ResidentLimits, waiting: usize) -> ResidentLimits {
    if waiting < INSERT_BOOST_PENDING {
        return base;
    }
    ResidentLimits {
        max_inserts: base.max_inserts.max(INSERT_BOOST_MAX_INSERTS),
        soft_cap_s: base.soft_cap_s.max(INSERT_BOOST_SOFT_CAP_S),
        ..base
    }
}

/// Whether a barrier deliverable only passed validation through the
/// salvage ladder: its bytes needed repair (layer other than `None`), so
/// they may be a half-written step rather than the model's final text.
pub fn delivery_was_salvaged(text: &str, scope: &ValidationScope) -> bool {
    contract::validate_salvaged(text, scope)
        .map(|(_, report)| report.layer != SalvageLayer::None)
        .unwrap_or(false)
}

/// Stability tracker for salvaged deliveries. The same step text seen on
/// two successive barrier evaluations is no longer growing; a text that
/// changed — or a step never seen before — is not stable yet. Records the
/// current sighting either way and reports whether it was already stable.
pub fn salvage_text_stable(seen: &mut Option<(i64, String)>, idx: i64, text: &str) -> bool {
    let stable = seen
        .as_ref()
        .is_some_and(|(prev_idx, prev_text)| *prev_idx == idx && prev_text == text);
    *seen = Some((idx, text.to_string()));
    stable
}

/// How a finished round ended, computed once from its final text.
///
/// The same two facts answer three different questions — whether an addressed
/// turn is allowed to stop here ([`Self::addressed_silence`]), whether the round
/// is evidence of health ([`Self::degraded`]) — so they are classified in one
/// place instead of being re-derived at each call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundShape {
    /// The text only validated through the salvage ladder: its bytes needed
    /// repair, so they may be a fragment rather than the model's final answer.
    pub salvaged: bool,
    /// The contract chose not to answer (`should_reply: false`, or nothing left
    /// to send). Silence is a legitimate outcome for an ambient batch.
    pub silent: bool,
}

impl RoundShape {
    pub fn of(text: &str, scope: &ValidationScope) -> Self {
        Self {
            salvaged: delivery_was_salvaged(text, scope),
            silent: delivered_text_is_silence(text, scope),
        }
    }

    /// A round that only held together because of the repair ladder, or that
    /// ended on a silence, is not proof the session is healthy.
    pub fn degraded(&self) -> bool {
        self.salvaged || self.silent
    }

    /// Whether this round owes an answer it did not give.
    ///
    /// Only an addressed round has to speak: a batch nobody aimed at the bot
    /// may be declined freely, and a repair of a declining answer is still a
    /// decline. Applied before the result is accepted.
    pub fn addressed_silence(&self, privileged: bool) -> bool {
        privileged && self.silent
    }
}

impl std::fmt::Debug for TurnExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnExecutor")
            .field("bot_id", &self.bot_id)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// How many successful turns one Agy conversation may serve before it is recycled.
///
/// Agy compacts its own transcript, so this is off by default (`0`): a session can live
/// indefinitely and long-term facts still live in `memories`. Operators who want a
/// predictable reset point raise `conversation_turn_limit` in `bot.toml`.
pub const CONVERSATION_TURN_LIMIT: i64 = 0;

/// Resolve the effective cap. `0` and negatives both mean "off", so a typo can never
/// silently start recycling a session the operator wanted to keep.
pub fn conversation_turn_limit(configured: i64) -> Option<i64> {
    (configured > 0).then_some(configured)
}

/// The cap as a shared, hot-swappable slot: -1 means unlimited. Readers
/// resolve it back with [`turn_limit_value`].
pub fn shared_turn_limit(limit: Option<i64>) -> Arc<std::sync::atomic::AtomicI64> {
    Arc::new(std::sync::atomic::AtomicI64::new(limit.unwrap_or(-1)))
}

/// Resolve a shared cap slot back to the plain form.
pub fn turn_limit_value(slot: &std::sync::atomic::AtomicI64) -> Option<i64> {
    let raw = slot.load(std::sync::atomic::Ordering::Relaxed);
    (raw >= 0).then_some(raw)
}

/// Pure gate decision, boundary-tested: drop only when strictly over.
/// Whether a completed logical round exhausts the conversation's turn budget.
///
/// Counted per delivered round, not per PTY round-trip: a resident turn that
/// absorbed three inserts is still one conversation turn.
pub fn rotation_due(count: i64, limit: Option<i64>) -> bool {
    limit.is_some_and(|cap| count >= cap)
}

/// First miss of any turn: nudge instead of retiring. A continuation
/// used to skip the reminder and die in `quiet_s` (~6s) with
/// `no_submission.calls0` after the model wrote agent text but never
/// called `submit_reply` (2026-10-29). The second miss still retires.
fn submission_nudge_due(already_nudged: bool) -> bool {
    !already_nudged
}

/// A no-submission death arms the one-shot replay only for an opted-in
/// group, and never for a job that is itself the replay: a second death
/// falls back to the plain release-and-merge behaviour (2026-10-29).
fn retry_arm_due(job_is_retry: bool, group_opted_in: bool) -> bool {
    !job_is_retry && group_opted_in
}

fn first_turn_nudge_prompt(channel: crate::config::OutputChannel) -> &'static str {
    if channel.is_mcp() {
        "本轮还没有调用 submit_reply。请立即调用 submit_reply 提交本轮结果，turn_token 照抄契约卡。"
    } else {
        "本轮还没有按契约给出 JSON。请立即只回复一个契约 JSON 对象。"
    }
}

/// When the turn started and when the agent call started.
///
/// Grouped into one value so the finish path keeps a readable signature: the two
/// instants always travel together and both exist only to produce the timing log.
#[derive(Debug, Clone, Copy)]
pub struct TurnTiming {
    /// Start of the turn (before preparation).
    pub turn_started: std::time::Instant,
    /// Start of the agent call (after preparation).
    pub agent_started: std::time::Instant,
}

pub trait FreshnessCoverage: Send + Sync {
    /// The newest message timestamp among everything this turn speaks for.
    fn newest(&self, messages: &[IncomingMessage]) -> Option<i64>;

    /// The oldest batch that arrived after `newest` and was never delivered,
    /// as `(batch_id, newest_received_at)`. `None` means nothing is uncovered.
    fn uncovered(&self, newest: i64) -> Option<(String, i64)>;

    /// Every pending row this turn consumes on success.
    fn event_ids(&self, job: &BatchJob) -> Vec<i64>;

    /// Audit tag naming the coverage kind in a drop's action.
    fn label(&self) -> &'static str;
}

/// One batch delivered into a resident turn: what it covered and when it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentDelivery {
    pub batch_id: String,
    pub event_ids: Vec<i64>,
    /// Newest `received_at` across this batch — the instant its newest message
    /// entered the group, which is what the send gate ages.
    pub newest: i64,
    /// Whether this batch was claimed with priority — someone addressed the bot
    /// (a mention, a poke, the operator speaking). A turn that covers such a
    /// batch owes an answer, so it may not end on a silence.
    pub privileged: bool,
}

/// Coverage for a resident turn built from every batch it delivered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResidentCoverage {
    pub batches: Vec<ResidentDelivery>,
}

impl ResidentCoverage {
    /// Every event id the turn consumed, deduplicated, for the single
    /// `delete_pending` that follows a successful send.
    pub fn event_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .batches
            .iter()
            .flat_map(|b| b.event_ids.iter().copied())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub fn batch_ids(&self) -> Vec<String> {
        self.batches.iter().map(|b| b.batch_id.clone()).collect()
    }

    /// Whether any batch this turn delivered was addressed to the bot.
    ///
    /// The flag rides on the batch, not on the turn, so one check covers both
    /// shapes of an addressed round: a mention that claimed its own batch, and
    /// an insert absorbed mid-turn from a mention that arrived while the model
    /// was already answering.
    pub fn any_privileged(&self) -> bool {
        self.batches.iter().any(|b| b.privileged)
    }
}

impl FreshnessCoverage for ResidentCoverage {
    fn newest(&self, messages: &[IncomingMessage]) -> Option<i64> {
        // The union: the reply speaks for every delivered batch, so its age is
        // the newest of all of them, not just the first one's messages.
        self.batches
            .iter()
            .map(|b| b.newest)
            .max()
            .or_else(|| messages.iter().map(|m| m.received_at).max())
    }

    fn uncovered(&self, newest: i64) -> Option<(String, i64)> {
        self.batches
            .iter()
            .filter(|b| b.newest > newest)
            .min_by_key(|b| b.newest)
            .map(|b| (b.batch_id.clone(), b.newest))
    }

    fn event_ids(&self, job: &BatchJob) -> Vec<i64> {
        if self.batches.is_empty() {
            return job.event_ids.clone();
        }
        self.event_ids()
    }

    fn label(&self) -> &'static str {
        "resident"
    }
}

/// Everything [`TurnExecutor::prepare_turn`] produced, in the order the agent
/// call and the post-agent steps consume it.
pub struct PreparedTurn {
    pub messages: Vec<IncomingMessage>,
    pub members: Vec<crate::store::MemberRecord>,
    pub state: Option<crate::store::GroupStateRecord>,
    pub prompt_batch: PromptBatch,
    pub scope: ValidationScope,
    /// Resolves the seed brief's history refs for turns that carried one;
    /// empty on continuation turns.
    pub history_targets: Vec<HistoryTarget>,
    pub workspace: SandboxWorkspace,
    pub prompt: String,
    /// MCP channel: the token this turn was armed with (`None` on the text
    /// channel). The finish paths read the turn's submissions by it.
    pub turn_token: Option<String>,
    /// Snapshot at prepare time: `agy_conversation_id` was None.
    ///
    /// Captured here because the conversation id is written as soon as the first
    /// prompt lands, which is before a missing submission is judged.
    pub is_first_turn: bool,
}

/// What one batch produced, for tests and the audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnResult {
    /// A reply was enqueued (whether or not it was delivered).
    Replied,
    /// The model chose to stay silent.
    Silent,
    /// A reply existed but the send-time freshness/anchoring policy
    /// discarded it (「宁可不回复也不要错位」). The batch is consumed.
    DroppedStale,
    /// The batch was skipped because the group was disabled/paused.
    SkippedInactive,
    /// The proxy gate was closed; the batch stays pending.
    DeferredProxy,
    /// The agent timed out; the group is paused.
    Unknown,
    /// The hourly fuse blew between claim and start; the batch stays pending and
    /// the lane defers. Nothing was sent to the agent.
    BudgetDeferred,
    /// The agent failed in a retryable way.
    AgentFailed,
    /// The agent replied with something that did not validate.
    BadOutput,
    /// The cheap gate decided the batch was not worth the agent.
    /// Kept for callers that match exhaustively; the pacing gate no longer produces it.
    Prefiltered,
}

impl TurnResult {
    pub fn outcome(&self) -> ExecOutcome {
        match self {
            TurnResult::Unknown => ExecOutcome::Halted,
            TurnResult::DeferredProxy => ExecOutcome::Deferred,
            TurnResult::BudgetDeferred => ExecOutcome::Deferred,
            _ => ExecOutcome::Done,
        }
    }
}

/// Registry of the (at most one per group) agent call currently running.
///
/// The executor registers a turn just before invoking the agent and deregisters it
/// as soon as the call returns. The single-worker lane invariant is what makes a
/// group key sufficient — two turns of one group can never be in flight at once.
#[derive(Debug, Default)]
pub struct RunningTurns {
    inner: std::sync::Mutex<std::collections::HashMap<String, i64>>,
}

impl RunningTurns {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a turn as running since `started_at`.
    pub fn register(&self, group_id: &str, started_at: i64) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(group_id.to_string(), started_at);
        }
    }

    /// Forget a group's turn (it returned on its own).
    pub fn deregister(&self, group_id: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(group_id);
        }
    }

    /// True while this group has a turn in flight. The intake flush consults
    /// this so it does not claim rows the resident insert is about to eat.
    pub fn contains(&self, group_id: &str) -> bool {
        match self.inner.lock() {
            Ok(map) => map.contains_key(group_id),
            Err(_) => false,
        }
    }

    /// `(group_id, started_at)` for every running turn.
    pub fn snapshot(&self) -> Vec<(String, i64)> {
        match self.inner.lock() {
            Ok(map) => map.iter().map(|(g, at)| (g.clone(), *at)).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Rebuild the business messages from stored pending payloads.
pub fn messages_from_payloads(payloads: &[String]) -> Vec<IncomingMessage> {
    payloads
        .iter()
        .filter_map(|raw| serde_json::from_str::<IncomingMessage>(raw).ok())
        .collect()
}

/// Build the prompt batch for a set of normalized messages.
///
/// Message refs (`m1`, `m2`, …) are batch local; speaker refs come from the store and
/// are group local. Nothing else about a sender reaches the model.
pub fn build_prompt_batch(
    messages: &[IncomingMessage],
    memory_context: crate::memory::retrieval::MemoryContext,
    topic_state: Option<String>,
    mood_label: Option<String>,
    persona: Option<String>,
    images: PromptImages,
    sticker_offers: Vec<crate::agent::prompt::StickerOffer>,
    quote_summaries: &std::collections::HashMap<String, String>,
) -> (PromptBatch, ValidationScope) {
    let mut prompt_messages = Vec::with_capacity(messages.len());
    let mut speaker_refs: Vec<String> = Vec::new();
    let mut message_refs: Vec<String> = Vec::new();
    // Batch-local lookup for quote notes: a quote pointing inside the batch
    // is named by its ref, one pointing further back gets the summary the
    // caller resolved from the group log (or a plain marker when even the
    // log has never seen the target).
    let batch_positions: std::collections::HashMap<&str, usize> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| (message.message_id.as_str(), index))
        .collect();
    for (index, message) in messages.iter().enumerate() {
        let message_ref = format!("m{}", index + 1);
        let images = message
            .segments
            .iter()
            .filter(|s| matches!(s, Segment::Image { .. }))
            .count();
        let text = match &message.quote_of {
            None => message.text.clone(),
            Some(target) => {
                let note = if let Some(&position) = batch_positions.get(target.as_str()) {
                    format!("（回复 m{}）", position + 1)
                } else if let Some(summary) = quote_summaries.get(target) {
                    format!("（回复消息“{summary}”）")
                } else {
                    "（回复了一条历史消息）".to_string()
                };
                format!("{}{note}", message.text)
            }
        };
        prompt_messages.push(PromptMessage {
            message_ref: message_ref.clone(),
            speaker_ref: message.speaker_ref.clone(),
            speaker_role: message.speaker_role,
            display_name: message.display_name.clone(),
            known_alias: message.known_alias.clone(),
            text,
            images,
            mentions_bot: message.mentions_bot,
            mentions: crate::onebot::normalize::mention_refs(&message.segments),
            received_at: message.received_at,
        });
        message_refs.push(message_ref);
        if !speaker_refs.contains(&message.speaker_ref) {
            speaker_refs.push(message.speaker_ref.clone());
        }
    }
    let scope = ValidationScope {
        speaker_refs,
        message_refs,
        memory_refs: memory_context.memory_refs(),
        history_refs: Vec::new(),
        sticker_hashes: images.sticker_hashes.clone(),
    };
    let anchor_items: Vec<AnchorItem> = messages
        .iter()
        .enumerate()
        .map(|(i, message)| AnchorItem {
            message_ref: format!("m{}", i + 1),
            message_id: message.message_id.clone(),
            sender_id: message.sender_id.clone(),
            image_count: message
                .segments
                .iter()
                .filter(|seg| matches!(seg, Segment::Image { .. }))
                .count(),
            quote_of: message.quote_of.clone(),
            received_at: None,
        })
        .collect();
    let anchor_hint = batch_anchor(&anchor_items).map(|anchor| match &anchor {
        BatchAnchor::Quoted { via_ref, .. } => format!(
            "本批消息引用了更早的一条消息；如果你的回复是针对被引用的内容，把那一段的 quote_ref 填 {via_ref}（引用它的那条消息），引用会挂在那条消息上。"
        ),
        BatchAnchor::Image { message_ref, .. } => format!(
            "本批主要在讨论 {message_ref} 的图片；如果你的回复是针对这张图，把那一段的 quote_ref 填 {message_ref}。"
        ),
    });
    let batch = PromptBatch {
        messages: prompt_messages,
        topic_state,
        mood_label,
        memory_context,
        digest: None,
        persona,
        images,
        sticker_offers,
        anchor_hint,
        seed_brief: None,
        annex: None,
        flooded: false,
        flood_background: None,
        turn_token: None,
        now_line: None,
    };
    (batch, scope)
}

/// One successfully tagged picture, ready to become a library row.
///
/// The bytes are kept so the caller can persist the file; they never reach the prompt.
#[derive(Debug, Clone)]
pub struct TaggedImage {
    pub hash: String,
    pub tag: crate::app::preflight::ImageTag,
    pub bytes: Vec<u8>,
    /// The batch-local message reference (`m1`, `m2`, ...) that carried this picture, so
    /// the tag can be stored next to its own message.
    pub message_ref: String,
}

/// Download the images in this batch, shrink them, and let the gate describe them in
/// words.
///
/// The agent never receives image bytes: it gets one line of text per picture. That
/// keeps the expensive model's context small and, more importantly, makes the picture
/// content reusable — the same description also feeds the sticker library later.
///
/// URLs and download errors never enter the prompt or the logs.
async fn prepare_image_context(
    fetcher: &dyn ImageFetcher,
    messages: &[IncomingMessage],
    max_bytes: u64,
    max_pixels: u64,
    preflight: Option<&dyn crate::app::preflight::ImageDescriber>,
    bot_id: &str,
    group_id: &str,
) -> (PromptImages, Vec<TaggedImage>) {
    let mut stats = crate::agent::prompt::ImagePrepStats::default();
    let mut slots: Vec<(String, usize, Option<String>)> = Vec::new();
    let mut data_urls: Vec<String> = Vec::new();
    let mut raw_bytes: Vec<Vec<u8>> = Vec::new();
    let mut unavailable = 0usize;
    let mut photos = 0usize;
    let mut stickers = 0usize;

    for (message_index, message) in messages.iter().enumerate() {
        let message_ref = format!("m{}", message_index + 1);
        let mut image_index = 0usize;
        for segment in &message.segments {
            let Segment::Image { url, sub_type, .. } = segment else {
                continue;
            };
            image_index += 1;
            // Photos and stickers have separate budgets. A sticker used to
            // consume one of the four picture slots, so a mixed batch dropped
            // a photo while the sticker still got described.
            let sticker = matches!(sub_type.as_deref(), Some("1") | Some("2"));
            let over_budget = if sticker {
                stickers += 1;
                stickers > crate::app::preflight::MAX_STICKERS_PER_CALL
            } else {
                photos += 1;
                photos > crate::app::preflight::MAX_IMAGES_PER_CALL
            };
            if over_budget {
                unavailable += 1;
                stats.capped += 1;
                continue;
            }
            let Some(url) = url.as_deref() else {
                unavailable += 1;
                stats.no_url += 1;
                continue;
            };
            let downloaded = match fetcher.fetch(url, max_bytes).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("image fetch failed url={} err={:?}", url, e);
                    unavailable += 1;
                    stats.download += 1;
                    continue;
                }
            };
            let Ok(prepared) =
                crate::media::resize::prepare_async(downloaded.bytes, max_pixels).await
            else {
                unavailable += 1;
                stats.prepare += 1;
                continue;
            };
            let Some(data_url) = crate::media::collage::fit_for_vision(&prepared.bytes) else {
                unavailable += 1;
                stats.fit += 1;
                continue;
            };
            slots.push((message_ref.clone(), image_index, sub_type.clone()));
            data_urls.push(data_url);
            // The library hash + stored file use the untouched source when
            // there is one (an animated GIF): the filmstrip in
            // `prepared.bytes` is a vision-only rendering, and re-sending it
            // later would post a static grid instead of the animation.
            raw_bytes.push(prepared.original.clone().unwrap_or(prepared.bytes));
        }
    }

    if data_urls.is_empty() {
        return (
            PromptImages {
                lines: Vec::new(),
                unavailable,
                sticker_hashes: Vec::new(),
                stats,
            },
            Vec::new(),
        );
    }

    let Some(preflight) = preflight else {
        stats.undescribed = data_urls.len();
        return (
            PromptImages {
                lines: Vec::new(),
                unavailable: unavailable + data_urls.len(),
                sticker_hashes: Vec::new(),
                stats,
            },
            Vec::new(),
        );
    };

    let tags = preflight.describe(bot_id, group_id, &data_urls).await;
    let mut lines: Vec<Option<String>> = vec![None; data_urls.len()];
    let mut described = vec![false; data_urls.len()];
    let mut tagged: Vec<TaggedImage> = Vec::new();
    let all_positions: Vec<usize> = (0..data_urls.len()).collect();
    apply_image_tags(
        tags,
        &all_positions,
        &slots,
        &raw_bytes,
        &mut described,
        &mut lines,
        &mut tagged,
    );
    // No second shot. Each picture was already its own call; a miss stays
    // undescribed rather than holding the reply path for another round.
    stats.described = described.iter().filter(|seen| **seen).count();
    let undescribed = described.iter().filter(|seen| !**seen).count();
    stats.undescribed = undescribed;
    let lines: Vec<String> = lines.into_iter().flatten().collect();
    // Only stickers the model actually saw become reachable hashes. A photo is
    // conversation material, never library material.
    let sticker_hashes: Vec<String> = tagged
        .iter()
        .filter(|image| image.tag.kind == "sticker")
        .map(|image| image.hash.clone())
        .collect();
    (
        PromptImages {
            lines,
            unavailable: unavailable + undescribed,
            sticker_hashes,
            stats,
        },
        tagged,
    )
}

/// Apply one describe answer to the batch's bookkeeping.
///
/// `positions` maps the answer's 1-based indexes to absolute slot positions.
#[allow(clippy::too_many_arguments)]
fn apply_image_tags(
    tags: Vec<crate::app::preflight::ImageTag>,
    positions: &[usize],
    slots: &[(String, usize, Option<String>)],
    raw_bytes: &[Vec<u8>],
    described: &mut [bool],
    lines: &mut [Option<String>],
    tagged: &mut Vec<TaggedImage>,
) {
    for tag in tags {
        let Some(&position) = tag.index.checked_sub(1).and_then(|i| positions.get(i)) else {
            continue;
        };
        if described[position] {
            continue;
        }
        described[position] = true;
        let (message_ref, image_index, sub_type) = &slots[position];
        // QQ already told us whether this is a sticker (sub_type 1) or a picture (0).
        // That flag is authoritative: it is what the sender chose in the client, so the
        // vision model's guess at `kind` only applies when the flag is absent. Without
        // this, a 240px sticker that the tagger calls a photo silently misses the
        // library.
        let mut tag = tag;
        tag.kind = crate::app::preflight::resolve_kind(sub_type.as_deref(), &tag.kind);
        // A sticker gets a handle so she can decide about it with `sticker_keep` in the
        // same turn. A photo never gets one: it can never enter her library.
        let hash = raw_bytes
            .get(position)
            .map(|bytes| crate::memory::stickers::hash_bytes(bytes));
        if tag.kind == "sticker"
            && let Some(hash) = hash.as_deref()
        {
            let short = &hash[..hash.len().min(8)];
            lines[position] = Some(format!(
                "{} 的第{}张图：表情 #{short}；{}",
                message_ref,
                image_index,
                tag.render()
            ));
        } else {
            lines[position] = Some(format!(
                "{} 的第{}张图：{}",
                message_ref,
                image_index,
                tag.render()
            ));
        }
        if let Some(hash) = hash {
            tagged.push(TaggedImage {
                hash,
                tag,
                bytes: raw_bytes.get(position).cloned().unwrap_or_default(),
                message_ref: message_ref.clone(),
            });
        }
    }
}

/// Plain lines for a resident insert. No `mN` refs: those belong to the
/// running batch's validation scope, and a colliding ref would let the model
/// quote a message this insert never showed it. Newlines collapse so one
/// message stays one line.
fn render_insert_lines(messages: &[IncomingMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        let text = message
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() && !message.mentions_bot {
            continue;
        }
        let who = {
            let name = message.display_name.trim();
            if name.is_empty() { "群友" } else { name }
        };
        if message.mentions_bot {
            out.push_str(&format!("- {who}：{text}（点了你）\n"));
        } else {
            out.push_str(&format!("- {who}：{text}\n"));
        }
    }
    out
}

/// The TUI is waiting for a prompt. Inserting while it is still painting the
/// previous answer blocks the loop that reads that answer.
fn screen_is_idle_for_insert(screen: &str) -> bool {
    screen.contains(crate::agent::resident::AGY_READY_MARKER)
}

/// Whether an accepted insert must stop the running generation (ESC)
/// before it is pasted: only in ESC mode, and only while the screen is
/// not already idle. The two `false` arms are the rollback contract —
/// with ESC mode off an insert never interrupts, however busy the screen,
/// and an idle screen never needs interrupting in either mode.
fn insert_needs_interrupt(insert_esc: bool, screen_idle: bool) -> bool {
    insert_esc && !screen_idle
}

/// A delivered document whose decision is "don't reply". While the screen is
/// still painting, that document is usually a stub the model rewrites in place;
/// it is not a finished silence.
fn delivered_text_is_silence(text: &str, scope: &crate::agent::contract::ValidationScope) -> bool {
    match crate::agent::contract::validate_salvaged(text, scope) {
        Ok((validated, _)) => !validated.output.should_reply,
        // The barrier only delivers text that already validated. A second pass
        // that fails is not a silence; the normal finish path judges it.
        Err(_) => false,
    }
}

/// An upstream content-filter refusal surfaces as prose where a contract or
/// a step stream was expected: agy prints the refusal instead of running the
/// prompt (2026-10-28, group 943: "The prompt could not be submitted. The
/// prompt contains sensitive words that viol\u{2026}"). The match is deliberately
/// narrow — a bare mention of "sensitive words" in ordinary chat must never
/// trip it, so the phrase only counts alongside the prompt it refused.
fn looks_like_prompt_rejection(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("could not be submitted")
        || (lower.contains("sensitive words") && lower.contains("prompt"))
}

/// Whether any of a turn's step texts is an upstream prompt rejection.
fn texts_show_prompt_rejection<'a>(texts: impl Iterator<Item = &'a str>) -> bool {
    texts.into_iter().any(looks_like_prompt_rejection)
}

/// Failures that mean the session itself gave up. Once the screen is idle these
/// retire the conversation instead of putting the batch back on it. `malformed`
/// used to release the rows into the same transcript (2026-10-27, group 112,
/// step 3).
fn failure_retires_session(reason: &str) -> bool {
    reason.contains("empty_reply")
        || reason.contains("malformed")
        || reason.contains("no_submission")
}

/// A fresh submission token for one turn (16 hex chars). Correlation, not
/// a credential: uniqueness is all it needs, same recipe as the store's
/// local ids.
fn new_turn_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:016x}", nanos ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Whether an MCP submission payload is the bare-refusal shape: silence
/// with nothing attached. The structured twin of
/// `contract::is_bare_refusal`, which reads the text channel's envelope.
fn payload_is_bare_refusal(args_json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return false;
    };
    let empty_array = |key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_array)
            .map(|a| a.is_empty())
            .unwrap_or(true)
    };
    v.get("should_reply").and_then(serde_json::Value::as_bool) == Some(false)
        && empty_array("reply_parts")
        && v.get("reply_text")
            .and_then(serde_json::Value::as_str)
            .map(|t| t.trim().is_empty())
            .unwrap_or(true)
        && empty_array("sticker_ops")
}

/// What the MCP-channel resident loop should do with this turn now.
enum SubmissionOutcome {
    /// A submission exists but is not committable yet (it predates the
    /// last insertion, or landed a second ago): keep polling.
    Wait,
    /// First miss of a brand-new session: deliver a one-shot reminder, then wait.
    Nudge,
    /// The turn is settled without a delivery (no submission at all, or a
    /// deep bare refusal): the outcome method already did the bookkeeping.
    Done(TurnResult),
    /// Commit this payload through the shared finish path.
    Commit(String),
}

impl TurnExecutor {}

#[async_trait::async_trait]
impl BatchExecutor for TurnExecutor {
    async fn execute(&self, job: BatchJob) -> ExecOutcome {
        let outcome = self.run_batch(&job).await.outcome();
        if let Some(hook) = &self.webhook {
            use crate::integrations::webhook::events;
            let event = match outcome {
                ExecOutcome::Done => events::TURN_COMPLETED,
                ExecOutcome::Deferred => events::TURN_DEFERRED,
                ExecOutcome::Halted => events::TURN_FAILED,
            };
            hook.fire(
                event,
                Some(&job.group_id),
                serde_json::json!({
                    "batch_id": job.batch_id,
                    "events": job.event_ids.len(),
                    "retry": job.retry,
                }),
            );
        }
        outcome
    }
}

mod commit;
mod delivery;
mod exec;
mod prepare;
mod resident_batch;
mod sessions;

pub mod history;

pub use self::history::seed_brief_for;

use self::history::{
    ANNEX_LINE_CHARS, ANNEX_REF_PREFIX, HistoryTarget, PreviousBatch, SEED_FACT_LIMIT,
    SEED_LINE_CHARS, SEED_MAX_CHARS, SEED_WINDOW_MESSAGES, assemble_annex, assemble_seed_brief,
};

pub mod gates;

use self::gates::{
    AnchorItem, BatchAnchor, QUOTE_ANCHOR_MAX_NEWER, QUOTE_HOT_THREAD_SECONDS,
    REPLAY_CONTAINMENT_RATIO, REPLAY_DICE_THRESHOLD, REPLAY_MIN_CHARS, REPLAY_WINDOW_SECONDS,
    SEND_GATE_MAX_AGE_SECONDS, SEND_GATE_MAX_NEWER_MESSAGES, batch_anchor, bigram_dice,
    ends_sentence_final, normalize_echo_text, normalize_for_replay, quote_drop_reason,
    quote_echo_check, replay_match, replay_preview, replay_sim_bucket, replay_similarity,
    segment_addresses_owner, send_gate_drop,
};

pub mod stickers;

use self::stickers::{
    EMOTION_TRIGGERS, MAX_OFFER_QUERIES, MAX_STICKER_OFFERS, OFFER_SCAN_LIMIT, RECENT_ROUND_LIMIT,
    RECENT_SEND_SCAN_LIMIT, emotion_bucket, first_label, habit_pick, pick_diverse, quote_summary,
    rounds_just_sent, trigger_queries,
};

#[cfg(test)]
mod tests;
