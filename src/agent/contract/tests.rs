//! Tests moved out verbatim (std-split); assertions unchanged.

use super::*;

fn scope() -> ValidationScope {
    ValidationScope {
        speaker_refs: vec!["u1".into(), "u2".into()],
        message_refs: vec!["m1".into(), "m2".into(), "m3".into()],
        memory_refs: vec!["mem_a".into()],
        history_refs: vec!["h1".into(), "h2".into()],
        sticker_hashes: vec![
            "a1b2c3d4e5f60718293a4b5c6d7e8f901a2b3c4d5e6f708192a3b4c5d6e7f809".into(),
            "b1c2d3e4f5061728394a5b6c7d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f90a".into(),
        ],
    }
}

fn ok_doc() -> serde_json::Value {
    serde_json::json!({
        "should_reply": true,
        "reply_text": "今天聊点轻松的",
        "mood_update": {"label": "calm", "reason": "话题温和"},
        "topic_state": "闲聊中",
        "memory_ops": [{
            "op": "add",
            "subject_ref": "u1",
            "content": "喜欢猫",
            "kind": "preference",
            "source_refs": ["m1"],
            "confidence": 0.8
        }]
    })
}

#[test]
fn accepts_the_design_example() {
    let v = validate_value(&ok_doc(), &scope()).expect("应当通过校验");
    assert!(v.output.should_reply);
    assert_eq!(v.output.reply_text, "今天聊点轻松的");
    assert_eq!(v.output.memory_ops.len(), 1);
    assert_eq!(v.rejections.total, 0);
    assert_eq!(v.output.mood_update.unwrap().label, "calm");
}

#[test]
fn quote_ref_must_point_inside_the_batch() {
    // In-batch target: kept.
    let mut doc = ok_doc();
    doc["quote_ref"] = serde_json::json!("m2");
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert_eq!(v.output.quote_ref.as_deref(), Some("m2"));
    assert_eq!(v.rejections.total, 0);

    // Outside the batch (another group's ref, a made-up one, a speaker
    // ref): dropped with a soft rejection, the reply itself survives.
    for bad in ["m9", "u1", "mem_a"] {
        let mut doc = ok_doc();
        doc["quote_ref"] = serde_json::json!(bad);
        let v = validate_value(&doc, &scope()).expect("通过校验");
        assert_eq!(v.output.quote_ref, None, "{bad} 不应保留");
        assert!(v.output.should_reply, "回复本体不应被引用问题连累");
        assert!(v.rejections.reasons.contains(&RejectReason::UnknownRef));
    }

    // Absent or blank: simply no quote, no rejection.
    let v = validate_value(&ok_doc(), &scope()).expect("通过校验");
    assert_eq!(v.output.quote_ref, None);
    let mut doc = ok_doc();
    doc["quote_ref"] = serde_json::json!("  ");
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert_eq!(v.output.quote_ref, None);
    assert_eq!(v.rejections.total, 0);
}

#[test]
fn poke_ref_only_survives_on_a_speaking_turn_and_inside_the_batch() {
    // In-batch speaker on a speaking turn: kept.
    let mut doc = ok_doc();
    doc["poke_ref"] = serde_json::json!("u2");
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert_eq!(v.output.poke_ref.as_deref(), Some("u2"));
    assert_eq!(v.rejections.total, 0);

    // A ref that is not a speaker of this batch (a message ref, a memory
    // key, a made-up one) is dropped like a bad quote: the reply still goes.
    for bad in ["u9", "m1", "mem_a"] {
        let mut doc = ok_doc();
        doc["poke_ref"] = serde_json::json!(bad);
        let v = validate_value(&doc, &scope()).expect("通过校验");
        assert_eq!(v.output.poke_ref, None, "{bad} 不应保留");
        assert!(v.output.should_reply, "回复本体不应被戳的问题连累");
        assert!(v.rejections.reasons.contains(&RejectReason::UnknownRef));
    }

    // Absent or blank: no poke, no rejection.
    let v = validate_value(&ok_doc(), &scope()).expect("通过校验");
    assert_eq!(v.output.poke_ref, None);
    let mut doc = ok_doc();
    doc["poke_ref"] = serde_json::json!("  ");
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert_eq!(v.output.poke_ref, None);
    assert_eq!(v.rejections.total, 0);
}

