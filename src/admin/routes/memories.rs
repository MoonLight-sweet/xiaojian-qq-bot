//! Admin panel memory endpoints.
//! Moved out of routes.rs verbatim (std-split-routes); logic unchanged.

use super::*;

/// List memories for one group. Raw user ids are replaced by this group's alias.
pub(super) async fn list_memories(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
    Query(page): Query<PageQuery>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let (limit, offset) = page.window();
    let memories = state
        .store
        .list_memories_page(&state.bot_id, &group_id, limit, offset)
        .await
        .map_err(|_| ApiError::internal())?;
    let total = state
        .store
        .count_memories(&state.bot_id, &group_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let members = state
        .store
        .list_members(&state.bot_id, &group_id)
        .await
        .unwrap_or_default();
    let alias_for = |user_id: &str| {
        members
            .iter()
            .find(|m| m.user_id == user_id)
            .map(|m| m.speaker_ref.clone())
    };
    let summary = state
        .store
        .get_summary(&state.bot_id, &group_id)
        .await
        .ok()
        .flatten();
    ok_json(json!({
        "group_id": group_id,
        "total": total,
        "memories": memories
            .into_iter()
            .map(|m| json!({
                "memory_ref": m.memory_ref,
                "subject_ref": m.subject_user_id.as_deref().and_then(alias_for),
                "content": m.content,
                "kind": m.kind,
                "confidence": m.confidence,
                "revision": m.revision,
                // The operator view includes retired rows, so it must say which ones
                // are retired: otherwise the list silently mixes two different states.
                "origin": m.origin,
                "archived_at": m.archived_at,
                "updated_at": m.updated_at,
            }))
            .collect::<Vec<_>>(),
        "summary": summary.map(|s| json!({
            "summary_text": s.summary_text,
            "source_revision": s.source_revision,
            "model_id": s.model_id,
            "updated_at": s.updated_at,
        })),
    }))
}

#[derive(Debug, Deserialize)]
pub(super) struct NewMemory {
    content: String,
    kind: String,
    /// Group-local alias (`u1`) or omitted for a group-level fact.
    subject_ref: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
}

/// Operator-created memory.
pub(super) async fn create_memory(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let input: NewMemory =
        serde_json::from_value(body).map_err(|_| ApiError::bad_request("invalid_body"))?;

    let content = input.content.trim();
    if content.is_empty() || content.chars().count() > MAX_MEMORY_CONTENT_CHARS {
        return Err(ApiError::bad_request("invalid_content"));
    }
    if !MEMORY_KINDS.contains(&input.kind.as_str()) {
        return Err(ApiError::bad_request("invalid_kind"));
    }
    let confidence = input.confidence.unwrap_or(1.0);
    if !(0.0..=1.0).contains(&confidence) {
        return Err(ApiError::bad_request("invalid_confidence"));
    }
    let subject_user_id = match input.subject_ref.as_deref() {
        None => None,
        Some(alias) => {
            let member = state
                .store
                .member_by_ref(&state.bot_id, &group_id, alias)
                .await
                .map_err(|_| ApiError::internal())?
                .ok_or_else(|| ApiError::bad_request("unknown_subject_ref"))?;
            Some(member.user_id)
        }
    };

    let now = crate::now_unix();
    // Origin is `panel`, so the store writes the `memory.create` audit row itself under
    // the panel actor. Writing one here as well would double the event.
    let memory_ref = state
        .store
        .insert_memory_with_origin(
            &state.bot_id,
            &group_id,
            subject_user_id.as_deref(),
            content,
            &input.kind,
            "[]",
            confidence,
            crate::store::origin::PANEL,
            now,
        )
        .await
        .map_err(|_| ApiError::internal())?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "memory_ref": memory_ref })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub(super) struct PatchMemoryRequest {
    content: String,
    kind: String,
    #[serde(default)]
    subject_ref: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
}

