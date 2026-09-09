//! Synthetic legacy-contract cases; never open real sessions.
#[path = "../src/local_source/normalize.rs"]
mod normalize;

use serde_json::{json, Value};
use std::io::Write;

fn parse_raw(raw: &[u8], harness: &str, now: f64, text: bool) -> Value {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(raw).unwrap();
    normalize::parse_page(file.path(), harness, now, text, 0, usize::MAX).unwrap()
}
fn encoded(rows: &[Value]) -> Vec<u8> {
    let mut raw = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut raw, row).unwrap();
        raw.push(b'\n');
    }
    raw
}
fn parse(rows: &[Value], harness: &str, now: f64, text: bool) -> Value {
    parse_raw(&encoded(rows), harness, now, text)
}
fn claude(id: &str, parent: Value, role: &str, time: Value, content: Value, stop: Value) -> Value {
    json!({"type":role,"version":"2.1.263","cwd":"/synthetic","uuid":id,"parentUuid":parent,"sessionId":"s","isSidechain":false,"origin":{"kind":"human"},"timestamp":time,"message":{"role":role,"content":content,"stop_reason":stop}})
}
fn omp(id: &str, parent: Value, role: &str, time: Value, content: Value, stop: &str) -> Value {
    json!({"type":"message","id":id,"parentId":parent,"message":{"role":role,"attribution":"user","timestamp":time,"content":content,"stopReason":stop,"completedAt":4000}})
}
fn codex(kind: &str, time: f64, payload: Value) -> Value {
    json!({"type":kind,"timestamp":time,"payload":payload})
}
fn conversation(harness: &str) -> Vec<Value> {
    match harness {
        "claude" => vec![
            claude("u", Value::Null, "user", json!(1), json!("Question"), Value::Null),
            claude(
                "a",
                json!("u"),
                "assistant",
                json!(3),
                json!([{ "type":"text","text":"Answer"},{"type":"thinking","thinking":"EXCLUDED"}]),
                json!("end_turn"),
            ),
        ],
        "omp" => vec![
            json!({"type":"session","id":"s","version":3,"cwd":"/synthetic"}),
            omp(
                "u",
                Value::Null,
                "user",
                json!(1000),
                json!([{"type":"text","text":"Question"}]),
                "",
            ),
            omp(
                "a",
                json!("u"),
                "assistant",
                json!(3000),
                json!([{"type":"text","text":"Answer"},{"type":"thinking","thinking":"EXCLUDED"}]),
                "stop",
            ),
        ],
        "codex" => vec![
            codex(
                "session_meta",
                0.0,
                json!({"id":"s","cwd":"/synthetic","cli_version":"0.144.1","source":"cli","thread_source":"user"}),
            ),
            codex("event_msg", 0.5, json!({"type":"task_started","turn_id":"t"})),
            codex("turn_context", 1.0, json!({"cwd":"/synthetic","turn_id":"t"})),
            codex(
                "response_item",
                2.0,
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Question"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t"}}),
            ),
            codex("event_msg", 2.0, json!({"type":"user_message","message":"Question"})),
            codex(
                "response_item",
                3.0,
                json!({"type":"message","id":"a","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Answer"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t"}}),
            ),
            codex("event_msg", 4.0, json!({"type":"task_complete","turn_id":"t"})),
        ],
        _ => unreachable!(),
    }
}

#[test]
fn metadata_has_identical_identity_membership_and_bytes_but_no_content() {
    for harness in ["claude", "codex", "omp"] {
        let rows = conversation(harness);
        let mut full = parse(&rows, harness, 10.0, true);
        assert_eq!(full["turns"][0]["invalid"], false);
        assert_eq!(full["turns"][0]["bytes"], 14);
        assert_eq!(full["turns"][0]["items"][0]["text"], "Question");
        assert_eq!(full["turns"][0]["items"][1]["text"], "Answer");
        let metadata = parse(&rows, harness, 10.0, false);
        assert!(!metadata.to_string().contains("Question"));
        assert!(!metadata.to_string().contains("Answer"));
        for item in full["turns"][0]["items"].as_array_mut().unwrap() {
            item.as_object_mut().unwrap().remove("text");
        }
        assert_eq!(metadata, full);
    }
}

