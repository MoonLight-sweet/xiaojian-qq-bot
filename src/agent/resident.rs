//! Resident (long-lived interactive) agy session supervisor.
//!
//! A resident session keeps one interactive `agy` alive per group so a follow-up
//! message can be merged into the turn that is already running, and so the CLI's
//! own compaction debt is paid once instead of per call. It is the only way the bot
//! talks to the agent: there is no one-shot subprocess lane left to fall back to.
//!
//! Everything in this module is subordinate to two facts established by the lab
//! driver (`/tmp/insert-lab/lab_driver.py`) and by local PTY experiments against
//! agy 1.4.7:
//!
//! * the TUI is usable over a PTY only if the onboarding screens are walked with a
//!   **fixed, observed** key sequence — colour scheme `Enter`, ToS `Tab`/`Right`/
//!   `Enter`, workspace trust `Enter`. Any screen that is not one of those is a
//!   stop condition, never an excuse to improvise keys.
//! * the conversation DB only appears **after the first message is delivered**, so
//!   session-to-DB binding is confirmed by a `user` step that was not already in
//!   the DB before the paste. A prefix match against an older step is a previous
//!   turn, not a confirmation.
//!
//! The supervisor deliberately does not decide anything about the reply: it
//! delivers text, reads steps, and hands the step stream to
//! [`crate::agent::barrier`]. Whether a turn is deliverable is the barrier's call.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use nix::sys::signal::{Signal, kill as send_signal, killpg};
#[cfg(unix)]
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::agent::agy::{AGY_EFFORT, AgyRunner, settings_path};
use crate::agent::barrier::{self, TurnSpec, Verdict};
use crate::agent::step_stream::{ParseError, ParseResult, StepEvent, parse_steps};

/// The idle prompt the TUI shows once it accepts input. Presence of this string in
/// the de-ANSI'd screen is the single readiness signal.
pub const AGY_READY_MARKER: &str = "? for shortcuts";
/// First-run screen: colour scheme picker.
pub const AGY_COLOR_SCHEME_MARKER: &str = "Choose your color scheme";
/// First-run screen: terms of service.
pub const AGY_TOS_MARKER: &str = "Terms of Service";
/// First-run screen: workspace trust.
pub const AGY_TRUST_MARKER: &str = "Do you trust";
/// Generic onboarding advance markers (handled by `lab_driver.py` the same way).
pub const AGY_ONBOARD_NEXT_MARKERS: [&str; 2] = ["press Enter to continue", "[Next]"];

/// Keys for each onboarding screen, exactly as the lab driver sent them.
pub const AGY_COLOR_SCHEME_KEYS: &[u8] = b"\r";
pub const AGY_TOS_KEYS: &[u8] = b"\t\x1b[C\r";
pub const AGY_TRUST_KEYS: &[u8] = b"\r";
pub const AGY_ONBOARD_NEXT_KEYS: &[u8] = b"\r";

/// Graceful shutdown command for the TUI.
pub const AGY_EXIT_COMMAND: &[u8] = b"/exit\r";

/// agy rewrites `settings.json` on every launch, so the queue policy has to be
/// asserted before each spawn rather than written once.
pub const QUEUED_MESSAGES_KEY: &str = "queuedMessages";
pub const QUEUED_SEND_IMMEDIATELY: &str = "send-immediately";

/// Where the CLI keeps one sqlite DB per conversation, relative to `$HOME`.
pub const CONVERSATIONS_RELATIVE_PATH: &str = ".gemini/antigravity-cli/conversations";

/// PTY geometry. Wide enough that the TUI does not wrap the ready marker away.
pub const PTY_ROWS: u16 = 40;
pub const PTY_COLS: u16 = 120;

/// Overall deadline for the TUI to reach the idle prompt, including onboarding.
pub const BOOTSTRAP_DEADLINE: Duration = Duration::from_secs(120);
/// How long a non-empty screen may sit unrecognised before the session is blocked.
pub const UNKNOWN_SCREEN_GRACE: Duration = Duration::from_secs(20);
/// Screen sampling interval while bootstrapping.
pub const SCREEN_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long a delivered prompt may take to show up as a `user` step.
pub const DELIVER_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);

/// Default confirmation window, in seconds, for [`ResidentConfig::deliver_timeout_s`].
/// Config-driven callers overwrite it; the constant stays the reference default.
pub const DELIVER_CONFIRM_TIMEOUT_SECONDS: u64 = 15;
/// Poll interval while confirming a delivery.
pub const DELIVER_CONFIRM_POLL: Duration = Duration::from_millis(250);