#[test]
fn a_silent_turn_may_not_carry_a_poke() {
    // `PokeWithoutReply`, not `UnknownRef`: the ref may be perfectly good,
    // the problem is that a silence has no business poking anyone.
    let mut doc = ok_doc();
    doc["should_reply"] = serde_json::json!(false);
    doc["reply_text"] = serde_json::json!(null);
    doc["poke_ref"] = serde_json::json!("u2");
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert_eq!(v.output.poke_ref, None);
    assert!(!v.output.should_reply);
    assert!(
        v.rejections
            .reasons
            .contains(&RejectReason::PokeWithoutReply),
        "got: {:?}",
        v.rejections
    );
    assert_eq!(
        RejectReason::PokeWithoutReply.as_str(),
        "poke_without_reply"
    );

    // The salvage path drops it the same way instead of voiding the reply.
    let (v, report) = validate_with_salvage(&doc, &scope(), SalvageLayer::Partial)
        .expect("抢救路径不应因戳而失败");
    assert_eq!(v.output.poke_ref, None);
    assert!(
        report
            .dropped
            .iter()
            .any(|(field, why, _)| *field == "poke_ref" && *why == "poke_without_reply"),
        "got: {:?}",
        report.dropped
    );
}

#[test]
fn salvage_keeps_a_good_poke_and_drops_a_bad_one() {
    let mut doc = ok_doc();
    doc["poke_ref"] = serde_json::json!("u2");
    // Ordinary salvage trip (a number where a string belongs) still keeps
    // the poke — it is the rest of the document that needed repair.
    doc["topic_state"] = serde_json::json!(42);
    let (v, _report) =
        validate_with_salvage(&doc, &scope(), SalvageLayer::Partial).expect("抢救通过");
    assert_eq!(v.output.poke_ref.as_deref(), Some("u2"));

    let mut doc = ok_doc();
    doc["poke_ref"] = serde_json::json!("u9");
    doc["topic_state"] = serde_json::json!(42);
    let (v, report) =
        validate_with_salvage(&doc, &scope(), SalvageLayer::Partial).expect("抢救通过");
    assert_eq!(v.output.poke_ref, None);
    assert!(
        report
            .dropped
            .iter()
            .any(|(field, why, _)| *field == "poke_ref" && *why == "unknown_ref"),
        "got: {:?}",
        report.dropped
    );
}

#[test]
fn per_part_quotes_validate_against_batch_and_history_refs() {
    let mut doc = ok_doc();
    doc["reply_parts"] = serde_json::json!([
        {"text": "答新问题", "quote_ref": "m2", "quote_echo": "原文片段"},
        "普通接话",
        {"text": "答历史里那条", "quote_ref": "h1", "quote_echo": "更早的话"},
        {"text": "瞎编的编号", "quote_ref": "m42", "quote_echo": "x"},
    ]);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert_eq!(v.output.reply_parts.len(), 4);
    assert_eq!(v.output.part_quotes.len(), 2, "只有合法编号的段带引用");
    assert_eq!(v.output.part_quotes[0].part_index, 0);
    assert_eq!(v.output.part_quotes[0].quote_ref, "m2");
    assert_eq!(
        v.output.part_quotes[0].quote_echo.as_deref(),
        Some("原文片段")
    );
    assert_eq!(v.output.part_quotes[1].part_index, 2);
    assert_eq!(v.output.part_quotes[1].quote_ref, "h1");
    assert!(v.rejections.reasons.contains(&RejectReason::UnknownRef));
}

#[test]
fn should_reply_false_suppresses_the_reply() {
    let mut doc = ok_doc();
    doc["should_reply"] = serde_json::json!(false);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(!v.output.should_reply);
}

#[test]
fn empty_reply_with_should_reply_true_is_rejected() {
    let mut doc = ok_doc();
    doc["reply_text"] = serde_json::json!("   ");
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(!v.output.should_reply, "空回复不得发送");
    assert_eq!(v.rejections.reasons, vec![RejectReason::EmptyReply]);
}

