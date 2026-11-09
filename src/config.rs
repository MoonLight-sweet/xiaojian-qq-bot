//! Configuration loading and validation.
//!
//! Everything security relevant is validated at startup: the process refuses to
//! boot with a weak internal token, an empty owner QQ id, a non-loopback media
//! origin, or an insecure OneBot endpoint. Secrets are only ever read from files
//! or the environment and are never logged.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{BotError, BotResult};

/// Default internal admin API bind address (inside the container).
pub const DEFAULT_ADMIN_BIND: &str = "0.0.0.0:8080";
/// Default Aux gateway base url.
pub const DEFAULT_AUX_BASE_URL: &str = "http://host.docker.internal:18088";
/// The only model the group bot may use for preflight, folding and vision.
///
/// `hy3` is free at the gateway (credit 0.0), reads images, and passed all three
/// jobs: cadence verdicts, memory folding, and image tagging. There is no fallback
/// and no other permitted value: a config that names something else refuses to
/// start rather than silently costing money.
pub const DEFAULT_AUX_MODEL: &str = "hy3";
/// Default name of the gateway's sticky-session header (see
/// `integrations::aux_gateway`). Configurable because the name is part of
/// the gateway's own protocol, not of this client.
pub const DEFAULT_AUX_SESSION_HEADER: &str = "X-Aux-Session";
/// Default Agy model id (unverified against the real CLI, see README).
pub const DEFAULT_AGY_MODEL: &str = "gemini-3.8-flash";
/// Default Agy binary path inside the bot container.
pub const DEFAULT_AGY_BIN: &str = "/opt/agy/agy";
/// Default Mihomo binary path inside the bot container.
pub const DEFAULT_MIHOMO_BIN: &str = "/opt/mihomo/mihomo";