#[test]
fn codex_fallback_identity_uses_exact_raw_newline_and_post_parse_context() {
    let rows = conversation("codex");
    let mut raw = encoded(&rows[..3]);
    raw.extend_from_slice(b"{\"type\":\"response_item\",\"timestamp\":2,\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Question\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"t\"}}}\n");
    raw.extend_from_slice(&encoded(&rows[4..]));
    let result = parse_raw(&raw, "codex", 10.0, true);
    // Derived independently with Python hashlib from the literal record above.
    assert_eq!(
        result["turns"][0]["user_id"],
        "72fdcfd09d25403015e5c1f89adefa2f9f7a822c13e96ff0c8b336da8928332f"
    );
    assert_eq!(
        result["turns"][0]["id"],
        "64e1cbd192fcbbf8b6f74c876f070615661069b6cf66de72eb15f71a6aede490"
    );
    assert_eq!(result["turns"][0]["items"][0]["id"], result["turns"][0]["id"]);
    let mut changed = raw.clone();
    let offset = encoded(&rows[..3]).len();
    changed.insert(offset + 1, b' ');
    assert_ne!(
        parse_raw(&changed, "codex", 10.0, true)["turns"][0]["id"],
        result["turns"][0]["id"]
    );
}

#[test]
fn missing_claude_session_uses_python_none_in_fallback_and_unit_hash() {
    let mut raw = b"{\"type\":\"user\",\"version\":\"2.1.263\",\"cwd\":\"/synthetic\",\"uuid\":null,\"parentUuid\":null,\"sessionId\":null,\"isSidechain\":false,\"origin\":{\"kind\":\"human\"},\"timestamp\":1,\"message\":{\"role\":\"user\",\"content\":\"Question\"}}\n".to_vec();
    let mut answer = claude(
        "a",
        json!("bb3d4607c5a6606998e3645d238cd32a377e6089dfb4600df21a9f6519564550"),
        "assistant",
        json!(2),
        json!("Answer"),
        json!("end_turn"),
    );
    answer["sessionId"] = Value::Null;
    raw.extend_from_slice(&encoded(&[answer]));
    let result = parse_raw(&raw, "claude", 10.0, true);
    assert_eq!(
        result["turns"][0]["user_id"],
        "bb3d4607c5a6606998e3645d238cd32a377e6089dfb4600df21a9f6519564550"
    );
    assert_eq!(
        result["turns"][0]["id"],
        "c5892ff765b5b78101a131415c62d4dd9874a48b4108acfe4f299f28c36d79b5"
    );
    assert!(result["turns"][0]["session_id"].is_null());
}

#[test]
fn partial_and_malformed_tails_retain_only_genuine_complete_prefix() {
    for harness in ["claude", "codex", "omp"] {
        let rows = conversation(harness);
        let prefix = parse(&rows, harness, 10.0, false)["turns"].clone();
        for (tail, status) in [
            (b"{\"type\":".as_slice(), "incomplete_write"),
            (b"{bad}\n".as_slice(), "malformed_record"),
        ] {
            let mut raw = encoded(&rows);
            raw.extend_from_slice(tail);
            let result = parse_raw(&raw, harness, 10.0, false);
            assert_eq!(result["turns"], prefix);
            assert_eq!(result["status"], status);
        }
        let incomplete = parse(&rows[..rows.len() - 1], harness, 10.0, true);
        assert_eq!(incomplete["turns"], json!([]));
        assert_eq!(incomplete["pending_turns"], 1);
    }
}

#[test]
fn future_verified_completion_and_excluded_tool_times_remain_retryable() {
    let mut rows = conversation("omp");
    let future = parse(&rows, "omp", 3.5, true);
    assert_eq!(future["status"], "deferred_future");
    assert_eq!(future["turns"], json!([]));
    assert_eq!(future["pending_turns"], 1);
    assert_eq!(parse(&rows, "omp", 4.0, true)["turns"][0]["end"], 4.0);
    rows.insert(
        2,
        omp(
            "tool",
            json!("u"),
            "toolResult",
            json!(5000),
            json!({"private":"EXCLUDED"}),
            "",
        ),
    );
    rows[3]["parentId"] = json!("tool");
    let excluded_future = parse(&rows, "omp", 4.0, true);
    assert_eq!(excluded_future["status"], "deferred_future");
    assert_eq!(excluded_future["turns"], json!([]));
    let later = parse(&rows, "omp", 6.0, true);
    assert_eq!(later["turns"][0]["invalid"], true);
    assert_eq!(later["turns"][0]["boundaries"][1]["time"], 5.0);
    assert!(!later.to_string().contains("EXCLUDED"));
}