/// Bytes of the prompt written per PTY write. The paste is wrapped in a single
/// bracketed-paste pair; only the write syscalls are split, which keeps a large
/// batch from overflowing the PTY buffer without changing what the CLI sees.
pub const PASTE_CHUNK_CHARS: usize = 1_200;
/// Gap between PTY writes of one prompt.
pub const PASTE_CHUNK_GAP: Duration = Duration::from_millis(30);
/// Bracketed paste markers (agy enables the mode with `ESC [ ? 2004 h`).
pub const PASTE_OPEN: &[u8] = b"\x1b[200~";
pub const PASTE_CLOSE: &[u8] = b"\x1b[201~";

/// Prefix length used when matching a step against a delivered prompt.
pub const MATCH_PREFIX_CHARS: usize = 120;
/// Cap on the retained raw screen bytes.
pub const SCREEN_BUFFER_MAX_BYTES: usize = 256 * 1024;
/// How many candidate conversation DBs to scan when locating a session.
pub const DB_SCAN_LIMIT: usize = 8;

/// An active turn whose step stream has not changed for this long, while the
/// screen shows no idle prompt, is treated as a crash.
pub const ACTIVE_STALL: Duration = Duration::from_secs(60);
/// A session paused for longer than this may be reclaimed by a tick.
pub const PAUSED_RECLAIM_SECONDS: i64 = 24 * 60 * 60;

/// Grace for the child to honour the in-TUI `/exit` command.
pub const CLOSE_GRACE: Duration = Duration::from_secs(5);
/// Grace for the child to honour SIGTERM before SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_secs(3);
/// Grace for the child to honour a turn-level SIGINT before we accept that it is
/// still running. resident owns the child here and has to interrupt it, so the
/// constant lives here.
pub const TURN_INTERRUPT_GRACE: Duration = Duration::from_secs(3);

/// How long the final kill is given to be observed by a `try_wait`.
///
/// `try_wait` is also the reap: it collects the exit status and clears the entry
/// from the kernel's table. A child that exits after the last poll, or one whose
/// handle is dropped while it is still running, is what shows up as `<defunct>`
/// under the bot pid, so every close ends on a bounded poll loop instead of
/// walking away from the handle.
pub const REAP_GRACE: Duration = Duration::from_secs(2);

/// Why a resident session cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ResidentError {
    /// PTY setup or child spawn failed.
    #[error("pty: {0}")]
    Pty(String),
    /// The TUI stopped on a screen the supervisor does not recognise, or never
    /// reached the idle prompt.
    #[error("session blocked: {0}")]
    Blocked(String),
    /// The child died, or the active turn stopped making progress.
    #[error("session crashed: {0}")]
    Crash(String),
    /// The prompt was written but never showed up as a `user` step.
    #[error("delivery not confirmed: {0}")]
    DeliverUnconfirmed(String),
    /// The conversation DB could not be read.
    #[error("steps: {0}")]
    Steps(String),
}

impl ResidentError {
    /// Stable audit suffix for this failure, so `resident.*` actions stay greppable.
    pub fn audit_tag(&self) -> &'static str {
        match self {
            ResidentError::Pty(_) => "pty",
            ResidentError::Blocked(_) => "blocked",
            ResidentError::Crash(_) => "crash",
            ResidentError::DeliverUnconfirmed(_) => "deliver_unconfirmed",
            ResidentError::Steps(_) => "steps",
        }
    }

    /// 若这是步流漂移（`ParseError::Drift` 经 `Steps` 包装），返回归一化短码。
    ///
    /// `Steps` 的载荷是 `ParseError` 的 Display：漂移以 `drift: ` 开头，
    /// 存储类错误以 `storage: ` 开头——只有前者才走漂移确认与回退。
    pub fn drift_kind(&self) -> Option<&'static str> {
        match self {
            ResidentError::Steps(msg) if msg.starts_with("drift: ") => {
                Some(crate::agent::step_stream::drift_kind(msg))
            }
            _ => None,
        }
    }
}

/// Lifecycle of one resident session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// Spawned, still walking onboarding.
    Spawning,
    /// Idle prompt seen; prompts may be delivered.
    Ready,
    /// The session cannot be used until rebuilt.
    Blocked(String),
    /// The child is gone.
    Dead(String),
}