/// Non-secret, file backed tunables. Loaded from `bot.toml` when present.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    /// Bot account id the OneBot implementation reports. Decimal string.
    pub bot_id: String,
    /// How long the bot waits after the last message before flushing a batch.
    pub quiet_window_seconds: u64,
    /// Hard upper bound from the first message of a batch.
    pub max_batch_age_seconds: u64,
    /// Max messages per batch.
    pub max_batch_messages: usize,
    /// Max characters per batch.
    pub max_batch_chars: usize,
    /// Max images per batch.
    pub max_batch_images: usize,
    /// Global Agy concurrency.
    pub agent_concurrency: usize,
    /// Agy call deadline.
    pub agent_timeout_seconds: u64,
    /// Mihomo API port (container loopback only).
    pub mihomo_api_port: u16,
    /// Mihomo mixed proxy port (container loopback only).
    pub mihomo_proxy_port: u16,
    /// Memory context character budget.
    pub memory_context_budget: usize,
    /// Memory compactor daily cap, including manual runs.
    pub compaction_daily_limit: u32,
    /// Per-group cap on agent calls started within any rolling hour.
    ///
    /// A fuse against runaway loops, not a pacing tool: when a group reaches the
    /// cap its batches stay pending until calls age out of the window. `0`
    /// disables the fuse.
    pub hourly_call_budget: u32,
    /// Flood drain: when a group's pending pool grows past the drain
    /// threshold, the oldest overflow is consumed as background (keeping the
    /// newest segment plus the latest addressed message) instead of being
    /// worked through oldest-first at full call price.
    ///
    /// A kill switch rather than a preference: the drain exists because a
    /// backlog otherwise burns the hourly budget on stale answers and blocks
    /// fresh questions behind the fuse. Turning it off restores plain
    /// oldest-first claiming with no code change.
    pub flood_enabled: bool,
    /// How many successful turns one Agy conversation may serve before it is dropped.
    ///
    /// `0` disables the cap: Agy already compacts its own transcript, so recycling the
    /// session is a safety net, not a requirement. A non-zero value only exists for
    /// operators who want a predictable reset point (the panel shows used/limit).
    pub conversation_turn_limit: i64,
    /// Whether her replies may quote a group message (OneBot `reply` segment).
    ///
    /// A kill switch rather than a preference: if the OneBot implementation
    /// ever rejects the segment, turning this off restores plain sends with
    /// no code change. Incoming quotes are shown to her either way.
    pub reply_quote_enabled: bool,
    /// P4: how long a resident turn must stay quiet before its latest agent text
    /// is considered the final answer. Seconds; mirrors the reference barrier.
    pub resident_quiet_s: f64,
    /// P4: past this turn age a resident turn stops accepting newly claimed
    /// batches. Seconds.
    pub resident_soft_cap_s: u64,
    /// P4: hard deadline for one resident turn. On expiry the session is
    /// interrupted and the turn ends with the ordinary failure semantics.
    pub resident_hard_cap_s: u64,
    /// P4: how many insertions one resident turn may absorb. `0` disables
    /// insertion entirely even when the group has it enabled.
    pub resident_max_inserts: u32,
    /// P4: barrier polling interval for an active resident turn.
    pub resident_poll_ms: u64,
    /// P4: how long `deliver` waits for its user step to appear in the session
    /// database before retrying once and then declaring a crash. Seconds.
    pub resident_deliver_timeout_s: u64,
    /// Insertion delivery mode. `false` (the default): an accepted insert
    /// waits for an idle screen and rides the running turn via agy's
    /// send-immediately queue. `true`: press ESC to stop the in-flight
    /// generation and paste as a fresh prompt. ESC mode leaves the TUI
    /// cancelled-and-not-ready (2026-10-29: four `interrupt_failed` then a
    /// 60s stall crash); keep it off unless a later agy build paints the
    /// ready marker after cancel. Kill switch: bot.toml + restart.
    pub resident_insert_esc: bool,
    /// Groups opted into the no-submission replay (2026-10-29): when a turn
    /// dies with no submission at all, the intake claims the released batch
    /// as one exclusive retry job in the fresh session instead of letting
    /// it merge into a later batch and lose its reply. Empty disables it.
    pub no_submission_retry_groups: Vec<String>,
    /// Groups that carry an outsized share of the traffic; the pacing
    /// projection weighs each listed group double. Empty = every group
    /// weighs the same. (Was a hardcoded group id until the open-source
    /// pass made it an operator setting.)
    pub heavy_group_ids: Vec<String>,
    /// Automatic rotation of the group main sessions at
    /// `conversation_turn_limit`. Off: a session runs until it faults
    /// or is dropped; the settings page names the long-session drift
    /// cost next to this switch.
    pub session_rotation_enabled: bool,
    /// Quota-driven degradation: pacing tiers shaping claims and caps.
    /// Off: the quota is display-only (`raw_tier` still reports the
    /// decision). A truly exhausted bucket is not this switch's
    /// business — pacing never had a hard-zero rule to switch off.
    pub quota_degradation_enabled: bool,
    /// Respawn cooldown: a retired resident session walks a backoff
    /// ladder (1m up to a 30m cap) before its group may spawn again.
    /// Off: a retirement costs the group nothing — the next tick spawns
    /// a fresh session immediately, so a group whose content keeps
    /// tripping the model's filter is never muted for it (operator call,
    /// 2026-11-03, a production group: seven content-filter retirements
    /// chained 30-minute mutes into a silent evening). The price is a
    /// hot respawn loop against a deterministically failing group;
    /// quota headroom is the operator's call. Kill switch: bot.toml +
    /// restart.
    pub respawn_backoff_enabled: bool,
    /// Names that count as addressing the bot when they appear in a
    /// message's text (members call her by name far more often than
    /// they @ her). Matched as substrings, ASCII case-insensitively;
    /// feeds the intake shape's addressed flag.
    pub bot_call_names: Vec<String>,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            bot_id: String::new(),
            quiet_window_seconds: 3,
            max_batch_age_seconds: 30,
            max_batch_messages: 50,
            max_batch_chars: 8_000,
            max_batch_images: 8,
            agent_concurrency: 2,
            agent_timeout_seconds: 180,
            mihomo_api_port: 9090,
            mihomo_proxy_port: 7890,
            memory_context_budget: 4_000,
            compaction_daily_limit: 10,
            hourly_call_budget: 60,
            flood_enabled: true,
            conversation_turn_limit: 35,
            reply_quote_enabled: true,
            resident_quiet_s: 5.0,
            resident_soft_cap_s: 45,
            resident_hard_cap_s: 90,
            resident_max_inserts: 3,
            resident_poll_ms: 500,
            resident_deliver_timeout_s: 15,
            resident_insert_esc: false,
            no_submission_retry_groups: Vec::new(),
            heavy_group_ids: Vec::new(),
            session_rotation_enabled: true,
            quota_degradation_enabled: true,
            respawn_backoff_enabled: true,
            bot_call_names: vec!["Gemini".to_string()],
        }
    }
}

