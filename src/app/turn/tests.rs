use super::*;
use crate::onebot::normalize::SpeakerRole;
use std::sync::atomic::{AtomicUsize, Ordering};

struct FixtureFetcher {
    bytes: Vec<u8>,
    calls: AtomicUsize,
}

/// Canned describer: the image path with the network taken out.
///
/// It answers exactly what the real tagger would, in the same JSON shape, so the
/// code under test is `parse_tags` + `resolve_kind` + ingest — not a test copy.
struct CannedDescriber {
    lines: Vec<serde_json::Value>,
    calls: AtomicUsize,
}

impl CannedDescriber {
    fn new(ocr: &str, description: &str) -> Self {
        Self {
            lines: vec![serde_json::json!({
                "kind": "sticker",
                "ocr": ocr,
                "description": description,
                "emotion": "无语",
                "self": "other",
                "nsfw": false,
            })],
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl crate::app::preflight::ImageDescriber for CannedDescriber {
    async fn describe(
        &self,
        _bot_id: &str,
        _group_id: &str,
        images: &[String],
    ) -> Vec<crate::app::preflight::ImageTag> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        // Round-trip through the real parser instead of building both sides by hand:
        // a schema drift then fails here rather than in production.
        assert!(!images.is_empty(), "描述器只该在真的有图时被调用");
        let payload = serde_json::json!({ "images": self.lines }).to_string();
        crate::app::preflight::parse_tags(&payload)
    }
}

#[async_trait::async_trait]
impl ImageFetcher for FixtureFetcher {
    async fn fetch(
        &self,
        _url: &str,
        _max_bytes: u64,
    ) -> Result<crate::media::download::DownloadedImage, crate::media::download::MediaError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(crate::media::download::DownloadedImage {
            bytes: self.bytes.clone(),
            content_type: "image/png".to_string(),
        })
    }
}

fn message(ref_id: &str, speaker: &str, text: &str, images: usize) -> IncomingMessage {
    let mut segments = vec![Segment::Text {
        text: text.to_string(),
    }];
    for _ in 0..images {
        segments.push(Segment::Image {
            url: None,
            file: None,
            sub_type: None,
        });
    }
    IncomingMessage {
        bot_id: "1".into(),
        group_id: "100".into(),
        message_id: ref_id.into(),
        sender_id: "555".into(),
        speaker_ref: speaker.into(),
        speaker_role: SpeakerRole::Member,
        display_name: String::new(),
        known_alias: None,
        received_at: 1,
        segments,
        text: text.to_string(),
        mentions_bot: false,
        quote_of: None,
    }
}

#[tokio::test]
async fn image_bundle_is_bounded_and_never_reaches_the_agent_as_pixels() {
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        8,
        6,
        image::Rgba([40, 80, 120, 255]),
    ))
    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    .expect("encode fixture png");
    let fetcher = FixtureFetcher {
        bytes: png,
        calls: AtomicUsize::new(0),
    };
    let mut messages = vec![
        message("raw-1", "u1", "first", 2),
        message("raw-2", "u2", "second", 3),
    ];
    for (message_index, message) in messages.iter_mut().enumerate() {
        let mut image_index = 0;
        for segment in &mut message.segments {
            if let Segment::Image { url, .. } = segment {
                image_index += 1;
                *url = Some(format!(
                    "https://media.example/m{}/{}.png",
                    message_index + 1,
                    image_index
                ));
            }
        }
    }

    // No gate configured: nothing may be described, and no pixels may be staged.
    let (images, tagged) = prepare_image_context(
        &fetcher,
        &messages,
        1024 * 1024,
        1_000_000,
        None,
        "1",
        "100",
    )
    .await;

    // Five declared images, but the per-call cap is four.
    assert_eq!(fetcher.calls.load(Ordering::Relaxed), 4);
    assert!(images.lines.is_empty());
    assert_eq!(images.unavailable, 5);
    assert!(tagged.is_empty());
}

#[test]
fn prompt_refs_are_batch_local_and_sequential() {
    let messages = vec![
        message("9001", "u1", "你好", 0),
        message("9002", "u2", "在吗", 1),
    ];
    let (batch, scope) = build_prompt_batch(
        &messages,
        crate::memory::retrieval::MemoryContext::empty(),
        None,
        None,
        None,
        PromptImages::default(),
        Vec::new(),
        &std::collections::HashMap::new(),
    );
    assert_eq!(batch.messages[0].message_ref, "m1");
    assert_eq!(batch.messages[1].message_ref, "m2");
    assert_eq!(scope.message_refs, vec!["m1", "m2"]);
    assert_eq!(scope.speaker_refs, vec!["u1", "u2"]);
    // Raw ids never appear.
    let rendered = render(&batch);
    assert!(!rendered.contains("9001"));
    assert!(!rendered.contains("555"));
    // Images are counted.
    assert_eq!(batch.messages[1].images, 1);
}

/// Arrival times reach the prompt as `[HH:MM]` prefixes, and the batch's own
/// clock line is left to `prepare` (which knows the assembly moment).
#[test]
fn batch_messages_carry_their_arrival_time_into_the_prompt() {
    let mut first = message("9001", "u1", "早", 0);
    // 2026-10-28 09:05 Asia/Shanghai.
    first.received_at = 1_793_149_500;
    let mut second = message("9002", "u2", "午", 0);
    second.received_at = 1_793_149_500 + 3_600;
    let (batch, _scope) = build_prompt_batch(
        &[first, second],
        crate::memory::retrieval::MemoryContext::empty(),
        None,
        None,
        None,
        PromptImages::default(),
        Vec::new(),
        &std::collections::HashMap::new(),
    );
    assert_eq!(batch.messages[0].received_at, 1_793_149_500);
    assert_eq!(batch.now_line, None, "组批不管时钟，prepare 才盖时间");
    let rendered = render(&batch);
    assert!(rendered.contains("[09:05] "), "got: {rendered}");
    assert!(rendered.contains("[10:05] "), "got: {rendered}");
    assert!(!rendered.contains("现在："), "got: {rendered}");
}

#[test]
fn quote_notes_name_in_batch_refs_and_summarize_older_ones() {
    let mut quoting = message("9002", "u2", "对，就是这个", 0);
    quoting.quote_of = Some("9001".into());
    let mut old_quote = message("9003", "u1", "那之前说的呢", 0);
    old_quote.quote_of = Some("5555".into());
    let mut unknown_quote = message("9004", "u1", "还有更早的", 0);
    unknown_quote.quote_of = Some("6666".into());
    let messages = vec![
        message("9001", "u1", "今晚吃火锅", 0),
        quoting,
        old_quote,
        unknown_quote,
    ];
    let summaries =
        std::collections::HashMap::from([("5555".to_string(), "上周就定好了去吃火锅".to_string())]);
    let (batch, _) = build_prompt_batch(
        &messages,
        crate::memory::retrieval::MemoryContext::empty(),
        None,
        None,
        None,
        PromptImages::default(),
        Vec::new(),
        &summaries,
    );
    // In-batch: named by ref so she can answer with quote_ref: "m1".
    assert_eq!(batch.messages[1].text, "对，就是这个（回复 m1）");
    // Older but logged: the summary rides along.
    assert_eq!(
        batch.messages[2].text,
        "那之前说的呢（回复消息“上周就定好了去吃火锅”）"
    );
    // Never logged: a plain marker, no invented content.
    assert_eq!(batch.messages[3].text, "还有更早的（回复了一条历史消息）");
    // Unquoted messages are untouched.
    assert_eq!(batch.messages[0].text, "今晚吃火锅");
}

#[test]
fn duplicate_speakers_collapse_in_the_scope() {
    let messages = vec![message("1", "u1", "a", 0), message("2", "u1", "b", 0)];
    let (_, scope) = build_prompt_batch(
        &messages,
        crate::memory::retrieval::MemoryContext::empty(),
        None,
        None,
        None,
        PromptImages::default(),
        Vec::new(),
        &std::collections::HashMap::new(),
    );
    assert_eq!(scope.speaker_refs, vec!["u1"]);
    assert_eq!(scope.message_refs.len(), 2);
}

#[test]
fn payload_parsing_ignores_corrupt_rows() {
    let good = serde_json::to_string(&message("1", "u1", "hi", 0)).expect("序列化");
    let payloads = vec![good, "not json".to_string()];
    let messages = messages_from_payloads(&payloads);
    assert_eq!(messages.len(), 1);
}

#[test]
fn skipped_and_deferred_map_to_distinct_outcomes() {
    assert_eq!(TurnResult::SkippedInactive.outcome(), ExecOutcome::Done);
    assert_eq!(TurnResult::DeferredProxy.outcome(), ExecOutcome::Deferred);
    assert_eq!(TurnResult::Unknown.outcome(), ExecOutcome::Halted);
    assert_eq!(TurnResult::AgentFailed.outcome(), ExecOutcome::Done);
}

#[test]
fn running_turns_registry_tracks_registered_groups() {
    let registry = RunningTurns::new();
    assert!(registry.snapshot().is_empty());

    registry.register("100", 1234);
    assert_eq!(registry.snapshot(), vec![("100".to_string(), 1234)]);
    assert!(registry.contains("100"));
    assert!(!registry.contains("200"));

    registry.deregister("100");
    assert!(registry.snapshot().is_empty());
}

#[test]
fn the_conversation_cap_is_off_unless_explicitly_set() {
    // Agy compacts its own transcript, so the default must not recycle a session.
    assert_eq!(CONVERSATION_TURN_LIMIT, 0);
    assert_eq!(conversation_turn_limit(0), None);
    // A typo can never silently start recycling a session the operator meant to keep.
    assert_eq!(conversation_turn_limit(-5), None);
    assert_eq!(conversation_turn_limit(30), Some(30));
}

// ---- resident (P4) wiring ----

#[test]
fn rotation_is_counted_per_delivered_round() {
    // No limit configured: never rotate.
    assert!(!rotation_due(1_000, None));
    assert!(!rotation_due(34, Some(35)));
    assert!(rotation_due(35, Some(35)));
    assert!(rotation_due(36, Some(35)));
}

#[tokio::test]
async fn conversation_rotates_after_the_turn_limit() {
    let dir = tempfile::tempdir().expect("tmp");
    let (mut executor, store) = executor_fixture(&dir).await;
    Arc::get_mut(&mut executor)
        .expect("唯一引用")
        .conversation_turn_limit
        .store(2, std::sync::atomic::Ordering::Relaxed);
    store
        .set_conversation_id("1", "100", Some("conv-keep"), 1)
        .await
        .expect("会话");
    let now = crate::now_unix();
    store
        .bump_agy_turn_count("1", "100", now)
        .await
        .expect("计数");
    let count = store
        .bump_agy_turn_count("1", "100", now)
        .await
        .expect("计数");
    assert_eq!(count, 2);
    executor.rotate_after_delivered_round("100", count, 0).await;
    let group = store
        .get_group("1", "100")
        .await
        .expect("读")
        .expect("存在");
    assert_eq!(group.agy_conversation_id, None, "轮换后应换新会话");
    assert_eq!(group.agy_turn_count, 0, "轮换后计数清零");
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a == "ops.conversation.rotated.g100.after2"),
        "缺轮换审计"
    );
}

#[tokio::test]
async fn a_resident_insert_is_held_when_the_group_has_it_disabled() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    // Inserts off: the paced default.
    store
        .set_group_flags("1", "100", None, None, Some(false), 2)
        .await
        .expect("开关");
    let group = store.get_group("1", "100").await.unwrap().unwrap();
    assert!(!group.resident_insert_enabled);
    // With inserts disabled, `insert::decide` must hold even for a mention.
    let decision = crate::app::insert::decide(
        &crate::app::insert::InsertSignals {
            mentions_bot: true,
            ..Default::default()
        },
        &crate::app::insert::InsertLimits {
            enabled: group.resident_insert_enabled,
            tier: Tier::T0,
            inserts_so_far: 0,
            max_inserts: executor.resident_limits.max_inserts,
            turn_age_s: 0,
            soft_cap_s: executor.resident_limits.soft_cap_s,
        },
    );
    assert_eq!(
        decision.reason_str(),
        "disabled",
        "开关关闭时插入必须被 hold"
    );
    assert!(!decision.is_accept());
}

// ---- resident respawn backoff ----

/// A group whose resident session was just retired stays silent and defers: the
/// batch keeps its place in the queue until the backoff deadline passes.
#[tokio::test]
async fn a_group_in_respawn_backoff_defers_instead_of_running() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let now = crate::now_unix();
    let (tier, until) = executor.resident.note_respawn_failure("100", now).await;
    assert_eq!(tier, 1, "第一次退役落在梯子第一级");
    assert_eq!(until, now + 60, "第一级只等一分钟");
    let mut event = message("m1", "u1", "在吗", 0);
    event.received_at = now;
    let job = claimed_job(&store, &[event]).await;

    assert_eq!(executor.run_batch(&job).await, TurnResult::DeferredProxy);
    let actions = audit_actions(&store).await;
    assert!(
        actions
            .iter()
            .any(|a| a == &format!("resident.turn.deferred_backoff.g100.until{until}")),
        "退避期必须留审计: {actions:?}"
    );
    assert!(
        !actions.iter().any(|a| a.starts_with("agent.")),
        "退避期不得真的发起 agent 调用: {actions:?}"
    );
    // The batch went back to the pool rather than through an agent.
    let pending = store.pending_for_group("1", "100").await.expect("读");
    assert_eq!(pending.len(), 1, "批次必须保持 pending");
}

