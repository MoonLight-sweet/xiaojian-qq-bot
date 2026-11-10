//! MCP stdio session for the memory tool.
//!
//! **Why roots instead of a per-turn config rewrite.** `mcp_config.json` lives in
//! `AGY_HOME` and is shared by every group. Rewriting it each turn to carry
//! `GEMINI_MEMORY_SCOPE=<bot>:<group>` would race: with `agent_concurrency = 2`, two
//! groups can be starting at once and the second write silently re-points the first
//! group's server at the wrong group. That is a cross-group leak, not a cosmetic bug.
//!
//! The MCP client already tells the server which directories it is working in
//! (`roots/list`, and it sends `notifications/roots/list_changed` when they move), so
//! the server asks for the current workspace and reads `scope.json` from it. The config
//! file then never changes, and no two groups can ever collide.
//!
//! **Environment first, file second.** Spawn pins `GEMINI_MEMORY_SCOPE={bot}:{group}`
//! onto the agy child so the MCP server is scoped with zero `roots/list` round-trip.
//! The project-dir `scope.json` is the second source, for a child that lost the env.
//! A missing scope is a plain tool error, never a silent read of some other group.
//!
//! 2026-10-28: `scope.json` used to live only in the per-turn sandbox while the
//! resident PTY cwd/`--add-dir` was the group project directory, so `roots/list`
//! never saw the file and every tool (including `submit_reply`) failed the gate.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::mcp::extras;
use crate::mcp::memory::{self, Scope};
use crate::mcp::stickers;
use crate::mcp::submit;
use crate::store::SqliteStore;

/// Per-workspace file the bot writes to declare which group this turn belongs to.
pub const SCOPE_FILE: &str = "scope.json";

/// Env var spawn pins on the agy child. Not on `ENV_ALLOWLIST`: a parent value
/// would be the wrong group (or empty); the child gets an explicit assignment.
pub const MEMORY_SCOPE_ENV: &str = "GEMINI_MEMORY_SCOPE";

/// A JSON-RPC error the session has to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// stdout closed: the client is gone.
    Closed,
    /// The frame could not be written.
    Write,
}