#[test]
fn a_bare_should_reply_false_is_a_refusal() {
    assert!(is_bare_refusal(r#"{"should_reply": false}"#));
    assert!(is_bare_refusal(
        r#"{"should_reply": false, "reply_parts": [], "reply_text": "  "}"#
    ));
    assert!(!is_bare_refusal(
        r#"{"should_reply": false, "reply_parts": ["在的"]}"#
    ));
    assert!(!is_bare_refusal(
        r#"{"should_reply": false, "sticker_ops": [{"op": "send", "hash": "a1b2c3d4"}]}"#
    ));
    assert!(!is_bare_refusal(
        r#"{"should_reply": true, "reply_parts": ["在的"]}"#
    ));
}

#[test]
fn overlong_reply_is_not_sent() {
    let mut doc = ok_doc();
    doc["reply_text"] = serde_json::json!("啊".repeat(MAX_REPLY_CHARS + 1));
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(!v.output.should_reply);
    assert!(v.rejections.reasons.contains(&RejectReason::TooLong));
}

/// Sending a sticker with no words is a real way to reply: a picture answers
/// on its own. Requiring text would silently drop those turns.
#[test]
fn a_sticker_alone_can_be_a_reply() {
    let mut doc = ok_doc();
    doc["reply_text"] = serde_json::json!("");
    doc["reply_parts"] = serde_json::json!([]);
    doc["sticker_ops"] = serde_json::json!([{"op": "send", "hash": "a1b2c3d4"}]);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(v.output.should_reply, "只发表情也是回复");
    assert_eq!(v.output.sticker_ops.len(), 1);
    // A `send` keeps the prefix as given: the turn resolves it against the
    // library, because the picture she sends is usually not in this batch.
    assert_eq!(v.output.sticker_ops[0].hash, "a1b2c3d4");
    assert_eq!(v.output.sticker_ops[0].op, StickerOpKind::Send);
    assert!(v.output.reply_parts.is_empty());
}

/// Nothing to say and nothing to send is still silence, not an empty message.
#[test]
fn no_text_and_no_sticker_is_still_empty() {
    let mut doc = ok_doc();
    doc["reply_text"] = serde_json::json!("");
    doc["reply_parts"] = serde_json::json!([]);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(!v.output.should_reply);
    assert_eq!(v.rejections.reasons, vec![RejectReason::EmptyReply]);
}

/// Keep/ignore belong to the sticker_keep MCP tool, not this JSON.
/// Unknown ops (delete/rename) are refused the same way.
#[test]
fn json_keep_ignore_and_unknown_sticker_ops_are_forbidden() {
    for op in ["keep", "ignore", "delete", "rename"] {
        let mut doc = ok_doc();
        doc["sticker_ops"] = serde_json::json!([{"op": op, "hash": "a1b2c3d4"}]);
        let v = validate_value(&doc, &scope()).expect("通过校验");
        assert!(v.output.sticker_ops.is_empty(), "{op} 必须被拒绝");
        assert!(v.rejections.reasons.contains(&RejectReason::Forbidden));
    }
}

/// Only three pictures may go out in one turn: beyond that it is spam, not a reply.
#[test]
fn sticker_sends_are_capped() {
    let mut scope = scope();
    scope.sticker_hashes = (0..5).map(|n| format!("{n:02x}").repeat(32)).collect();
    let mut doc = ok_doc();
    doc["sticker_ops"] = serde_json::json!(
        (0..5)
            .map(|n| serde_json::json!({"op": "send", "hash": format!("{n:02x}")}))
            .collect::<Vec<_>>()
    );
    let v = validate_value(&doc, &scope).expect("通过校验");
    assert_eq!(v.output.sticker_ops.len(), MAX_STICKER_SENDS);
    assert!(v.rejections.reasons.contains(&RejectReason::TooManyOps));
}

/// The same hash twice is one action, not two.
#[test]
fn a_repeated_hash_is_deduplicated() {
    let mut doc = ok_doc();
    doc["sticker_ops"] = serde_json::json!([
        {"op": "send", "hash": "a1b2c3d4"},
        {"op": "send", "hash": "a1b2c3d4"},
    ]);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert_eq!(v.output.sticker_ops.len(), 1);
}

#[test]
fn cq_code_and_mass_mention_are_forbidden() {
    for bad in ["[CQ:at,qq=all]", "@全体成员 来看", "<@1234>"] {
        let mut doc = ok_doc();
        doc["reply_text"] = serde_json::json!(bad);
        let v = validate_value(&doc, &scope()).expect("通过校验");
        assert!(!v.output.should_reply, "{bad} 必须被拒绝");
        assert!(v.rejections.reasons.contains(&RejectReason::Forbidden));
    }
}

#[test]
fn memory_delete_is_forbidden() {
    let mut doc = ok_doc();
    doc["memory_ops"] = serde_json::json!([{
        "op": "delete",
        "memory_ref": "mem_a",
        "content": "x",
        "kind": "fact",
        "source_refs": ["m1"]
    }]);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(v.output.memory_ops.is_empty(), "模型不得删除记忆");
    assert!(v.rejections.reasons.contains(&RejectReason::Forbidden));
}

#[test]
fn unknown_and_raw_id_refs_are_rejected() {
    // Alias not in this batch.
    let mut doc = ok_doc();
    doc["memory_ops"][0]["subject_ref"] = serde_json::json!("u9");
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(v.output.memory_ops.is_empty());
    assert!(v.rejections.reasons.contains(&RejectReason::UnknownRef));

    // A raw QQ number where an alias belongs.
    let mut doc = ok_doc();
    doc["memory_ops"][0]["subject_ref"] = serde_json::json!("123456789");
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(v.output.memory_ops.is_empty());
    assert!(v.rejections.reasons.contains(&RejectReason::CrossGroup));

    // A source message ref from another batch.
    let mut doc = ok_doc();
    doc["memory_ops"][0]["source_refs"] = serde_json::json!(["m99"]);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(v.output.memory_ops.is_empty());
    assert!(v.rejections.reasons.contains(&RejectReason::UnknownRef));
}

#[test]
fn missing_sources_and_bad_kind_and_bad_confidence_are_rejected() {
    for (field, value, reason) in [
        ("source_refs", serde_json::json!([]), RejectReason::NoSource),
        ("kind", serde_json::json!("sql"), RejectReason::BadKind),
        (
            "confidence",
            serde_json::json!(1.5),
            RejectReason::BadConfidence,
        ),
    ] {
        let mut doc = ok_doc();
        doc["memory_ops"][0][field] = value;
        let v = validate_value(&doc, &scope()).expect("通过校验");
        assert!(v.output.memory_ops.is_empty(), "{field} 非法时必须拒绝");
        assert!(
            v.rejections.reasons.contains(&reason),
            "{field} 应当产生 {reason:?}"
        );
    }
}

#[test]
fn update_requires_an_existing_memory_ref() {
    let mut doc = ok_doc();
    doc["memory_ops"] = serde_json::json!([{
        "op": "update",
        "content": "改了",
        "kind": "fact",
        "source_refs": ["m1"]
    }]);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(v.output.memory_ops.is_empty());
    assert!(v.rejections.reasons.contains(&RejectReason::MissingTarget));

    // Not a store key: unknown, not a cross-group leak.
    let mut doc = ok_doc();
    doc["memory_ops"] = serde_json::json!([{
        "op": "update",
        "memory_ref": "mem_other",
        "content": "改了",
        "kind": "fact",
        "source_refs": ["m1"]
    }]);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(v.output.memory_ops.is_empty());
    assert!(v.rejections.reasons.contains(&RejectReason::UnknownRef));

    // A QQ number in the ref slot is a cross-group attempt.
    let mut doc = ok_doc();
    doc["memory_ops"] = serde_json::json!([{
        "op": "update",
        "memory_ref": "123456789",
        "content": "改了",
        "kind": "fact",
        "source_refs": ["m1"]
    }]);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(v.output.memory_ops.is_empty());
    assert!(v.rejections.reasons.contains(&RejectReason::CrossGroup));

    // A well-formed store key is accepted even if this batch did not inject it.
    let mut doc = ok_doc();
    doc["memory_ops"] = serde_json::json!([{
        "op": "update",
        "memory_ref": "mem:mem_abc123",
        "content": "改了",
        "kind": "fact",
        "source_refs": ["m1"]
    }]);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert_eq!(v.output.memory_ops.len(), 1);
    assert_eq!(v.output.memory_ops[0].op, MemoryOpKind::Update);
    assert_eq!(
        v.output.memory_ops[0].memory_ref.as_deref(),
        Some("mem_abc123")
    );
}

#[test]
fn add_with_a_chosen_memory_ref_is_forbidden() {
    let mut doc = ok_doc();
    doc["memory_ops"][0]["memory_ref"] = serde_json::json!("mem_mine");
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(
        v.output.memory_ops.is_empty(),
        "新增记忆的 ref 必须由 Rust 生成"
    );
    assert!(v.rejections.reasons.contains(&RejectReason::Forbidden));
}

#[test]
fn overlong_memory_content_is_rejected() {
    let mut doc = ok_doc();
    doc["memory_ops"][0]["content"] = serde_json::json!("啊".repeat(501));
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert!(v.output.memory_ops.is_empty());
    assert!(v.rejections.reasons.contains(&RejectReason::TooLong));
}

#[test]
fn too_many_ops_are_capped() {
    let ops: Vec<serde_json::Value> = (0..12)
        .map(|_| {
            serde_json::json!({
                "op": "add", "content": "c", "kind": "fact",
                "source_refs": ["m1"], "confidence": 0.5
            })
        })
        .collect();
    let mut doc = ok_doc();
    doc["memory_ops"] = serde_json::json!(ops);
    let v = validate_value(&doc, &scope()).expect("通过校验");
    assert_eq!(v.output.memory_ops.len(), MAX_MEMORY_OPS);
    assert!(v.rejections.reasons.contains(&RejectReason::TooManyOps));
}

#[test]
fn malformed_json_is_reported_not_panicking() {
    assert_eq!(
        validate("{not json", &scope()).unwrap_err(),
        RejectReason::Malformed
    );
    assert_eq!(
        validate("[1,2,3]", &scope()).unwrap_err(),
        RejectReason::Malformed
    );
}

#[test]
fn rejection_summary_is_grouped() {
    let mut r = Rejections::default();
    r.push(RejectReason::TooLong);
    r.push(RejectReason::TooLong);
    r.push(RejectReason::BadKind);
    assert_eq!(r.total, 3);
    assert_eq!(r.summary(), vec![("too_long", 2), ("bad_kind", 1)]);
}

// ---- salvage (L3/L4) -------------------------------------------------

/// A clean document must not be touched by the salvage path: same output,
/// same rejections, and `layer` stays `None`.
#[test]
fn clean_output_passes_with_no_layer() {
    let doc = ok_doc();
    let strict = validate(&doc.to_string(), &scope()).expect("strict ok");
    let (salvaged, report) = validate_salvaged(&doc.to_string(), &scope()).expect("salvaged ok");
    assert_eq!(report.layer, SalvageLayer::None);
    assert!(report.is_clean());
    assert_eq!(salvaged.output, strict.output);
    assert_eq!(salvaged.rejections, strict.rejections);
}

/// The accident this whole module exists for: the reply is intact, only one
/// memory op names a ref that is not in the batch.
#[test]
fn one_bad_memory_op_no_longer_kills_the_reply() {
    let raw = r#"{
            "should_reply": true,
            "reply_parts": ["在的"],
            "memory_ops": [
                {"op":"add","content":"喜欢猫","kind":"fact","source_refs":["m1"]},
                {"op":"add","content":"喜欢狗","kind":"fact","source_refs":["m9"]}
            ]
        }"#;
    // The strict path accepts this too (a bad op is dropped and counted), so
    // the interesting assertion is that both paths agree on the outcome.
    let strict = validate(raw, &scope()).expect("strict ok");
    let (salvaged, report) = validate_salvaged(raw, &scope()).expect("salvaged ok");
    assert_eq!(salvaged.output.reply_parts, vec!["在的".to_string()]);
    assert_eq!(salvaged.output.memory_ops.len(), 1);
    assert_eq!(report.layer, SalvageLayer::None);
    assert_eq!(strict.output.memory_ops.len(), 1);
}

/// Prose around the answer is no longer fatal, and the layer is reported.
#[test]
fn prose_wrapped_document_is_salvaged_from_a_span() {
    let raw = "好的，我来回答。\n\n{\"should_reply\":true,\"reply_parts\":[\"在的\"]}\n\n以上。";
    assert!(validate(raw, &scope()).is_err(), "严格路径应该失败");
    let (validated, report) = validate_salvaged(raw, &scope()).expect("跨度可用");
    assert_eq!(report.layer, SalvageLayer::Span);
    assert_eq!(validated.output.reply_parts, vec!["在的".to_string()]);
}

/// A trailing comma is a repair, not a loss.
#[test]
fn trailing_comma_is_repaired() {
    let raw = r#"{"should_reply":true,"reply_parts":["在的"],}"#;
    let (validated, report) = validate_salvaged(raw, &scope()).expect("修好可用");
    assert_eq!(report.layer, SalvageLayer::Repair);
    assert_eq!(report.dropped_total(), 0);
    assert_eq!(validated.output.reply_parts, vec!["在的".to_string()]);
}

/// A part carrying forbidden markup is dropped, but the other parts still go.
#[test]
fn forbidden_part_is_dropped_and_the_rest_survives() {
    let raw = r#"{"should_reply":true,"reply_parts":["正常的一句","[CQ:at,qq=all]"],"memory_ops":"nope"}"#;
    let (validated, report) = validate_salvaged(raw, &scope()).expect("还有话可说");
    assert_eq!(validated.output.reply_parts, vec!["正常的一句".to_string()]);
    assert!(validated.output.should_reply);
    assert!(
        report
            .dropped
            .iter()
            .any(|(s, r, n)| *s == "reply_parts" && *r == "forbidden" && *n == 1),
        "{:?}",
        report.dropped
    );
}

/// ...but if every part is forbidden, the document is refused outright. The
/// safety rule is not downgraded by the salvage path.
#[test]
fn all_parts_forbidden_still_refuses_the_document() {
    let raw = r#"{"should_reply":true,"reply_parts":["[CQ:at,qq=all]"],"memory_ops":"nope"}"#;
    assert_eq!(
        validate_salvaged(raw, &scope()).unwrap_err(),
        RejectReason::EmptyReply
    );
}

/// An out-of-range confidence is clamped, not dropped with its whole op.
///
/// Called straight on the section validator: `validate_salvaged` runs the strict
/// path first, which for this document succeeds by dropping just the op — the
/// clamp only matters when the document failed strict for some other reason.
#[test]
fn out_of_range_confidence_is_clamped() {
    let raw = r#"{"should_reply":true,"reply_parts":["在的"],"memory_ops":[
            {"op":"add","content":"喜欢猫","kind":"fact","source_refs":["m1"],"confidence":7.5}
        ]}"#;
    let value: Value = serde_json::from_str(raw).expect("json");
    let (validated, report) =
        validate_with_salvage(&value, &scope(), SalvageLayer::Partial).expect("钳制后可用");
    assert_eq!(validated.output.memory_ops.len(), 1);
    assert_eq!(validated.output.memory_ops[0].confidence, 1.0);
    assert_eq!(report.clamped, 1);
}

