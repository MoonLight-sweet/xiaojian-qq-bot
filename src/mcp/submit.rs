//! MCP tool `submit_reply`: the model's structured delivery channel.
//!
//! Why a tool instead of the terminal text the delivery barrier used to
//! scrape: arguments arrive as a decoded JSON object over JSON-RPC, so the
//! whole class of "the JSON was almost right" failures (truncation, stray
//! prose, markdown fences) disappears. What a tool call can never guarantee
//! — that the model calls it, calls it once, or fills it semantically — is
//! covered on this side:
//!
//! * **Arming.** The executor writes an `armed_turns` row (group, token)
//!   before a turn starts and puts the token in the turn's contract card. A
//!   call whose token does not match the live arm is rejected, so a late
//!   submission from a dead turn can never be committed to a newer one.
//! * **Buffering.** Every call is recorded; nothing is delivered from here.
//!   The executor commits the last accepted submission when the turn ends,
//!   which is what lets a mid-turn insertion supersede an earlier draft.
//! * **Structural validation only.** This layer checks shape and caps with
//!   precise, fixable reasons; semantic checks (quote resolution, alias
//!   mapping, sticker existence) stay in `contract::validate`, which the
//!   executor runs on the committed payload exactly as before.

use serde_json::{Value, json};

use crate::agent::contract::{
    MAX_MEMORY_OPS, MAX_REPLY_CHARS, MAX_STICKER_OPS, MAX_STICKER_SENDS, is_sticker_hash,
};
use crate::mcp::Scope;
use crate::store::SqliteStore;

/// Tool name the model sees.
pub const TOOL_NAME: &str = "submit_reply";

/// The tool's JSON schema, advertised on `tools/list`.
///
/// Field-for-field mirror of [`crate::agent::contract::AgentOutputRaw`]
/// plus `turn_token`; the `schema_fields_match_contract` test pins the two
/// together, so adding a contract field without updating this schema (or
/// the reverse) fails the test run. Compound fields stay untyped here on
/// purpose: the schema is documentation for the model, the enforcement is
/// [`validate_structure`] plus the executor's `contract::validate`.
pub fn tool_definition() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "提交本轮最终结果。这是结束本轮的唯一方式：每轮必须调用、只调用一次，\
                        作为最后一个动作。should_reply 表示本轮是否发言；发言时正文与表情\
                        至少要有其一，静默时两者都必须为空。turn_token 照抄本轮契约卡上\
                        给的口令。提交被拒绝时会说明原因，修正后在本轮内重新调用。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "turn_token": {
                    "type": "string",
                    "description": "本轮口令，照抄契约卡上给的值。"
                },
                "should_reply": {
                    "type": "boolean",
                    "description": "本轮是否发言。只发表情也算发言，填 true。"
                },
                "reply_text": {
                    "type": "string",
                    "description": "整段回复正文（与 reply_parts 二选一）。"
                },
                "reply_parts": {
                    "type": "array",
                    "items": {},
                    "description": "分段回复：每段是字符串，或 {text, quote_ref, quote_echo}。"
                },
                "nicknames": {
                    "type": "array",
                    "items": {},
                    "description": "别名提议：{subject_ref, alias}。"
                },
                "mood_update": {
                    "type": "object",
                    "description": "情绪更新：{label, reason}。"
                },
                "topic_state": {
                    "type": "string",
                    "description": "当前话题标签，一句话以内。"
                },
                "memory_ops": {
                    "type": "array",
                    "items": {},
                    "description": "记忆提议：{op, subject_ref, content, kind, source_refs, confidence}。"
                },
                "sticker_ops": {
                    "type": "array",
                    "items": {},
                    "description": "表情动作：{op, hash, reason}，op 只能是 send，hash 用候选里给的编号。"
                },
                "quote_ref": {
                    "type": "string",
                    "description": "本轮主要回复的那条消息的 message_ref。"
                },
                "poke_ref": {
                    "type": "string",
                    "description": "想戳一下本批里某个人的 speaker_ref。一轮最多一个，should_reply 必须为 true。"
                }
            },
            "required": ["turn_token", "should_reply"]
        }
    })
}

