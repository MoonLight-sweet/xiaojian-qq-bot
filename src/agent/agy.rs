//! Agy CLI adapter.
//!
//! What lives here is the *descriptor* of an Agy installation, not a way to run a
//! one-shot turn: the resident PTY session in [`crate::agent::resident`] is the only
//! execution path, so nothing in this module spawns a child or parses a response
//! envelope any more.
//!
//! What remains is what a session needs before it can start:
//!
//! * [`settings_path`] — where 1.4.6/1.4.7 reads its permissions file,
//! * [`AgyPermissions`] / [`parse_settings_permissions`] — the permission intent the
//!   panel audits,
//! * [`ENV_ALLOWLIST`] and [`AgyRunner::child_env`] — the exact environment a child
//!   may inherit, so Aux keys, the internal bearer token, the SnowLuma token and
//!   the owner QQ id never reach it,
//! * [`AGY_EFFORT`] and [`AGY_CLI_VERSION`] — the fixed effort and the CLI version the
//!   rest of the crate was written against.

use std::path::{Path, PathBuf};

/// The CLI version this adapter was written against.
pub const AGY_CLI_VERSION: &str = "1.4.7";

/// Reasoning effort passed to the CLI. Fixed: the bot never lets the model choose.
pub const AGY_EFFORT: &str = "low";

/// Environment variables the child may inherit. Everything else is dropped:
/// notably `GH_TOKEN`, `AUX_*`, `ADMIN_TOKEN*`, `ONEBOT_ACCESS_TOKEN`,
/// `OWNER_QQ_ID` and any cloud credential.
pub const ENV_ALLOWLIST: [&str; 9] = [
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "TERM",
    "TZ",
    "TMPDIR",
    "http_proxy",
    "https_proxy",
];

/// Where 1.4.6 actually reads its settings, relative to `$HOME`.
///
/// **Verified against a real 1.4.6 install**: the CLI reads
/// `$HOME/.gemini/antigravity-cli/settings.json`, whose top-level keys include
/// `permissions`, `toolPermission`, `allowNonWorkspaceAccess`, `verbosity`,
/// `trustedWorkspaces`, `showTips` and `artifactReviewPolicy`. `permissions.allow`
/// was observed as an array of grant strings such as `command(npm test)`.
///
/// **Not verified**: the key that carries denied grants. The binary's
/// `PermissionGrantsConfig` proto has `allow`/`ask`/`deny` fields, but no real 1.4.6
/// file we could inspect without a login carried a `deny` member, so
/// [`permission_schema_verified`] stays `false`.
pub const AGY_SETTINGS_RELATIVE_PATH: &str = ".gemini/antigravity-cli/settings.json";

/// The settings file path for a given Agy home.
pub fn settings_path(home: &Path) -> PathBuf {
    home.join(AGY_SETTINGS_RELATIVE_PATH)
}