/// An unknown quote ref costs the quote, not the reply.
#[test]
fn unknown_quote_ref_is_dropped_softly() {
    let raw = r#"{"should_reply":true,"reply_parts":["在的"],"quote_ref":"m99"}"#;
    let value: Value = serde_json::from_str(raw).expect("json");
    let (validated, report) =
        validate_with_salvage(&value, &scope(), SalvageLayer::Partial).expect("回复仍在");
    assert_eq!(validated.output.quote_ref, None);
    assert_eq!(validated.output.reply_parts, vec!["在的".to_string()]);
    assert!(
        report
            .dropped
            .iter()
            .any(|(s, r, _)| *s == "quote_ref" && *r == "unknown_ref"),
        "{:?}",
        report.dropped
    );
}

/// `should_reply` written as a number or a string still means what it says.
#[test]
fn should_reply_is_read_leniently() {
    for raw in [
        r#"{"should_reply":1,"reply_parts":["在的"]}"#,
        r#"{"should_reply":"true","reply_parts":["在的"]}"#,
    ] {
        let (validated, _) = validate_salvaged(raw, &scope()).expect("归一后可用");
        assert!(validated.output.should_reply, "{raw}");
        assert_eq!(validated.output.reply_parts, vec!["在的".to_string()]);
    }
}