/// With the cooldown switch off a retirement arms nothing: no tier,
/// no deadline, nothing for the panel to report — the operator's answer
/// to a group muted by repeated content-filter retirements
/// (2026-11-03, a production group).
#[tokio::test]
async fn with_the_cooldown_switch_off_a_retirement_arms_no_backoff() {
    let dir = tempfile::tempdir().expect("tmp");
    let pool = crate::store::migrations::open_and_migrate(&dir.path().join("bot.sqlite3"))
        .await
        .expect("迁移");
    let store = Arc::new(crate::store::SqliteStore::new(pool));
    let manager = crate::app::resident::ResidentManager::new(
        Arc::clone(&store),
        "1".into(),
        dir.path().join("agy"),
        dir.path().join("home"),
        "model".into(),
    )
    .with_respawn_backoff_enabled(false);
    let now = crate::now_unix();
    let (tier, until) = manager.note_respawn_failure("100", now).await;
    assert_eq!(tier, 0, "开关关闭时退役不进梯子");
    assert_eq!(until, now, "开关关闭时不买任何等待");
    assert!(
        !manager.backoff_active("100", now).await,
        "开关关闭时不得有生效的退避"
    );
    assert!(
        manager.backoff_until("100", now).await.is_none(),
        "开关关闭时没有待等期限"
    );
    assert!(
        manager.active_backoffs(now).await.is_empty(),
        "开关关闭时面板不得报退避"
    );
}

/// While the deadline is live the group is not eligible — that is the whole
/// point of the backoff — and once it passes the group is eligible again
/// without any operator action and without the turn path writing anything.
#[tokio::test]
async fn an_expired_respawn_backoff_puts_the_group_back_without_a_write() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let now = crate::now_unix();
    let manager = &executor.resident;
    // Inside the first rung the group may not run.
    let (_, until) = manager.note_respawn_failure("100", now).await;
    let group = store.get_group("1", "100").await.unwrap().unwrap();
    assert!(
        !executor.resident_eligible(&group, now).await,
        "退避期内 resident_eligible 必须为假"
    );
    assert!(
        !executor.resident_eligible(&group, until - 1).await,
        "退避期最后一秒仍然为假"
    );
    // The deadline expires on its own; nothing writes it back.
    assert!(
        executor.resident_eligible(&group, until).await,
        "截止时间过后必须自动放回"
    );
    assert!(
        manager.backoff_until("100", until).await.is_none(),
        "过了截止时间就没有待等的期限了"
    );
}

/// A ladder that only ever climbs would park a group at the ceiling forever.
/// Ten delivered rounds clear the record outright, so the next retirement starts
/// at the bottom again.
#[tokio::test]
async fn ten_clean_rounds_clear_the_respawn_ladder() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, _store) = executor_fixture(&dir).await;
    let manager = &executor.resident;
    // Walk two rungs up, using a `now` far enough back that the deadlines are
    // already in the past and clean rounds are allowed to count.
    let past = crate::now_unix() - 7_200;
    manager.note_respawn_failure("100", past).await;
    let (tier, _) = manager.note_respawn_failure("100", past).await;
    assert_eq!(tier, 2);
    for _ in 0..(crate::app::resident::RESPAWN_CLEAN_ROUNDS - 1) {
        manager.note_respawn_clean("100").await;
    }
    assert!(
        !manager.backoff_active("100", crate::now_unix()).await,
        "截止时间已过就不再算退避"
    );
    manager.note_respawn_clean("100").await;
    // The record is gone, so the next failure is tier 1 again.
    let (tier, until) = manager.note_respawn_failure("100", crate::now_unix()).await;
    assert_eq!(tier, 1, "十轮干净之后必须从第一级重新开始");
    assert_eq!(
        until,
        crate::now_unix() + crate::app::resident::RESPAWN_BACKOFF_STEPS[0]
    );
}

/// Clean rounds inside the backoff window are not evidence: the session is
/// about to be retired, so counting them would clear the wait early.
#[tokio::test]
async fn clean_rounds_during_the_backoff_do_not_count() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, _store) = executor_fixture(&dir).await;
    let manager = &executor.resident;
    let now = crate::now_unix();
    manager.note_respawn_failure("100", now).await;
    for _ in 0..(crate::app::resident::RESPAWN_CLEAN_ROUNDS * 2) {
        manager.note_respawn_clean("100").await;
    }
    assert!(
        manager.backoff_active("100", now).await,
        "退避期内干净轮不得提前解封"
    );
}

/// The ladder climbs one rung per consecutive retirement, then stops at
/// thirty minutes: a permanently broken group costs two spawns an hour instead
/// of one per flush tick.
#[tokio::test]
async fn the_respawn_ladder_climbs_to_its_ceiling_and_stays_there() {
    use crate::app::resident::RESPAWN_BACKOFF_STEPS;
    assert_eq!(RESPAWN_BACKOFF_STEPS, [60, 300, 900, 1800]);
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, _store) = executor_fixture(&dir).await;
    let manager = &executor.resident;
    // Each retirement is booked at the same instant, so the deadline is
    // `now + step` and the step is readable straight off the delta.
    let now = crate::now_unix();
    let mut seen = Vec::new();
    for _ in 0..6 {
        let (tier, until) = manager.note_respawn_failure("100", now).await;
        seen.push((tier, until - now));
    }
    assert_eq!(
        seen,
        vec![
            (1, 60),
            (2, 300),
            (3, 900),
            (4, 1_800),
            (5, 1_800),
            (6, 1_800),
        ],
        "梯子必须逐级加长并在 1800 秒封顶"
    );
    // And the ceiling is a wait, not a lifetime ban: eligibility returns the
    // moment the deadline passes, one second earlier is still a wait.
    assert!(
        manager.backoff_active("100", now + 1_799).await,
        "封顶之后仍然是退避期"
    );
    assert!(
        !manager.backoff_active("100", now + 1_800).await,
        "到点即放行，包含边界"
    );
}

#[test]
fn replay_preview_flattens_separators_and_truncates() {
    assert_eq!(replay_preview("你好 世界.喵"), "你好_世界_喵");
    let long: String = std::iter::repeat_n('a', 60).collect();
    assert_eq!(replay_preview(&long).chars().count(), 48);
}

#[test]
fn resident_limits_default_to_the_documented_budget() {
    let limits = ResidentLimits::default();
    assert_eq!(limits.quiet_s, 5.0);
    assert_eq!(limits.soft_cap_s, 45);
    assert_eq!(limits.hard_cap_s, 90);
    assert_eq!(limits.max_inserts, 3);
    assert_eq!(limits.poll_ms, 500);
    assert_eq!(limits.deliver_timeout_s, 15);
    assert!(!limits.insert_esc);
    // Hysteresis: a turn must be able to absorb work for less time than the
    // point at which it is killed, or the soft cap would be meaningless.
    assert!(limits.soft_cap_s < limits.hard_cap_s);
}

#[test]
fn insert_boost_only_fires_with_a_real_queue_behind_the_turn() {
    let base = ResidentLimits::default();
    // Below the threshold nothing changes.
    assert_eq!(boosted_limits(base, 0), base);
    assert_eq!(boosted_limits(base, INSERT_BOOST_PENDING - 1), base);
    // At the threshold the turn gets more seats and a longer soft cap —
    // and nothing else moves (hard cap and quiet window stay put).
    let boosted = boosted_limits(base, INSERT_BOOST_PENDING);
    assert_eq!(boosted.max_inserts, INSERT_BOOST_MAX_INSERTS);
    assert_eq!(boosted.soft_cap_s, INSERT_BOOST_SOFT_CAP_S);
    assert_eq!(boosted.hard_cap_s, base.hard_cap_s);
    assert_eq!(boosted.quiet_s, base.quiet_s);
    // The boost is a floor, not a cap: an operator who configured wider
    // limits keeps them.
    let wide = ResidentLimits {
        max_inserts: 9,
        soft_cap_s: 75,
        ..base
    };
    assert_eq!(boosted_limits(wide, INSERT_BOOST_PENDING + 5), wide);
}

// ---- send-time freshness gate / quote fallback / rotation ----

#[test]
fn send_gate_boundaries_are_strictly_over() {
    assert!(!send_gate_drop(SEND_GATE_MAX_AGE_SECONDS, 0));
    assert!(!send_gate_drop(0, SEND_GATE_MAX_NEWER_MESSAGES));
    assert!(!send_gate_drop(45, 3));
    assert!(send_gate_drop(SEND_GATE_MAX_AGE_SECONDS + 1, 0));
    assert!(send_gate_drop(0, SEND_GATE_MAX_NEWER_MESSAGES + 1));
}

fn anchor_item(ref_id: &str, sender: &str, images: usize, quote_of: Option<&str>) -> AnchorItem {
    AnchorItem {
        message_ref: ref_id.into(),
        message_id: format!("id-{ref_id}"),
        sender_id: sender.into(),
        image_count: images,
        quote_of: quote_of.map(str::to_string),
        received_at: Some(100),
    }
}

#[test]
fn anchor_prefers_a_focused_common_quote_target() {
    let items = vec![anchor_item("m1", "u1", 0, Some("oldpost"))];
    assert_eq!(
        batch_anchor(&items),
        Some(BatchAnchor::Quoted {
            target_id: "oldpost".into(),
            via_ref: "m1".into()
        })
    );
    // A stray quote inside a big unrelated batch is not an anchor.
    let mut items: Vec<AnchorItem> = (1..=10)
        .map(|i| anchor_item(&format!("m{i}"), "u1", 0, None))
        .collect();
    items[3].quote_of = Some("oldpost".into());
    assert_eq!(batch_anchor(&items), None);
}

#[test]
fn anchor_uses_first_image_of_a_single_speaker() {
    let items = vec![
        anchor_item("m1", "u1", 0, None),
        anchor_item("m2", "u1", 1, None),
        anchor_item("m3", "u1", 1, None),
    ];
    match batch_anchor(&items) {
        Some(BatchAnchor::Image {
            message_id,
            message_ref,
            ..
        }) => {
            assert_eq!(message_id, "id-m2");
            assert_eq!(message_ref, "m2");
        }
        other => panic!("应为图片锚点，得到 {other:?}"),
    }
    // Two speakers' pictures: no single anchor.
    let mixed = vec![
        anchor_item("m1", "u1", 1, None),
        anchor_item("m2", "u2", 1, None),
    ];
    assert_eq!(batch_anchor(&mixed), None);
}

#[test]
fn quote_guard_drops_a_stale_sender_in_a_multi_sender_batch() {
    // The misquote shape: the quote would pin an older message from a
    // different sender than the batch's newest message.
    assert_eq!(
        quote_drop_reason("u1", false, false, "u2", false, None, false, false),
        Some("not_latest")
    );
    // Same sender as the newest message: a thread she is visibly in.
    assert_eq!(
        quote_drop_reason("u1", false, false, "u1", false, None, false, false),
        None
    );
    // The newest message itself always passes the sender rule.
    assert_eq!(
        quote_drop_reason("u1", true, false, "u2", false, None, false, false),
        None
    );
}

#[test]
fn quote_guard_drops_resident_quotes_on_a_hot_thread() {
    // 17s after her last send — the 2026-10-27 lag cases ran 17–37s.
    assert_eq!(
        quote_drop_reason("u1", true, false, "u1", true, Some(17), false, false),
        Some("hot_thread")
    );
    // A quiet gap re-arms quotes.
    assert_eq!(
        quote_drop_reason("u1", true, false, "u1", true, Some(120), false, false),
        None
    );
    // No previous send on record: nothing to lag behind.
    assert_eq!(
        quote_drop_reason("u1", true, false, "u1", true, None, false, false),
        None
    );
}

#[test]
fn quote_guard_drops_seeded_turns_and_owner_addressee_mismatches() {
    // A seeded (fresh-conversation) turn drops even a perfect quote:
    // its content answers the brief, not the batch.
    assert_eq!(
        quote_drop_reason("u9", true, true, "u9", true, Some(999), false, true),
        Some("cold_seed")
    );
    // The 15:15 case: words open by addressing the owner, the named
    // target is the newest message but belongs to a member.
    assert_eq!(
        quote_drop_reason("u2", true, false, "u2", false, None, true, false),
        Some("addressee_mismatch")
    );
    // Addressing the owner while quoting the owner's own message is
    // the legitimate shape and survives.
    assert_eq!(
        quote_drop_reason("u9", true, true, "u2", false, None, true, false),
        None
    );
    assert!(segment_addresses_owner("怎么啦主人？是引用又抽风了"));
    assert!(segment_addresses_owner("主人在琢磨什么呢？"));
    assert!(!segment_addresses_owner("杂鱼没有 root 权限"));
    assert!(!segment_addresses_owner(
        "这还用数？群里名字带鱼的笨笨鱼算一个"
    ));
}

#[test]
fn quote_echo_check_requires_a_verbatim_snippet() {
    // The 15:15 target: an echo from its own text passes, punctuation
    // differences are ignored.
    assert_eq!(
        quote_echo_check(Some("0.25回复频率"), "我这条是0.25回复频率"),
        Ok(())
    );
    // Words from a different message (the seed thread she was actually
    // answering) do not appear in the named target: mismatch.
    assert_eq!(
        quote_echo_check(Some("引用功能"), "我这条是0.25回复频率"),
        Err("echo_mismatch")
    );
    // No echo at all, or a one-character shrug: no evidence, no quote.
    assert_eq!(
        quote_echo_check(None, "我这条是0.25回复频率"),
        Err("echo_missing")
    );
    assert_eq!(
        quote_echo_check(Some("是"), "我这条是0.25回复频率"),
        Err("echo_missing")
    );
    // An image-only target has no text to check against.
    assert_eq!(quote_echo_check(None, ""), Ok(()));
}