/// Operator edit. Marks the row `panel` so a later hy3 guess cannot overwrite it.
pub(super) async fn patch_memory(
    State(state): State<Arc<AdminState>>,
    AxumPath((group_id, memory_ref)): AxumPath<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let input: PatchMemoryRequest =
        serde_json::from_value(body).map_err(|_| ApiError::bad_request("invalid_body"))?;
    let content = input.content.trim();
    if content.is_empty() || content.chars().count() > MAX_MEMORY_CONTENT_CHARS {
        return Err(ApiError::bad_request("invalid_content"));
    }
    if !MEMORY_KINDS.contains(&input.kind.as_str()) {
        return Err(ApiError::bad_request("invalid_kind"));
    }
    if let Some(confidence) = input.confidence
        && !(0.0..=1.0).contains(&confidence)
    {
        return Err(ApiError::bad_request("invalid_confidence"));
    }
    let subject_user_id = match input.subject_ref.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(alias) => {
            let member = state
                .store
                .member_by_ref(&state.bot_id, &group_id, alias)
                .await
                .map_err(|_| ApiError::internal())?
                .ok_or_else(|| ApiError::bad_request("unknown_subject_ref"))?;
            Some(member.user_id)
        }
    };
    let now = crate::now_unix();
    let updated = state
        .store
        .update_memory_panel(
            &state.bot_id,
            &group_id,
            &memory_ref,
            content,
            &input.kind,
            subject_user_id.as_deref(),
            input.confidence,
            now,
        )
        .await
        .map_err(|err| match err {
            crate::error::BotError::MemoryArchived(_) => ApiError::bad_request("archived"),
            _ => ApiError::internal(),
        })?;
    if !updated {
        return Err(ApiError::not_found("not_found"));
    }
    ok_json(json!({ "ok": true }))
}

pub(super) async fn delete_memory(
    State(state): State<Arc<AdminState>>,
    AxumPath((group_id, memory_ref)): AxumPath<(String, String)>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let now = crate::now_unix();
    let removed = state
        .store
        .delete_memory(&state.bot_id, &group_id, &memory_ref, now)
        .await
        .map_err(|_| ApiError::internal())?;
    if !removed {
        return Err(ApiError::not_found("not_found"));
    }
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "memory.delete",
            Some(&state.bot_id),
            Some(&group_id),
            now,
        )
        .await;
    ok_json(json!({ "ok": true }))
}

/// List separated hot meme entries for one group.
pub(super) async fn list_hot_memes(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
    Query(page): Query<PageQuery>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let (limit, offset) = page.window();
    let hot_memes = state
        .store
        .list_hot_memes_page(&state.bot_id, &group_id, limit, offset)
        .await
        .map_err(|_| ApiError::internal())?;
    let total = state
        .store
        .count_hot_memes(&state.bot_id, &group_id)
        .await
        .map_err(|_| ApiError::internal())?;
    ok_json(json!({
        "group_id": group_id,
        "total": total,
        "hot_memes": hot_memes
            .into_iter()
            .map(|hm| json!({
                "meme_ref": hm.meme_ref,
                "content": hm.content,
                "source": hm.source,
                "confidence": hm.confidence,
                "created_at": hm.created_at,
                "updated_at": hm.updated_at,
            }))
            .collect::<Vec<_>>(),
    }))
}

pub(super) async fn delete_hot_meme(
    State(state): State<Arc<AdminState>>,
    AxumPath((group_id, meme_ref)): AxumPath<(String, String)>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let now = crate::now_unix();
    let removed = state
        .store
        .delete_hot_meme(&state.bot_id, &group_id, &meme_ref)
        .await
        .map_err(|_| ApiError::internal())?;
    if !removed {
        return Err(ApiError::not_found("not_found"));
    }
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "hot_meme.delete",
            Some(&state.bot_id),
            Some(&group_id),
            now,
        )
        .await;
    ok_json(json!({ "ok": true }))
}

/// List all pinned memories for a group.
pub(super) async fn list_pins(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let pins = state
        .store
        .list_pinned_memories(&state.bot_id, &group_id)
        .await
        .map_err(|_| ApiError::internal())?;
    ok_json(json!({
        "group_id": group_id,
        "pins": pins,
    }))
}

#[derive(Debug, Deserialize)]
pub(super) struct NewPinRequest {
    content: String,
}