/// One connected MCP client.
pub struct Session {
    next_id: Mutex<i64>,
    pending: Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>,
    /// Serialises writes: protocol frames must never interleave on stdout.
    out: Mutex<tokio::io::Stdout>,
    /// lite 基座版只提供 submit_reply 一个工具（见 [`Session::submit_only`]）。
    /// 默认 false，完整版行为不变。
    submit_only: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            next_id: Mutex::new(1),
            pending: Mutex::new(HashMap::new()),
            out: Mutex::new(tokio::io::stdout()),
            submit_only: false,
        }
    }

    /// 只提供 `submit_reply` 的会话（gemini-qq-bot-lite 的 `--mcp-lite`
    /// 模式用）：没有记忆可查、没有表情库，工具面就只剩交付通道本身。
    pub fn submit_only() -> Self {
        Self {
            submit_only: true,
            ..Self::new()
        }
    }

    async fn send(&self, frame: &Value) -> Result<(), SessionError> {
        let mut payload = frame.to_string();
        payload.push('\n');
        let mut out = self.out.lock().await;
        out.write_all(payload.as_bytes())
            .await
            .map_err(|_| SessionError::Write)?;
        out.flush().await.map_err(|_| SessionError::Write)?;
        Ok(())
    }

    /// Ask the client for its roots. Returns the workspace paths, newest first.
    async fn roots(&self) -> Vec<String> {
        let id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next += 1;
            id
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let request = json!({"jsonrpc": "2.0", "id": id, "method": "roots/list"});
        if self.send(&request).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Vec::new();
        }
        let response = match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok(value)) => value,
            _ => {
                self.pending.lock().await.remove(&id);
                return Vec::new();
            }
        };
        response
            .pointer("/result/roots")
            .and_then(Value::as_array)
            .map(|roots| {
                roots
                    .iter()
                    .filter_map(|r| r.get("uri").and_then(Value::as_str))
                    .map(strip_file_uri)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Resolve the group for this turn: env first, workspace `scope.json` second.
    ///
    /// Env first is literal, not just the resolver's tie-break: the spawn pins
    /// the env scope, so production resolves here with no `roots/list`
    /// round-trip. The previous shape awaited `roots()` (up to 5s) before the
    /// env was ever consulted, taxing every tool call for a fallback that
    /// only exists for a child that lost its env (2026-10-29).
    pub async fn resolve_scope(&self) -> Option<Scope> {
        let env = std::env::var(MEMORY_SCOPE_ENV).ok();
        if let Some(scope) = resolve_scope_from(&[], env.as_deref()) {
            return Some(scope);
        }
        resolve_scope_from(&self.roots().await, env.as_deref())
    }

    /// Record one MCP wire request in the audit trail (2026-10-29): the
    /// zero-step investigation found no per-phase MCP timing anywhere, so
    /// whether the client re-discovers tools per turn and what our own
    /// responses cost was unanswerable from production data.
    async fn audit_wire(
        &self,
        store: &SqliteStore,
        action: String,
        bot_id: Option<&str>,
        group_id: Option<&str>,
    ) {
        let _ = store
            .audit("system", &action, bot_id, group_id, crate::now_unix())
            .await;
    }

    /// Serve until stdin closes.
    ///
    /// Requests are dispatched on their own task. This is not an optimisation: a
    /// `tools/call` may have to send `roots/list` and wait for the answer, and that
    /// answer arrives on the very stdin this loop reads. Dispatching inline would
    /// deadlock the loop against its own request until the timeout fires, which shows up
    /// as "no group scope" on every call.
    pub async fn run(self: Arc<Self>, store: Arc<SqliteStore>) -> Result<(), SessionError> {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let frame: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                // Garbage on the transport is not a protocol request; dropping it keeps
                // the session alive instead of tearing the tool down mid-turn.
                Err(_) => continue,
            };

            // A response to one of our own requests: hand it to the waiter. Handled
            // inline because the waiting task cannot make progress otherwise.
            if frame.get("method").is_none() {
                if let Some(id) = frame.get("id").and_then(Value::as_i64)
                    && let Some(tx) = self.pending.lock().await.remove(&id)
                {
                    let _ = tx.send(frame);
                }
                continue;
            }

            let session = Arc::clone(&self);
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                if let Some(response) = session.dispatch(&store, &frame).await {
                    let _ = session.send(&response).await;
                }
            });
        }
        Ok(())
    }

    async fn dispatch(&self, store: &SqliteStore, frame: &Value) -> Option<Value> {
        let started = std::time::Instant::now();
        let method = frame
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();

        // `tools/list` is the one method that has to answer for both servers, so it never
        // reaches either module's own handler.
        if method == "tools/list" {
            let mut tools = if self.submit_only {
                Vec::new()
            } else {
                vec![memory::tool_definition()]
            };
            if !self.submit_only
                && let Some(extra) = stickers::tool_definitions().as_array()
            {
                tools.extend(extra.iter().cloned());
            }
            if !self.submit_only
                && let Some(extra) = extras::tool_definitions().as_array()
            {
                tools.extend(extra.iter().cloned());
            }
            tools.push(submit::tool_definition());
            self.audit_wire(
                store,
                wire_action("list", None, started.elapsed().as_millis(), None, false),
                None,
                None,
            )
            .await;
            return answer(frame, json!({"tools": tools}));
        }

        if method != "tools/call" {
            // Every other method is a pure function of the request; an empty scope is
            // never read by them.
            let scope = Scope {
                bot_id: String::new(),
                group_id: String::new(),
            };
            let result = memory::handle(store, &scope, frame).await;
            self.audit_wire(
                store,
                wire_action(method, None, started.elapsed().as_millis(), None, false),
                None,
                None,
            )
            .await;
            return result;
        }

        // A tool call does need the scope, and it is resolved from the workspace the
        // client is running in - never from a tool argument.
        let scope_started = std::time::Instant::now();
        let scope_result = self.resolve_scope().await;
        let scope_ms = scope_started.elapsed().as_millis();
        let Some(scope) = scope_result else {
            let tool = frame
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            self.audit_wire(
                store,
                wire_action(
                    "call",
                    Some(tool),
                    started.elapsed().as_millis(),
                    Some(scope_ms),
                    true,
                ),
                None,
                None,
            )
            .await;
            return answer(
                frame,
                json!({
                    "content": [{"type": "text", "text": "这个回合没有确定群范围，无法检索。"}],
                    "isError": true
                }),
            );
        };

        let name = frame
            .pointer("/params/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let args = frame
            .pointer("/params/arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        // lite 基座版：submit_reply 之外的工具一律不存在，明确报错让
        // 模型收手，不要落到 memory 的处理里假装查过。
        if self.submit_only && name != submit::TOOL_NAME {
            return answer(
                frame,
                json!({
                    "content": [{"type": "text", "text": "基座版不提供这个工具。"}],
                    "isError": true
                }),
            );
        }

        // The sticker tools are bot-wide by design: her library is one drawer, not one
        // per group, so they deliberately ignore `scope.group_id` for scoping — it
        // only attributes the audit rows the tools now leave.
        let result = match name {
            stickers::SEARCH_TOOL => {
                stickers::call_search(store, &scope.bot_id, Some(&scope.group_id), &args).await
            }
            stickers::KEEP_TOOL => {
                stickers::call_keep(store, &scope.bot_id, Some(&scope.group_id), &args).await
            }
            extras::BILIBILI_TOOL => extras::call_bilibili(store, &scope, &args).await,
            extras::PROFILE_TOOL => extras::call_profile_update(store, &scope, &args).await,
            extras::QZONE_TOOL => extras::call_qzone_publish(store, &scope, &args).await,
            extras::MEMEWIKI_TOOL => extras::call_search_meme_wiki(store, &scope, &args).await,
            submit::TOOL_NAME => submit::call_submit(store, &scope, &args).await,
            _ => return memory::handle(store, &scope, frame).await,
        };
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.audit_wire(
            store,
            wire_action(
                "call",
                Some(name),
                started.elapsed().as_millis(),
                Some(scope_ms),
                is_error,
            ),
            Some(&scope.bot_id),
            Some(&scope.group_id),
        )
        .await;
        answer(frame, result)
    }
}

/// Audit action for one MCP wire request: `mcp.wire.<method>[.<tool>].ms<total>`
/// plus `.scope<ms>` when scope resolution was timed and `.err` on an error
/// result. Kept as one flat string because the audit trail is queried by
/// prefix and substring, not parsed into columns.
fn wire_action(
    method: &str,
    tool: Option<&str>,
    ms: u128,
    scope_ms: Option<u128>,
    is_error: bool,
) -> String {
    let mut action = format!("mcp.wire.{}", method.replace('/', "_"));
    if let Some(tool) = tool {
        action.push_str(&format!(".{tool}"));
    }
    action.push_str(&format!(".ms{ms}"));
    if let Some(scope_ms) = scope_ms {
        action.push_str(&format!(".scope{scope_ms}"));
    }
    if is_error {
        action.push_str(".err");
    }
    action
}

/// Wrap a tool result in a JSON-RPC response.
fn answer(frame: &Value, result: Value) -> Option<Value> {
    let id = frame.get("id")?.clone();
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Pure resolver used by [`Session::resolve_scope`] and by tests that must not
/// stand up an MCP client.
///
/// Env wins when both sources are present: spawn pins the group with zero
/// round-trip, and a stale `scope.json` from a previous occupant of the same
/// project dir (should not happen — one group, one dir — but must not outrank
/// the process env if it does). A malformed env falls through to the file.
pub fn resolve_scope_from(roots: &[String], env: Option<&str>) -> Option<Scope> {
    if let Some(raw) = env
        && let Some(scope) = Scope::parse(raw)
    {
        return Some(scope);
    }
    for root in roots {
        let path = Path::new(root).join(SCOPE_FILE);
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Some(scope) = parse_scope_json(&raw)
        {
            return Some(scope);
        }
    }
    None
}

/// Accept either a bare object or a `file://` path prefix from the client.
pub fn strip_file_uri(uri: &str) -> String {
    let stripped = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = stripped.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        stripped[1..].to_string()
    } else {
        stripped.to_string()
    }
}

/// Parse the tiny scope file. Both halves must be present and non-empty.
pub fn parse_scope_json(raw: &str) -> Option<Scope> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let bot_id = value.get("bot_id")?.as_str()?.to_string();
    let group_id = value.get("group_id")?.as_str()?.to_string();
    if bot_id.is_empty() || group_id.is_empty() {
        return None;
    }
    Some(Scope { bot_id, group_id })
}