#[test]
fn replay_guard_catches_the_seed_echo() {
    let priors = vec![normalize_for_replay("都下锅了还想着水群冲浪呢！")];
    assert!(replay_match("都下锅了还想着水群冲浪呢！", &priors).is_some());
    // A swapped kaomoji tail is still the same line.
    let priors = vec![normalize_for_replay(
        "那本猫给你接根网线，这下总跑不掉了吧 (=｀ω´=)",
    )];
    assert!(replay_match("那本猫给你接根网线，这下总跑不掉了吧 (¬‿¬)", &priors).is_some());
    // A short tail appended to a re-sent line is still a replay.
    let priors = vec![normalize_for_replay("杂鱼攻击力甚至不如路边野草")];
    assert!(replay_match("杂鱼攻击力甚至不如路边野草笑死", &priors).is_some());
}

#[test]
fn replay_guard_catches_a_truncated_prefix() {
    // 2026-10-27 live pair: the half line went out first, the completed
    // line a minute later. Dice scores them 0.76 and the length ratio is
    // 0.63, so only the mid-sentence prefix rule catches it.
    let priors = vec![normalize_for_replay("又在长篇大论念你的青蛙经，字多")];
    assert!(
        replay_match(
            "又在长篇大论念你的青蛙经，字多得本猫看着都犯困喵 ( ´_ゝ｀)",
            &priors
        )
        .is_some()
    );
    // The reverse direction (full first, truncation after) is caught too.
    let priors = vec![normalize_for_replay(
        "又在长篇大论念你的青蛙经，字多得本猫看着都犯困喵",
    )];
    assert!(replay_match("又在长篇大论念你的青蛙经，字多", &priors).is_some());
    // A short line that actually ends its sentence is a fresh reply,
    // not a truncation, even when a longer line starts the same way.
    let priors = vec![normalize_for_replay("这条线我先睡了。")];
    assert!(replay_match("这条线我先睡了。明天再战", &priors).is_none());
}

#[test]
fn replay_guard_lets_fresh_and_short_lines_through() {
    let priors = vec![normalize_for_replay("都下锅了还想着水群冲浪呢！")];
    assert!(replay_match("这还用数？群里名字带鱼的笨笨鱼算一个", &priors).is_none());
    // Short reactions repeat naturally and are exempt on both sides.
    assert!(replay_match("草", &priors).is_none());
    assert!(replay_match("好呀好呀", &[normalize_for_replay("好呀好呀")]).is_none());
}

#[test]
fn an_insert_renders_plain_lines_without_message_refs() {
    let mut first = message("c1", "u1", "第一行\n第二行", 0);
    first.display_name = "Ai".into();
    let mut second = message("c2", "u2", "", 0);
    second.mentions_bot = true;
    second.display_name = "  ".into();
    let rendered = render_insert_lines(&[first, second]);
    assert_eq!(rendered, "- Ai：第一行 第二行\n- 群友：（点了你）\n");
    assert!(!rendered.contains("m1"));
    assert!(!rendered.contains("c1"));
}

#[test]
fn an_insert_waits_until_the_prompt_marker_is_back() {
    assert!(!screen_is_idle_for_insert("generating a reply"));
    assert!(screen_is_idle_for_insert("done\n? for shortcuts"));
}

#[test]
fn a_refusal_is_silence_and_a_real_reply_is_not() {
    let scope = crate::agent::contract::ValidationScope::default();
    assert!(delivered_text_is_silence(
        r#"{"should_reply": false}"#,
        &scope
    ));
    assert!(!delivered_text_is_silence(
        r#"{"should_reply": true, "reply_parts": ["在"]}"#,
        &scope
    ));
}

#[test]
fn only_an_addressed_round_owes_an_answer() {
    let scope = crate::agent::contract::ValidationScope::default();
    let declined = RoundShape::of(r#"{"should_reply": false}"#, &scope);
    let replied = RoundShape::of(r#"{"should_reply": true, "reply_parts": ["在"]}"#, &scope);
    // The 2026-10-28 shape: the owner asked a question, the batch was claimed
    // with priority, and the turn ended on a decline.
    assert!(declined.addressed_silence(true));
    // An ambient batch may be left alone; that is the whole point of the switch.
    assert!(!declined.addressed_silence(false));
    // A real answer is an answer whoever asked.
    assert!(!replied.addressed_silence(true));
    assert!(!replied.silent);
    assert!(declined.silent);
}

/// A round that only validated through the repair ladder is not the same round as
/// a clean one, even though both pass validation.
#[test]
fn a_repaired_round_counts_as_degraded() {
    let scope = crate::agent::contract::ValidationScope::default();
    // Prose around the document: the strict path refuses it, L1 repairs it.
    let repaired = RoundShape::of(
        "想好了\n```json\n{\"should_reply\":true,\"reply_parts\":[\"收到\"]}\n```",
        &scope,
    );
    assert!(repaired.salvaged, "这段文字只能靠抢救通过");
    assert!(!repaired.silent);
    assert!(repaired.degraded(), "抢救过的轮次不计入健康证据");
    // A clean answer is neither repaired nor silent.
    let clean = RoundShape::of(r#"{"should_reply":true,"reply_parts":["收到"]}"#, &scope);
    assert!(!clean.salvaged);
    assert!(!clean.degraded());
    // And a silence is degraded whether or not it needed repairing: the round
    // produced nothing the group can read.
    let declined = RoundShape::of(r#"{"should_reply":false}"#, &scope);
    assert!(declined.degraded());
}

/// An ambient salvaged silence is still accepted (nobody asked), but it stops
/// counting as evidence that the session is healthy — which is what lets the
/// degraded window catch a session that only ever repairs and declines.
#[test]
fn an_ambient_salvaged_silence_is_accepted_but_recorded_as_degraded() {
    let scope = crate::agent::contract::ValidationScope::default();
    // A decline that also needed repairing: the worst of both.
    let mut shape = RoundShape::of("想好了\n```json\n{\"should_reply\":false}\n```", &scope);
    assert!(shape.silent && shape.salvaged);
    // Nobody aimed a batch at the bot, so this silence is allowed to stand.
    assert!(!shape.addressed_silence(false));
    assert!(shape.degraded());
    // Same round, but addressed: now it owes an answer.
    shape.silent = true;
    assert!(shape.addressed_silence(true));
}

/// The flag rides on the batch, so both ways a batch lands in a resident turn
/// count: the initial claim, and an insert absorbed mid-turn.
#[test]
fn one_addressed_batch_makes_the_whole_turn_addressed() {
    let delivery = |batch: &str, privileged: bool| ResidentDelivery {
        batch_id: batch.into(),
        event_ids: vec![1],
        newest: 100,
        privileged,
    };
    // The claim itself was addressed (a mention claimed its own batch).
    let claimed = ResidentCoverage {
        batches: vec![delivery("b1", true)],
    };
    assert!(claimed.any_privileged());
    // The turn started ambient and a mention arrived while the model was already
    // answering: the absorbed batch still makes the turn addressed.
    let absorbed = ResidentCoverage {
        batches: vec![delivery("b1", false), delivery("b2", true)],
    };
    assert!(absorbed.any_privileged());
    // Nothing was addressed: an ambient turn keeps its freedom to decline.
    let ambient = ResidentCoverage {
        batches: vec![delivery("b1", false), delivery("b2", false)],
    };
    assert!(!ambient.any_privileged());
    // A turn that covered no batch at all is ambient by definition.
    assert!(!ResidentCoverage::default().any_privileged());
}

#[test]
fn only_empty_and_malformed_contracts_retire_the_session() {
    assert!(failure_retires_session("contract_invalid.empty_reply"));
    assert!(failure_retires_session("contract_invalid.malformed"));
    assert!(!failure_retires_session("contract_invalid.too_long"));
}

#[test]
fn submission_nudge_due_on_the_first_miss_of_any_turn() {
    assert!(submission_nudge_due(false));
    assert!(!submission_nudge_due(true));
}

#[test]
fn retry_arm_due_only_for_a_first_death_in_an_opted_in_group() {
    assert!(retry_arm_due(false, true));
    assert!(!retry_arm_due(true, true), "a replay never re-arms");
    assert!(!retry_arm_due(false, false), "group must be opted in");
    assert!(!retry_arm_due(true, false));
}

#[test]
fn first_turn_nudge_prompt_asks_for_the_channel_contract() {
    let mcp = first_turn_nudge_prompt(crate::config::OutputChannel::Mcp);
    assert!(mcp.contains("submit_reply"));
    assert!(mcp.contains("turn_token"));
    assert!(!mcp.contains("不要执行任何工具"));
    let text = first_turn_nudge_prompt(crate::config::OutputChannel::Text);
    assert!(text.contains("JSON"));
    assert!(!text.contains("submit_reply"));
}

#[test]
fn prompt_batch_renders_an_anchor_hint() {
    let mut quoting = message("c1", "u1", "有点气动狗的感觉", 0);
    quoting.quote_of = Some("oldpost".into());
    let (batch, _) = build_prompt_batch(
        &[quoting],
        crate::memory::retrieval::MemoryContext::empty(),
        None,
        None,
        None,
        PromptImages::default(),
        Vec::new(),
        &std::collections::HashMap::new(),
    );
    let hint = batch.anchor_hint.clone().expect("应有引用提示");
    assert!(hint.contains("m1"), "提示应点名句柄：{hint}");
    let rendered = crate::agent::prompt::render(&batch);
    assert!(rendered.contains("# 引用提示"), "render 应含提示节");
}

/// An addressed batch that comes back as a decline is accepted as a decline:
/// exactly one call, no second attempt, and its own audit row so the rate of
/// dropped questions stays queryable. The old behaviour re-ran the batch once on
/// a fresh conversation, which doubled the cost of every shy answer and bought
/// nothing a log line could not.
#[tokio::test]
async fn an_addressed_decline_is_accepted_without_a_retry() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("m1", "u1", "114514是什么梗", 0);
    event.mentions_bot = true;
    event.received_at = crate::now_unix();
    let mut job = claimed_job(&store, &[event]).await;
    job.privileged = true;
    assert_eq!(
        finish_with(&executor, &store, &job, DECLINE_CONTRACT).await,
        TurnResult::Silent
    );
    let actions = audit_actions(&store).await;
    assert!(
        actions
            .iter()
            .any(|a| a == "agent.silent.addressed.g100.declined"),
        "点名轮的被拒静默必须留痕：{actions:?}"
    );
    assert!(
        !actions.iter().any(|a| a == "agent.silent.g100.declined"),
        "点名轮的静默不能记成普通静默：{actions:?}"
    );
    assert!(
        !actions
            .iter()
            .any(|a| a.starts_with("agent.silent.addressed.g100.retry")),
        "重试那条路已经拆掉，不得再出现 retry 审计：{actions:?}"
    );
    assert!(all_outbox_rows(&store).await.is_empty(), "拒了就没有可发的");
    // The single attempt is a closed call: a declining answer is not left
    // `running` just because it was a silence.
    let running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_calls WHERE bot_id = '1' AND group_id = '100' AND state = 'running'",
        )
        .fetch_one(store.pool())
        .await
        .expect("读调用状态");
    assert_eq!(running, 0, "不得留下悬挂的 running 调用");
}

/// The single addressed call is booked to the privileged pool, so it cannot
/// quietly spend the ambient budget of the group.
#[tokio::test]
async fn an_addressed_decline_is_booked_as_a_privileged_call() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("m1", "u1", "在吗", 0);
    event.mentions_bot = true;
    event.received_at = crate::now_unix();
    let mut job = claimed_job(&store, &[event]).await;
    job.privileged = true;
    let _ = finish_with(&executor, &store, &job, DECLINE_CONTRACT).await;
    let since = crate::now_unix() - 60;
    let privileged = store
        .count_agent_calls_since_by_priority("1", "100", since, true)
        .await
        .expect("读优先级记账");
    assert_eq!(privileged, 1, "点名轮的唯一一次调用记在优先池");
    let ambient = store
        .count_agent_calls_since_by_priority("1", "100", since, false)
        .await
        .expect("读 ambient 记账");
    assert_eq!(ambient, 0, "点名轮不得扣 ambient 池");
}

/// An ambient batch keeps its freedom to decline: nothing is retried, and the
/// silence is audited as an ordinary one.
#[tokio::test]
async fn an_ambient_decline_is_accepted_without_a_retry() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("m1", "u1", "今天天气不错", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    assert_eq!(
        finish_with(&executor, &store, &job, DECLINE_CONTRACT).await,
        TurnResult::Silent
    );
    let actions = audit_actions(&store).await;
    assert!(
        actions.iter().any(|a| a == "agent.silent.g100.declined"),
        "普通静默沿用原审计动作：{actions:?}"
    );
    assert!(
        !actions
            .iter()
            .any(|a| a.starts_with("agent.silent.addressed")),
        "ambient 轮不得出现点名静默痕迹：{actions:?}"
    );
}

/// A reply whose body is intact but whose side sections are unusable now goes out
/// instead of failing the whole turn, and the rescue leaves its own audit row.
#[tokio::test]
async fn a_salvageable_answer_is_delivered_and_audited() {
    let dir = tempfile::tempdir().expect("tmp");
    // Prose around the document and a memory op aimed outside the batch: the
    // strict path can do nothing with this buffer, L1 + L3 can.
    let raw = "想好了\n```json\n{\"should_reply\":true,\"reply_parts\":[\"收到，看下记录\"],\"memory_ops\":[{\"op\":\"add\",\"content\":\"喜欢猫\",\"kind\":\"fact\",\"source_refs\":[\"m404\"]}]}\n```";
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("m1", "u1", "在吗", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    assert_eq!(
        finish_with(&executor, &store, &job, raw).await,
        TurnResult::Replied
    );
    let rows = all_outbox_rows(&store).await;
    assert_eq!(rows.len(), 1, "回复主体完好就必须发出去");
    assert!(rows[0].text.contains("收到"), "{}", rows[0].text);
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a.starts_with("agent.salvaged.g100.")),
        "被抢救的轮次必须留审计"
    );
}