/// Which channel a turn's final output arrives through.
///
/// `Mcp` (the default): the model ends the turn by calling the
/// `submit_reply` MCP tool and the executor commits the recorded
/// submission. `Text`: the pre-MCP behaviour — the contract JSON is read
/// out of the model's final text. Selected with
/// `GEMINI_OUTPUT_CHANNEL=text`; anything else, including unset, means
/// `Mcp`. The switch exists so a broken MCP channel can be rolled back by
/// configuration alone, without a redeploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputChannel {
    Mcp,
    Text,
}

impl OutputChannel {
    pub fn is_mcp(self) -> bool {
        matches!(self, Self::Mcp)
    }
}

/// Fully resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub bot_id: String,
    pub onebot_ws_url: String,
    pub onebot_access_token: Option<String>,
    pub owner_qq_id: String,
    pub admin_bind: String,
    pub admin_token: String,
    pub db_path: PathBuf,
    pub aux_base_url: String,
    pub aux_api_key_file: PathBuf,
    /// Built web console directory (`WEB_DIST`). When set and populated,
    /// the admin listener also serves the console under `/console/`.
    pub web_dist: Option<PathBuf>,
    /// Outbound event webhook (`EVENT_WEBHOOK_URL`); unset disables it.
    pub event_webhook_url: Option<String>,
    /// HMAC secret for webhook signatures (`EVENT_WEBHOOK_SECRET`).
    pub event_webhook_secret: Option<String>,
    pub aux_model: String,
    pub aux_enabled: bool,
    pub aux_session_header: String,
    pub media_origins: Vec<String>,
    pub media_max_bytes: u64,
    pub media_max_pixels: u64,
    /// Where sticker bytes live. Empty disables the library entirely.
    pub sticker_dir: PathBuf,
    /// Root for per-group Agy project directories (each holds that group's AGENTS.md).
    pub agy_projects_dir: PathBuf,
    pub agy_bin: PathBuf,
    pub agy_model: String,
    /// Output channel for agent turns (see [`OutputChannel`]).
    pub output_channel: OutputChannel,
    pub agy_home: PathBuf,
    pub mihomo_bin: PathBuf,
    pub mihomo_enabled: bool,
    pub file: FileConfig,
    /// Path of the Agy settings file whose permissions we audit.
    ///
    /// Verified location for 1.4.6: `$AGY_HOME/.gemini/antigravity-cli/settings.json`.
    pub agy_settings_path: PathBuf,
    /// Path of the Mihomo config file handed to the child process.
    pub mihomo_config_path: PathBuf,
}

