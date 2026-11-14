//! The agy backend of the aux layer: ONE dedicated Agy session that takes
//! over every aux job (judging, image tagging, memory work, flood
//! summaries) when no gateway is configured.
//!
//! Why one session for everything (the operator's design call): the same
//! "mind" sees the pictures, keeps the memory and makes the judgements,
//! so its outputs stay consistent with each other. The price is
//! serialisation — jobs queue behind each other — and callers keep their
//! fail-open behaviour on timeouts, exactly as with an unreachable
//! gateway.
//!
//! Mechanics: the session is an ordinary resident Agy session in its own
//! workspace (`__aux__` next to the per-group project dirs), driven
//! directly — a prompt is delivered, then the conversation's step stream
//! is polled until the model step settles, and its text is the answer.
//! No MCP scope is pinned on it, so the reply tools stay unusable from
//! this session; it can only answer text. Rotation follows the same
//! conversation turn limit as the group sessions.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;

use crate::agent::resident::{DbStepReader, ResidentConfig, ResidentSession};
use crate::agent::step_stream::parse_steps;
use crate::error::ErrorClass;
use crate::integrations::aux_gateway::SUMMARY_SYSTEM;
use crate::integrations::aux_llm::AuxLlm;
use crate::memory::compaction::SummaryModel;

/// Per-call deadlines. Agy turns are far slower than gateway calls, and
/// every caller already fails open, so a deadline expiry costs one aux
/// job, never a group turn.
const TEXT_TIMEOUT: Duration = Duration::from_secs(90);
const VISION_TIMEOUT: Duration = Duration::from_secs(120);
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(150);
const POLL_INTERVAL: Duration = Duration::from_millis(400);
/// Polls with an unchanged step stream before a model text counts as
/// the finished answer.
const SETTLE_POLLS: u8 = 2;

/// Decides when a polled step stream counts as settled on an answer.
///
/// The step *count* can sit still while the model step's text is still
/// streaming in (the row is rewritten in place), so settling on the count
/// alone can return a truncated answer: on 2026-11-06 a compaction summary
/// came back as a half-written JSON and the job failed with
/// `aux_bad_output`. Settling therefore requires the answer text itself to
/// be byte-identical across consecutive polls, with no new steps either.
#[derive(Debug, Default)]
struct SettleTracker {
    last: Option<(usize, String)>,
    stable: u8,
}

impl SettleTracker {
    /// Observe one poll; returns true once the same (event count, answer)
    /// pair has been seen for `SETTLE_POLLS` consecutive polls.
    fn observe(&mut self, event_count: usize, answer: Option<&str>) -> bool {
        let Some(answer) = answer else {
            self.last = None;
            self.stable = 0;
            return false;
        };
        let snap = (event_count, answer.to_string());
        if self.last.as_ref() == Some(&snap) {
            self.stable += 1;
        } else {
            self.last = Some(snap);
            self.stable = 0;
        }
        self.stable >= SETTLE_POLLS
    }
}

#[derive(Debug, Clone)]
pub struct AgyAuxConfig {
    pub bin: PathBuf,
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub model: String,
    /// Rotate the aux conversation after this many answered jobs.
    pub turn_limit: i64,
}

/// The session-driving seam: production drives a real resident session,
/// tests drive a fake.
#[async_trait]
trait AuxDriver: Send + std::fmt::Debug {
    async fn ask(&mut self, prompt: &str, timeout: Duration) -> Result<String, ErrorClass>;
}

struct ResidentDriver {
    config: AgyAuxConfig,
    session: Option<ResidentSession>,
    turns: i64,
    /// True while a delivered prompt has not yet settled cleanly. If an outer
    /// caller (`tokio::time::timeout` in preflight) cancels `ask` mid-turn,
    /// this flag stays `true` so the next call interrupts and drops the
    /// half-finished session instead of pasting on top of a running turn.
    in_flight: bool,
}

impl std::fmt::Debug for ResidentDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResidentDriver")
            .field("turns", &self.turns)
            .field("session_live", &self.session.is_some())
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

impl ResidentDriver {
    fn new(config: AgyAuxConfig) -> Self {
        Self {
            config,
            session: None,
            turns: 0,
            in_flight: false,
        }
    }

    async fn ensure_session(&mut self) -> Result<(), ErrorClass> {
        if self.in_flight {
            if let Some(session) = &self.session {
                let _ = session.interrupt_generation(Duration::from_secs(3)).await;
            }
            self.drop_session().await;
        }
        if self.session.is_some() {
            return Ok(());
        }
        let workspace = self.config.workspace.clone();
        tokio::fs::create_dir_all(&workspace)
            .await
            .map_err(|_| ErrorClass::Unavailable)?;
        let agents_md = workspace.join("AGENTS.md");
        if !agents_md.exists() {
            let _ = tokio::fs::write(
                &agents_md,
                "You are a utility model serving another program. Answer each \
                 request exactly as instructed, with no extra commentary. \
                 When image files are referenced with @path, look at them. \
                 Never call reply or memory tools.\n",
            )
            .await;
        }
        let config = ResidentConfig {
            bin: self.config.bin.clone(),
            home: self.config.home.clone(),
            workspace,
            model: self.config.model.clone(),
            conversation_id: None,
            deliver_timeout_s: 45,
            memory_scope: None,
        };
        let session = ResidentSession::spawn(&config, "aux", Arc::new(DbStepReader))
            .await
            .map_err(|_| ErrorClass::Unavailable)?;
        self.session = Some(session);
        self.turns = 0;
        Ok(())
    }