/// A poke the group can see is fired exactly once, after the reply is queued,
/// and leaves its own audit row. The fixture's socket is never connected, so
/// delivery is honestly unknown — the point here is that the attempt happened
/// and that it was recorded, not that OneBot answered.
#[tokio::test]
async fn a_valid_poke_ref_fires_one_send_poke_and_is_audited() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("m1", "u1", "在吗", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    let contract = r#"{"should_reply":true,"reply_parts":["喂，理我一下"],"poke_ref":"u1"}"#;
    assert_eq!(
        finish_with(&executor, &store, &job, contract).await,
        TurnResult::Replied
    );
    let pokes: Vec<String> = audit_actions(&store)
        .await
        .into_iter()
        .filter(|a| a.starts_with("agent.poke."))
        .collect();
    assert_eq!(pokes.len(), 1, "一轮只能戳一次：{pokes:?}");
    assert!(
        pokes[0].starts_with("agent.poke.unknown.g100.refu1")
            || pokes[0].starts_with("agent.poke.sent.g100.refu1"),
        "戳的审计要带上目标 ref：{pokes:?}"
    );
    // The reply itself is unaffected by the poke's fate.
    assert_eq!(all_outbox_rows(&store).await.len(), 1);
}

/// A ref that is not a speaker of this batch never reaches the wire: validation
/// drops it, so there is not even a poke audit row to find.
#[tokio::test]
async fn an_unknown_poke_ref_never_fires() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("m1", "u1", "在吗", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    // `u9` is a plausible-looking speaker ref that is not in this batch.
    let contract = r#"{"should_reply":true,"reply_parts":["喂"],"poke_ref":"u9"}"#;
    assert_eq!(
        finish_with(&executor, &store, &job, contract).await,
        TurnResult::Replied
    );
    let pokes: Vec<String> = audit_actions(&store)
        .await
        .into_iter()
        .filter(|a| a.starts_with("agent.poke."))
        .collect();
    assert!(pokes.is_empty(), "非法 ref 不得发戳：{pokes:?}");
    // …and the reply still goes out.
    assert_eq!(all_outbox_rows(&store).await.len(), 1);
}

/// A silent turn cannot poke: validation drops the ref, the turn stays silent,
/// and nothing is poked.
#[tokio::test]
async fn a_silent_turn_does_not_poke() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("m1", "u1", "在吗", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    let contract = r#"{"should_reply":false,"poke_ref":"u1"}"#;
    assert_eq!(
        finish_with(&executor, &store, &job, contract).await,
        TurnResult::Silent
    );
    assert!(
        !audit_actions(&store)
            .await
            .iter()
            .any(|a| a.starts_with("agent.poke.")),
        "静默轮不得戳人"
    );
}

/// A clean answer stays silent: only the rescued minority shows up.
#[tokio::test]
async fn a_clean_answer_leaves_no_salvage_row() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("m1", "u1", "在吗", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    assert_eq!(
        finish_with(&executor, &store, &job, PLAIN_CONTRACT).await,
        TurnResult::Replied
    );
    assert!(
        !audit_actions(&store)
            .await
            .iter()
            .any(|a| a.starts_with("agent.salvaged.")),
        "干净输出不得留抢救审计"
    );
}

fn empty_scope() -> ValidationScope {
    ValidationScope {
        speaker_refs: Vec::new(),
        message_refs: Vec::new(),
        memory_refs: Vec::new(),
        history_refs: Vec::new(),
        sticker_hashes: Vec::new(),
    }
}

/// Only a delivery whose bytes needed repair counts as salvaged; a
/// clean contract must not be held back by the stability gate.
#[test]
fn only_repaired_deliveries_count_as_salvaged() {
    let scope = empty_scope();
    let clean = "{\"should_reply\":true,\"reply_parts\":[\"收到\"]}";
    assert!(!delivery_was_salvaged(clean, &scope));
    let wrapped = "想好了\n```json\n{\"should_reply\":true,\"reply_parts\":[\"收到\"]}\n```";
    assert!(delivery_was_salvaged(wrapped, &scope));
}

/// Two identical sightings of the same step make a stable text; any
/// change — new bytes or a new step — restarts the wait.
#[test]
fn salvage_stability_needs_two_identical_sightings() {
    let mut seen = None;
    assert!(!salvage_text_stable(&mut seen, 8, "half"));
    assert!(salvage_text_stable(&mut seen, 8, "half"));
    assert!(!salvage_text_stable(&mut seen, 8, "half plus more"));
    assert!(salvage_text_stable(&mut seen, 8, "half plus more"));
    assert!(!salvage_text_stable(&mut seen, 9, "half plus more"));
}

/// The 2026-10-28 group-943 shape as a decision sequence. The barrier
/// first sees a step whose bytes only exist as a repairable fragment
/// (the lead-in segment, prose-wrapped): the hold must engage. When
/// the step finishes growing, its text validates clean and the turn
/// delivers the complete answer — never the fragment.
#[test]
fn a_growing_step_is_held_until_its_text_settles() {
    let scope = empty_scope();
    let mut seen = None;
    // Poll 1: fragment sighting — salvaged, never seen before.
    let fragment =
        "写一半\n```json\n{\"should_reply\":true,\"reply_parts\":[\"放着我来给你盘\"]}\n```";
    let hold =
        delivery_was_salvaged(fragment, &scope) && !salvage_text_stable(&mut seen, 8, fragment);
    assert!(hold, "第一次见到抢救片段必须按住");
    // Poll 2: the finished step — clean contract, no hold, and the
    // deliverable is the full text rather than the fragment.
    let full = "{\"should_reply\":true,\"reply_parts\":[\"放着我来给你盘\",\"DDR3是2007年的第三代标准\",\"主流频率1333到1600\",\"洋垃圾服务器里便宜大碗\"]}";
    let hold = delivery_was_salvaged(full, &scope) && !salvage_text_stable(&mut seen, 8, full);
    assert!(!hold, "完整合约不得被按住");
    // A genuinely finished repair (the model is done, the bytes just
    // needed the ladder) delivers on its second identical sighting.
    let mut seen2 = None;
    assert!(!salvage_text_stable(&mut seen2, 8, fragment));
    let hold =
        delivery_was_salvaged(fragment, &scope) && !salvage_text_stable(&mut seen2, 8, fragment);
    assert!(!hold, "文本不再变化的抢救结果应当放行");
}

/// Salvage is not a licence to send anything: markup in every part still means
/// the turn was a genuine bad output.
#[tokio::test]
async fn a_markup_only_answer_is_still_a_bad_output() {
    let dir = tempfile::tempdir().expect("tmp");
    let raw = "日志开始 {\"should_reply\":true,\"reply_parts\":[\"[CQ:at,qq=all]\"]} 日志结束";
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("m1", "u1", "在吗", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    assert_eq!(
        finish_with(&executor, &store, &job, raw).await,
        TurnResult::BadOutput
    );
    assert!(
        all_outbox_rows(&store).await.is_empty(),
        "不得借抢救通道流出"
    );
}

/// A rejected answer used to keep only `agent_bad_output`. The classifier
/// and a flattened head of the body now land in the audit, and nothing is
/// sent back to the model.
///
/// The body has to be one the salvage layers refuse: a plain-text body of
/// two characters or more is a legitimate answer now, so the specimen here
/// is a single character — too short for the text layer, not JSON.
#[tokio::test]
async fn a_bad_answer_keeps_its_text_in_the_audit() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("m1", "u1", "在吗", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    assert_eq!(
        finish_with(&executor, &store, &job, "x").await,
        TurnResult::BadOutput
    );
    assert!(all_outbox_rows(&store).await.is_empty());
    let actions = audit_actions(&store).await;
    assert!(
        actions
            .iter()
            .any(|a| a == "agent.bad_output.g100.kind.malformed"),
        "分类必须留下: {actions:?}"
    );
    assert!(
        actions.iter().any(|a| a == "agent.bad_output.g100.text.x"),
        "原文开头必须留下: {actions:?}"
    );
}

async fn claimed_job(store: &SqliteStore, events: &[IncomingMessage]) -> BatchJob {
    for event in events {
        let payload = serde_json::to_string(event).expect("序列化");
        store
            .insert_pending(
                "1",
                "100",
                &format!("ev-{}", event.message_id),
                "555",
                event.received_at,
                &payload,
                "[]",
            )
            .await
            .expect("入队");
    }
    let rows = store.pending_for_group("1", "100").await.expect("读");
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    store.claim_batch(&ids, "b-test").await.expect("认领");
    BatchJob {
        group_id: "100".into(),
        batch_id: "b-test".into(),
        event_ids: ids,
        privileged: false,
        chars: 10,
        images: 0,
        flooded: false,
        flood_background: None,
        retry: false,
    }
}

async fn audit_actions(store: &SqliteStore) -> Vec<String> {
    store
        .recent_audit(200)
        .await
        .expect("审计")
        .iter()
        .map(|e| e.action.clone())
        .collect()
}

async fn seed_group_message(store: &SqliteStore, message_id: &str, created_at: i64) {
    store
        .insert_group_message(
            "1", "100", message_id, "777", None, "member", "记录", false, None, None, created_at,
        )
        .await
        .expect("记录群消息");
}

async fn all_outbox_rows(store: &SqliteStore) -> Vec<crate::store::OutboxRecord> {
    let mut rows = Vec::new();
    for state in [
        crate::store::OutboxState::Pending,
        crate::store::OutboxState::Sending,
        crate::store::OutboxState::Sent,
        crate::store::OutboxState::Unknown,
    ] {
        rows.extend(store.outbox_batch(state, 100).await.expect("outbox"));
    }
    rows
}

#[tokio::test]
async fn stale_batch_is_dropped_by_the_send_gate() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut event = message("s1", "u1", "旧消息", 0);
    event.received_at = crate::now_unix() - 400;
    let job = claimed_job(&store, &[event]).await;
    let result = finish_with(&executor, &store, &job, PLAIN_CONTRACT).await;
    assert_eq!(result, TurnResult::DroppedStale);
    assert!(
        all_outbox_rows(&store).await.is_empty(),
        "过期批次不得入 outbox"
    );
    assert!(
        store
            .pending_for_group("1", "100")
            .await
            .expect("读")
            .is_empty(),
        "批次应被正常消耗"
    );
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a.starts_with("sendgate.drop.stale.g100.age")),
        "缺新鲜度门审计"
    );
}

#[tokio::test]
async fn fresh_batch_is_dropped_when_the_group_moved_on() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let now = crate::now_unix();
    let mut event = message("f1", "u1", "新消息", 0);
    event.received_at = now - 10;
    for i in 0..9 {
        seed_group_message(&store, &format!("n{i}"), now - 5).await;
    }
    let job = claimed_job(&store, &[event]).await;
    let result = finish_with(&executor, &store, &job, PLAIN_CONTRACT).await;
    assert_eq!(result, TurnResult::DroppedStale);
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a.starts_with("sendgate.drop.stale.g100.") && a.contains(".new9")),
        "顺延审计应带新到条数"
    );
}

#[tokio::test]
async fn quote_fallback_auto_attaches_a_scrolled_away_anchor() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let now = crate::now_unix();
    seed_group_message(&store, "oldpost", now - 1000).await;
    for i in 0..6 {
        seed_group_message(&store, &format!("after{i}"), now - 900 + i * 10).await;
    }
    let mut event = message("c1", "u1", "有点气动狗的感觉", 0);
    event.quote_of = Some("oldpost".into());
    event.received_at = now - 30;
    let job = claimed_job(&store, &[event]).await;
    let result = finish_with(&executor, &store, &job, PLAIN_CONTRACT).await;
    assert_eq!(result, TurnResult::Replied);
    let rows = all_outbox_rows(&store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].quote_message_id.as_deref(), Some("oldpost"));
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a == "reply.quote.auto_attached.g100.new6"),
        "缺自动补挂审计"
    );
}

#[tokio::test]
async fn quote_fallback_sends_plain_when_anchor_unverifiable() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let now = crate::now_unix();
    let mut event = message("c1", "u1", "有点气动狗的感觉", 0);
    event.quote_of = Some("ghost".into());
    event.received_at = now - 30;
    let job = claimed_job(&store, &[event]).await;
    let result = finish_with(&executor, &store, &job, PLAIN_CONTRACT).await;
    assert_ne!(result, TurnResult::DroppedStale);
    let rows = all_outbox_rows(&store).await;
    assert!(
        !rows.is_empty(),
        "锚点不可验证时应降级为无引用发送，不得整条丢弃"
    );
    assert!(
        rows.iter().all(|r| r.quote_message_id.is_none()),
        "降级发送不得携带引用"
    );
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a == "reply.quote.fallback_plain.g100.unverifiable"),
        "缺降级发送审计"
    );
}