impl Config {
    /// Read a secret from a file, trimming whitespace.
    ///
    /// Returns `None` when the variable is unset or the file is missing: callers
    /// decide whether that is fatal.
    pub fn read_secret_file(path: &Path) -> BotResult<Option<String>> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(BotError::Config(format!(
                "无法读取密钥文件 {}: {e}",
                path.display()
            ))),
        }
    }

    fn env_str(key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }

    /// Load configuration from the environment, with an optional TOML file.
    pub fn from_env() -> BotResult<Self> {
        let file = match Self::env_str("GEMINI_BOT_CONFIG") {
            Some(p) => Self::load_file(Path::new(&p))?,
            None => FileConfig::default(),
        };
        Self::build_from_parts(file, |k| std::env::var(k).ok())
    }

    /// Parse a TOML config file.
    pub fn load_file(path: &Path) -> BotResult<FileConfig> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| BotError::Config(format!("无法读取配置文件 {}: {e}", path.display())))?;
        toml::from_str(&raw)
            .map_err(|e| BotError::Config(format!("配置文件 {} 解析失败: {e}", path.display())))
    }

    /// Build and validate the configuration.
    ///
    /// `get` is injected so tests never touch the process environment.
    pub fn build_from_parts<F>(file: FileConfig, get: F) -> BotResult<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let env = |k: &str| get(k).filter(|v| !v.trim().is_empty());

        let bot_id = env("ONEBOT_BOT_ID")
            .or_else(|| Some(file.bot_id.clone()).filter(|v| !v.is_empty()))
            .ok_or_else(|| BotError::Config("缺少 ONEBOT_BOT_ID".into()))?;
        validate_decimal_id("ONEBOT_BOT_ID", &bot_id)?;

        let onebot_ws_url =
            env("ONEBOT_WS_URL").ok_or_else(|| BotError::Config("缺少 ONEBOT_WS_URL".into()))?;
        validate_ws_url(&onebot_ws_url)?;

        let owner_qq_id =
            env("OWNER_QQ_ID").ok_or_else(|| BotError::Config("缺少 OWNER_QQ_ID".into()))?;
        validate_decimal_id("OWNER_QQ_ID", &owner_qq_id)?;

        let admin_bind = env("ADMIN_BIND").unwrap_or_else(|| DEFAULT_ADMIN_BIND.to_string());
        let admin_token = match env("ADMIN_TOKEN_FILE") {
            Some(p) => Self::read_secret_file(Path::new(&p))?
                .ok_or_else(|| BotError::Config("ADMIN_TOKEN_FILE 为空或不存在".into()))?,
            None => env("ADMIN_TOKEN")
                .ok_or_else(|| BotError::Config("缺少 ADMIN_TOKEN_FILE 或 ADMIN_TOKEN".into()))?,
        };
        validate_internal_token(&admin_token)?;

        let db_path =
            PathBuf::from(env("GEMINI_DB_PATH").unwrap_or_else(|| "/data/bot.sqlite3".to_string()));
        let web_dist = env("WEB_DIST").map(PathBuf::from);
        let event_webhook_url = env("EVENT_WEBHOOK_URL");
        let event_webhook_secret = env("EVENT_WEBHOOK_SECRET");

        let aux_base_url = env("AUX_BASE_URL").unwrap_or_else(|| DEFAULT_AUX_BASE_URL.to_string());
        if !aux_base_url.starts_with("http://") && !aux_base_url.starts_with("https://") {
            return Err(BotError::Config("AUX_BASE_URL 必须是 http(s) URL".into()));
        }
        let aux_api_key_file = PathBuf::from(
            env("AUX_API_KEY_FILE").unwrap_or_else(|| "/run/secrets/aux_api_key".to_string()),
        );
        let aux_model = env("AUX_MODEL").unwrap_or_else(|| DEFAULT_AUX_MODEL.to_string());
        let aux_enabled = env("AUX_ENABLED")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);

        let aux_session_header = env("AUX_SESSION_HEADER")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_AUX_SESSION_HEADER.to_string());

        let media_origins = match env("SNOWLUMA_MEDIA_ORIGINS") {
            Some(raw) => raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        for origin in &media_origins {
            validate_media_origin(origin)?;
        }

        let media_max_bytes = Self::parse_u64(&get, "MEDIA_MAX_BYTES", 10 * 1024 * 1024)?;
        let media_max_pixels = Self::parse_u64(&get, "MEDIA_MAX_PIXELS", 20_000_000)?;
        // Sticker bytes are written under this directory; it must be on the same volume
        // as the database so a backup of /data captures both halves.
        let sticker_dir =
            PathBuf::from(env("STICKER_DIR").unwrap_or_else(|| "/data/stickers".to_string()));
        // Long-lived per-group Agy project directories. The character card lives in the
        // group's directory as AGENTS.md, which Agy reads by itself each run, so the card
        // is not re-sent as prompt text on every turn.
        let agy_projects_dir = PathBuf::from(
            env("AGY_PROJECTS_DIR").unwrap_or_else(|| "/data/agy-projects".to_string()),
        );

        let agy_bin = PathBuf::from(env("AGY_BIN").unwrap_or_else(|| DEFAULT_AGY_BIN.to_string()));
        let agy_model = env("AGY_MODEL").unwrap_or_else(|| DEFAULT_AGY_MODEL.to_string());
        let output_channel = match env("GEMINI_OUTPUT_CHANNEL").as_deref() {
            Some("text") => OutputChannel::Text,
            _ => OutputChannel::Mcp,
        };
        let agy_home =
            PathBuf::from(env("AGY_HOME").unwrap_or_else(|| "/data/agy-home".to_string()));
        // The CLI has no `--config` flag; it reads its settings from $HOME. The path
        // below is the verified 1.4.6 location, derived from AGY_HOME.
        let agy_settings_path = PathBuf::from(env("AGY_SETTINGS_PATH").unwrap_or_else(|| {
            crate::agent::agy::settings_path(&agy_home)
                .display()
                .to_string()
        }));
        let mihomo_config_path = PathBuf::from(
            env("MIHOMO_CONFIG_PATH").unwrap_or_else(|| "/etc/mihomo/config.yaml".to_string()),
        );

        let mihomo_bin =
            PathBuf::from(env("MIHOMO_BIN").unwrap_or_else(|| DEFAULT_MIHOMO_BIN.to_string()));
        let mihomo_enabled = env("MIHOMO_ENABLED")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(true);

        if file.max_batch_messages == 0
            || file.max_batch_chars == 0
            || file.max_batch_images == 0
            || file.agent_concurrency == 0
            || file.memory_context_budget == 0
        {
            return Err(BotError::Config("批量与预算参数必须大于 0".into()));
        }

        Ok(Self {
            bot_id,
            onebot_ws_url,
            onebot_access_token: env("ONEBOT_ACCESS_TOKEN"),
            owner_qq_id,
            admin_bind,
            admin_token,
            db_path,
            web_dist,
            event_webhook_url,
            event_webhook_secret,
            aux_base_url,
            aux_api_key_file,
            aux_model,
            aux_enabled,
            aux_session_header,
            media_origins,
            media_max_bytes,
            media_max_pixels,
            sticker_dir,
            agy_projects_dir,
            agy_bin,
            agy_model,
            output_channel,
            agy_home,
            mihomo_bin,
            mihomo_enabled,
            file,
            agy_settings_path,
            mihomo_config_path,
        })
    }

    fn parse_u64<F>(get: &F, key: &str, default: u64) -> BotResult<u64>
    where
        F: Fn(&str) -> Option<String>,
    {
        match get(key).filter(|v| !v.trim().is_empty()) {
            None => Ok(default),
            Some(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|_| BotError::Config(format!("{key} 不是整数"))),
        }
    }

    /// Duration of the quiet window.
    pub fn quiet_window(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.file.quiet_window_seconds)
    }

    /// Hard batch deadline.
    pub fn max_batch_age(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.file.max_batch_age_seconds)
    }

    /// Agy call deadline.
    pub fn agent_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.file.agent_timeout_seconds)
    }

    /// Whether image transport to Agy is usable. Unverified contract → false.
    pub fn image_transport_enabled(&self) -> bool {
        image_transport_verified()
    }
}

