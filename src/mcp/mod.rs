//! MCP integration: the tools the agent reaches for on its own.
//!
//! `agy` does not expose MCP tools as first-class tool names; it gives the model one
//! generic `call_mcp_tool`. That is fine here — the point is that the model decides
//! *whether* to look something up, instead of Rust injecting a fixed blob into every
//! prompt.
//!
//! * [`memory`] — read-only `memory_search` over reviewed facts, folded windows, and the unarchived log.
//! * [`stickers`] — `sticker_search` over her library plus `sticker_keep`.
//! * [`submit`] — `submit_reply`, the structured channel a turn's final output is delivered through.
//!
//! Both are scoped by the workspace `scope.json` handed over via MCP roots, never by a
//! tool argument: a model that could pass a group id could read another group.

pub mod extras;
pub mod memory;
pub mod session;
pub mod stickers;
pub mod submit;

pub use memory::{Scope, TOOL_NAME, handle, tool_definition};
pub use session::{
    MEMORY_SCOPE_ENV, SCOPE_FILE, Session, SessionError, parse_scope_json, render_scope_json,
    resolve_scope_from,
};