#[tokio::test]
async fn paused_group_never_rotates() {
    let dir = tempfile::tempdir().expect("tmp");
    let (mut executor, store) = executor_fixture(&dir).await;
    Arc::get_mut(&mut executor)
        .expect("唯一引用")
        .conversation_turn_limit
        .store(1, std::sync::atomic::Ordering::Relaxed);
    store
        .set_group_flags("1", "100", Some(true), Some(true), None, crate::now_unix())
        .await
        .expect("暂停");
    let mut event = message("p1", "u1", "在吗", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    assert_eq!(executor.run_batch(&job).await, TurnResult::SkippedInactive);
    let group = store
        .get_group("1", "100")
        .await
        .expect("读")
        .expect("存在");
    assert_eq!(group.agy_conversation_id.as_deref(), Some("conv-keep"));
    assert_eq!(group.agy_turn_count, 0);
    assert!(
        !audit_actions(&store)
            .await
            .iter()
            .any(|a| a.starts_with("ops.conversation.rotated")),
        "暂停群不得触发轮换"
    );
}

#[test]
fn seed_brief_is_none_when_the_group_has_no_past() {
    assert_eq!(
        assemble_seed_brief(None, None, &[], &[], &std::collections::HashMap::new()),
        None
    );
}

#[test]
fn seed_brief_combines_summary_facts_and_window() {
    let facts = vec![crate::store::MemoryRecord {
        memory_ref: "mem_x".into(),
        bot_id: "1".into(),
        group_id: "100".into(),
        subject_user_id: None,
        content: "群主在参谋二手车".into(),
        kind: "fact".into(),
        source_message_ids: "[]".into(),
        confidence: 0.9,
        revision: 1,
        origin: "agy".into(),
        archived_at: None,
        created_at: 1,
        updated_at: 1,
    }];
    let window = vec![crate::store::GroupMessageRecord {
        id: 1,
        bot_id: "1".into(),
        group_id: "100".into(),
        message_id: "w1".into(),
        sender_id: "555".into(),
        display_name: Some("Ai".into()),
        speaker_role: "member".into(),
        text: "这台星火3怎么样".into(),
        has_image: false,
        sticker_hash: None,
        sub_type: None,
        image_tags: None,
        created_at: 1,
        archived: false,
    }];
    let (brief, targets) = assemble_seed_brief(
        Some("Ai 还在等你点评那台星火3，别忘了回他。"),
        Some("摘要：一直在聊买车"),
        &facts,
        &window,
        &std::collections::HashMap::new(),
    )
    .expect("应有提要");
    // The seam material comes first: continuity line, then the handover
    // note, then the archive layers.
    assert!(brief.starts_with("（这是同一场对话的继续"), "got: {brief}");
    assert!(brief.contains("接力纸条"));
    assert!(brief.contains("还在等你点评那台星火3"));
    assert!(brief.contains("已归档对话摘要"));
    assert!(brief.contains("一直在聊买车"));
    assert!(brief.contains("当前活跃事实"));
    assert!(brief.contains("群主在参谋二手车"));
    assert!(brief.contains("最近的群聊原文"));
    assert!(brief.contains("[h1] Ai：这台星火3怎么样"));
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].history_ref, "h1");
    assert_eq!(targets[0].message_id, "w1");
}

#[test]
fn seed_brief_names_a_facts_subject_when_the_content_does_not() {
    let facts = vec![crate::store::MemoryRecord {
        memory_ref: "mem_y".into(),
        bot_id: "1".into(),
        group_id: "100".into(),
        subject_user_id: Some("555".into()),
        content: "不吃香菜，辣的也不行".into(),
        kind: "preference".into(),
        source_message_ids: "[]".into(),
        confidence: 0.8,
        revision: 1,
        origin: "hy3".into(),
        archived_at: None,
        created_at: 1,
        updated_at: 1,
    }];
    let names: std::collections::HashMap<String, String> = [("555".to_string(), "Ai".to_string())]
        .into_iter()
        .collect();
    let (brief, _) = assemble_seed_brief(None, None, &facts, &[], &names).expect("应有提要");
    assert!(brief.contains("关于Ai：不吃香菜"), "got: {brief}");
}

/// Render the prompt one turn would send, without a session behind it.
///
/// `prepare_turn` is the whole prompt-building half of a turn; running it
/// directly is what the prompt tests actually care about.
async fn prompt_for(executor: &TurnExecutor, store: &SqliteStore, job: &BatchJob) -> String {
    let now = crate::now_unix();
    let group = store
        .get_group("1", &job.group_id)
        .await
        .expect("读组")
        .expect("应有组");
    executor
        .prepare_turn(job, &group, now)
        .await
        .expect("组装 prompt")
        .prompt
}

async fn seed_the_group(store: &SqliteStore) {
    let now = crate::now_unix();
    store
        .insert_chunk(
            "chunk-1",
            "1",
            "100",
            1,
            1,
            2,
            "大家一直在聊二手公路车，重点看了星火3和法拉斯EVO。",
            "test",
            now,
        )
        .await
        .expect("写 chunk");
    store
        .insert_memory(
            "1",
            "100",
            None,
            "群主 tester 在给群友参谋二手车",
            "fact",
            "[]",
            0.9,
            now,
        )
        .await
        .expect("写记忆");
    seed_group_message(store, "w1", now - 60).await;
}

#[tokio::test]
async fn fresh_conversation_first_turn_gets_the_seed_brief() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    store
        .set_conversation_id("1", "100", None, 1)
        .await
        .expect("模拟刚轮换");
    seed_the_group(&store).await;
    let mut event = message("n1", "u1", "那台车到底怎么样", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    let prompt = prompt_for(&executor, &store, &job).await;
    assert!(
        prompt.contains("# 前情提要（新会话播种）"),
        "首轮应带播种节"
    );
    assert!(prompt.contains("星火3和法拉斯EVO"), "应含 fold 摘要");
    assert!(
        prompt.contains("群主 tester 在给群友参谋二手车"),
        "应含活跃事实"
    );
    assert!(prompt.contains("最近的群聊原文"), "应含原始窗口");
}

#[tokio::test]
async fn prepare_turn_snapshots_first_turn_before_the_conversation_id_is_set() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    store
        .set_conversation_id("1", "100", None, 1)
        .await
        .expect("清会话");
    let mut event = message("n1", "u1", "hello", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event.clone()]).await;
    let group = store
        .get_group("1", "100")
        .await
        .expect("读组")
        .expect("应有组");
    let prepared = executor
        .prepare_turn(&job, &group, crate::now_unix())
        .await
        .expect("prepare");
    assert!(prepared.is_first_turn, "无会话 id 时应记为首轮");
    store
        .set_conversation_id("1", "100", Some("conv-1"), 1)
        .await
        .expect("设会话");
    store.delete_pending(&job.event_ids).await.expect("清理");
    let job2 = claimed_job(&store, &[event]).await;
    let group2 = store
        .get_group("1", "100")
        .await
        .expect("读组")
        .expect("应有组");
    let prepared2 = executor
        .prepare_turn(&job2, &group2, crate::now_unix())
        .await
        .expect("prepare");
    assert!(!prepared2.is_first_turn, "已有会话 id 时不得再记为首轮");
}

#[tokio::test]
async fn a_panel_summary_seeds_a_group_that_was_never_folded() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    store
        .set_conversation_id("1", "100", None, 1)
        .await
        .expect("模拟刚轮换");
    // No memory_chunks at all: the only archived-summary source is the panel's
    // whole-store compaction.
    store
        .put_summary(
            "1",
            "100",
            "群里长期在讨论显卡散热和电费。",
            1,
            "test",
            crate::now_unix(),
        )
        .await
        .expect("写 summary");
    seed_group_message(&store, "w1", crate::now_unix() - 60).await;
    let mut event = message("n1", "u1", "最近电费多少", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    let prompt = prompt_for(&executor, &store, &job).await;
    assert!(
        prompt.contains("显卡散热和电费"),
        "面板摘要应能充当播种摘要"
    );
}

#[tokio::test]
async fn a_fold_chunk_wins_over_the_panel_summary() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    store
        .set_conversation_id("1", "100", None, 1)
        .await
        .expect("模拟刚轮换");
    seed_the_group(&store).await;
    store
        .put_summary(
            "1",
            "100",
            "面板压缩得到的旧摘要：在聊显卡。",
            1,
            "test",
            crate::now_unix(),
        )
        .await
        .expect("写 summary");
    let mut event = message("n1", "u1", "那台车到底怎么样", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    let prompt = prompt_for(&executor, &store, &job).await;
    assert!(prompt.contains("星火3和法拉斯EVO"), "点应优先用 fold 摘要");
    assert!(
        !prompt.contains("面板压缩得到的旧摘要"),
        "两份摘要只渲染一份，免得占满播种预算"
    );
}

#[test]
fn prompt_rejection_wording_is_detected() {
    // The refusal as agy prints it (2026-10-28, group 943).
    assert!(looks_like_prompt_rejection(
        "The prompt could not be submitted. The prompt contains sensitive words that violate policy."
    ));
    assert!(looks_like_prompt_rejection(
        "ERROR: PROMPT COULD NOT BE SUBMITTED"
    ));
    assert!(looks_like_prompt_rejection(
        "the prompt contains sensitive words"
    ));
    // A contract is never a refusal, and ordinary chat that merely
    // mentions sensitive words (without the refused prompt) is not one
    // either — the classifier must stay narrow.
    assert!(!looks_like_prompt_rejection(
        "{\"should_reply\": true, \"reply_parts\": []}"
    ));
    assert!(!looks_like_prompt_rejection(
        "我不能复述那些 sensitive words，不太合适"
    ));
    assert!(!looks_like_prompt_rejection("这个问题我提交不了，换个问法"));
}

#[tokio::test]
async fn seed_tier_one_drops_only_the_newest_chunk() {
    let dir = tempfile::tempdir().expect("tmp");
    let (_executor, store) = executor_fixture(&dir).await;
    let now = crate::now_unix();
    for (id, seq, summary) in [
        ("chunk-a", 1, "摘要甲：先聊了显卡。"),
        ("chunk-b", 2, "摘要乙：接着聊电费。"),
        ("chunk-c", 3, "摘要丙：最后聊二手车。"),
    ] {
        store
            .insert_chunk(id, "1", "100", seq, seq, seq, summary, "test", now)
            .await
            .expect("写 chunk");
    }
    let (full, _) = seed_brief_for(&store, "1", "100").await.expect("全量种子");
    assert!(full.contains("摘要甲") && full.contains("摘要乙") && full.contains("摘要丙"));
    // Simulate a recorded quarantine tier without a rejection: assembly
    // must start at tier 1.
    store
        .record_seed_spawn("1", "100", "tier-marker", 1, now)
        .await
        .expect("写 tier");
    let (trimmed, _) = seed_brief_for(&store, "1", "100").await.expect("一档种子");
    assert!(trimmed.contains("摘要甲"), "旧段保留");
    assert!(trimmed.contains("摘要乙"), "次新段保留");
    assert!(!trimmed.contains("摘要丙"), "最新一段折叠摘要被摘掉");
}

#[tokio::test]
async fn rejected_seed_is_never_resubmitted_verbatim() {
    let dir = tempfile::tempdir().expect("tmp");
    let (_executor, store) = executor_fixture(&dir).await;
    seed_the_group(&store).await;
    let now = crate::now_unix();

    let (full, _) = seed_brief_for(&store, "1", "100").await.expect("全量种子");
    assert!(full.contains("星火3和法拉斯EVO"), "全量含 fold 摘要");

    // The full seed is refused: the next spawn drops the group's only
    // folded segment but keeps facts and the raw window.
    store
        .mark_seed_rejected("1", "100", now)
        .await
        .expect("标记被拒");
    let (tier1, _) = seed_brief_for(&store, "1", "100").await.expect("一档种子");
    assert_ne!(tier1, full);
    assert!(!tier1.contains("星火3和法拉斯EVO"), "被拒种子不得原样重提");
    assert!(tier1.contains("群主 tester 在给群友参谋二手车"), "事实保留");
    assert!(tier1.contains("最近的群聊原文"), "窗口保留");

    // Refused again: the minimal seed is facts only.
    store
        .mark_seed_rejected("1", "100", now)
        .await
        .expect("再标记被拒");
    let (tier2, _) = seed_brief_for(&store, "1", "100").await.expect("极简种子");
    assert!(
        tier2.contains("群主 tester 在给群友参谋二手车"),
        "极简只剩事实"
    );
    assert!(!tier2.contains("最近的群聊原文"), "极简去掉窗口");
    assert!(!tier2.contains("星火3和法拉斯EVO"), "极简去掉折叠摘要");

    // Refused a third time with unchanged facts: every tier assembles
    // to rejected bytes, so the group is born seedless rather than
    // resubmitting them.
    store
        .mark_seed_rejected("1", "100", now)
        .await
        .expect("三标记被拒");
    assert!(seed_brief_for(&store, "1", "100").await.is_none());
}

#[tokio::test]
async fn continuing_conversation_gets_no_seed() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    seed_the_group(&store).await;
    let mut event = message("n1", "u1", "那台车到底怎么样", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    let prompt = prompt_for(&executor, &store, &job).await;
    assert!(!prompt.contains("前情提要"), "续接轮次不付播种成本");
}