/// Undo one model deformation before validation and recording:
/// structured values that arrive as a JSON *string* containing an
/// object are parsed back into objects. Found 2026-10-29 twice: in
/// group 900000004 every sticker op arrived stringified (validation
/// skipped the strings and the stickers silently never sent while the
/// reply text promised them), and in group 900000003 a whole
/// submission arrived stringified at every level — a reply part went
/// out to the group as raw `{"quote_echo":...}` text. Covers the op
/// lists, reply parts, and mood_update. Values that do not parse stay
/// as they are; [`validate_structure`] rejects the harmful shapes
/// loudly so the model re-submits within the turn.
fn normalize_submission(args: &Value) -> Value {
    let mut out = args.clone();
    let Some(obj) = out.as_object_mut() else {
        return out;
    };
    for field in ["sticker_ops", "memory_ops", "nicknames", "reply_parts"] {
        let Some(items) = obj.get_mut(field).and_then(Value::as_array_mut) else {
            continue;
        };
        for item in items.iter_mut() {
            let Value::String(text) = item else { continue };
            if let Ok(parsed) = serde_json::from_str::<Value>(text.trim())
                && parsed.is_object()
            {
                *item = parsed;
            }
        }
    }
    if let Some(mood) = obj.get_mut("mood_update")
        && let Value::String(text) = mood
        && let Ok(parsed) = serde_json::from_str::<Value>(text.trim())
        && parsed.is_object()
    {
        *mood = parsed;
    }
    out
}

/// Structural validation of one submission. `Err` carries the exact,
/// fixable reason the model is told. Semantic checks deliberately do not
/// run here; they belong to `contract::validate` on the executor side.
pub fn validate_structure(args: &Value) -> Result<(), String> {
    let obj = args.as_object().ok_or("参数必须是对象")?;
    let should_reply = obj
        .get("should_reply")
        .and_then(Value::as_bool)
        .ok_or("should_reply 必填且必须是布尔值")?;

    // Reply body: `reply_text` plus every part's text.
    let mut total_chars = 0usize;
    let mut has_text = false;
    if let Some(text) = obj.get("reply_text") {
        let text = text.as_str().ok_or("reply_text 必须是字符串")?;
        total_chars += text.chars().count();
        has_text |= !text.trim().is_empty();
    }
    if let Some(parts) = obj.get("reply_parts") {
        let parts = parts.as_array().ok_or("reply_parts 必须是数组")?;
        for part in parts {
            // A part that opens like a JSON object and carries a "text"
            // key is a stringified part object that failed to parse back
            // (normalization already rescued the parseable ones). Sending
            // it would post raw protocol JSON to the group as message
            // text — group 900000003, 2026-10-29 — so reject loudly.
            if let Value::String(s) = part {
                let trimmed = s.trim_start();
                if trimmed.starts_with('{') && s.contains("\"text\"") {
                    return Err(
                        "reply_parts 的对象段被写成了字符串，请直接传对象（不要字符串化）".into(),
                    );
                }
            }
            let text = match part {
                Value::String(s) => s.clone(),
                Value::Object(o) => o
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or("reply_parts 的对象段必须带 text 字符串")?
                    .to_string(),
                _ => return Err("reply_parts 的段必须是字符串或对象".into()),
            };
            if text.trim().is_empty() {
                return Err("reply_parts 里不许有空段".into());
            }
            total_chars += text.chars().count();
            has_text = true;
        }
    }
    if total_chars > MAX_REPLY_CHARS {
        return Err(format!(
            "正文总长 {total_chars} 字，超过上限 {MAX_REPLY_CHARS}"
        ));
    }

    // Sticker ops: shape, send count, hash form.
    let mut sends = 0usize;
    if let Some(ops) = obj.get("sticker_ops") {
        let ops = ops.as_array().ok_or("sticker_ops 必须是数组")?;
        if ops.len() > MAX_STICKER_OPS {
            return Err(format!("sticker_ops 至多 {MAX_STICKER_OPS} 条"));
        }
        for op in ops {
            if !op.is_object() {
                return Err("sticker_ops 的元素必须是 {op, hash} 对象".into());
            }
            if op.get("op").and_then(Value::as_str) == Some("send") {
                sends += 1;
                let hash = op.get("hash").and_then(Value::as_str).unwrap_or("");
                // Shape only, with the contract's own predicate: the model
                // only ever sees the short display hash (#xxxxxxxx), and
                // contract::validate resolves the prefix against the library.
                // Demanding the full 64 chars here (found 2026-10-29)
                // rejected every MCP-era sticker send; the model then
                // resubmitted without sticker_ops and effectively stopped
                // sending stickers.
                if !is_sticker_hash(&hash.trim().to_ascii_lowercase()) {
                    return Err("sticker send 的 hash 必须是小写 hex（短 hash 前缀即可）".into());
                }
            }
        }
        if sends > MAX_STICKER_SENDS {
            return Err(format!("一轮至多发 {MAX_STICKER_SENDS} 张表情"));
        }
    }

    if let Some(ops) = obj.get("memory_ops") {
        let ops = ops.as_array().ok_or("memory_ops 必须是数组")?;
        if ops.len() > MAX_MEMORY_OPS {
            return Err(format!("memory_ops 至多 {MAX_MEMORY_OPS} 条"));
        }
    }

    // One poke per turn, and never on a silent turn: a poke is a reply action.
    if let Some(poke) = obj.get("poke_ref").filter(|v| !v.is_null()) {
        let poke = poke.as_str().ok_or("poke_ref 必须是字符串")?;
        if poke.trim().is_empty() {
            return Err("poke_ref 不能是空字符串".into());
        }
        if !should_reply {
            return Err("should_reply=false 时不许戳人".into());
        }
    }

    if should_reply {
        if !has_text && sends == 0 {
            return Err("should_reply=true 时正文与表情不能同时为空".into());
        }
    } else if has_text || sends > 0 {
        return Err("should_reply=false 时不许带正文或表情".into());
    }
    Ok(())
}