/// Render the scope file the bot writes into each workspace.
pub fn render_scope_json(bot_id: &str, group_id: &str) -> String {
    serde_json::to_string(&json!({"bot_id": bot_id, "group_id": group_id}))
        .unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_file_round_trips() {
        let raw = render_scope_json("2000000001", "900000002");
        let scope = parse_scope_json(&raw).expect("解析");
        assert_eq!(scope.bot_id, "2000000001");
        assert_eq!(scope.group_id, "900000002");
    }

    #[test]
    fn empty_halves_are_rejected() {
        assert!(parse_scope_json(r#"{"bot_id":"","group_id":"9"}"#).is_none());
        assert!(parse_scope_json(r#"{"bot_id":"9"}"#).is_none());
        assert!(parse_scope_json("not json").is_none());
    }

    #[test]
    fn file_uris_are_stripped() {
        assert_eq!(strip_file_uri("file:///data/ws"), "/data/ws");
        assert_eq!(strip_file_uri("/data/ws"), "/data/ws");
    }

    #[test]
    fn turn_sandbox_scope_json_is_invisible_from_the_project_root() {
        // Accident pin: roots/list reports the project dir, not turns/turn-*.
        let dir = tempfile::tempdir().expect("tmp");
        let project = dir.path().join("g");
        let turn = project.join("turns/turn-1");
        std::fs::create_dir_all(&turn).expect("turn");
        std::fs::write(turn.join(SCOPE_FILE), render_scope_json("bot", "100")).expect("write");
        let roots = vec![project.display().to_string()];
        assert!(
            resolve_scope_from(&roots, None).is_none(),
            "a file only under turns/ must not resolve from the project root"
        );
    }

    #[test]
    fn project_dir_scope_json_is_visible_from_the_project_root() {
        let dir = tempfile::tempdir().expect("tmp");
        let project = dir.path().join("g");
        crate::media::SandboxWorkspace::write_project_scope(&project, "bot", "100").expect("scope");
        let roots = vec![project.display().to_string()];
        let scope = resolve_scope_from(&roots, None).expect("file");
        assert_eq!(scope.bot_id, "bot");
        assert_eq!(scope.group_id, "100");
    }

    #[test]
    fn env_pins_scope_without_roots() {
        let scope = resolve_scope_from(&[], Some("bot:100")).expect("env");
        assert_eq!(scope.bot_id, "bot");
        assert_eq!(scope.group_id, "100");
        assert!(resolve_scope_from(&[], Some("bad")).is_none());
        assert!(resolve_scope_from(&[], None).is_none());
    }

    #[test]
    fn env_wins_over_a_conflicting_scope_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let project = dir.path().join("g");
        crate::media::SandboxWorkspace::write_project_scope(&project, "bot", "file-group")
            .expect("file");
        let roots = vec![project.display().to_string()];
        let scope = resolve_scope_from(&roots, Some("bot:env-group")).expect("env");
        assert_eq!(scope.group_id, "env-group");
    }
}
