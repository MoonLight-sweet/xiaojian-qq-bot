//! The internal admin API.
//!
//! Reached only by the 8002 gateway over container loopback, with a bearer token the
//! gateway reads from a protected file. The surface is a **fixed path allowlist** — no
//! generic proxy, no user-supplied path, no user-supplied prompt or model name. Every
//! handler re-validates the group id and the JSON body even though the gateway already
//! does, because the gateway is a separate process with its own bug surface.
//!
//! Deliberate omissions:
//!
//! * there is no route that accepts an arbitrary URL or prompt,
//! * the owner QQ id is reported as `configured: true|false` and never echoed,
//! * a compaction POST accepts no model, prompt or content: only the group in the path.

use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::contract::{MAX_MEMORY_CONTENT_CHARS, MEMORY_KINDS};
use crate::config::validate_decimal_id;
use crate::health::HealthRegistry;
use crate::integrations::aux_gateway::AuxGatewayClient;
use crate::integrations::quota::QuotaService;
use crate::memory::compaction::{CompactionDecision, CompactionScheduler, SummaryModel};
use crate::state::mood::{MoodState, TopicState};
use crate::store::{ActorKind, SqliteStore};

/// Shared state for the admin router.
pub struct AdminState {
    pub store: Arc<SqliteStore>,
    pub health: Arc<HealthRegistry>,
    pub compactor: Arc<CompactionScheduler>,
    pub summary_model: Arc<dyn crate::memory::compaction::SummaryModel>,
    pub quota: Arc<QuotaService>,
    /// Quota pacing, so the panel can show the tier that is actually shaping claims
    /// instead of leaving it to be inferred from the audit log.
    pub pacing: Arc<crate::app::pacing::PacingEngine>,
    /// Lane scheduler, so an operator unpause can also clear a halted lane
    /// (the timeout-unknown safety stop) and the status view can show it.
    pub scheduler: Arc<crate::pipeline::scheduler::Scheduler>,
    pub bot_id: String,
    pub persona_default: String,
    /// Shared Agy conversation cap slot (-1 = unlimited), so the panel can
    /// say "不限制" instead of inventing a denominator when recycling is
    /// off — and a settings change shows up without a restart.
    pub conversation_turn_limit: Arc<std::sync::atomic::AtomicI64>,
    /// Runtime settings behind the `/api/v1/settings` endpoints.
    pub settings: Arc<crate::app::runtime_settings::RuntimeSettings>,
    /// Live respawn backoff. Tests leave this `None`; the panel then omits
    /// cooldown fields rather than inventing a deadline from the audit log.
    pub resident: Option<Arc<crate::app::resident::ResidentManager>>,
}

/// Page window for the newest-first listings. Both bounds are clamped so a hostile
/// or sloppy caller cannot ask for an unbounded scan.
#[derive(Debug, Deserialize)]
pub struct PageQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

const PAGE_DEFAULT: i64 = 25;
const PAGE_MAX: i64 = 200;

impl PageQuery {
    fn window(&self) -> (i64, i64) {
        let limit = self.limit.unwrap_or(PAGE_DEFAULT).clamp(1, PAGE_MAX);
        let offset = self.offset.unwrap_or(0).max(0);
        (limit, offset)
    }
}

/// Build the router. The caller applies bearer auth as a layer.
pub fn router(state: Arc<AdminState>) -> Router {
    Router::new()
        .nest("/internal/v1", shared_routes())
        .nest("/api/v1", public_routes())
        .with_state(state)
}

/// Every pre-existing handler, prefix-relative. Mounted under both
/// `/internal/v1` (legacy prefix, the existing gateway/panel) and
/// `/api/v1` (the public surface documented in docs/openapi.yaml).
fn shared_routes() -> Router<Arc<AdminState>> {
    Router::new()
        .route("/status", get(status))
        .route("/quota", get(quota))
        .route("/groups", get(list_groups))
        .route("/groups/{group_id}", patch(patch_group))
        .route("/groups/{group_id}/state", get(get_state).put(put_state))
        .route(
            "/groups/{group_id}/memories",
            get(list_memories).post(create_memory),
        )
        .route(
            "/groups/{group_id}/memories/{memory_ref}",
            delete(delete_memory).patch(patch_memory),
        )
        .route(
            "/persona",
            get(get_persona).put(put_persona).delete(delete_persona),
        )
        .route(
            "/groups/{group_id}/memory/compaction",
            get(compaction_status).post(start_compaction),
        )
        .route("/groups/{group_id}/session/reset", post(reset_session))
        .route("/stickers", get(list_stickers))
        .route(
            "/stickers/{hash}",
            patch(patch_sticker).delete(delete_sticker),
        )
        .route("/stickers/{hash}/image", get(sticker_image))
        .route("/outbox/{id}/retry", post(retry_outbox))
        .route("/pending", get(pending_overview))
        .route("/audit", get(audit_log))
}