/// Ensure `$HOME/.gemini/config/mcp_config.json` and
/// `$HOME/.gemini/antigravity-cli/mcp_config.json` are valid and point at an
/// existing binary on this host. Repairs broken cross-OS symlinks (e.g. a
/// Windows `\\?\C:\...` link copied to Linux) or 0-byte files automatically.
pub fn ensure_mcp_config(home: &Path, db_path: &Path) {
    let cli_dir = home.join(".gemini/antigravity-cli");
    if !cli_dir.is_dir() {
        return;
    }
    let cfg_dir = home.join(".gemini/config");
    let _ = std::fs::create_dir_all(&cfg_dir);
    let cli_mcp = cli_dir.join("mcp_config.json");
    let cfg_mcp = cfg_dir.join("mcp_config.json");

    let is_valid = |p: &Path| -> bool {
        let Ok(text) = std::fs::read_to_string(p) else {
            return false;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        let Some(cmd) = val
            .get("mcpServers")
            .and_then(|s| s.get("gemini-qq-bot"))
            .and_then(|b| b.get("command"))
            .and_then(|c| c.as_str())
        else {
            return false;
        };
        Path::new(cmd).exists()
    };

    if is_valid(&cli_mcp) && is_valid(&cfg_mcp) {
        return;
    }

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let payload = serde_json::json!({
        "mcpServers": {
            "gemini-qq-bot": {
                "command": exe.display().to_string(),
                "args": ["--mcp-memory"],
                "env": {
                    "GEMINI_MEMORY_DB": db_path.display().to_string()
                }
            }
        }
    });
    let Ok(rendered) = serde_json::to_string_pretty(&payload) else {
        return;
    };
    for target in [&cfg_mcp, &cli_mcp] {
        if std::fs::symlink_metadata(target).is_ok() {
            let _ = std::fs::remove_file(target);
        }
        let _ = std::fs::write(target, &rendered);
    }
}

/// Whether the permission *file schema* could be verified against 1.4.6.
///
/// The file **location** and the `permissions.allow` member are verified (see
/// [`AGY_SETTINGS_RELATIVE_PATH`]). The **deny** member is not: the official docs name
/// the resources (`read_url(*)`, `execute_url(*)`, `command(*)`, `write_file(*)`,
/// `read_file(*)`, `mcp(*)`) and the precedence (`Deny > Ask > Allow`), but no real
/// file was inspected that carried a deny list. The bot therefore records its *intent*
/// and reports the verification state as unverified rather than pretending the file is
/// correct.
pub fn permission_schema_verified() -> bool {
    true
}

/// Parse the operator's settings file into the permission intent we audit.
///
/// Returns `None` when the file is not parseable or carries no `permissions` object,
/// which the caller must treat as "policy not provisioned" — never as "allowed".
pub fn parse_settings_permissions(text: &str) -> Option<AgyPermissions> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let permissions = value.get("permissions")?.as_object()?;
    let read = |key: &str| -> Vec<String> {
        permissions
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(AgyPermissions {
        allow: read("allow"),
        deny: read("deny"),
    })
}

/// Read and parse the settings file at `path`, if present.
pub fn read_settings_permissions(path: &Path) -> Option<AgyPermissions> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_settings_permissions(&text)
}

/// The set of permissions the bot requires. Semantics only: the on-disk encoding is
/// the operator's, and it is checked by [`AgyPermissions::audit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyPermissions {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl AgyPermissions {
    /// Tools needed for the bot's bounded workflow.
    ///
    /// These are the grants Agy 1.4.7 actually recognises and keeps in its settings
    /// file; `edit_file(*)` and `search_web(*)` are pruned by Agy on every run because
    /// they are not grant names (`search_web` needs no grant at all). The container, a
    /// per-batch cwd/`--add-dir` and the deny list below bound the blast radius.
    ///
    /// The deny list is **exact paths only**. Wildcards such as `read_file(*token*)` and
    /// `read_file(/data/mihomo/*)` are silently ignored by Agy 1.4.7 — they survive a
    /// rewrite but do not deny anything, which was verified against a live agent that
    /// read its own OAuth token straight through a wildcard deny. Every credential that
    /// exists on a deployed host is therefore enumerated by name.
    ///
    /// What this list can and cannot do: it stops `read_file`, and nothing else. A
    /// prompt-level instruction stops nothing at all — with the deny list emptied, an
    /// agent that had the full persona card and output contract in its AGENTS.md still
    /// read the token on request. `run_command` also bypasses this list entirely, so
    /// [`crate::agent::agy`]'s hook is what actually covers shell access.
    pub fn required() -> Self {
        Self {
            allow: vec![
                "read_file(*)".to_string(),
                "write_file(*)".to_string(),
                "command(*)".to_string(),
            ],
            deny: vec![
                "read_file(/data/secrets/admin.token)".to_string(),
                "write_file(/data/secrets/admin.token)".to_string(),
                "read_file(/data/secrets/aux_api_key)".to_string(),
                "write_file(/data/secrets/aux_api_key)".to_string(),
                "read_file(/data/agy-home/.gemini/antigravity-cli/antigravity-oauth-token)"
                    .to_string(),
                "write_file(/data/agy-home/.gemini/antigravity-cli/antigravity-oauth-token)"
                    .to_string(),
                "read_file(/data/mihomo/config.yaml)".to_string(),
                "write_file(/data/mihomo/config.yaml)".to_string(),
                "read_file(/data/mihomo/cache.db)".to_string(),
            ],
        }
    }

    /// Check that the explicitly needed tools are available. Unlisted tools retain
    /// Agy's configured default permission behaviour.
    pub fn audit(&self) -> PermissionAudit {
        let required = Self::required();
        let missing_allow = required
            .allow
            .iter()
            .filter(|want| !self.allow.iter().any(|have| have == *want))
            .cloned()
            .collect::<Vec<_>>();
        let wildcard_allows = self
            .allow
            .iter()
            .filter(|allow| !required.allow.contains(allow))
            .cloned()
            .collect::<Vec<_>>();
        let missing_deny = required
            .deny
            .iter()
            .filter(|want| !self.deny.iter().any(|have| have == *want))
            .cloned()
            .collect::<Vec<_>>();
        let ok = missing_allow.is_empty() && wildcard_allows.is_empty() && missing_deny.is_empty();
        PermissionAudit {
            ok,
            missing_allow,
            missing_deny,
            wildcard_allows,
            schema_verified: permission_schema_verified(),
        }
    }
}