    async fn drop_session(&mut self) {
        if let Some(session) = self.session.take() {
            session.close().await;
        }
        self.turns = 0;
        self.in_flight = false;
    }
}

#[async_trait]
impl AuxDriver for ResidentDriver {
    async fn ask(&mut self, prompt: &str, timeout: Duration) -> Result<String, ErrorClass> {
        self.ensure_session().await?;
        let session = self.session.as_ref().expect("session ensured");
        self.in_flight = true;
        let receipt = match session.deliver(prompt).await {
            Ok(receipt) => receipt,
            Err(_) => {
                self.drop_session().await;
                return Err(ErrorClass::Unavailable);
            }
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let mut settle = SettleTracker::default();
        loop {
            if tokio::time::Instant::now() >= deadline {
                let _ = session.interrupt_generation(Duration::from_secs(3)).await;
                self.drop_session().await;
                return Err(ErrorClass::AuxFailed);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            let parsed = match parse_steps(&receipt.db_path).await {
                Ok(parsed) => parsed,
                Err(_) => continue,
            };
            let mut answer: Option<String> = None;
            for event in &parsed.events {
                if event.idx <= receipt.step_idx {
                    continue;
                }
                if event.is_kind("error") {
                    self.drop_session().await;
                    return Err(ErrorClass::AuxFailed);
                }
                if event.is_kind("agent_text") {
                    answer = Some(event.text.clone());
                }
            }
            if settle.observe(parsed.events.len(), answer.as_deref()) {
                self.in_flight = false;
                self.turns += 1;
                let answer = answer.expect("settled answer");
                if self.turns >= self.config.turn_limit.max(1) {
                    self.drop_session().await;
                }
                return Ok(answer);
            }
        }
    }
}

/// The aux provider backed by the single Agy session.
impl std::fmt::Debug for AgyAuxClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgyAuxClient")
            .field("workspace", &self.workspace)
            .field("degraded", &self.degraded.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

pub struct AgyAuxClient {
    workspace: PathBuf,
    driver: tokio::sync::Mutex<Box<dyn AuxDriver>>,
    degraded: AtomicBool,
    image_seq: std::sync::atomic::AtomicU64,
}

impl AgyAuxClient {
    pub fn new(config: AgyAuxConfig) -> Self {
        let workspace = config.workspace.clone();
        Self {
            workspace,
            driver: tokio::sync::Mutex::new(Box::new(ResidentDriver::new(config))),
            degraded: AtomicBool::new(false),
            image_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    fn with_driver(workspace: PathBuf, driver: Box<dyn AuxDriver>) -> Self {
        Self {
            workspace,
            driver: tokio::sync::Mutex::new(driver),
            degraded: AtomicBool::new(false),
            image_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    async fn ask(&self, prompt: &str, timeout: Duration) -> Result<String, ErrorClass> {
        let result = self.driver.lock().await.ask(prompt, timeout).await;
        self.degraded.store(result.is_err(), Ordering::Relaxed);
        result
    }

    /// Decode one data URL into a workspace file and return its path.
    async fn stage_image(&self, data_url: &str) -> Result<PathBuf, ErrorClass> {
        let (meta, payload) = data_url
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(";base64,"))
            .ok_or(ErrorClass::AuxBadOutput)?;
        let ext = match meta {
            "image/jpeg" => "jpg",
            "image/png" => "png",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "bin",
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .map_err(|_| ErrorClass::AuxBadOutput)?;
        tokio::fs::create_dir_all(&self.workspace)
            .await
            .map_err(|_| ErrorClass::Unavailable)?;
        let seq = self.image_seq.fetch_add(1, Ordering::Relaxed);
        let path = self.workspace.join(format!("aux-img-{seq}.{ext}"));
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|_| ErrorClass::Unavailable)?;
        Ok(path)
    }
}

#[async_trait]
impl AuxLlm for AgyAuxClient {
    async fn complete_streaming(
        &self,
        system: &str,
        prompt: &str,
        _session: Option<&str>,
    ) -> Result<String, ErrorClass> {
        self.ask(&format!("{system}\n\n{prompt}"), TEXT_TIMEOUT)
            .await
    }

    async fn complete_vision_on(
        &self,
        _model: &str,
        system: &str,
        prompt: &str,
        images: &[String],
        _session: Option<&str>,
    ) -> Result<String, ErrorClass> {
        // A staged image the session cannot open would silently shift the
        // caller's positional mapping, so a bad data URL fails the call —
        // the caller's per-image fallback marks that picture untagged.
        let mut refs = String::new();
        for url in images {
            let path = self.stage_image(url).await?;
            refs.push_str(&format!("\n@{}", path.display()));
        }
        let full = format!("{system}\n\n{prompt}\n\n本轮图片（逐个查看）：{refs}");
        self.ask(&full, VISION_TIMEOUT).await
    }

    async fn complete_summary(&self, prompt: &str) -> Result<String, ErrorClass> {
        self.ask(&format!("{SUMMARY_SYSTEM}\n\n{prompt}"), SUMMARY_TIMEOUT)
            .await
    }

    fn mode_name(&self) -> &'static str {
        "agy"
    }

    async fn healthy(&self) -> bool {
        !self.degraded.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl SummaryModel for AgyAuxClient {
    async fn summarise(&self, prompt: String) -> Result<String, ErrorClass> {
        self.complete_summary(&prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Debug)]
    struct FakeDriver {
        prompts: Arc<StdMutex<Vec<String>>>,
        reply: String,
    }

    #[async_trait]
    impl AuxDriver for FakeDriver {
        async fn ask(&mut self, prompt: &str, _timeout: Duration) -> Result<String, ErrorClass> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            Ok(self.reply.clone())
        }
    }

    fn client(reply: &str) -> (AgyAuxClient, tempfile::TempDir, Arc<StdMutex<Vec<String>>>) {
        let dir = tempfile::tempdir().expect("tmp");
        let prompts = Arc::new(StdMutex::new(Vec::new()));
        let client = AgyAuxClient::with_driver(
            dir.path().to_path_buf(),
            Box::new(FakeDriver {
                prompts: Arc::clone(&prompts),
                reply: reply.to_string(),
            }),
        );
        (client, dir, prompts)
    }

    #[tokio::test]
    async fn streaming_prepends_the_system_text() {
        let (client, _dir, prompts) = client("verdict");
        let out = client
            .complete_streaming("SYS", "judge this", None)
            .await
            .expect("ok");
        assert_eq!(out, "verdict");
        assert_eq!(
            prompts.lock().unwrap().as_slice(),
            ["SYS\n\njudge this".to_string()]
        );
    }

    #[tokio::test]
    async fn summary_uses_the_pinned_summary_system() {
        let (client, _dir, prompts) = client("{}");
        client.complete_summary("fold this").await.expect("ok");
        let prompts = prompts.lock().unwrap();
        assert!(prompts[0].starts_with(SUMMARY_SYSTEM));
        assert!(prompts[0].ends_with("fold this"));
    }

    #[tokio::test]
    async fn vision_stages_images_as_files() {
        let (client, dir, prompts) = client("[]");
        // 1x1 transparent PNG.
        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
        client
            .complete_vision_on("any", "SYS", "tag", &[png.to_string()], None)
            .await
            .expect("ok");
        let staged = dir.path().join("aux-img-0.png");
        assert!(staged.exists(), "image staged into the workspace");
        let prompts = prompts.lock().unwrap();
        assert!(prompts[0].contains(&format!("@{}", staged.display())));
    }

    #[tokio::test]
    async fn vision_rejects_a_bad_data_url() {
        let (client, _dir, _prompts) = client("[]");
        let err = client
            .complete_vision_on("any", "SYS", "tag", &["not-a-data-url".to_string()], None)
            .await
            .expect_err("must fail");
        assert_eq!(err, ErrorClass::AuxBadOutput);
    }
}

#[cfg(test)]
mod settle_tests {
    use super::*;

    #[test]
    fn growing_text_does_not_settle() {
        let mut t = SettleTracker::default();
        assert!(!t.observe(5, Some("{\"summary\":\"本群")));
        // Text still streaming in under the same step count: must not settle.
        assert!(!t.observe(5, Some("{\"summary\":\"本群是以高达为核心的交流群\"}")));
        assert!(!t.observe(5, Some("{\"summary\":\"本群是以高达为核心的交流群\"}")));
        assert!(t.observe(5, Some("{\"summary\":\"本群是以高达为核心的交流群\"}")));
    }

    #[test]
    fn new_steps_reset_settling() {
        let mut t = SettleTracker::default();
        assert!(!t.observe(5, Some("done")));
        assert!(!t.observe(6, Some("done")));
        assert!(!t.observe(6, Some("done")));
        assert!(t.observe(6, Some("done")));
    }

    #[test]
    fn missing_answer_resets_settling() {
        let mut t = SettleTracker::default();
        assert!(!t.observe(5, Some("done")));
        assert!(!t.observe(5, None));
        assert!(!t.observe(5, Some("done")));
        assert!(!t.observe(5, Some("done")));
        assert!(t.observe(5, Some("done")));
    }
}