#[test]
fn cwd_boundaries_are_not_authorized_by_normalizer_and_bad_times_are_structural() {
    let mut rows = conversation("claude");
    rows.insert(
        1,
        json!({"type":"progress","uuid":"tool","parentUuid":"u","cwd":"/other","timestamp":2}),
    );
    rows[2]["parentUuid"] = json!("tool");
    let changed_project = parse(&rows, "claude", 10.0, true);
    assert_eq!(changed_project["turns"][0]["invalid"], false);
    assert_eq!(changed_project["turns"][0]["boundaries"][1]["cwd"], "/other");
    rows[2]["timestamp"] = json!("invalid");
    let bad_time = parse(&rows, "claude", 10.0, true);
    assert_eq!(bad_time["turns"][0]["invalid"], true);
    assert_eq!(bad_time["turns"][0]["boundaries"][2]["invalid"], true);
}

#[test]
fn linked_next_user_finishes_previous_without_replaying_shared_ancestors() {
    let mut rows = conversation("claude");
    rows[1]["message"]["stop_reason"] = json!("max_tokens");
    rows.push(claude("u2", json!("a"), "user", json!(4), json!("Next"), Value::Null));
    rows.push(claude(
        "a2",
        json!("u2"),
        "assistant",
        json!(5),
        json!("Second"),
        json!("end_turn"),
    ));
    let result = parse(&rows, "claude", 10.0, true);
    assert_eq!(result["turns"][0]["end"], 4.0);
    assert_eq!(result["turns"][0]["boundaries"][2]["time"], 4.0);
    assert_eq!(result["turns"][0]["message_ids"], json!(["u", "a"]));
    assert_eq!(result["turns"][1]["message_ids"], json!(["u2", "a2"]));
    rows.push(claude("u3", json!("u"), "user", json!(6), json!("Branch"), Value::Null));
    rows.push(claude(
        "a3",
        json!("u3"),
        "assistant",
        json!(7),
        json!("Third"),
        json!("end_turn"),
    ));
    let branched = parse(&rows, "claude", 10.0, true);
    assert_eq!(branched["turns"][2]["message_ids"], json!(["u3", "a3"]));
    assert_eq!(branched["turns"][2]["ordinal"], 2);
    assert_eq!(branched["pending_turns"], 0);
}

#[test]
fn human_provenance_confirmation_and_reset_are_required() {
    let mut codex_rows = conversation("codex");
    codex_rows[4]["payload"]["message"] = json!("Not the canonical user message");
    assert_eq!(parse(&codex_rows, "codex", 10.0, true)["turns"], json!([]));
    let mut missing_context = conversation("codex");
    missing_context.remove(2);
    assert_eq!(parse(&missing_context, "codex", 10.0, true)["turns"], json!([]));
    let mut omp_rows = conversation("omp");
    omp_rows[1]["message"]["attribution"] = json!("agent");
    assert_eq!(parse(&omp_rows, "omp", 10.0, true)["turns"], json!([]));
    let mut claude_rows = conversation("claude");
    claude_rows[0]["origin"] = json!({"kind":"agent"});
    claude_rows[0]["promptSource"] = json!("typed");
    assert_eq!(parse(&claude_rows, "claude", 10.0, true)["turns"], json!([]));
    let mut reset_rows = conversation("claude");
    reset_rows[1]["isVisibleInTranscriptOnly"] = json!(true);
    reset_rows.push(claude("u2", json!("a"), "user", json!(4), json!("Next"), Value::Null));
    reset_rows.push(claude(
        "a2",
        json!("u2"),
        "assistant",
        json!(5),
        json!("Safe"),
        json!("end_turn"),
    ));
    let result = parse(&reset_rows, "claude", 10.0, true);
    assert_eq!(result["turns"].as_array().unwrap().len(), 1);
    assert_eq!(result["turns"][0]["message_ids"], json!(["u2", "a2"]));
}