/// The one fixture every test here builds on.
///
/// There is no agent runner to inject any more: a turn is finished by handing
/// [`TurnExecutor::finish_turn_with_coverage`] the contract JSON the model was
/// supposed to produce, which is what the resident path does after it has read a
/// step off the PTY. Tests therefore exercise the real post-agent code — contract
/// validation, salvage, memory, the reply gates, the outbox — without spawning a
/// terminal or a subprocess.
async fn executor_fixture(dir: &tempfile::TempDir) -> (Arc<TurnExecutor>, Arc<SqliteStore>) {
    executor_fixture_with_budget(dir, 0).await
}

async fn executor_fixture_with_budget(
    dir: &tempfile::TempDir,
    hourly_call_budget: u32,
) -> (Arc<TurnExecutor>, Arc<SqliteStore>) {
    executor_fixture_full(dir, hourly_call_budget, Vec::new(), None).await
}

/// Fixture whose image path is fully wired: bytes to serve and a describer to answer.
///
/// Kept separate from [`executor_fixture_with_budget`] instead of growing its
/// signature — two dozen call sites want a fetcher that never answers and no
/// describer at all, and only the ingest tests want either.
async fn executor_fixture_full(
    dir: &tempfile::TempDir,
    hourly_call_budget: u32,
    bytes: Vec<u8>,
    describer: Option<Arc<dyn crate::app::preflight::ImageDescriber>>,
) -> (Arc<TurnExecutor>, Arc<SqliteStore>) {
    let path = dir.path().join("bot.sqlite3");
    let pool = crate::store::migrations::open_and_migrate(&path)
        .await
        .expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    store.ensure_group("1", "100", 1).await.expect("建组");
    store
        .set_group_flags("1", "100", Some(true), Some(false), None, 1)
        .await
        .expect("开启");
    store
        .set_conversation_id("1", "100", Some("conv-keep"), 1)
        .await
        .expect("会话");
    let (_supervisor, connection) =
        crate::onebot::connection::Connection::new("ws://127.0.0.1:1", None);
    let executor = Arc::new(TurnExecutor {
        no_submission_retry_groups: Vec::new(),
        webhook: None,
        store: Arc::clone(&store),
        connection,
        outbox: Outbox::new(Arc::clone(&store)),
        memory: MemoryApplier::new(Arc::clone(&store)),
        mihomo: Arc::new(Mihomo::new(
            crate::process::mihomo::MihomoConfig {
                bin: std::path::PathBuf::new(),
                config_path: std::path::PathBuf::new(),
                api_port: 0,
                proxy_port: 0,
                api_secret: None,
            },
            false,
        )),
        health: Arc::new(crate::health::HealthRegistry::new(
            crate::health::HealthSnapshot::default(),
        )),
        bot_id: "1".into(),
        owner_qq_id: "9".into(),
        model: "gemini-3.8-flash".into(),
        timeout: std::time::Duration::from_secs(30),
        output_channel: crate::config::OutputChannel::Text,
        conversation_turn_limit: shared_turn_limit(None),
        context_builder: MemoryContextBuilder::new(1200),
        persona_default: crate::agent::prompt::DEFAULT_PERSONA.to_string(),
        media_fetcher: Arc::new(FixtureFetcher {
            bytes,
            calls: AtomicUsize::new(0),
        }),
        media_max_bytes: 1_000_000,
        media_max_pixels: 1_000_000,
        preflight: None,
        image_describer: describer,
        hourly_call_budget,
        sticker_dir: dir.path().join("stickers"),
        agy_projects_dir: dir.path().join("projects"),
        secrets: Vec::new(),
        running_turns: Arc::new(RunningTurns::new()),
        quote_enabled: true,
        digest: None,
        // Always present: the resident supervisor is the only execution path, so
        // the executor has no shape without it. Constructing one is free — a PTY is
        // only spawned when `session()` is called, and the tests that reach it stop
        // at the backoff gate first.
        resident: Arc::new(crate::app::resident::ResidentManager::new(
            Arc::clone(&store),
            "1".into(),
            dir.path().join("agy"),
            dir.path().to_path_buf(),
            "model".into(),
        )),
        resident_limits: ResidentLimits::default(),
        pacing: None,
        previous_batch: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    });
    (executor, store)
}

/// Drive one turn the way the resident path does, but with the model's answer
/// supplied directly: prepare, book the call, then finish with `contract_json`.
///
/// This is the entry point every migrated test uses. It is deliberately the same
/// call sequence as [`TurnExecutor::run_resident_batch`] minus the PTY, so the
/// parts under test are the real ones.
async fn finish_with(
    executor: &TurnExecutor,
    store: &SqliteStore,
    job: &BatchJob,
    contract_json: &str,
) -> TurnResult {
    let now = crate::now_unix();
    let group = store
        .get_group("1", &job.group_id)
        .await
        .expect("读组")
        .expect("应有组");
    let turn_started = std::time::Instant::now();
    let prepared = match executor.prepare_turn(job, &group, now).await {
        Ok(prepared) => prepared,
        Err(result) => return result,
    };
    let call_id = store
        .start_call_with_priority(
            &executor.bot_id,
            &job.group_id,
            &job.batch_id,
            now,
            job.privileged,
        )
        .await
        .expect("记调用");
    let newest = prepared
        .messages
        .iter()
        .map(|m| m.received_at)
        .max()
        .unwrap_or(now);
    let coverage = ResidentCoverage {
        batches: vec![ResidentDelivery {
            batch_id: job.batch_id.clone(),
            event_ids: job.event_ids.clone(),
            newest,
            privileged: job.privileged,
        }],
    };
    let timing = TurnTiming {
        turn_started,
        agent_started: std::time::Instant::now(),
    };
    match executor
        .finish_turn_with_coverage(job, &call_id, contract_json, timing, prepared, coverage)
        .await
    {
        Ok(result) => result,
        Err(result) => result,
    }
}

/// The contract a cooperating model returns: one plain, clean part.
const PLAIN_CONTRACT: &str =
    r#"{"should_reply":true,"reply_parts":["收到"],"mood_update":null,"topic_state":null}"#;

/// The contract a model returns when it chooses not to answer.
const DECLINE_CONTRACT: &str = r#"{"should_reply":false}"#;

#[tokio::test]
async fn an_exhausted_fuse_denies_the_call_after_the_claim() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture_with_budget(&dir, 2).await;
    let now = crate::now_unix();
    // Two calls already started inside the rolling hour: the fuse is blown,
    // but the intake claimed this batch before it could know that.
    store
        .start_call("1", "100", "b-seed-1", now)
        .await
        .expect("开一");
    store
        .start_call("1", "100", "b-seed-2", now)
        .await
        .expect("开二");
    let job = claimed_job(&store, &[message("m1", "u1", "在吗", 0)]).await;

    assert_eq!(executor.run_batch(&job).await, TurnResult::BudgetDeferred);
    assert_eq!(
        store
            .count_agent_calls_since("1", "100", now - 3_600)
            .await
            .expect("读"),
        2,
        "被拒的批次不得启动调用"
    );
    // The batch is back in the unclaimed pool, not consumed: the next tick
    // re-claims it once calls age out of the window.
    let pending = store.pending_for_group("1", "100").await.expect("读");
    assert_eq!(pending.len(), 1);
    let actions = audit_actions(&store).await;
    assert!(
        actions
            .iter()
            .any(|a| a.starts_with("scheduler.budget.exec_denied.g100.")),
        "审计动作: {actions:?}"
    );
}

async fn seed_library_row(
    store: &SqliteStore,
    hash: &str,
    ocr: &str,
    emotion: &str,
    description: &str,
    kept: bool,
) {
    store
        .upsert_sticker(
            hash,
            "1",
            "100",
            "stickers/seed.jpg",
            "sticker",
            "no",
            ocr,
            description,
            emotion,
            &format!("{ocr} {emotion} {description}"),
            None,
            false,
            true,
            1,
        )
        .await
        .expect("入库");
    if kept {
        store.set_sticker_kept("1", hash, 1).await.expect("收藏");
    }
}

#[tokio::test]
async fn sticker_offers_prefer_tag_signals_and_skip_unkept_rows() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    // A kept sticker whose tags match the batch's picture tags.
    seed_library_row(&store, "aaaa1111bbbb", "", "无语", "被噎住时", true).await;
    // Equally tagged but never kept: not hers to offer.
    seed_library_row(&store, "cccc2222dddd", "", "无语", "被噎住时", false).await;
    // Kept but unrelated to this batch.
    seed_library_row(&store, "eeee3333ffff", "晚安月亮", "困", "睡前", true).await;

    let messages = vec![message("m1", "u1", "今天也要加油", 0)];
    let offers = executor
        .select_sticker_offers(&["无语 被噎住时".to_string()], &messages)
        .await;
    assert_eq!(offers.len(), 1, "offers: {offers:?}");
    assert_eq!(offers[0].hash, "aaaa1111bbbb");
    assert_eq!(offers[0].short_hash, "aaaa1111");
    assert_eq!(offers[0].label, "无语");
    assert_eq!(offers[0].usage, "被噎住时");

    // A batch with no relation to the shelf offers nothing at all.
    let quiet = vec![message("m2", "u1", "量子力学考试范围", 0)];
    let none = executor.select_sticker_offers(&[], &quiet).await;
    assert!(none.is_empty(), "offers: {none:?}");
}

#[tokio::test]
async fn sticker_offers_admit_the_weak_band_that_search_rejects() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    // "被噎住时" against the query "噎住": one shared bigram out of
    // (2 query grams, 4 field grams) = 2*1/6, times W_USAGE 2.5 = 0.83.
    // The band under test needs a genuinely faint hit, so the query only
    // brushes the scene field: "午休室" vs "午休" = 2*1/5 * W_SCENE 0.8 = 0.32.
    // Inside 0.25..0.35: too weak to search, good enough to offer.
    seed_library_row(&store, "aaaa1111bbbb", "", "", "", true).await;
    seed_library_row(&store, "bbbb2222cccc", "", "", "", true).await;
    // A row tagged before the description change: its signal sits in the legacy
    // `scene` column, which is exactly the shape production databases still hold.
    sqlx::query("UPDATE stickers SET scene = '午休室里', search_blob = '午休室里' WHERE hash = 'bbbb2222cccc'")
            .execute(store.pool())
            .await
            .expect("写旧行");

    // "中午休息" (3 bigrams) shares exactly one with the scene "午休室里"
    // (3 bigrams): 2*1/6 * W_SCENE 0.8 = 0.267. Inside 0.25..0.35 — too
    // weak to search, good enough to offer.
    let weak = crate::memory::stickers::score_text(
        "中午休息",
        &crate::memory::stickers::StickerText {
            ocr: "",
            scene: "午休室里",
            emotion: "",
            intent: "",
            usage_hint: "",
        },
    );
    assert!(
        weak >= crate::memory::stickers::OFFER_MIN_SCORE
            && weak < crate::memory::stickers::SEARCH_MIN_SCORE,
        "sample must sit in the offer-only band, got {weak}"
    );
    assert_eq!(crate::memory::stickers::match_tier(weak), None);

    let messages = vec![message("m1", "u1", "中午休息", 0)];
    let offers = executor.select_sticker_offers(&[], &messages).await;
    assert_eq!(offers.len(), 1, "offers: {offers:?}");
    assert_eq!(offers[0].hash, "bbbb2222cccc");
}

#[tokio::test]
async fn sticker_offers_translate_wording_into_queries() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    seed_library_row(&store, "1111aaaa2222", "", "气鼓鼓", "被叫错名字", true).await;
    seed_library_row(&store, "3333bbbb4444", "", "无语", "被噎住时", true).await;

    // "绉号" alone is not in the shelf; the trigger phrase is.
    let messages = vec![message("m1", "u1", "你刚给我起的绉号太难听了", 0)];
    let offers = executor.select_sticker_offers(&[], &messages).await;
    assert_eq!(offers.len(), 1, "offers: {offers:?}");
    assert_eq!(offers[0].hash, "1111aaaa2222");
    assert_eq!(offers[0].label, "气鼓鼓");
}

#[tokio::test]
async fn sticker_offers_prefer_distinct_emotions_over_a_high_scoring_tie() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    // Three confident 无语 rows and one 困 row on the same usage: the 困 row
    // scores strictly lower because its emotion contributes nothing.
    seed_library_row(&store, "aaaa0001", "", "无语、翻白眼", "被噎住时", true).await;
    seed_library_row(&store, "aaaa0002", "", "无语、叹气", "被噎住时", true).await;
    seed_library_row(&store, "aaaa0003", "", "无语", "被噎住时", true).await;
    seed_library_row(&store, "bbbb0004", "", "困", "被噎住时", true).await;

    let messages = vec![message("m1", "u1", "无语 被噎住时", 0)];
    let offers = executor
        .select_sticker_offers(&["无语 被噎住时".to_string()], &messages)
        .await;
    let hashes: Vec<&str> = offers.iter().map(|o| o.hash.as_str()).collect();
    assert_eq!(hashes.len(), 3, "offers: {offers:?}");
    // The best row keeps the top seat; the second seat goes to the only
    // differently-tagged candidate, ahead of two higher-scoring duplicates.
    let buckets: Vec<&str> = offers.iter().map(|offer| offer.label.as_str()).collect();
    assert_eq!(buckets[0], "无语", "offers: {offers:?}");
    assert_eq!(buckets[1], "困", "offers: {offers:?}");
    assert!(hashes.contains(&"bbbb0004"), "offers: {offers:?}");
}