impl SessionState {
    /// Short label for audit strings.
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Spawning => "spawning",
            SessionState::Ready => "ready",
            SessionState::Blocked(_) => "blocked",
            SessionState::Dead(_) => "dead",
        }
    }

    /// Whether prompts may be delivered.
    pub fn is_ready(&self) -> bool {
        matches!(self, SessionState::Ready)
    }
}

/// What a rebuild produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildKind {
    /// The same conversation was resumed with `--conversation`.
    Resume,
    /// A fresh conversation; the caller must seed it (rotation/seed path).
    NeedsSeed,
}

/// How an interrupt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptOutcome {
    /// The turn was interrupted and the session is still alive.
    StillRunning,
    /// The child exited.
    Exited,
}

/// Inputs needed to spawn a session.
#[derive(Debug, Clone)]
pub struct ResidentConfig {
    /// The agy binary (or a stand-in in tests).
    pub bin: PathBuf,
    /// The `$HOME` carrying OAuth state and settings.
    pub home: PathBuf,
    /// The group project directory; also the child's cwd and `--add-dir`.
    pub workspace: PathBuf,
    /// Model id passed through unchanged.
    pub model: String,
    /// Resume this conversation instead of starting a new one.
    pub conversation_id: Option<String>,
    /// How long to wait for a delivered prompt to appear as a `user` step.
    pub deliver_timeout_s: u64,
    /// `GEMINI_MEMORY_SCOPE={bot}:{group}` pinned on the child after the
    /// allowlist copy. `None` in tests that do not exercise MCP.
    pub memory_scope: Option<String>,
}

impl ResidentConfig {
    fn deliver_timeout(&self) -> Duration {
        Duration::from_secs(self.deliver_timeout_s.max(1))
    }
}

/// A confirmed delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliverReceipt {
    /// The conversation DB that received the prompt.
    pub db_path: PathBuf,
    /// Conversation id (the DB file stem).
    pub conversation_id: String,
    /// Index of the matching `user` step — the start of the turn.
    pub step_idx: i64,
}

/// Reads a step stream from a conversation DB.
///
/// Production uses [`DbStepReader`]; tests inject a synthetic reader so the
/// supervisor's PTY plumbing can be exercised without a real agy.
#[async_trait::async_trait]
pub trait StepReader: Send + Sync {
    /// Read every step, in idx order.
    async fn read(&self, db_path: &Path) -> Result<ParseResult, ParseError>;
}

/// The real reader: [`crate::agent::step_stream::parse_steps`].
#[derive(Debug, Default, Clone, Copy)]
pub struct DbStepReader;

#[async_trait::async_trait]
impl StepReader for DbStepReader {
    async fn read(&self, db_path: &Path) -> Result<ParseResult, ParseError> {
        parse_steps(db_path).await
    }
}

struct Inner {
    // Held for the session's lifetime: the master is the pty handle and dropping it
    // would close the slave out from under the child.
    #[expect(dead_code, reason = "owns the pty; must stay alive for the session")]
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Set once the exit status has been collected. `waitpid` can only be called
    /// once, so a second `close()` must not try again.
    reaped: bool,
    writer: Box<dyn Write + Send>,
    state: SessionState,
    db_path: Option<PathBuf>,
    conversation_id: Option<String>,
    sent: std::collections::BTreeSet<&'static str>,
    last_progress: Instant,
    progress_signal: (usize, String),
}

/// One resident interactive `agy` bound to one group.
pub struct ResidentSession {
    group_id: String,
    workspace: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    model: String,
    reader: Arc<dyn StepReader>,
    screen: Arc<Mutex<Vec<u8>>>,
    audit: Arc<Mutex<Vec<String>>>,
    /// How long a delivered prompt may take to appear as a `user` step.
    deliver_timeout: Duration,
    /// Pinned `GEMINI_MEMORY_SCOPE` so a rebuild re-injects the same group.
    memory_scope: Option<String>,
    inner: tokio::sync::Mutex<Inner>,
}