#[test]
fn injected_framing_is_removed_but_raw_codex_confirmation_is_required() {
    let mut rows = conversation("codex");
    let text = "Question <SYSTEM-REMINDER mode='x'>PRIVATE</SYSTEM-REMINDER> tail";
    rows[3]["payload"]["content"][0]["text"] = json!(text);
    rows[4]["payload"]["message"] = json!(text);
    let result = parse(&rows, "codex", 10.0, true);
    assert_eq!(result["turns"][0]["items"][0]["text"], "Question  tail");
    assert_eq!(result["turns"][0]["bytes"], 20);
    rows[4]["payload"]["message"] = json!("Question  tail");
    assert_eq!(parse(&rows, "codex", 10.0, true)["turns"], json!([]));
    let mut claude_rows = conversation("claude");
    claude_rows[0]["message"]["content"] = json!("Question <system-directive missing-close");
    let incomplete = parse(&claude_rows, "claude", 10.0, true);
    assert_eq!(incomplete["turns"][0]["invalid"], true);
    assert!(!incomplete.to_string().contains("missing-close"));
}

#[test]
fn unsupported_versions_and_content_are_typed_without_losing_complete_prefix() {
    for harness in ["claude", "codex", "omp"] {
        let rows = conversation(harness);
        let mut unsupported = rows[0].clone();
        match harness {
            "claude" => unsupported["version"] = json!("999"),
            "codex" => unsupported["payload"]["cli_version"] = json!("999"),
            _ => unsupported["version"] = json!(999),
        }
        let mut appended = rows.clone();
        appended.push(unsupported);
        let result = parse(&appended, harness, 10.0, false);
        assert_eq!(result["status"], "unsupported_version");
        assert_eq!(result["turns"], parse(&rows, harness, 10.0, false)["turns"]);
        let mut unknown = rows.clone();
        let index = if harness == "codex" {
            unknown.len() - 2
        } else {
            unknown.len() - 1
        };
        let field = if harness == "codex" { "payload" } else { "message" };
        unknown[index][field]["content"] = json!([{"type":"future_private_payload","text":"PRIVATE"}]);
        let result = parse(&unknown, harness, 10.0, true);
        assert_eq!(result["status"], "unknown_content_schema");
        assert_eq!(result["turns"], json!([]));
        assert!(!result.to_string().contains("PRIVATE"));
    }
}

#[test]
fn empty_ordinary_messages_keep_full_membership_without_leaking_reasoning() {
    let mut rows = conversation("omp");
    rows.insert(
        2,
        omp(
            "thinking",
            json!("u"),
            "assistant",
            json!(2000),
            json!([{"type":"thinking","thinking":"PRIVATE"}]),
            "toolUse",
        ),
    );
    rows[3]["parentId"] = json!("thinking");
    let metadata = parse(&rows, "omp", 10.0, false);
    assert_eq!(metadata["turns"][0]["message_ids"], json!(["u", "thinking", "a"]));
    assert_eq!(metadata["turns"][0]["items"][1]["message_id"], "thinking");
    let full = parse(&rows, "omp", 10.0, true);
    assert_eq!(full["turns"][0]["items"][1]["text"], "");
    assert_eq!(full["turns"][0]["bytes"], 14);
    assert!(!full.to_string().contains("PRIVATE"));
}

#[test]
fn oversized_turn_is_terminal_metadata_not_an_excerpt_or_a_structural_error() {
    let text = "x".repeat(17 * 1024 * 1024);
    let rows = vec![
        claude("u", Value::Null, "user", json!(1), json!(text), Value::Null),
        claude("a", json!("u"), "assistant", json!(2), json!(text), json!("end_turn")),
        claude("u2", json!("a"), "user", json!(3), json!("Next"), Value::Null),
        claude(
            "a2",
            json!("u2"),
            "assistant",
            json!(4),
            json!("Safe"),
            json!("end_turn"),
        ),
    ];
    let raw = encoded(&rows);
    let metadata = parse_raw(&raw, "claude", 10.0, false);
    assert_eq!(metadata["status"], "oversized_source");
    assert_eq!(metadata["turns"][0]["invalid"], false);
    assert_eq!(metadata["turns"][0]["bytes"], 34 * 1024 * 1024);
    assert_eq!(metadata["turns"][1]["message_ids"], json!(["u2", "a2"]));
    let full = parse_raw(&raw, "claude", 10.0, true);
    assert_eq!(full["turns"][0], metadata["turns"][0]);
    assert_eq!(full["turns"][1]["items"][1]["text"], "Safe");
}