/// Create a pinned memory entry.
pub(super) async fn create_pin(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let input: NewPinRequest =
        serde_json::from_value(body).map_err(|_| ApiError::bad_request("invalid_body"))?;
    let content = input.content.trim();
    if content.is_empty() || content.chars().count() > MAX_MEMORY_CONTENT_CHARS {
        return Err(ApiError::bad_request("invalid_content"));
    }
    let now = crate::now_unix();
    let pin_ref = state
        .store
        .insert_pinned_memory(&state.bot_id, &group_id, content, now)
        .await
        .map_err(|_| ApiError::internal())?;
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "pin.create",
            Some(&state.bot_id),
            Some(&group_id),
            now,
        )
        .await;
    Ok((StatusCode::CREATED, Json(json!({ "pin_ref": pin_ref }))).into_response())
}

/// Delete a pinned memory entry.
pub(super) async fn delete_pin(
    State(state): State<Arc<AdminState>>,
    AxumPath((group_id, pin_ref)): AxumPath<(String, String)>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let now = crate::now_unix();
    let removed = state
        .store
        .delete_pinned_memory(&state.bot_id, &group_id, &pin_ref)
        .await
        .map_err(|_| ApiError::internal())?;
    if !removed {
        return Err(ApiError::not_found("not_found"));
    }
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "pin.delete",
            Some(&state.bot_id),
            Some(&group_id),
            now,
        )
        .await;
    ok_json(json!({ "ok": true }))
}

/// List all member profiles for a group, enriched with their `speaker_ref` alias.
pub(super) async fn list_profiles(
    State(state): State<Arc<AdminState>>,
    AxumPath(group_id): AxumPath<String>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let profiles = state
        .store
        .list_member_profiles(&state.bot_id, &group_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let members = state
        .store
        .list_members(&state.bot_id, &group_id)
        .await
        .unwrap_or_default();
    let alias_for = |user_id: &str| {
        members
            .iter()
            .find(|m| m.user_id == user_id)
            .map(|m| m.speaker_ref.clone())
    };
    ok_json(json!({
        "group_id": group_id,
        "profiles": profiles
            .into_iter()
            .map(|p| json!({
                "user_id": p.user_id,
                "speaker_ref": alias_for(&p.user_id),
                "display_name": p.display_name,
                "identity_tags": p.identity_tags,
                "interests": p.interests,
                "personality_notes": p.personality_notes,
                "recent_status": p.recent_status,
                "updated_at": p.updated_at,
            }))
            .collect::<Vec<_>>(),
    }))
}

#[derive(Debug, Deserialize)]
pub(super) struct PutProfileRequest {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    identity_tags: String,
    #[serde(default)]
    interests: String,
    #[serde(default)]
    personality_notes: String,
    #[serde(default)]
    recent_status: String,
}

/// Operator edit or creation of a member's 4-section profile.
pub(super) async fn put_profile(
    State(state): State<Arc<AdminState>>,
    AxumPath((group_id, user_id)): AxumPath<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let user_id = checked_group_id(&user_id)?;
    let input: PutProfileRequest =
        serde_json::from_value(body).map_err(|_| ApiError::bad_request("invalid_body"))?;
    let now = crate::now_unix();
    state
        .store
        .upsert_member_profile(
            &state.bot_id,
            &group_id,
            &user_id,
            input.display_name.trim(),
            input.identity_tags.trim(),
            input.interests.trim(),
            input.personality_notes.trim(),
            input.recent_status.trim(),
            0,
            now,
        )
        .await
        .map_err(|_| ApiError::internal())?;
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "profile.update",
            Some(&state.bot_id),
            Some(&group_id),
            now,
        )
        .await;
    ok_json(json!({ "ok": true }))
}

pub(super) async fn delete_profile(
    State(state): State<Arc<AdminState>>,
    AxumPath((group_id, user_id)): AxumPath<(String, String)>,
) -> ApiResult {
    let group_id = checked_group_id(&group_id)?;
    let user_id = checked_group_id(&user_id)?;
    let now = crate::now_unix();
    let removed = state
        .store
        .delete_member_profile(&state.bot_id, &group_id, &user_id)
        .await
        .map_err(|_| ApiError::internal())?;
    if !removed {
        return Err(ApiError::not_found("not_found"));
    }
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "profile.delete",
            Some(&state.bot_id),
            Some(&group_id),
            now,
        )
        .await;
    ok_json(json!({ "ok": true }))
}