/// The public mount: everything shared, plus the settings routes, which
/// exist only under `/api/v1`.
fn public_routes() -> Router<Arc<AdminState>> {
    shared_routes().merge(settings::routes())
}

// ------------------------------------------------------------------ helpers

type ApiResult = Result<Response, ApiError>;

/// An error that renders as `{"error": "code"}` with a sensible status.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
}

impl ApiError {
    fn bad_request(code: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
        }
    }

    fn not_found(code: &'static str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code,
        }
    }

    fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.code }))).into_response()
    }
}

/// Reject any group id that is not a plain decimal string.
fn checked_group_id(raw: &str) -> Result<String, ApiError> {
    if validate_decimal_id("group_id", raw).is_ok() {
        Ok(raw.to_string())
    } else {
        Err(ApiError::bad_request("invalid_group_id"))
    }
}

fn ok_json<T: Serialize>(value: T) -> ApiResult {
    Ok(Json(value).into_response())
}

// ------------------------------------------------------------------ handlers

/// Aggregate status for the panel header.
async fn status(State(state): State<Arc<AdminState>>, Query(page): Query<PageQuery>) -> ApiResult {
    let (limit, offset) = page.window();
    let health = state.health.snapshot().await;
    let groups = state
        .store
        .list_groups(&state.bot_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let pending = state
        .store
        .pending_total(&state.bot_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let outbox = state
        .store
        .outbox_counts(&state.bot_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let calls = state
        .store
        .recent_calls_page(&state.bot_id, limit, offset)
        .await
        .map_err(|_| ApiError::internal())?;
    let calls_total = state
        .store
        .count_calls(&state.bot_id)
        .await
        .map_err(|_| ApiError::internal())?;
    // The same observation the flush tick makes, from the same engine: the tier in this
    // payload is the one shaping the next claim, not a second opinion computed here.
    let pacing = state.pacing.observe_known_groups(crate::now_unix()).await;
    // Lane state, including the halted flag a timeout-unknown result sets:
    // without it the panel cannot distinguish "quiet group" from "lane
    // stopped, batches queueing silently" (2026-10-28, group 900000003).
    let lanes: Vec<_> = state
        .scheduler
        .snapshot()
        .await
        .iter()
        .map(|lane| {
            json!({
                "group_id": lane.group_id,
                "queued": lane.queued,
                "running": lane.running,
                "halted": lane.halted,
            })
        })
        .collect();
    ok_json(json!({
        "bot_id": state.bot_id,
        "health": health,
        "groups": groups.len(),
        "lanes": lanes,
        "pending_messages": pending,
        "outbox": outbox
            .into_iter()
            .map(|(state, count)| json!({"state": state, "count": count}))
            .collect::<Vec<_>>(),
        "recent_calls": calls,
        "calls_total": calls_total,
        "pacing": pacing,
        "backoffs": match &state.resident {
            Some(resident) => resident
                .active_backoffs(crate::now_unix())
                .await
                .into_iter()
                .map(|(group_id, tier, until)| {
                    json!({
                        "group_id": group_id,
                        "tier": tier,
                        "until": until,
                    })
                })
                .collect::<Vec<_>>(),
            None => Vec::new(),
        },
    }))
}

/// Antigravity account quota (Gemini group) for the panel's quota card.
///
/// The service serves a cache and refreshes in the background, so this
/// handler never waits on Google or on a TUI scrape; early after startup the
/// buckets may legitimately still be `null`.
async fn quota(State(state): State<Arc<AdminState>>) -> ApiResult {
    ok_json(state.quota.snapshot())
}

async fn get_persona(State(state): State<Arc<AdminState>>) -> ApiResult {
    let persona = state
        .store
        .get_persona()
        .await
        .map_err(|_| ApiError::internal())?;
    ok_json(json!({
        "persona": persona.unwrap_or_else(|| state.persona_default.clone()),
        "is_default": state.store.get_persona().await.ok().flatten().is_none(),
        "max_chars": 10_000,
    }))
}

#[derive(Debug, Deserialize)]
struct PersonaUpdate {
    persona: String,
}

/// Operator persona edit. Bounded and CQ-free so it cannot become a payload channel.
async fn put_persona(State(state): State<Arc<AdminState>>, Json(body): Json<Value>) -> ApiResult {
    let update: PersonaUpdate =
        serde_json::from_value(body).map_err(|_| ApiError::bad_request("invalid_body"))?;
    let text = update.persona.trim();
    if text.is_empty() {
        return Err(ApiError::bad_request("empty_persona"));
    }
    if text.chars().count() > 10_000 {
        return Err(ApiError::bad_request("persona_too_long"));
    }
    if text.contains("[CQ:") {
        return Err(ApiError::bad_request("forbidden_markup"));
    }
    // The stored card replaces the compiled-in one in the project AGENTS.md,
    // which Agy stops reading past AGENTS_MD_MAX_BYTES — reject a card whose
    // rendered file (card + contract, either channel) would not fit.
    for channel in [
        crate::config::OutputChannel::Text,
        crate::config::OutputChannel::Mcp,
    ] {
        let rendered = crate::agent::prompt::agents_md_with_persona(channel, Some(text));
        if rendered.len() > crate::agent::prompt::AGENTS_MD_MAX_BYTES {
            return Err(ApiError::bad_request("persona_too_long"));
        }
    }
    let now = crate::now_unix();
    state
        .store
        .put_persona(text, now)
        .await
        .map_err(|_| ApiError::internal())?;
    let _ = state
        .store
        .audit(ActorKind::Panel.as_str(), "persona.update", None, None, now)
        .await;
    ok_json(json!({ "ok": true }))
}

/// Restore the compiled-in default card by dropping the stored one.
async fn delete_persona(State(state): State<Arc<AdminState>>) -> ApiResult {
    state
        .store
        .delete_persona()
        .await
        .map_err(|_| ApiError::internal())?;
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "persona.reset",
            None,
            None,
            crate::now_unix(),
        )
        .await;
    ok_json(json!({ "ok": true }))
}

async fn audit_log(
    State(state): State<Arc<AdminState>>,
    Query(page): Query<PageQuery>,
) -> ApiResult {
    let (limit, offset) = page.window();
    let events = state
        .store
        .recent_audit_page(limit, offset)
        .await
        .map_err(|_| ApiError::internal())?;
    let total = state
        .store
        .count_audit()
        .await
        .map_err(|_| ApiError::internal())?;
    ok_json(json!({ "events": events, "total": total }))
}

/// Re-exported for tests that want the router's route table without a live store.
pub fn route_paths() -> Vec<&'static str> {
    vec![
        "/internal/v1/status",
        "/internal/v1/groups",
        "/internal/v1/groups/{group_id}",
        "/internal/v1/groups/{group_id}/state",
        "/internal/v1/groups/{group_id}/memories",
        "/internal/v1/groups/{group_id}/memories/{memory_ref}",
        "/internal/v1/persona",
        "/internal/v1/groups/{group_id}/memory/compaction",
        "/internal/v1/groups/{group_id}/session/reset",
        "/internal/v1/stickers",
        "/internal/v1/stickers/{hash}",
        "/internal/v1/stickers/{hash}/image",
        "/internal/v1/outbox/{id}/retry",
        "/internal/v1/pending",
        "/internal/v1/audit",
    ]
}

mod groups;
mod memories;
mod ops;
mod settings;
mod stickers;

use self::groups::{get_state, list_groups, patch_group, put_state};
use self::memories::{create_memory, delete_memory, list_memories, patch_memory};
use self::ops::{
    compaction_status, pending_overview, reset_session, retry_outbox, start_compaction,
};
use self::stickers::{delete_sticker, list_stickers, patch_sticker, sticker_image};

#[cfg(test)]
mod tests;
