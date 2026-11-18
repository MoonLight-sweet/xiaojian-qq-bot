//! Folding the raw group log into `memory_chunks`.
//!
//! The raw log (`group_messages`) is written on intake and never deleted. This module
//! is the only thing that reads it in bulk: when the unarchived tail grows past
//! [`TRIGGER_MESSAGES`], the oldest window is folded into one summary by the cheap
//! Aux model and the folded rows are flagged `archived = 1`.
//!
//! Two rules shape everything here:
//!
//! * **Never delete.** Archiving flips a flag. A failed or bad summary leaves the rows
//!   unarchived, so the next run retries the same window instead of losing it.
//! * **Never leak ids.** The prompt carries group-local aliases only; the validator
//!   rejects a summary containing digit runs or CQ markup.

use std::sync::Arc;

use crate::error::ErrorClass;
use crate::memory::compaction::{SummaryModel, leaks_identifier};
use crate::store::{GroupMessageRecord, SqliteStore};

/// Fold once the unarchived tail exceeds this many rows.
pub const TRIGGER_MESSAGES: i64 = 300;

/// Always leave at least this many recent rows unarchived: the cadence gate and
/// `memory_search` read them. The agent prompt itself only sees the current batch.
pub const KEEP_RECENT: i64 = 100;

/// Upper bound on one fold, so a long-neglected group does not produce a single
/// enormous request.
pub const MAX_FOLD_MESSAGES: i64 = 200;

/// Bounds on one chunk summary.
pub const MIN_CHUNK_CHARS: usize = 40;
pub const MAX_CHUNK_CHARS: usize = 1_500;

/// Pinned system prompt, byte-identical on every call to keep the prefix cache warm.
pub const CHUNK_SYSTEM: &str =
    "You fold a window of QQ group chat into one dense memory note. Output JSON only.";

/// Why a fold did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldSkip {
    /// The unarchived tail is still below the trigger.
    BelowTrigger,
    /// The store refused a read.
    StoreError,
    /// The summary model call itself failed (Aux unreachable or errored).
    ModelError,
    /// The model answered, but the summary was unusable. The payload says
    /// why: "parse" (no summary block in the reply), "too_short",
    /// "too_long" or "leak" (validation refusals).
    BadSummary(&'static str),
}

impl FoldSkip {
    pub fn as_str(self) -> &'static str {
        match self {
            FoldSkip::BelowTrigger => "below_trigger",
            FoldSkip::StoreError => "store",
            FoldSkip::ModelError => "model",
            FoldSkip::BadSummary(_) => "bad_summary",
        }
    }

    /// Audit tag: [`Self::as_str`] with the bad-summary sub-reason appended.
    /// A bare "bad_summary" bucket hid for weeks whether the model had
    /// stopped emitting the block or the block kept failing validation —
    /// the two have opposite fixes.
    pub fn audit_tag(self) -> String {
        match self {
            FoldSkip::BadSummary(reason) => format!("bad_summary.{reason}"),
            other => other.as_str().to_string(),
        }
    }
}

/// What one fold attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldOutcome {
    pub chunk_id: String,
    pub seq: i64,
    pub folded_messages: i64,
    pub summary: String,
    /// What the extraction pass harvested from the same response. Always present, and
    /// always harmless: a fold is a success even when this says `Absent`.
    pub extracted: crate::memory::extract::ExtractionReport,
}

/// A tail older than this folds even below [`TRIGGER_MESSAGES`]: without an age
/// trigger, a group chatting steadily under the count trigger (or a quiet group
/// that never reaches it) keeps its oldest hours out of chunks and extraction
/// indefinitely — 2026-10-27: one group went two hours of heavy chat with zero
/// new memories because the count trigger had not tripped.
pub const TRIGGER_AGE_SECONDS: i64 = 2_700;

/// Decide how many of the oldest rows to fold right now. Pure so it can be tested
/// without a database: the caller passes the current unarchived count and the
/// age of the oldest unarchived row.
pub fn fold_size(unarchived: i64, oldest_age_seconds: Option<i64>) -> i64 {
    if unarchived <= KEEP_RECENT {
        return 0;
    }
    let over_trigger = unarchived > TRIGGER_MESSAGES;
    let stale = oldest_age_seconds.is_some_and(|age| age >= TRIGGER_AGE_SECONDS);
    if !over_trigger && !stale {
        return 0;
    }
    (unarchived - KEEP_RECENT).clamp(1, MAX_FOLD_MESSAGES)
}