fn hashes(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

#[test]
fn only_the_last_three_rounds_count_as_just_sent() {
    // Newest first, three rows in round "new", then round "mid", then round "old".
    // A round is a batch, not a row: five parts of one delivery are one round.
    let recent: Vec<(String, String, i64)> = vec![
        ("h1".into(), "new".into(), 30),
        ("h2".into(), "new".into(), 30),
        ("h3".into(), "new".into(), 30),
        ("h4".into(), "mid".into(), 20),
        ("h5".into(), "old".into(), 10),
    ];
    let just = rounds_just_sent(&recent);
    assert!(just.contains(&"h1".to_string()));
    assert!(just.contains(&"h4".to_string()));
    assert!(just.contains(&"h5".to_string()));
    // The fourth round is out of scope, so nothing past it is claimed as recent.
    let far: Vec<(String, String, i64)> = vec![
        ("h1".into(), "a".into(), 40),
        ("h2".into(), "b".into(), 30),
        ("h3".into(), "c".into(), 20),
        ("h9".into(), "d".into(), 10),
    ];
    assert!(!rounds_just_sent(&far).contains(&"h9".to_string()));
}

#[test]
fn the_habit_slot_gives_way_to_a_fresh_candidate() {
    let ranked = hashes(&["proven", "runner", "other"]);
    let over = hashes(&["proven", "runner"]);
    // Proven just went out, runner clears the threshold and has not: it wins.
    let choice = habit_pick(&ranked, &hashes(&["proven"]), &over, &hashes(&["already"]));
    assert_eq!(choice, Some("runner".to_string()));
}

#[test]
fn the_habit_slot_keeps_the_proven_row_when_nothing_can_replace_it() {
    let ranked = hashes(&["proven", "runner"]);
    // Runner is over the threshold but also just went out, so it is no replacement.
    let choice = habit_pick(
        &ranked,
        &hashes(&["proven", "runner"]),
        &hashes(&["proven", "runner"]),
        &[],
    );
    assert_eq!(
        choice,
        Some("proven".to_string()),
        "no fresh option: the fallback still falls back"
    );
    // No history at all: no habit pick.
    assert_eq!(habit_pick(&[], &[], &[], &[]), None);
    // The proven row is already on the menu: the slot stays empty.
    assert_eq!(
        habit_pick(
            &ranked,
            &[],
            &hashes(&["proven", "runner"]),
            &hashes(&["proven"])
        ),
        None
    );
}

#[tokio::test]
async fn a_repeat_only_gets_offered_at_quarter_weight() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    seed_library_row(&store, "aaaa1111bbbb", "", "无语", "被噎住时", true).await;
    seed_library_row(&store, "eeee5555ffff", "", "无语", "被噎住时", true).await;
    // Only one of the two just went out, so only one of them is damped. Both rows
    // score identically on the batch text, which makes the ordering the damping.
    sqlx::query(
            "INSERT INTO outbox (id, bot_id, group_id, batch_id, text, state, kind, sticker_hash, created_at, updated_at) \
             VALUES ('o-aa', '1', '100', 'b', '', 'sent', 'sticker', 'aaaa1111bbbb', 1, 1)",
        )
        .execute(store.pool())
        .await
        .expect("outbox");

    // Both cleared the threshold; the damped one still clears it at quarter weight, so
    // it stays on the menu — a discount is not a ban.
    let messages = vec![message("m1", "u1", "无语 被噎住时", 0)];
    let offers = executor
        .select_sticker_offers(&["无语 被噎住时".to_string()], &messages)
        .await;
    let hashes: Vec<&str> = offers.iter().map(|o| o.hash.as_str()).collect();
    assert!(
        hashes.contains(&"aaaa1111bbbb"),
        "quarter weight still offers it: {hashes:?}"
    );
    // And the un-damped row comes first.
    assert_eq!(hashes[0], "eeee5555ffff", "offers: {hashes:?}");
}

#[tokio::test]
async fn a_habit_pick_with_no_fresh_replacement_still_comes_along() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    // Two rows share the "无语" bucket so the diverse pass picks exactly one and
    // leaves room for the habit slot.
    seed_library_row(&store, "aaaa1111bbbb", "", "无语", "被噎住时", true).await;
    seed_library_row(&store, "cccc2222dddd", "", "无语", "被噎住时", true).await;
    seed_library_row(&store, "dddd3333eeee", "", "得意", "炫耀时", true).await;
    // The group sends cccc most, then dddd, and all three deliveries fall inside the
    // last three rounds — so neither can replace the other.
    for (id, hash, batch) in [
        ("o-cc-1", "cccc2222dddd", "old"),
        ("o-cc-2", "cccc2222dddd", "older"),
        ("o-dd-1", "dddd3333eeee", "newest"),
    ] {
        sqlx::query(
                "INSERT INTO outbox (id, bot_id, group_id, batch_id, text, state, kind, sticker_hash, created_at, updated_at) \
                 VALUES (?, '1', '100', ?, '', 'sent', 'sticker', ?, 1, 1)",
            )
            .bind(id)
            .bind(batch)
            .bind(hash)
            .execute(store.pool())
            .await
            .expect("outbox");
    }

    // aaaa scores on the batch text, cccc is the habit pick but just went out,
    // dddd is the runner-up and also just went out — so nothing replaces it and the
    // proven row keeps the slot (the "无替代时照旧保底" case).
    let messages = vec![message("m1", "u1", "被噎住时", 0)];
    let offers = executor
        .select_sticker_offers(&["被噎住时".to_string()], &messages)
        .await;
    let hashes: Vec<&str> = offers.iter().map(|o| o.hash.as_str()).collect();
    assert!(
        hashes.contains(&"cccc2222dddd"),
        "no replacement: proven row still rides along: {hashes:?}"
    );
}

#[tokio::test]
async fn the_habit_slot_skips_a_row_the_group_just_saw() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    seed_library_row(&store, "aaaa1111bbbb", "", "无语", "被噎住时", true).await;
    seed_library_row(&store, "cccc2222dddd", "", "无语", "被噎住时", true).await;
    seed_library_row(&store, "dddd3333eeee", "", "无语", "被噎住时", true).await;
    // cccc is the group's habit, but it went out in the newest round; dddd went out
    // long before that. So dddd is the fresher over-threshold row.
    for (id, hash, batch, at) in [
        ("o-cc-1", "cccc2222dddd", "newest", 100),
        ("o-cc-2", "cccc2222dddd", "older", 50),
        ("o-dd-1", "dddd3333eeee", "ancient", 1),
    ] {
        sqlx::query(
                "INSERT INTO outbox (id, bot_id, group_id, batch_id, text, state, kind, sticker_hash, created_at, updated_at) \
                 VALUES (?, '1', '100', ?, '', 'sent', 'sticker', ?, ?, ?)",
            )
            .bind(id)
            .bind(batch)
            .bind(hash)
            .bind(at)
            .bind(at)
            .execute(store.pool())
            .await
            .expect("outbox");
    }

    let messages = vec![message("m1", "u1", "被噎住时", 0)];
    let offers = executor
        .select_sticker_offers(&["被噎住时".to_string()], &messages)
        .await;
    let hashes: Vec<&str> = offers.iter().map(|o| o.hash.as_str()).collect();
    assert!(
        hashes.contains(&"dddd3333eeee"),
        "the fresh runner-up takes the slot: {hashes:?}"
    );
}

#[tokio::test]
async fn sticker_offers_append_the_groups_most_sent_row_when_there_is_room() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    seed_library_row(&store, "aaaa1111bbbb", "", "无语", "被噎住时", true).await;
    seed_library_row(&store, "cccc2222dddd", "", "得意", "炫耀时", true).await;
    for index in 0..2 {
        sqlx::query(
                "INSERT INTO outbox (id, bot_id, group_id, batch_id, text, state, kind, sticker_hash, created_at, updated_at) \
                 VALUES (?, '1', '100', 'b', '', 'sent', 'sticker', 'cccc2222dddd', 1, 1)",
            )
            .bind(format!("o-cc-{index}"))
            .execute(store.pool())
            .await
            .expect("outbox");
    }
    sqlx::query(
            "INSERT INTO outbox (id, bot_id, group_id, batch_id, text, state, kind, sticker_hash, created_at, updated_at) \
             VALUES ('o-aa', '1', '100', 'b', '', 'sent', 'sticker', 'aaaa1111bbbb', 1, 1)",
        )
        .execute(store.pool())
        .await
        .expect("outbox");

    // One scored match leaves room for the habit pick.
    let messages = vec![message("m1", "u1", "无语 被噎住时", 0)];
    let offers = executor
        .select_sticker_offers(&["无语 被噎住时".to_string()], &messages)
        .await;
    let hashes: Vec<&str> = offers.iter().map(|o| o.hash.as_str()).collect();
    assert_eq!(hashes.len(), 2, "offers: {offers:?}");
    assert_eq!(hashes[0], "aaaa1111bbbb");
    assert_eq!(hashes[1], "cccc2222dddd", "habit pick goes last");

    // Nothing scored: no habit pick either — an unrelated batch stays unrelated.
    let quiet = vec![message("m2", "u1", "量子力学考试范围", 0)];
    let none = executor.select_sticker_offers(&[], &quiet).await;
    assert!(none.is_empty(), "offers: {none:?}");

    // Already picked by score: never duplicated by the habit slot.
    let hit = vec![message("m3", "u1", "得意 炫耀时", 0)];
    let offers = executor
        .select_sticker_offers(&["得意 炫耀时".to_string()], &hit)
        .await;
    let hashes: Vec<&str> = offers.iter().map(|o| o.hash.as_str()).collect();
    assert_eq!(hashes.len(), 1, "offers: {offers:?}");
    assert_eq!(hashes.iter().filter(|h| **h == "cccc2222dddd").count(), 1);
}

// ------------------------------------------------- server-side auto-collection

/// A PNG whose pixels are stable enough for the perceptual hash to recognise it.
fn sticker_png(size: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(size, size, |x, y| {
        let fx = x as f32 / size as f32;
        let fy = y as f32 / size as f32;
        let lit = [(0.25, 0.25), (0.7, 0.35), (0.5, 0.75)]
            .iter()
            .any(|(cx, cy)| ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() < 0.16);
        if lit {
            image::Rgba([240, 240, 240, 255])
        } else {
            image::Rgba([16, 16, 16, 255])
        }
    });
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("编码");
    out
}

/// One message carrying `images` pictures, each with a URL the fixture can serve.
fn pictured_message(ref_id: &str, text: &str, images: usize) -> IncomingMessage {
    let mut message = message(ref_id, "u1", text, images);
    // `message()` defaults to epoch 1, which the send gate correctly drops as stale.
    message.received_at = crate::now_unix();
    let mut index = 0;
    for segment in &mut message.segments {
        if let Segment::Image { url, .. } = segment {
            index += 1;
            *url = Some(format!("https://media.example/{ref_id}/{index}.png"));
        }
    }
    message
}

/// A claimed job whose batch has one pictured message.
async fn pictured_job(store: &SqliteStore, message: &IncomingMessage) -> BatchJob {
    claimed_job(store, std::slice::from_ref(message)).await
}

#[tokio::test]
async fn a_new_picture_is_collected_without_any_agent_call() {
    let dir = tempfile::tempdir().expect("tmp");
    let describer = Arc::new(CannedDescriber::new(
        "不许摸鱼",
        "白猫趴在键盘上，画风简笔，表情无奈，适合被说摸鱼时发",
    ));
    let (executor, store) = executor_fixture_full(
        &dir,
        0,
        sticker_png(64),
        Some(Arc::clone(&describer) as Arc<dyn crate::app::preflight::ImageDescriber>),
    )
    .await;
    let message = pictured_message("s1", "看这个", 1);
    let job = pictured_job(&store, &message).await;

    assert_eq!(
        finish_with(&executor, &store, &job, PLAIN_CONTRACT).await,
        TurnResult::Replied
    );

    // The row exists because the server put it there, not because the model asked.
    // The contract carries no sticker op and no tool call.
    let rows = store
        .list_stickers_page("1", None, 10, 0)
        .await
        .expect("读");
    assert_eq!(rows.len(), 1, "rows: {rows:?}");
    assert_eq!(rows[0].kind, "sticker");
    assert_eq!(rows[0].ocr, "不许摸鱼");
    assert!(rows[0].description.contains("白猫趴在键盘上"));
    assert_eq!(describer.calls.load(Ordering::Relaxed), 1);
    // The bytes really landed on disk next to the row.
    assert!(std::path::Path::new(&rows[0].file_path).is_file());
    // Described, unjudged: the shelf still needs her verdict.
    assert_eq!(rows[0].kept, crate::store::KEPT_UNJUDGED);
    assert_eq!(rows[0].seen_count, 1);

    // The audit trail says which door the picture came through.
    let mut events = store.recent_audit(200).await.expect("读");
    events.retain(|e| e.action.starts_with("sticker."));
    let actions: Vec<&str> = events.iter().map(|e| e.action.as_str()).collect();
    assert!(
        actions.iter().any(|a| a.starts_with("sticker.ingest.new")),
        "actions: {actions:?}"
    );
}