/// Non-string entries in `reply_parts` are dropped and counted.
#[test]
fn non_string_reply_parts_are_dropped() {
    let raw = r#"{"should_reply":true,"reply_parts":["在的",42,null]}"#;
    let (validated, report) = validate_salvaged(raw, &scope()).expect("可用");
    assert_eq!(validated.output.reply_parts, vec!["在的".to_string()]);
    assert!(
        report
            .dropped
            .iter()
            .any(|(s, r, n)| *s == "reply_parts" && *r == "malformed" && *n == 1),
        "{:?}",
        report.dropped
    );
}

/// A reply over the character budget is truncated on a character boundary, so
/// the turn still says something instead of nothing.
#[test]
fn over_long_reply_is_truncated_not_voided() {
    let long = "字".repeat(MAX_REPLY_CHARS + 50);
    let doc = serde_json::json!({ "should_reply": true, "reply_parts": [long] });
    let (validated, report) =
        validate_with_salvage(&doc, &scope(), SalvageLayer::Partial).expect("截断后可用");
    assert_eq!(
        validated.output.reply_parts[0].chars().count(),
        MAX_REPLY_CHARS
    );
    assert_eq!(report.clamped, 1);
    assert!(validated.output.should_reply);
}

/// A truncated document is closed by the repair layer and the incomplete tail
/// element is dropped.
#[test]
fn truncated_document_is_closed_and_salvaged() {
    let raw = r#"{"should_reply":true,"reply_parts":["在的"],"memory_ops":[{"op":"add","cont"#;
    let (validated, report) = validate_salvaged(raw, &scope()).expect("截断可补");
    assert_eq!(report.layer, SalvageLayer::Repair);
    assert_eq!(validated.output.reply_parts, vec!["在的".to_string()]);
    assert!(validated.output.memory_ops.is_empty());
}