#[test]
fn late_page_remains_reachable_beyond_former_state_and_record_caps() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&encoded(&conversation("claude"))).unwrap();
    // The former state charge rejected this prefix at ~33,000 tiny records;
    // the independent record cap rejected 100,000. Generate, do not retain it.
    for _ in 0..100_001 {
        file.write_all(b"{\"type\":\"progress\"}\n").unwrap();
    }
    let late = vec![
        claude(
            "late-u",
            Value::Null,
            "user",
            json!(5),
            json!("Late question"),
            Value::Null,
        ),
        claude(
            "late-a",
            json!("late-u"),
            "assistant",
            json!(6),
            json!("Late answer"),
            json!("end_turn"),
        ),
    ];
    file.write_all(&encoded(&late)).unwrap();
    // Replaying a complete identity must not advance global page ordinals.
    file.write_all(&encoded(&conversation("claude"))).unwrap();
    file.write_all(&encoded(&[claude(
        "pending",
        Value::Null,
        "user",
        json!(7),
        json!("Still working"),
        Value::Null,
    )]))
    .unwrap();
    let first = normalize::parse_page(file.path(), "claude", 10.0, false, 0, 1).unwrap();
    assert_eq!(first["turns"][0]["message_ids"], json!(["u", "a"]));
    assert_eq!(first["next_offset"], 1);
    assert_eq!(first["status"], "incomplete_turn");
    assert_eq!(first["pending_turns"], 1);
    let mut page = normalize::parse_page(file.path(), "claude", 10.0, true, 1, 1).unwrap();
    assert_eq!(page["turns"].as_array().unwrap().len(), 1);
    assert_eq!(page["turns"][0]["ordinal"], 1);
    assert_eq!(page["turns"][0]["message_ids"], json!(["late-u", "late-a"]));
    assert_eq!(page["turns"][0]["items"][0]["text"], "Late question");
    assert_eq!(page["turns"][0]["items"][1]["text"], "Late answer");
    assert_eq!(page["status"], "incomplete_turn");
    assert_eq!(page["pending_turns"], 1);
    assert_eq!(page["next_offset"], Value::Null);
    for item in page["turns"][0]["items"].as_array_mut().unwrap() {
        item.as_object_mut().unwrap().remove("text");
    }
    let metadata = normalize::parse_page(file.path(), "claude", 10.0, false, 1, 1).unwrap();
    assert_eq!(metadata, page);
}

#[test]
fn duplicate_native_reference_rereads_first_record_not_latest_branch_record() {
    let rows = vec![
        claude("u", Value::Null, "user", json!(1), json!("Question"), Value::Null),
        claude(
            "a",
            json!("u"),
            "assistant",
            json!(2),
            json!("First"),
            json!("max_tokens"),
        ),
        claude(
            "a",
            json!("u"),
            "assistant",
            json!(3),
            json!("Replacement"),
            json!("max_tokens"),
        ),
        claude("u2", json!("a"), "user", json!(4), json!("Next"), Value::Null),
        claude(
            "a2",
            json!("u2"),
            "assistant",
            json!(5),
            json!("Done"),
            json!("end_turn"),
        ),
    ];
    let result = parse(&rows, "claude", 10.0, true);
    assert_eq!(result["status"], "complete");
    assert_eq!(result["pending_turns"], 0);
    assert_eq!(result["turns"][0]["message_ids"], json!(["u", "a"]));
    assert_eq!(result["turns"][0]["items"][1]["text"], "First");
    assert_eq!(result["turns"][0]["items"][1]["time"], 2.0);
    assert_eq!(result["turns"][0]["boundaries"][1]["time"], 3.0);
    assert_eq!(result["turns"][0]["bytes"], 13);
    assert_eq!(result["turns"][1]["ordinal"], 1);
}

#[test]
fn iso_timestamps_match_python_microseconds_and_explicit_offsets() {
    let mut rows = conversation("claude");
    rows[0]["timestamp"] = json!("19700101T000001,123456999+0000");
    rows[1]["timestamp"] = json!("1970-01-01 00:00:03+00:00:01");
    let result = parse(&rows, "claude", 10.0, true);
    assert_eq!(result["turns"][0]["start"], 1.123456);
    assert_eq!(result["turns"][0]["end"], 2.0);
    assert_eq!(result["turns"][0]["invalid"], false);
    rows[0]["timestamp"] = json!("1970-01-01T00:00:01");
    assert_eq!(parse(&rows, "claude", 10.0, true)["turns"][0]["invalid"], true);
}