/// Wrap an answer in the tool-result shape the other tools use.
fn tool_answer(text: String, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error
    })
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Handle one `submit_reply` call: check the arm, validate, record, answer.
///
/// Every call is recorded — including rejected ones — under the presented
/// token, so the executor and the audit trail see the full attempt history
/// of the turn, not just the submission that was finally committed.
pub async fn call_submit(store: &SqliteStore, scope: &Scope, args: &Value) -> Value {
    let now = now_epoch();
    let token = args
        .get("turn_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let arm_check = match store.armed_turn(&scope.group_id).await {
        Ok(Some(arm)) if arm.turn_token == token => {
            if now > arm.expires_at {
                Err("本轮已过期，提交窗口关闭".to_string())
            } else {
                Ok(())
            }
        }
        Ok(Some(_)) => Err("turn_token 与本轮不符".to_string()),
        Ok(None) => Err("本轮未武装或已结束".to_string()),
        Err(_) => Err("暂时无法核对本轮口令，请重试".to_string()),
    };
    let normalized = normalize_submission(args);
    let args = &normalized;
    let outcome = arm_check.and_then(|()| validate_structure(args));
    let (verdict, reason) = match &outcome {
        Ok(()) => ("accepted", String::new()),
        Err(r) => ("rejected", r.clone()),
    };
    let _ = store
        .record_submission(
            &scope.group_id,
            &token,
            &args.to_string(),
            verdict,
            &reason,
            now,
        )
        .await;
    match outcome {
        Ok(()) => tool_answer("已记录本轮提交。".to_string(), false),
        Err(r) => tool_answer(
            format!("提交被拒绝：{r}。修正后在本轮内重新调用 submit_reply。"),
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::migrations;

    async fn store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().expect("临时目录");
        let pool = migrations::open_temp(dir.path()).await.expect("迁移");
        (dir, SqliteStore::new(pool))
    }

    fn scope() -> Scope {
        Scope {
            bot_id: "b".into(),
            group_id: "g".into(),
        }
    }

    fn is_error(result: &Value) -> bool {
        result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    #[test]
    fn schema_fields_match_contract() {
        // AgentOutputRaw's fields, plus turn_token. When the contract grows
        // a field, add it to the schema and to this list in the same change.
        let mut expected = vec![
            "turn_token",
            "should_reply",
            "reply_text",
            "reply_parts",
            "nicknames",
            "mood_update",
            "topic_state",
            "memory_ops",
            "sticker_ops",
            "quote_ref",
            "poke_ref",
        ];
        let def = tool_definition();
        let props = def
            .pointer("/inputSchema/properties")
            .and_then(Value::as_object)
            .expect("schema 属性");
        let mut actual: Vec<&str> = props.keys().map(String::as_str).collect();
        expected.sort_unstable();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn string_form_sticker_op_is_normalized_before_validation() {
        // 2026-10-29, group 900000004: the model stringified each sticker
        // op into a JSON string inside the array. Structure validation
        // skipped the strings (not objects), the submission was accepted,
        // and the sticker silently never sent while the text promised it.
        let raw = json!({
            "should_reply": true,
            "reply_parts": ["给你发！"],
            "sticker_ops": ["{\"op\":\"send\",\"hash\":\"a1b2c3d4\"}"]
        });
        let normalized = normalize_submission(&raw);
        let ops = normalized["sticker_ops"].as_array().expect("ops 数组");
        assert!(ops[0].is_object(), "字符串形 op 应被解析回对象: {ops:?}");
        assert!(validate_structure(&normalized).is_ok());
    }

    #[test]
    fn stringified_reply_part_is_normalized_with_quote_intact() {
        // Group 900000003, 2026-10-29: the part object arrived as an
        // escaped JSON string and went out to the group verbatim.
        let raw = json!({
            "should_reply": true,
            "reply_parts": ["{\"quote_echo\":\"只看到个框\",\"quote_ref\":\"m1\",\"text\":\"正文\"}"]
        });
        let normalized = normalize_submission(&raw);
        let part = &normalized["reply_parts"][0];
        assert!(part.is_object(), "字符串形 part 应被解析回对象: {part:?}");
        assert_eq!(part["quote_ref"], "m1");
        assert!(validate_structure(&normalized).is_ok());
    }

    #[test]
    fn unparseable_stringified_reply_part_is_rejected() {
        // Unterminated JSON that still looks like a part object: never
        // let it ride out as message text.
        assert!(
            validate_structure(&json!({
                "should_reply": true,
                "reply_parts": ["{\"text\":\"好\""]
            }))
            .is_err()
        );
        // A plain text part that merely starts with a brace is fine.
        assert!(
            validate_structure(&json!({
                "should_reply": true,
                "reply_parts": ["{这不是 JSON，只是语气词开头"]
            }))
            .is_ok()
        );
    }

    #[test]
    fn loose_stringified_sticker_op_is_rejected() {
        // The 900000003 sample had unquoted keys (`{hash: "…", op:
        // "send"}`): not parseable JSON, so normalization leaves it and
        // the structural check must refuse it, not skip it.
        assert!(
            validate_structure(&json!({
                "should_reply": true,
                "reply_parts": ["好"],
                "sticker_ops": ["{hash: \"b487c57d\", op: \"send\"}"]
            }))
            .is_err()
        );
    }

    #[test]
    fn unparseable_sticker_op_string_is_rejected_not_dropped() {
        // A sticker_ops element that is neither an object nor a JSON
        // object string must fail loudly so the model re-submits within
        // the turn, instead of being silently skipped (2026-10-29).
        assert!(
            validate_structure(&json!({
                "should_reply": true,
                "reply_parts": ["好"],
                "sticker_ops": ["发个表情"]
            }))
            .is_err()
        );
    }

    #[test]
    fn structure_rejects_empty_reply_and_dirty_silence() {
        assert!(validate_structure(&json!({"should_reply": true, "reply_parts": []})).is_err());
        assert!(validate_structure(&json!({"should_reply": false, "reply_parts": ["x"]})).is_err());
        assert!(validate_structure(&json!({"should_reply": false, "reply_parts": []})).is_ok());
        assert!(validate_structure(&json!({"should_reply": true, "reply_parts": ["好"]})).is_ok());
    }

    #[test]
    fn structure_accepts_a_short_sticker_hash_prefix() {
        // The model only ever sees the 8-char display prefix; the contract
        // layer resolves it against the library. Structure validation must
        // not demand the full 64 chars (regression guard for 2026-10-29,
        // when every MCP sticker send was rejected on hash length).
        assert!(
            validate_structure(&json!({
                "should_reply": true,
                "reply_parts": ["好"],
                "sticker_ops": [{"op": "send", "hash": "a1b2c3d4"}]
            }))
            .is_ok()
        );
        // Non-hex hashes still fail the shape check.
        assert!(
            validate_structure(&json!({
                "should_reply": true,
                "reply_parts": ["好"],
                "sticker_ops": [{"op": "send", "hash": "not-a-hash"}]
            }))
            .is_err()
        );
    }

    #[test]
    fn structure_rejects_a_poke_on_a_silent_turn() {
        // The field is a string, never on a silence, never blank.
        assert!(
            validate_structure(&json!({
                "should_reply": true,
                "reply_parts": ["好"],
                "poke_ref": "u2"
            }))
            .is_ok()
        );
        assert!(
            validate_structure(&json!({
                "should_reply": false,
                "poke_ref": "u2"
            }))
            .is_err()
        );
        assert!(
            validate_structure(&json!({
                "should_reply": true,
                "reply_parts": ["好"],
                "poke_ref": "   "
            }))
            .is_err()
        );
        assert!(
            validate_structure(&json!({
                "should_reply": true,
                "reply_parts": ["好"],
                "poke_ref": 7
            }))
            .is_err()
        );
        // null is how an omitted optional field often arrives; treat it as absent.
        assert!(
            validate_structure(&json!({
                "should_reply": true,
                "reply_parts": ["好"],
                "poke_ref": null
            }))
            .is_ok()
        );
    }

    #[tokio::test]
    async fn unarmed_and_wrong_token_are_rejected_but_recorded() {
        let (_dir, store) = store().await;
        let args = json!({"turn_token": "tok", "should_reply": true, "reply_parts": ["好"]});
        let result = call_submit(&store, &scope(), &args).await;
        assert!(is_error(&result), "未武装应当被拒");
        let rows = store.submissions_for("tok").await.expect("读提交");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, "rejected");

        store
            .arm_turn("g", "right", "batch", now_epoch(), now_epoch() + 600)
            .await
            .expect("武装");
        let result = call_submit(&store, &scope(), &args).await;
        assert!(is_error(&result), "口令不符应当被拒");
    }

    #[tokio::test]
    async fn armed_turn_accepts_and_sequences_calls() {
        let (_dir, store) = store().await;
        store
            .arm_turn("g", "tok", "batch", now_epoch(), now_epoch() + 600)
            .await
            .expect("武装");
        let bad = json!({"turn_token": "tok", "should_reply": true, "reply_parts": []});
        assert!(is_error(&call_submit(&store, &scope(), &bad).await));
        let good = json!({"turn_token": "tok", "should_reply": true, "reply_parts": ["好"]});
        assert!(!is_error(&call_submit(&store, &scope(), &good).await));
        let rows = store.submissions_for("tok").await.expect("读提交");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[0].verdict, "rejected");
        assert_eq!(rows[1].seq, 2);
        assert_eq!(rows[1].verdict, "accepted");
    }

    #[tokio::test]
    async fn env_resolved_scope_lets_submit_reply_record() {
        let (_dir, store) = store().await;
        let scope = crate::mcp::resolve_scope_from(&[], Some("b:g")).expect("env");
        store
            .arm_turn(
                &scope.group_id,
                "tok",
                "batch",
                now_epoch(),
                now_epoch() + 600,
            )
            .await
            .expect("装备");
        let args = json!({
            "turn_token": "tok",
            "should_reply": true,
            "reply_parts": ["好"]
        });
        assert!(!is_error(&call_submit(&store, &scope, &args).await));
        let rows = store.submissions_for("tok").await.expect("读提交");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, "accepted");
    }

    #[tokio::test]
    async fn expired_arm_rejects() {
        let (_dir, store) = store().await;
        store
            .arm_turn("g", "tok", "batch", 1, 2)
            .await
            .expect("武装");
        let args = json!({"turn_token": "tok", "should_reply": false, "reply_parts": []});
        assert!(is_error(&call_submit(&store, &scope(), &args).await));
    }
}