/// The Agy CLI image input mechanism has never been verified against 1.4.6, so the
/// transport stays disabled until an operator confirms it with a real contract
/// sample. This function is the single source of truth for that decision.
pub fn image_transport_verified() -> bool {
    false
}

pub fn validate_decimal_id(field: &str, value: &str) -> BotResult<()> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(BotError::Config(format!("{field} 必须是十进制数字字符串")));
    }
    if value.len() > 20 {
        return Err(BotError::Config(format!("{field} 长度异常")));
    }
    Ok(())
}

fn validate_ws_url(url: &str) -> BotResult<()> {
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        return Err(BotError::Config(
            "ONEBOT_WS_URL 必须是 ws:// 或 wss://".into(),
        ));
    }
    Ok(())
}

pub fn validate_internal_token(token: &str) -> BotResult<()> {
    if token.len() < 32 {
        return Err(BotError::Config("内部 bearer token 至少 32 字符".into()));
    }
    if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(BotError::Config("内部 bearer token 不得包含空白".into()));
    }
    Ok(())
}

/// Media origins must be explicit https/http origins; no wildcards, no paths.
fn validate_media_origin(origin: &str) -> BotResult<()> {
    let rest = match origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
    {
        Some(r) => r,
        None => {
            return Err(BotError::Config(format!(
                "媒体源 {origin} 必须以 http:// 或 https:// 开头"
            )));
        }
    };
    if rest.is_empty() || rest.contains('/') || rest.contains('*') || rest.contains('@') {
        return Err(BotError::Config(format!("媒体源 {origin} 必须是纯 origin")));
    }
    if rest.contains(':') {
        let mut parts = rest.split(':');
        let host = parts.next().unwrap_or("");
        let port = parts.next().unwrap_or("");
        if host.is_empty() || port.is_empty() || parts.next().is_some() {
            return Err(BotError::Config(format!("媒体源 {origin} 端口格式非法")));
        }
        if port.parse::<u16>().is_err() {
            return Err(BotError::Config(format!("媒体源 {origin} 端口非法")));
        }
    }
    Ok(())
}