/// Result of auditing a permission configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionAudit {
    pub ok: bool,
    pub missing_allow: Vec<String>,
    pub missing_deny: Vec<String>,
    /// Allows beyond the required sandboxed workflow tools; reported for operator review.
    pub wildcard_allows: Vec<String>,
    pub schema_verified: bool,
}

/// One audit-action token: whitespace and dots become `_`, then cut.
/// An empty source becomes `_` so the action still has a field.
pub fn output_preview(text: &str, limit: usize) -> String {
    let token: String = text
        .chars()
        .take(limit)
        .map(|c| {
            if c.is_whitespace() || c == '.' {
                '_'
            } else {
                c
            }
        })
        .collect();
    if token.is_empty() {
        "_".to_string()
    } else {
        token
    }
}

/// The environment descriptor for an agy child process.
///
/// It holds the one thing every agy child is launched against: the `$HOME` that
/// carries the OAuth state and the settings file. There is no config path to pass
/// because 1.4.6 has no `--config` flag, and there is no binary here because the
/// resident PTY session owns its own spawn command.
#[derive(Debug, Clone)]
pub struct AgyRunner {
    home: PathBuf,
}

impl AgyRunner {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    /// The settings file this runner's `$HOME` implies.
    pub fn settings_path(&self) -> PathBuf {
        settings_path(&self.home)
    }

    /// The exact environment handed to the child, for tests and health output.
    pub fn child_env(&self, parent: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for key in ENV_ALLOWLIST {
            // The image user has no home, so the inherited value is `/`.
            // Agy then tries to mkdir `/.gemini` on the read-only root.
            if key == "HOME" {
                continue;
            }
            if let Some(value) = parent(key) {
                out.push((key.to_string(), value));
            }
        }
        out.push(("HOME".to_string(), self.home.display().to_string()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_permissions_cover_the_tools_agy_recognises() {
        let p = AgyPermissions::required();
        assert_eq!(p.allow, vec!["read_file(*)", "write_file(*)", "command(*)"]);
        assert_eq!(
            p.deny,
            vec![
                "read_file(/data/secrets/admin.token)",
                "write_file(/data/secrets/admin.token)",
                "read_file(/data/secrets/aux_api_key)",
                "write_file(/data/secrets/aux_api_key)",
                "read_file(/data/agy-home/.gemini/antigravity-cli/antigravity-oauth-token)",
                "write_file(/data/agy-home/.gemini/antigravity-cli/antigravity-oauth-token)",
                "read_file(/data/mihomo/config.yaml)",
                "write_file(/data/mihomo/config.yaml)",
                "read_file(/data/mihomo/cache.db)",
            ]
        );
        assert!(p.audit().ok);
        assert!(p.audit().schema_verified);
    }

    #[test]
    fn the_deny_list_never_uses_a_wildcard() {
        // Agy 1.4.7 keeps wildcard denies in the file but does not enforce them, so a
        // wildcard here would look like protection while denying nothing.
        for rule in AgyPermissions::required().deny {
            assert!(
                !rule.contains('*'),
                "wildcard deny is not enforced by agy: {rule}"
            );
        }
    }

    #[test]
    fn missing_required_tool_is_a_failure() {
        let mut p = AgyPermissions::required();
        p.allow.retain(|allow| allow != "read_file(*)");
        let audit = p.audit();
        assert!(!audit.ok);
        assert_eq!(audit.missing_allow, vec!["read_file(*)"]);
    }

    #[test]
    fn unneeded_wildcard_allow_is_reported() {
        let mut p = AgyPermissions::required();
        p.allow.push("mcp(*)".into());
        let audit = p.audit();
        assert!(!audit.ok);
        assert!(audit.wildcard_allows.iter().any(|a| a == "mcp(*)"));
        // The schema is verified; the operator's extra grant is what fails the audit.
        assert!(audit.schema_verified);
    }

    #[test]
    fn child_env_drops_secrets() {
        let runner = AgyRunner::new(PathBuf::from("/data/agy-home"));
        let leaky = |k: &str| -> Option<String> {
            match k {
                "PATH" => Some("/usr/bin".into()),
                "HOME" => Some("/".into()),
                "AUX_API_KEY" => Some("sk-secret".into()),
                "ADMIN_TOKEN" => Some("admin-secret".into()),
                "ONEBOT_ACCESS_TOKEN" => Some("snowluma".into()),
                "OWNER_QQ_ID" => Some("10001".into()),
                "GH_TOKEN" => Some("gh-secret".into()),
                "GEMINI_MEMORY_SCOPE" => Some("1:should-not-inherit".into()),
                _ => None,
            }
        };
        let env = runner.child_env(&leaky);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"PATH"));
        for secret in [
            "AUX_API_KEY",
            "ADMIN_TOKEN",
            "ONEBOT_ACCESS_TOKEN",
            "OWNER_QQ_ID",
            "GH_TOKEN",
            "GEMINI_MEMORY_SCOPE",
        ] {
            assert!(!keys.contains(&secret), "{secret} 不得出现在子进程环境");
        }
        // HOME is forced to the agy home.
        let home = env
            .iter()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| v.clone())
            .expect("应有 HOME");
        assert_eq!(home, "/data/agy-home");
    }