/// Render the fold prompt. The instructions come first and never vary; only the
/// message window changes, so the cacheable prefix stays intact.
pub fn render_fold_prompt(messages: &[GroupMessageRecord], aliases: &[(String, String)]) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str(
        "Fold the messages below into one memory note for a group bot.\n\
         Rules:\n\
         - Keep: who decided what, facts about people, ongoing plans, running jokes, \
         anything a participant would expect the bot to remember.\n\
         - Drop: greetings, filler, repeated noise, anything with no later use.\n\
         - Address people by the alias shown in brackets, never by a number.\n\
         - Write in the language the group used.\n\
         - No ids, no markup. Never copy a line label (m1, m2, …) into the note.\n\
         - Lines tagged [bot] are the assistant Gemini itself — a participant in \
         the chat, not a human member. Never attribute its lines to a member or \
         call it one; in the note, call it Gemini.\n\
         Respond with {\"summary\": \"<note>\"}.\n\n",
    );
    if !aliases.is_empty() {
        // Only the aliases themselves go out. Printing the alias-to-id mapping here
        // would hand the model exactly the identifiers this module exists to hide.
        out.push_str("People (refer to them this way):\n");
        for (alias, _raw_id) in aliases {
            out.push_str("- ");
            out.push_str(alias);
            out.push('\n');
        }
        out.push_str("- bot (the assistant itself, Gemini — not a human member)\n");
        out.push('\n');
    }
    out.push_str("Messages:\n");
    for (index, message) in messages.iter().enumerate() {
        // 1-based line numbers, so a memory candidate can cite `m3` and the server
        // resolves it back to a row id it already knows it folded. The number is a
        // position, not an identifier: it reveals nothing about the real message.
        out.push('m');
        out.push_str(&(index + 1).to_string());
        out.push_str(". ");
        out.push('[');
        out.push_str(&alias_for(message, aliases));
        out.push_str("] ");
        if message.has_image {
            out.push_str("[image] ");
        }
        out.push_str(message.text.trim());
        out.push('\n');
    }
    out.push('\n');
    out.push_str(
        // Extraction shares this call: a second round trip to the same model would
        // double the cost of the most frequent background job for no new information.
        "Also list the durable facts worth remembering long-term, separately from the \
         note. Only facts a participant would expect the bot to still know weeks later: \
         stable preferences, lasting relations between people, decided plans, things that \
         already happened. Never greetings, filler, or anything already obvious from the \
         note.\n\
         Each item: {\"kind\": one of fact|preference|topic|relation|event, \
         \"subject\": the alias of the person it is about (required for any item about a person; \"\" only for facts about the group as a whole) or \"\" for the whole group, \
         \"content\": one sentence under 200 characters, \
         \"confidence\": 0.0-0.8, \
         \"source_refs\": [the line labels, e.g. \"m3\"]}.\n\
         An item about the assistant itself uses subject \"bot\". An item about a \
         person MUST carry that person's alias as subject — if you cannot tell \
         who it is about, drop the item instead of guessing, and never write the \
         placeholder \"member\" in content; refer to people by their alias.\n\
         Skip jokes, hyperbole, memes, irony, roleplay, copy-paste repetition and \
         one-off banter: a claim the group is laughing at or repeating as a meme \
         is not a fact, even when phrased like one. Distinguish 'someone said X' \
         from 'X is objectively true'. Skip the \
         assistant's own persona, catchphrases and settings — that is \
         configuration, not a group fact.\n\
         At most 6 items. Use an empty array when nothing qualifies.\n\
         Respond with {\"summary\": \"<note>\", \"memories\": [...]}.\n",
    );
    out
}

/// Map a message onto the alias its sender is shown under. The bot's own lines
/// are tagged `bot`, never `member`: folding them in as an anonymous member is
/// what produced summaries like "a member calls herself 本小姐" about the
/// assistant itself (2026-10-27). Unmapped humans are shown as `member` rather
/// than by id, because the id must not reach the model.
fn alias_for(message: &GroupMessageRecord, aliases: &[(String, String)]) -> String {
    if message.speaker_role == "bot" {
        return "bot".to_string();
    }
    aliases
        .iter()
        .find(|(_, raw)| raw == &message.sender_id)
        .map(|(alias, _)| alias.clone())
        .unwrap_or_else(|| "member".to_string())
}

/// Pull the summary out of the model's JSON, tolerating fenced output.
pub fn parse_chunk_summary(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let candidate = if trimmed.starts_with('{') {
        trimmed
    } else {
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        if end <= start {
            return None;
        }
        &trimmed[start..=end]
    };
    let value: serde_json::Value = serde_json::from_str(candidate).ok()?;
    let text = value
        .get("summary")
        .or_else(|| value.get("summary_text"))
        .and_then(|v| v.as_str())?
        .trim()
        .to_string();
    Some(text)
}