/// Plain prose that is obviously a reply becomes the reply.
#[test]
fn plain_text_reply_is_the_last_resort() {
    let raw = "这个我熟，昨天刚看过那本书。";
    assert!(validate(raw, &scope()).is_err());
    let (validated, report) = validate_salvaged(raw, &scope()).expect("文本兜底");
    assert_eq!(report.layer, SalvageLayer::Text);
    assert_eq!(validated.output.reply_text, raw);
    assert!(validated.output.should_reply);
}

/// Text that looks like a mangled contract, or carries forbidden markup, is
/// never sent as plain text.
#[test]
fn text_fallback_refuses_markup_and_contract_debris() {
    for raw in [
        "[CQ:at,qq=all] 都出来",
        "should_reply true 但是我只说了一堆字段名",
        "```code```",
    ] {
        assert!(
            validate_salvaged(raw, &scope()).is_err(),
            "{raw} 不应被当作文本回复"
        );
    }
}

/// The report renders a stable audit suffix.
#[test]
fn salvage_report_renders_audit_suffix() {
    let mut report = SalvageReport::new(SalvageLayer::Partial);
    report.note("memory_ops", "unknown_ref");
    report.note("memory_ops", "unknown_ref");
    report.note("reply_parts", "forbidden");
    report.clamped = 2;
    assert_eq!(report.audit_suffix(), "partial.d3.c2");
    assert!(!report.is_clean());
    assert!(
        report.detail().starts_with("partial: "),
        "{}",
        report.detail()
    );
}