#[tokio::test]
async fn feeding_the_same_picture_again_only_bumps_the_counter() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture_full(
        &dir,
        0,
        sticker_png(64),
        Some(
            Arc::new(CannedDescriber::new("不许摸鱼", "白猫趴在键盘上，画风简笔"))
                as Arc<dyn crate::app::preflight::ImageDescriber>,
        ),
    )
    .await;

    let first = pictured_job(&store, &pictured_message("s1", "看这个", 1)).await;
    assert_eq!(
        finish_with(&executor, &store, &first, PLAIN_CONTRACT).await,
        TurnResult::Replied
    );
    let rows = store
        .list_stickers_page("1", None, 10, 0)
        .await
        .expect("读");
    assert_eq!(rows.len(), 1);
    let hash = rows[0].hash.clone();

    // Same picture, different encoding and size — what a re-upload looks like.
    let second = pictured_job(&store, &pictured_message("s2", "再来一次", 1)).await;
    assert_eq!(
        finish_with(&executor, &store, &second, PLAIN_CONTRACT).await,
        TurnResult::Replied
    );

    let rows = store
        .list_stickers_page("1", None, 10, 0)
        .await
        .expect("读");
    assert_eq!(rows.len(), 1, "重复喂同一张图不得新增行: {rows:?}");
    let row = store
        .sticker_by_hash("1", &hash)
        .await
        .expect("读")
        .expect("行还在");
    assert_eq!(row.seen_count, 2, "第二次喂必须 bump seen_count");
    assert!(row.last_seen_at.is_some());
}

// ---- Annex: the previous batch's refs on a resident continuation turn ----

/// One finished turn of a live resident conversation, as the executor sees
/// it: `conv` is the id the PTY handed back, which is what keys the annex.
async fn deliver_batch(
    executor: &TurnExecutor,
    store: &SqliteStore,
    conv: &str,
    events: &[IncomingMessage],
) {
    let job = claimed_job(store, events).await;
    let prepared = {
        let now = crate::now_unix();
        let group = store
            .get_group("1", &job.group_id)
            .await
            .expect("读组")
            .expect("应有组");
        executor
            .prepare_turn(&job, &group, now)
            .await
            .expect("组装 prompt")
    };
    executor
        .record_previous_batch(&job.group_id, conv, &prepared.messages)
        .await;
    // The batch is consumed by the delivery, exactly like a real turn.
    store.delete_pending(&job.event_ids).await.expect("清理");
}

#[tokio::test]
async fn annex_renames_the_previous_batch_for_a_continuation_turn() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut first = message("a1", "u1", "那台星火3还在吗", 0);
    first.received_at = crate::now_unix();
    deliver_batch(&executor, &store, "conv-keep", &[first]).await;

    let mut event = message("b1", "u2", "在的，昨天刚看过", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    let prompt = prompt_for(&executor, &store, &job).await;

    assert!(
        prompt.contains("# 上一轮消息（续接轮的引用编号）"),
        "续轮必须有对照表: {prompt}"
    );
    assert!(prompt.contains("[p1]"), "上批那条必须拿到 p1: {prompt}");
    assert!(
        prompt.contains("那台星火3还在吗"),
        "对照表要点出原文: {prompt}"
    );
    assert!(!prompt.contains("前情提要"), "续轮不付播种成本");
}

#[tokio::test]
async fn the_annex_shows_the_bots_own_previous_reply() {
    // Her own lines are never annex targets, but the next turn must still
    // see what she said: without it a correction ("没叫你") reads as a
    // fresh tease (2026-11-03, a production group).
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut first = message("a1", "u1", "为什么你还活着", 0);
    first.received_at = crate::now_unix();
    deliver_batch(&executor, &store, "conv-keep", &[first]).await;

    // A note keyed to a different conversation is ignored.
    executor
        .note_previous_reply("100", "conv-other", "不该出现")
        .await;
    let mut second = message("b1", "u2", "没叫你", 0);
    second.received_at = crate::now_unix();
    let job = claimed_job(&store, &[second]).await;
    let prompt = prompt_for(&executor, &store, &job).await;
    assert!(
        !prompt.contains("不该出现"),
        "错会话的备注不得出现: {prompt}"
    );
    assert!(
        !prompt.contains("你上一轮的回复"),
        "还没记下她的话时不得有此行: {prompt}"
    );

    executor
        .note_previous_reply("100", "conv-keep", "猫可是有九条命的")
        .await;
    let mut third = message("c1", "u2", "没叫你", 0);
    third.received_at = crate::now_unix();
    let job = claimed_job(&store, &[third]).await;
    let prompt = prompt_for(&executor, &store, &job).await;
    assert!(
        prompt.contains("- 你上一轮的回复：猫可是有九条命的"),
        "annex 必须带出她上一轮的话: {prompt}"
    );
}

#[tokio::test]
async fn a_first_turn_has_no_annex_section() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    store
        .set_conversation_id("1", "100", None, 1)
        .await
        .expect("清会话");
    // Even with a cache entry present (a dead conversation's leftovers),
    // a turn that starts a fresh conversation must not render one.
    let mut stale = message("z1", "u1", "上辈子的事", 0);
    stale.received_at = crate::now_unix();
    deliver_batch(&executor, &store, "conv-gone", &[stale]).await;

    let mut event = message("f1", "u2", "第一次说话", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    let prompt = prompt_for(&executor, &store, &job).await;
    assert!(
        !prompt.contains("# 上一轮消息"),
        "首轮不得出现 annex 段: {prompt}"
    );
}

#[tokio::test]
async fn a_rotated_conversation_drops_the_stale_annex() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut first = message("a1", "u1", "上一轮的话", 0);
    first.received_at = crate::now_unix();
    deliver_batch(&executor, &store, "conv-keep", &[first]).await;
    // A rotation writes a new conversation id; the cached annex belongs to
    // the dead one and must not leak into the fresh session.
    store
        .set_conversation_id("1", "100", Some("conv-new"), crate::now_unix())
        .await
        .expect("换会话");

    let mut event = message("b1", "u2", "新会话第一句", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    let prompt = prompt_for(&executor, &store, &job).await;
    assert!(
        !prompt.contains("[p1]"),
        "旧会话的编号不得串到新会话: {prompt}"
    );
}

#[tokio::test]
async fn a_previous_batch_quote_resolves_to_its_real_message_id() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut first = message("a1", "u1", "那台星火3还在吗", 0);
    first.received_at = crate::now_unix();
    deliver_batch(&executor, &store, "conv-keep", &[first]).await;

    let mut event = message("b1", "u2", "在的，昨天刚看过", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    let contract = r#"{"should_reply":true,"reply_parts":["还在，你去看看"],"quote_ref":"p1","mood_update":null,"topic_state":null}"#;
    assert_eq!(
        finish_with(&executor, &store, &job, contract).await,
        TurnResult::Replied
    );

    let rows = all_outbox_rows(&store).await;
    assert_eq!(rows.len(), 1, "应有一条回复: {rows:?}");
    assert_eq!(
        rows[0].quote_message_id.as_deref(),
        Some("a1"),
        "p1 必须挂到上一轮那条消息上，而不是本批的 b1"
    );
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a == "reply.quote.attached.g100.refp1"),
        "命中必须留审计: {:?}",
        audit_actions(&store).await
    );
    // The history channel's own hit counter, parallel to the batch one, so
    // annex usage is countable with a single prefix match.
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a == "reply.quote.history_attached.g100.refp1"),
        "历史通道命中必须有自己的审计行: {:?}",
        audit_actions(&store).await
    );
}

#[tokio::test]
async fn an_annex_ref_never_lands_on_the_batch_message_with_the_same_number() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    // Same shape on both sides: two messages each, so a ref that leaked
    // across the number spaces would resolve — to the wrong message.
    let mut old_one = message("old1", "u1", "上一轮第一条", 0);
    old_one.received_at = crate::now_unix();
    let mut old_two = message("old2", "u2", "上一轮第二条", 0);
    old_two.received_at = crate::now_unix();
    deliver_batch(&executor, &store, "conv-keep", &[old_one, old_two]).await;

    let mut new_one = message("new1", "u3", "这一轮第一条", 0);
    new_one.received_at = crate::now_unix();
    let mut new_two = message("new2", "u4", "这一轮第二条", 0);
    new_two.received_at = crate::now_unix();
    let job = claimed_job(&store, &[new_one, new_two]).await;

    // The model points at the *second* previous-batch message (p2), which is
    // where a `m2` leak would have landed instead.
    let contract = r#"{"should_reply":true,"reply_parts":[{"text":"对，第二条说的对","quote_ref":"p2","quote_echo":"上一轮第二条"}],"mood_update":null,"topic_state":null}"#;
    assert_eq!(
        finish_with(&executor, &store, &job, contract).await,
        TurnResult::Replied
    );

    let rows = all_outbox_rows(&store).await;
    assert_eq!(rows.len(), 1, "应有一条回复: {rows:?}");
    assert_eq!(
        rows[0].quote_message_id.as_deref(),
        Some("old2"),
        "p2 必须是上一轮第二条，绝不能落到本批同号的 new2"
    );
}

#[tokio::test]
async fn a_stale_annex_ref_is_dropped_softly_and_the_text_survives() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut first = message("a1", "u1", "上一轮的话", 0);
    first.received_at = crate::now_unix();
    deliver_batch(&executor, &store, "conv-keep", &[first]).await;

    let mut event = message("b1", "u2", "新的消息", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    // p7 is not in this turn's annex (only p1 was cached).
    let contract = r#"{"should_reply":true,"reply_parts":["正文照发"],"quote_ref":"p7","mood_update":null,"topic_state":null}"#;
    assert_eq!(
        finish_with(&executor, &store, &job, contract).await,
        TurnResult::Replied
    );

    let rows = all_outbox_rows(&store).await;
    assert_eq!(rows.len(), 1, "正文必须照发: {rows:?}");
    assert_eq!(rows[0].text, "正文照发");
    assert!(
        rows[0].quote_message_id.is_none(),
        "失效编号不得挂到任何消息上: {rows:?}"
    );
    // The stale ref fails validation (it is not in this turn's annex), which
    // is the existing top-level soft-drop path: the raw ref is recorded and
    // the reply goes out without it.
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a == "reply.quote.dropped.g100.refp7"),
        "软丢必须留审计: {:?}",
        audit_actions(&store).await
    );
}

#[tokio::test]
async fn the_mcp_channel_reads_the_same_annex() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut first = message("a1", "u1", "那台星火3还在吗", 0);
    first.received_at = crate::now_unix();
    deliver_batch(&executor, &store, "conv-keep", &[first]).await;

    let mut event = message("b1", "u2", "在的", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;

    // Validation is where the two channels meet: the MCP barrier hands
    // `validate_salvaged` the very same scope the text channel uses, so a
    // contract that survives here survives there, and the annex ref is
    // accepted only because `scope.history_refs` carries it.
    let now = crate::now_unix();
    let group = store
        .get_group("1", &job.group_id)
        .await
        .expect("读组")
        .expect("应有组");
    let prepared = executor
        .prepare_turn(&job, &group, now)
        .await
        .expect("组装 prompt");
    assert!(
        prepared.scope.history_refs.iter().any(|r| r == "p1"),
        "scope 必须带上这一轮的 p 编号: {:?}",
        prepared.scope.history_refs
    );
    let contract = r#"{"should_reply":true,"reply_parts":["还在"],"quote_ref":"p1"}"#;
    let (validated, _report) = crate::agent::contract::validate_salvaged(contract, &prepared.scope)
        .expect("MCP 屏障必须接受同一个 p1");
    assert_eq!(validated.output.quote_ref.as_deref(), Some("p1"));
    assert!(
        validated.rejections.reasons.is_empty(),
        "p1 不得被当成 unknown ref: {:?}",
        validated.rejections
    );
}

#[tokio::test]
async fn a_previous_batch_target_still_obeys_the_addressee_rule() {
    let dir = tempfile::tempdir().expect("tmp");
    let (executor, store) = executor_fixture(&dir).await;
    let mut first = message("a1", "u1", "上一轮是群友说的", 0);
    first.received_at = crate::now_unix();
    deliver_batch(&executor, &store, "conv-keep", &[first]).await;

    let mut event = message("b1", "u2", "新的消息", 0);
    event.received_at = crate::now_unix();
    let job = claimed_job(&store, &[event]).await;
    // First part addresses the 主人, so it may only quote the 主人.
    let contract = r#"{"should_reply":true,"reply_parts":["主人，刚才那条不是说你"],"quote_ref":"p1","mood_update":null,"topic_state":null}"#;
    assert_eq!(
        finish_with(&executor, &store, &job, contract).await,
        TurnResult::Replied
    );

    let rows = all_outbox_rows(&store).await;
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].quote_message_id.is_none(),
        "称呼主人却引用群友 -> 必须丢引用: {rows:?}"
    );
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a == "reply.quote.dropped.g100.addressee_mismatch"),
        "必须记 addressee_mismatch: {:?}",
        audit_actions(&store).await
    );
    // An annex target that loses its gate also gets the history-specific
    // line, so the annex's own drop rate is readable without having to guess
    // from the shared `reply.quote.dropped` bucket.
    assert!(
        audit_actions(&store)
            .await
            .iter()
            .any(|a| a == "reply.quote.history_dropped.g100.refp1.addressee_mismatch"),
        "必须记历史专属丢引用行: {:?}",
        audit_actions(&store).await
    );
}

#[test]
fn insert_interrupt_only_in_esc_mode_while_busy() {
    // The rollback contract in one table: ESC mode off never interrupts,
    // an idle screen never needs interrupting, and the only cell that
    // interrupts is ESC mode with a generation still on screen.
    assert!(!insert_needs_interrupt(false, false));
    assert!(!insert_needs_interrupt(false, true));
    assert!(!insert_needs_interrupt(true, true));
    assert!(insert_needs_interrupt(true, false));
}