/// Reject loopback / link-local / metadata style hosts used by the SSRF guard.
/// RFC1918 and IPv6 ULA addresses are allowed only after `check_url` has matched the
/// exact operator-configured media origin.
pub fn host_is_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                // 169.254.169.254 is link-local already, 100.64/10 is CGNAT.
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Collect all configured media origins as a set for exact matching.
pub fn media_origin_set(origins: &[String]) -> BTreeSet<String> {
    origins.iter().map(|o| o.to_ascii_lowercase()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(vars: &[(&str, &str)]) -> BotResult<Config> {
        let owned: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::build_from_parts(FileConfig::default(), move |k| {
            owned
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        })
    }

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![
            ("ONEBOT_BOT_ID", "10000"),
            ("ONEBOT_WS_URL", "ws://127.0.0.1:3001"),
            ("OWNER_QQ_ID", "20000"),
            ("ADMIN_TOKEN", "0123456789abcdef0123456789abcdef"),
        ]
    }

    #[test]
    fn rejects_invalid_config() {
        let cases: &[(&str, fn(&mut Vec<(&str, &str)>))] = &[
            ("missing OWNER_QQ_ID", |vars| {
                vars.retain(|(k, _)| *k != "OWNER_QQ_ID");
            }),
            ("missing ONEBOT_BOT_ID", |vars| {
                vars.retain(|(k, _)| *k != "ONEBOT_BOT_ID");
            }),
            ("non-decimal bot id", |vars| {
                vars[0] = ("ONEBOT_BOT_ID", "10000abc");
            }),
            ("short admin token", |vars| {
                vars[3] = ("ADMIN_TOKEN", "short");
            }),
            ("media origin with path", |vars| {
                vars.push(("SNOWLUMA_MEDIA_ORIGINS", "https://media.example.com/path"));
            }),
            ("wildcard media origin", |vars| {
                vars.push(("SNOWLUMA_MEDIA_ORIGINS", "https://*.example.com"));
            }),
        ];
        for (label, mutate) in cases {
            let mut vars = base();
            mutate(&mut vars);
            assert!(build(&vars).is_err(), "{label} 应当拒绝");
        }
    }

    #[test]
    fn aux_disabled_by_default() {
        let cfg = build(&base()).expect("配置应当有效");
        assert!(!cfg.aux_enabled);
        assert_eq!(cfg.aux_model, DEFAULT_AUX_MODEL);
        assert_eq!(cfg.aux_base_url, DEFAULT_AUX_BASE_URL);
        assert_eq!(cfg.aux_session_header, DEFAULT_AUX_SESSION_HEADER);
    }

    #[test]
    fn switches_default_to_on() {
        let file = FileConfig::default();
        assert!(file.session_rotation_enabled);
        assert!(file.quota_degradation_enabled);
    }

    #[test]
    fn accepts_a_custom_aux_model_and_session_header() {
        // The model is an ordinary setting since the aux rename: any
        // non-empty name the gateway publishes is accepted (the default
        // stays hy3), and the sticky-session header name follows the
        // gateway's protocol via AUX_SESSION_HEADER.
        let mut vars = base();
        vars.push(("AUX_MODEL", "some-other-model"));
        vars.push(("AUX_SESSION_HEADER", "X-Legacy-Session"));
        let cfg = build(&vars).expect("配置应当有效");
        assert_eq!(cfg.aux_model, "some-other-model");
        assert_eq!(cfg.aux_session_header, "X-Legacy-Session");
    }

    #[test]
    fn accepts_media_origin_with_port() {
        let mut vars = base();
        vars.push(("SNOWLUMA_MEDIA_ORIGINS", "https://media.example.com:8443"));
        let cfg = build(&vars).expect("配置应当有效");
        assert_eq!(cfg.media_origins, vec!["https://media.example.com:8443"]);
    }

    #[test]
    fn host_screening_allows_configured_private_media_ranges() {
        for raw in [
            "127.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "100.64.0.1",
            "::1",
            "fe80::1",
        ] {
            let ip: IpAddr = raw.parse().expect("合法 IP");
            assert!(host_is_forbidden(ip), "{raw} 应当被拒绝");
        }
        for raw in [
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "fc00::1",
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700::1111",
        ] {
            let ip: IpAddr = raw.parse().expect("合法 IP");
            assert!(!host_is_forbidden(ip), "{raw} 可在精确 allowlist 后使用");
        }
    }

    #[test]
    fn image_transport_is_disabled_until_verified() {
        assert!(!image_transport_verified());
    }

    #[test]
    fn reads_file_config_defaults() {
        let file = FileConfig::default();
        assert_eq!(file.quiet_window_seconds, 3);
        assert_eq!(file.max_batch_age_seconds, 30);
        assert_eq!(file.max_batch_messages, 50);
        assert_eq!(file.max_batch_chars, 8_000);
        assert_eq!(file.max_batch_images, 8);
        assert_eq!(file.memory_context_budget, 4_000);
        assert_eq!(file.compaction_daily_limit, 10);
        assert_eq!(file.hourly_call_budget, 60);
        // Off by default: Agy compacts its own transcript, so recycling a session is
        // an opt-in safety net rather than the default behaviour.
        assert_eq!(file.conversation_turn_limit, 35);
        // Off by default: ESC insert cancelled the TUI without returning the
        // ready marker (2026-10-29). The toml override below is the opt-in.
        assert!(!file.resident_insert_esc);
    }

    #[test]
    fn insert_esc_can_be_switched_on_in_toml() {
        let file: FileConfig = toml::from_str("resident_insert_esc = true").expect("parse toml");
        assert!(file.resident_insert_esc);
    }
}