/// Validate a candidate summary against the length and leak rules.
pub fn validate_chunk(text: &str) -> Option<String> {
    let chars = text.chars().count();
    if !(MIN_CHUNK_CHARS..=MAX_CHUNK_CHARS).contains(&chars) {
        return None;
    }
    if leaks_identifier(text) {
        return None;
    }
    Some(text.to_string())
}

/// Why [`validate_chunk`] refused a summary, for the fold audit trail.
/// Mirrors its checks in the same order.
pub fn chunk_reject_reason(text: &str) -> &'static str {
    let chars = text.chars().count();
    if chars < MIN_CHUNK_CHARS {
        "too_short"
    } else if chars > MAX_CHUNK_CHARS {
        "too_long"
    } else if leaks_identifier(text) {
        "leak"
    } else {
        "invalid"
    }
}

/// Runner for one group.
#[derive(Debug, Clone)]
pub struct Chunker {
    store: Arc<SqliteStore>,
    /// Aux backend that produced the fold (see `Extractor::with_origin`).
    origin: &'static str,
    /// Model label recorded on committed chunks.
    model_label: String,
}

impl Chunker {
    pub fn new(store: Arc<SqliteStore>) -> Self {
        Self {
            store,
            origin: crate::store::origin::HY3,
            model_label: crate::config::DEFAULT_AUX_MODEL.to_string(),
        }
    }

    pub fn with_origin(mut self, origin: &'static str) -> Self {
        self.origin = origin;
        self
    }

    pub fn with_model_label(mut self, model_label: String) -> Self {
        self.model_label = model_label;
        self
    }