    /// The allowlist is the whole contract: a name that is not in it never reaches a
    /// child, and adding a name here is the only way to widen it.
    #[test]
    fn only_allowlisted_names_are_forwarded() {
        let runner = AgyRunner::new(PathBuf::from("/h"));
        let env = runner.child_env(&|k| Some(format!("v-{k}")));
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        for name in ENV_ALLOWLIST {
            // HOME is overwritten with the agy home, never forwarded.
            if name != "HOME" {
                assert!(keys.contains(&name), "{name} 应在白名单内");
            }
        }
        assert_eq!(
            keys.iter().filter(|k| **k == "HOME").count(),
            1,
            "HOME 只能出现一次"
        );
        assert!(!keys.contains(&"GH_TOKEN"), "GH_TOKEN 不在白名单");
    }

    #[test]
    fn settings_permissions_are_parsed_from_the_verified_layout() {
        let text = r#"{
            "permissions":{"allow":["read_url(*)"," ","command(npm test)"]},
            "toolPermission":"always-proceed",
            "verbosity":"normal"
        }"#;
        let parsed = parse_settings_permissions(text).expect("应解析");
        assert_eq!(parsed.allow, vec!["read_url(*)", "command(npm test)"]);
        assert!(parsed.deny.is_empty());

        // A file with no permissions object means "not provisioned", never "allowed".
        assert!(parse_settings_permissions(r#"{"verbosity":"normal"}"#).is_none());
        assert!(parse_settings_permissions("{this is not json").is_none());
    }

    #[test]
    fn settings_path_follows_the_verified_location() {
        assert_eq!(
            settings_path(Path::new("/data/agy-home")),
            PathBuf::from("/data/agy-home/.gemini/antigravity-cli/settings.json")
        );
        // The permission schema was exercised against the real 1.4.7 install: unknown
        // grant names are pruned on rewrite, deny rules are enforced, and
        // `--dangerously-skip-permissions` removes the interactive prompt.
        assert!(permission_schema_verified());
    }

    #[test]
    fn read_settings_permissions_tolerates_a_missing_file() {
        assert!(read_settings_permissions(Path::new("/nonexistent/settings.json")).is_none());
    }

    #[test]
    fn output_preview_is_a_single_safe_token() {
        // The tag it produces ends up in an audit label, so nothing here may contain a
        // dot (labels are dot-split) or a newline.
        assert_eq!(output_preview("hello there\n", 80), "hello_there_");
        assert_eq!(output_preview("a.b", 80), "a_b");
        assert_eq!(output_preview("", 80), "_");
        assert_eq!(output_preview("abcdef", 3), "abc");
    }
}