impl ResidentSession {
    /// Spawn the TUI and walk onboarding until the idle prompt appears.
    pub async fn spawn(
        config: &ResidentConfig,
        group_id: &str,
        reader: Arc<dyn StepReader>,
    ) -> Result<Self, ResidentError> {
        let audit = Arc::new(Mutex::new(Vec::new()));
        let screen = Arc::new(Mutex::new(Vec::new()));

        let settings = settings_path(&config.home);
        let changed = ensure_queued_messages(&settings).map_err(ResidentError::Blocked)?;
        if changed {
            push_audit(
                &audit,
                format!("resident.settings.write.g{group_id}.{QUEUED_MESSAGES_KEY}"),
            );
        }

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: PTY_ROWS,
                cols: PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ResidentError::Pty(format!("openpty: {e}")))?;
        let reader_fd = pair
            .master
            .try_clone_reader()
            .map_err(|e| ResidentError::Pty(format!("clone reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| ResidentError::Pty(format!("take writer: {e}")))?;

        let argv = build_interactive_argv(config);
        let mut cmd = CommandBuilder::new(&config.bin);
        cmd.args(&argv[1..]);
        // The allowlist comes from one place rather than being re-derived here:
        // dropping http_proxy here is the known "Authentication required" trap.
        let env_source = AgyRunner::new(config.home.clone());
        cmd.env_clear();
        for (key, value) in env_source.child_env(&|k| std::env::var(k).ok()) {
            cmd.env(key, value);
        }
        #[cfg(windows)]
        {
            for key in [
                "SystemRoot",
                "SYSTEMDRIVE",
                "WINDIR",
                "COMSPEC",
                "PATHEXT",
                "APPDATA",
                "LOCALAPPDATA",
                "PROGRAMDATA",
                "TEMP",
                "TMP",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
            ] {
                if let Ok(val) = std::env::var(key) {
                    cmd.env(key, val);
                }
            }
            cmd.env("USERPROFILE", &config.home);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("TMPDIR", &config.workspace);
        // Explicit assignment, not an allowlist inherit: the parent process does
        // not carry a per-group value, and inheriting one would be the wrong group.
        if let Some(scope) = &config.memory_scope {
            cmd.env(crate::mcp::MEMORY_SCOPE_ENV, scope);
        }
        let _ = std::fs::create_dir_all(&config.workspace);
        cmd.cwd(&config.workspace);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| ResidentError::Pty(format!("spawn: {e}")))?;
        drop(pair.slave);

        spawn_reader(reader_fd, Arc::clone(&screen));

        let session = Self {
            group_id: group_id.to_string(),
            workspace: config.workspace.clone(),
            home: config.home.clone(),
            bin: config.bin.clone(),
            model: config.model.clone(),
            reader,
            screen,
            audit,
            deliver_timeout: config.deliver_timeout(),
            memory_scope: config.memory_scope.clone(),
            inner: tokio::sync::Mutex::new(Inner {
                master: pair.master,
                child,
                reaped: false,
                writer,
                state: SessionState::Spawning,
                db_path: None,
                conversation_id: config.conversation_id.clone(),
                sent: std::collections::BTreeSet::new(),
                last_progress: Instant::now(),
                progress_signal: (0, String::new()),
            }),
        };
        push_audit(
            &session.audit,
            format!(
                "resident.session.start.g{group_id}.pid{}",
                session.pid().await.unwrap_or(0)
            ),
        );

        // The CLI rewrites settings.json on launch; if the queue policy did not
        // survive, the session cannot honour the insert contract.
        if !queued_messages_intact(&settings) {
            session.block("settings_dropped_queued_messages").await.ok();
            // Dropping the session would drop the child handle while it is still
            // running, which is the `<defunct>` row this change exists to stop: a
            // failed spawn has to be closed like any other session.
            session.close().await;
            return Err(ResidentError::Blocked(
                "settings.json 丢失 queuedMessages".to_string(),
            ));
        }

        if let Err(err) = session.wait_ready().await {
            // Same rule as above: a session that never reached the prompt owns a
            // child, so it is closed before the error leaves this function.
            session.close().await;
            return Err(err);
        }
        Ok(session)
    }

    /// Group this session belongs to.
    pub fn group_id(&self) -> &str {
        &self.group_id
    }

    /// The group project directory used as cwd and `--add-dir`.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The agy binary this session runs.
    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// The model id passed to the child.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Current lifecycle state.
    pub async fn state(&self) -> SessionState {
        self.inner.lock().await.state.clone()
    }

    /// The conversation DB once it has been located.
    pub async fn db_path(&self) -> Option<PathBuf> {
        self.inner.lock().await.db_path.clone()
    }

    /// The conversation id, if known.
    pub async fn conversation_id(&self) -> Option<String> {
        self.inner.lock().await.conversation_id.clone()
    }

    /// Child pid, for audit/logging.
    pub async fn pid(&self) -> Option<u32> {
        self.inner.lock().await.child.process_id()
    }

    /// Drain accumulated audit actions. Rev 3 forwards these to `audit_events`.
    pub fn drain_audit(&self) -> Vec<String> {
        match self.audit.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => Vec::new(),
        }
    }

    /// The current screen with ANSI stripped.
    pub fn screen_text(&self) -> String {
        screen_snapshot(&self.screen)
    }

    async fn write_raw(&self, bytes: &[u8]) -> Result<(), ResidentError> {
        let mut inner = self.inner.lock().await;
        inner
            .writer
            .write_all(bytes)
            .map_err(|e| ResidentError::Pty(format!("write: {e}")))?;
        inner
            .writer
            .flush()
            .map_err(|e| ResidentError::Pty(format!("flush: {e}")))
    }

    async fn block(&self, reason: &str) -> Result<(), ResidentError> {
        let mut inner = self.inner.lock().await;
        inner.state = SessionState::Blocked(reason.to_string());
        drop(inner);
        push_audit(
            &self.audit,
            format!(
                "resident.session.blocked.g{}.{}",
                self.group_id,
                sanitize(reason)
            ),
        );
        Err(ResidentError::Blocked(reason.to_string()))
    }

    async fn child_exit(&self) -> Option<String> {
        let mut inner = self.inner.lock().await;
        match inner.child.try_wait() {
            Ok(Some(status)) => Some(format!(
                "exit code {}{}",
                status.exit_code(),
                status
                    .signal()
                    .map(|s| format!(" signal {s}"))
                    .unwrap_or_default()
            )),
            Ok(None) => None,
            Err(e) => Some(format!("wait: {e}")),
        }
    }

    /// Walk onboarding until the idle prompt, or block on an unknown screen.
    pub async fn wait_ready(&self) -> Result<(), ResidentError> {
        let started = Instant::now();
        let mut last_digest = String::new();
        let mut last_change = Instant::now();
        let mut dsr_answered = 0usize;
        loop {
            let dsr_count = self
                .screen
                .lock()
                .map(|buf| buf.windows(4).filter(|w| *w == b"\x1b[6n").count())
                .unwrap_or(0);
            if dsr_count > dsr_answered {
                let mut inner = self.inner.lock().await;
                for _ in dsr_answered..dsr_count {
                    let _ = inner.writer.write_all(b"\x1b[1;1R");
                }
                let _ = inner.writer.flush();
                dsr_answered = dsr_count;
            }
            let screen = self.screen_text();
            if screen.contains(AGY_READY_MARKER) {
                let mut inner = self.inner.lock().await;
                inner.state = SessionState::Ready;
                inner.last_progress = Instant::now();
                drop(inner);
                push_audit(
                    &self.audit,
                    format!("resident.session.ready.g{}", self.group_id),
                );
                return Ok(());
            }
            if let Some(reason) = self.child_exit().await {
                let _ = self.mark_dead(&reason).await;
                return Err(ResidentError::Crash(reason));
            }
            let matched = self.dispatch_bootstrap(&screen).await;

            let digest = digest_of(&screen);
            if digest != last_digest {
                last_digest = digest;
                last_change = Instant::now();
            }
            let now = Instant::now();
            if !matched
                && !screen.trim().is_empty()
                && now.duration_since(last_change) > UNKNOWN_SCREEN_GRACE
            {
                let tail = screen_tail(&self.screen);
                return self.block(&format!("unrecognized_screen:{tail}")).await;
            }
            if now.duration_since(started) > BOOTSTRAP_DEADLINE {
                let tail = screen_tail(&self.screen);
                return self.block(&format!("bootstrap_timeout:{tail}")).await;
            }
            tokio::time::sleep(SCREEN_POLL_INTERVAL).await;
        }
    }

    /// Send the one observed key sequence for whichever onboarding screen is up.
    ///
    /// The PTY buffer is an append-only log of everything the CLI ever painted, not a
    /// redrawn page: once the colour-scheme screen is written it stays in the buffer
    /// forever. So "which screen are we on" means the marker that appears **latest**
    /// in the buffer, not the first one that matches — otherwise the supervisor keeps
    /// answering the first screen it ever saw and never progresses.
    async fn dispatch_bootstrap(&self, screen: &str) -> bool {
        let candidates: [(&'static str, &str, &'static [u8]); 3] = [
            (
                "color_scheme",
                AGY_COLOR_SCHEME_MARKER,
                AGY_COLOR_SCHEME_KEYS,
            ),
            ("tos", AGY_TOS_MARKER, AGY_TOS_KEYS),
            ("trust", AGY_TRUST_MARKER, AGY_TRUST_KEYS),
        ];
        let mut latest: Option<(usize, &'static str, &'static [u8])> = None;
        for (tag, marker, keys) in candidates {
            if let Some(at) = screen.rfind(marker)
                && latest.is_none_or(|(best, _, _)| at > best)
            {
                latest = Some((at, tag, keys));
            }
        }
        for marker in AGY_ONBOARD_NEXT_MARKERS {
            if let Some(at) = screen.rfind(marker)
                && latest.is_none_or(|(best, _, _)| at > best)
            {
                latest = Some((at, "onboard_next", AGY_ONBOARD_NEXT_KEYS));
            }
        }
        let Some((_, tag, keys)) = latest else {
            return false;
        };
        let mut inner = self.inner.lock().await;
        // One shot per screen: re-sending keys on a redrawn screen would walk past
        // an option the operator needs to see, which is exactly what the brief
        // forbids.
        if inner.sent.insert(tag) {
            let _ = inner.writer.write_all(keys);
            let _ = inner.writer.flush();
            drop(inner);
            push_audit(
                &self.audit,
                format!(
                    "resident.session.onboard.g{}.{}",
                    self.group_id,
                    sanitize(tag)
                ),
            );
        }
        true
    }

    async fn mark_dead(&self, reason: &str) -> Result<(), ResidentError> {
        let mut inner = self.inner.lock().await;
        inner.state = SessionState::Dead(reason.to_string());
        drop(inner);
        push_audit(
            &self.audit,
            format!(
                "resident.session.crash.g{}.{}",
                self.group_id,
                sanitize(reason)
            ),
        );
        Ok(())
    }

    /// Read the current step stream.
    pub async fn poll(&self) -> Result<ParseResult, ResidentError> {
        let path = match self.inner.lock().await.db_path.clone() {
            Some(path) => path,
            None => self
                .candidate_dbs()
                .await
                .into_iter()
                .next()
                .ok_or_else(|| ResidentError::Steps("会话 DB 尚未定位".to_string()))?,
        };
        let parsed = self
            .reader
            .read(&path)
            .await
            .map_err(|e| ResidentError::Steps(e.to_string()))?;
        let signal = progress_signal(&parsed.events);
        let mut inner = self.inner.lock().await;
        if inner.progress_signal != signal {
            inner.progress_signal = signal;
            inner.last_progress = Instant::now();
        }
        Ok(parsed)
    }

    /// Read the step stream and evaluate one turn against the delivery barrier.
    pub async fn evaluate(
        &self,
        spec: &TurnSpec,
        now: f64,
        quiet_s: f64,
        validator: Option<
            &(dyn Fn(&str) -> Result<(), crate::agent::contract::RejectReason> + Send + Sync),
        >,
    ) -> Result<(ParseResult, Verdict), ResidentError> {
        let parsed = self.poll().await?;
        let verdict = barrier::evaluate(&parsed.events, spec, now, quiet_s, validator)
            .map_err(ResidentError::Steps)?;
        Ok((parsed, verdict))
    }

    /// Interrupt the running turn: on unix, SIGINT — agy treats it as
    /// "interrupt this turn" and the TUI survives. There is no directed
    /// signal equivalent on Windows, so this is a no-op there and the
    /// caller's grace poll simply reports the turn still running.
    #[cfg(unix)]
    fn signal_interrupt(pid: u32) {
        let _ = send_signal(Pid::from_raw(pid as i32), Signal::SIGINT);
    }

    /// Windows no-op counterpart of the unix SIGINT sender above.
    #[cfg(not(unix))]
    fn signal_interrupt(_pid: u32) {}

    /// Send SIGINT to the child: agy treats it as "interrupt this turn".
    ///
    /// This must not assume the child dies — the TUI survives an interrupt by
    /// design.
    pub async fn interrupt(&self) -> Result<InterruptOutcome, ResidentError> {
        let pid = self.pid().await;
        if let Some(pid) = pid {
            Self::signal_interrupt(pid);
        }
        push_audit(
            &self.audit,
            format!("resident.turn.interrupt.g{}", self.group_id),
        );
        let deadline = Instant::now() + TURN_INTERRUPT_GRACE;
        loop {
            if let Some(reason) = self.child_exit().await {
                self.mark_dead(&reason).await?;
                return Ok(InterruptOutcome::Exited);
            }
            if Instant::now() >= deadline {
                return Ok(InterruptOutcome::StillRunning);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Watchdog tick. Returns `Err(Crash)` when the session is unusable.
    ///
    /// `active_turn` marks whether a turn is in flight; only then is a silent step
    /// stream suspicious.
    pub async fn tick(&self, active_turn: bool) -> Result<(), ResidentError> {
        if let Some(reason) = self.child_exit().await {
            self.mark_dead(&reason).await?;
            return Err(ResidentError::Crash(reason));
        }
        if !active_turn {
            return Ok(());
        }
        let idle_screen = self.screen_text().contains(AGY_READY_MARKER);
        let stalled = {
            let inner = self.inner.lock().await;
            inner.last_progress.elapsed() > ACTIVE_STALL
        };
        if stalled && !idle_screen {
            let reason = format!("步流 {}s 无变化且屏幕无就绪信号", ACTIVE_STALL.as_secs());
            self.mark_dead(&reason).await?;
            return Err(ResidentError::Crash(reason));
        }
        Ok(())
    }

    /// Reclaim a session that has been paused too long.
    ///
    /// Returns `true` when the session was closed by this call.
    pub async fn reclaim_if_idle(&self, paused_since: Option<i64>, now: i64) -> bool {
        let Some(since) = paused_since else {
            return false;
        };
        if now - since < PAUSED_RECLAIM_SECONDS {
            return false;
        }
        push_audit(
            &self.audit,
            format!(
                "resident.session.reclaimed.g{}.paused{}s",
                self.group_id,
                now - since
            ),
        );
        self.close().await;
        true
    }

    /// Close the session: `/exit`, then SIGTERM, then SIGKILL.
    ///
    /// Every exit path ends with [`Self::reap`], because a `try_wait` that observes
    /// the exit is what actually clears the child from the process table. The old
    /// code returned as soon as `await_exit` saw the exit status, which is fine for
    /// the status but still leaves the last `kill(-pgid)` of a wrapper shell able to
    /// outlive the handle and turn into a `<defunct>` under the bot pid.
    pub async fn close(&self) {
        if self.child_exit().await.is_none() {
            let _ = self.write_raw(AGY_EXIT_COMMAND).await;
            if self.await_exit(CLOSE_GRACE).await {
                self.reap().await;
                return;
            }
        }
        self.force_stop().await;
    }

    /// Escalating stop after `/exit` was ignored: SIGTERM, then SIGKILL,
    /// aimed at the child's process group (it is a session leader, and a
    /// wrapper shell's grandchild must not outlive the session).
    #[cfg(unix)]
    async fn force_stop(&self) {
        for (signal, grace) in [
            (Signal::SIGTERM, TERM_GRACE),
            (Signal::SIGKILL, Duration::from_millis(500)),
        ] {
            // The child is a session leader (portable-pty calls `setsid` in its
            // `pre_exec`), so its pgid equals its pid and the signal can be aimed at
            // the whole group. `agy` is often a wrapper shell around the real CLI;
            // signalling only the leader would leave the grandchild running while the
            // leader exits, and the grandchild then belongs to the bot process.
            let pid = self.pid().await;
            if let Some(pid) = pid {
                let pgrp = Pid::from_raw(pid as i32);
                if killpg(pgrp, signal).is_err() {
                    // No group of our own (a race with the child exiting, or a pty
                    // implementation that did not setsid): fall back to the leader.
                    let _ = send_signal(pgrp, signal);
                }
            }
            if self.await_exit(grace).await {
                self.reap().await;
                return;
            }
        }
        let _ = self.await_exit(Duration::from_millis(500)).await;
        self.reap().await;
    }

    /// Off unix there are no process-group signals; the PTY child's own
    /// kill is the only escalation the platform offers.
    #[cfg(not(unix))]
    async fn force_stop(&self) {
        {
            let mut inner = self.inner.lock().await;
            let _ = inner.child.kill();
        }
        let _ = self.await_exit(Duration::from_millis(500)).await;
        self.reap().await;
    }

    /// Collect the child's exit status so nothing is left as `<defunct>`.
    ///
    /// On unix `try_wait` *is* the reap: `waitpid(WNOHANG)` collects the status and
    /// clears the entry, so a bounded poll loop is enough and no blocking call is
    /// needed (a `wait()` here would also have to run off the reactor, which the
    /// single-threaded test runtimes do not allow).
    ///
    /// The loop is bounded because SIGKILL can still leave a process in an
    /// uninterruptible wait; blocking a close forever is worse than leaving that one
    /// to the kernel.
    async fn reap(&self) {
        let deadline = Instant::now() + REAP_GRACE;
        loop {
            if self.try_reap().await {
                return;
            }
            if Instant::now() >= deadline {
                push_audit(
                    &self.audit,
                    format!("resident.session.reap_deferred.g{}", self.group_id),
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// One non-blocking reap attempt: `true` once the exit status is collected.
    ///
    /// Kept separate from [`Self::reap`] so the lock is never held across an await.
    async fn try_reap(&self) -> bool {
        let mut inner = self.inner.lock().await;
        if !matches!(inner.state, SessionState::Dead(_)) {
            inner.state = SessionState::Dead("closed".to_string());
        }
        if inner.reaped {
            return true;
        }
        match inner.child.try_wait() {
            Ok(Some(_)) => {
                inner.reaped = true;
                true
            }
            // Already collected by an earlier `try_wait`; the entry is gone.
            #[cfg(unix)]
            Err(err) if err.raw_os_error() == Some(nix::libc::ECHILD) => {
                inner.reaped = true;
                true
            }
            Ok(None) | Err(_) => false,
        }
    }

    async fn await_exit(&self, grace: Duration) -> bool {
        let deadline = Instant::now() + grace;
        loop {
            if self.child_exit().await.is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Rebuild the session after a crash.
    ///
    /// Resuming the same conversation is the preferred path — verified on 1.4.7 that
    /// the TUI honours `--conversation` and appends to the same DB. If the resume
    /// spawn cannot reach the idle prompt the session falls back to a fresh
    /// conversation and reports [`RebuildKind::NeedsSeed`], which makes the caller use
    /// the existing rotation/seed path instead of inventing a new one.
    pub async fn rebuild(&self) -> Result<(Self, RebuildKind), ResidentError> {
        let previous = self.conversation_id().await;
        self.close().await;
        push_audit(
            &self.audit,
            format!("resident.session.rebuild_start.g{}", self.group_id),
        );

        if let Some(id) = previous.clone() {
            let config = ResidentConfig {
                bin: self.bin.clone(),
                home: self.home.clone(),
                workspace: self.workspace.clone(),
                model: self.model.clone(),
                conversation_id: Some(id.clone()),
                deliver_timeout_s: self.deliver_timeout.as_secs(),
                memory_scope: self.memory_scope.clone(),
            };
            match ResidentSession::spawn(&config, &self.group_id, Arc::clone(&self.reader)).await {
                Ok(session) => {
                    push_audit(
                        &session.audit,
                        format!("resident.session.rebuild_resume.g{}.{id}", self.group_id),
                    );
                    return Ok((session, RebuildKind::Resume));
                }
                Err(e) => {
                    push_audit(
                        &self.audit,
                        format!(
                            "resident.session.rebuild_resume_failed.g{}.{}",
                            self.group_id,
                            sanitize(&e.to_string())
                        ),
                    );
                }
            }
        }

        let config = ResidentConfig {
            bin: self.bin.clone(),
            home: self.home.clone(),
            workspace: self.workspace.clone(),
            model: self.model.clone(),
            conversation_id: None,
            deliver_timeout_s: self.deliver_timeout.as_secs(),
            memory_scope: self.memory_scope.clone(),
        };
        let session =
            ResidentSession::spawn(&config, &self.group_id, Arc::clone(&self.reader)).await?;
        push_audit(
            &session.audit,
            format!("resident.session.rebuild_seed.g{}", self.group_id),
        );
        Ok((session, RebuildKind::NeedsSeed))
    }
}

mod deliver;
mod launch;
mod screen;
mod steps;

use self::launch::{
    build_interactive_argv, conversations_dir, ensure_queued_messages, queued_messages_intact,
};
use self::screen::{
    digest_of, progress_signal, push_audit, sanitize, screen_snapshot, screen_tail, spawn_reader,
};
use self::steps::{
    DeliveryFloor, UserStepHit, choose_fresh_user_step, chunk_bounds, normalized, step_match_kind,
    step_text_matches, strip_ansi,
};

#[cfg(test)]
mod tests;