    /// Fold one window if the group is past the trigger.
    ///
    /// `model` and `now` are injected so tests can run without a network or a clock.
    pub async fn fold_once(
        &self,
        bot_id: &str,
        group_id: &str,
        model: &Arc<dyn SummaryModel>,
        aliases: Vec<(String, String)>,
        now: i64,
    ) -> Result<Option<FoldOutcome>, FoldSkip> {
        let unarchived = self
            .store
            .unarchived_message_count(bot_id, group_id)
            .await
            .map_err(|_| FoldSkip::StoreError)?;
        // The age trigger needs the oldest tail row's timestamp; peeking one
        // row is cheaper than a dedicated query shape.
        let oldest_age = if unarchived > KEEP_RECENT {
            self.store
                .oldest_unarchived_messages(bot_id, group_id, 1)
                .await
                .ok()
                .and_then(|rows| rows.first().map(|m| now - m.created_at))
        } else {
            None
        };
        let size = fold_size(unarchived, oldest_age);
        if size == 0 {
            return Ok(None);
        }
        let window = self
            .store
            .oldest_unarchived_messages(bot_id, group_id, size)
            .await
            .map_err(|_| FoldSkip::StoreError)?;
        if window.is_empty() {
            return Ok(None);
        }

        let prompt = render_fold_prompt(&window, &aliases);
        let raw = model
            .summarise(prompt)
            .await
            .map_err(|_: ErrorClass| FoldSkip::ModelError)?;
        let text = parse_chunk_summary(&raw).ok_or(FoldSkip::BadSummary("parse"))?;
        let summary = validate_chunk(&text)
            .ok_or_else(|| FoldSkip::BadSummary(chunk_reject_reason(&text)))?;

        let seq = self
            .store
            .next_chunk_seq(bot_id, group_id)
            .await
            .map_err(|_| FoldSkip::StoreError)?;
        let chunk_id = format!("chunk_{group_id}_{seq}");
        let from_msg_id = window.first().map(|m| m.id).unwrap_or_default();
        let to_msg_id = window.last().map(|m| m.id).unwrap_or_default();
        let ids: Vec<i64> = window.iter().map(|m| m.id).collect();

        self.store
            .commit_chunk(
                &chunk_id,
                bot_id,
                group_id,
                seq,
                from_msg_id,
                to_msg_id,
                &summary,
                &self.model_label,
                now,
                &ids,
            )
            .await
            .map_err(|_| FoldSkip::StoreError)?;

        // Extraction runs after the chunk is committed and on the same raw response.
        // It can only add rows; nothing here can undo the fold above.
        let extracted = crate::memory::extract::Extractor::new(Arc::clone(&self.store))
            .with_origin(self.origin)
            .run(bot_id, group_id, &window, &raw, &aliases, now)
            .await;

        Ok(Some(FoldOutcome {
            chunk_id,
            seq,
            folded_messages: ids.len() as i64,
            summary,
            extracted,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: i64, sender: &str, text: &str) -> GroupMessageRecord {
        GroupMessageRecord {
            id,
            bot_id: "9999".into(),
            group_id: "500".into(),
            message_id: format!("m{id}"),
            sender_id: sender.into(),
            display_name: Some("tester".into()),
            speaker_role: "member".into(),
            text: text.into(),
            has_image: false,
            sticker_hash: None,
            sub_type: None,
            image_tags: None,
            created_at: id,
            archived: false,
        }
    }

    #[test]
    fn below_trigger_folds_nothing() {
        assert_eq!(fold_size(TRIGGER_MESSAGES, None), 0);
        assert_eq!(fold_size(0, None), 0);
        // Below the count trigger AND fresh: still nothing.
        assert_eq!(fold_size(KEEP_RECENT + 50, Some(600)), 0);
    }

    #[test]
    fn fold_keeps_the_recent_tail() {
        // Past the trigger the oldest rows are folded, capped so one request stays
        // bounded; the newest KEEP_RECENT rows always survive unarchived.
        assert_eq!(fold_size(TRIGGER_MESSAGES + 20, None), MAX_FOLD_MESSAGES);
        assert_eq!(fold_size(TRIGGER_MESSAGES + 500, None), MAX_FOLD_MESSAGES);
        assert!(fold_size(TRIGGER_MESSAGES + 20, None) <= TRIGGER_MESSAGES + 20 - KEEP_RECENT);
    }

    #[test]
    fn stale_tail_folds_below_the_count_trigger() {
        // The age trigger: a modest tail whose oldest row is old folds its
        // excess over KEEP_RECENT, so extraction stops lagging the chat.
        let size = fold_size(KEEP_RECENT + 30, Some(TRIGGER_AGE_SECONDS + 1));
        assert_eq!(size, 30);
        // But the recent tail is never touched, however old it is.
        assert_eq!(fold_size(KEEP_RECENT, Some(TRIGGER_AGE_SECONDS * 10)), 0);
    }

    #[test]
    fn prompt_tags_the_bot_as_bot_never_member() {
        let mut bot_line = record(2, "9999", "本小姐才不吃香菜");
        bot_line.speaker_role = "bot".into();
        let prompt = render_fold_prompt(
            &[record(1, "42", "你吃香菜吗"), bot_line],
            &[("u1".to_string(), "42".to_string())],
        );
        assert!(prompt.contains("[bot] 本小姐才不吃香菜"), "got: {prompt}");
        assert!(
            prompt.contains("- bot (the assistant itself"),
            "got: {prompt}"
        );
        assert!(!prompt.contains("[member]"), "got: {prompt}");
    }

    #[test]
    fn prompt_maps_senders_to_aliases_and_hides_ids() {
        let prompt = render_fold_prompt(
            &[record(1, "2000000002", "明天上午十点开会")],
            &[("u1".to_string(), "2000000002".to_string())],
        );
        assert!(prompt.contains("[u1] 明天上午十点开会"));
        assert!(!prompt.contains("2000000002"));
        let instructions = prompt.find("Fold the messages").expect("说明");
        let body = prompt.find("[u1]").expect("正文");
        assert!(instructions < body);
    }

    #[test]
    fn unmapped_sender_stays_anonymous() {
        let prompt = render_fold_prompt(&[record(1, "777", "hi")], &[]);
        assert!(prompt.contains("[member] hi"));
        assert!(!prompt.contains("777"));
    }

    #[test]
    fn summary_parses_from_fenced_json() {
        let raw = "```json\n{\"summary\": \"讨论了下周的计划。\"}\n```";
        assert_eq!(
            parse_chunk_summary(raw).as_deref(),
            Some("讨论了下周的计划。")
        );
    }

    #[test]
    fn summary_rejects_leaks_and_extremes() {
        let long = "字".repeat(MAX_CHUNK_CHARS + 1);
        assert!(validate_chunk(&long).is_none());
        assert!(validate_chunk("太短").is_none());
        assert!(validate_chunk("提到 2000000002 这个人").is_none());
        assert!(
            validate_chunk(
                "他们约好周末一起去看电影，顺便讨论了行程安排和谁去买票的事，最后定在下午两点集合。"
            )
            .is_some()
        );
    }

    struct StubModel(Result<String, ErrorClass>);

    #[async_trait::async_trait]
    impl SummaryModel for StubModel {
        async fn summarise(&self, _prompt: String) -> Result<String, ErrorClass> {
            self.0.clone()
        }
    }

    /// A temp store whose only group sits one message past the fold trigger.
    async fn seeded_chunker() -> (Chunker, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("bot.sqlite3");
        let pool = crate::store::migrations::open_and_migrate(&path)
            .await
            .expect("迁移");
        let store = Arc::new(SqliteStore::new(pool));
        store.ensure_group("9999", "500", 0).await.expect("建群");
        for i in 0..=TRIGGER_MESSAGES {
            store
                .insert_group_message(
                    "9999",
                    "500",
                    &format!("m{i}"),
                    "42",
                    None,
                    "member",
                    "一条普通群聊消息，用来把未归档尾巴顶过折叠触发线。",
                    false,
                    None,
                    None,
                    i,
                )
                .await
                .expect("写消息");
        }
        (Chunker::new(store), dir)
    }

    #[tokio::test]
    async fn fold_once_distinguishes_model_error_from_bad_summary() {
        let (chunker, _dir) = seeded_chunker().await;

        let failing: Arc<dyn SummaryModel> = Arc::new(StubModel(Err(ErrorClass::AuxFailed)));
        let result = chunker
            .fold_once("9999", "500", &failing, vec![], 1_000)
            .await;
        assert_eq!(result, Err(FoldSkip::ModelError));

        let garbage: Arc<dyn SummaryModel> = Arc::new(StubModel(Ok("不是 JSON".into())));
        let result = chunker
            .fold_once("9999", "500", &garbage, vec![], 1_001)
            .await;
        assert_eq!(result, Err(FoldSkip::BadSummary("parse")));

        let good_summary = "大家这周聊了攒机、周末聚餐和下个月的旅行计划，几件事都定了时间和负责人，还约好下周继续跟进后续的安排和预算情况。";
        let good: Arc<dyn SummaryModel> = Arc::new(StubModel(Ok(
            serde_json::json!({ "summary": good_summary }).to_string(),
        )));
        let outcome = chunker
            .fold_once("9999", "500", &good, vec![], 1_002)
            .await
            .expect("应折叠")
            .expect("应有结果");
        assert_eq!(outcome.folded_messages, MAX_FOLD_MESSAGES);
    }

    #[test]
    fn malformed_model_output_is_not_a_summary() {
        assert!(parse_chunk_summary("我觉得挺好").is_none());
        assert!(parse_chunk_summary("{\"note\":\"x\"}").is_none());
    }

    #[test]
    fn the_prompt_asks_for_candidates_in_the_same_call() {
        let prompt = render_fold_prompt(&[record(1, "42", "明天开会")], &[]);
        assert!(prompt.contains("\"memories\""), "必须同批请求候选段");
        // Line labels are how a candidate cites the window it came from.
        assert!(prompt.contains("m1. "), "每条消息需要行号前缀");
        assert!(prompt.contains("source_refs"));
    }

    #[tokio::test]
    async fn a_broken_candidate_block_still_leaves_a_finished_fold() {
        let (chunker, _dir) = seeded_chunker().await;
        // Valid summary, unusable candidates: the fold is a success and extraction
        // reports the breakage instead of failing the whole pass.
        let summary = "大家这周聊了攒机、周末聚餐和下个月的旅行计划，几件事都定了时间和负责人，还约好下周继续跟进后续的安排和预算情况。";
        let body = serde_json::json!({ "summary": summary, "memories": "not an array" });
        let model: Arc<dyn SummaryModel> = Arc::new(StubModel(Ok(body.to_string())));
        let outcome = chunker
            .fold_once("9999", "500", &model, vec![], 1_003)
            .await
            .expect("候选段损坏不得让 fold 失败")
            .expect("应有结果");
        assert_eq!(outcome.summary, summary);
        assert_eq!(
            outcome.extracted.status,
            Some(crate::memory::extract::CandidateParseStatus::Broken)
        );
        assert_eq!(outcome.extracted.created, 0);
    }

    #[tokio::test]
    async fn a_missing_candidate_block_still_leaves_a_finished_fold() {
        let (chunker, _dir) = seeded_chunker().await;
        let summary = "大家这周聊了攒机、周末聚餐和下个月的旅行计划，几件事都定了时间和负责人，还约好下周继续跟进后续的安排和预算情况。";
        let body = serde_json::json!({ "summary": summary });
        let model: Arc<dyn SummaryModel> = Arc::new(StubModel(Ok(body.to_string())));
        let outcome = chunker
            .fold_once("9999", "500", &model, vec![], 1_004)
            .await
            .expect("缺失候选段不得让 fold 失败")
            .expect("应有结果");
        assert_eq!(outcome.summary, summary);
        assert_eq!(
            outcome.extracted.status,
            Some(crate::memory::extract::CandidateParseStatus::Absent)
        );
    }
}
