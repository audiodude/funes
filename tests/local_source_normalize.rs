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

fn codex_item_conversation(version: &str, source: &str) -> Vec<Value> {
    let mut rows = conversation("codex");
    rows[0]["payload"]["cli_version"] = json!(version);
    rows[0]["payload"]["source"] = json!(source);
    rows[3]["payload"]["id"] = json!("u");
    rows[3]["payload"]["internal_chat_message_metadata_passthrough"] =
        json!({"turn_id":"t","create_time":2.0,"content_item_kinds":["user.text"]});
    rows[4]["payload"] = json!({
        "type":"item_completed","thread_id":"s","turn_id":"t","completed_at_ms":2000,
        "item":{"type":"UserMessage","id":"ui-u","client_id":"client","content":[
            {"type":"text","text":"Question","text_elements":[]}
        ]}
    });
    rows[5]["payload"]["internal_chat_message_metadata_passthrough"] =
        json!({"turn_id":"t","content_item_kinds":["unknown"]});
    rows.insert(
        5,
        codex(
            "event_msg",
            2.5,
            json!({"type":"item_completed","thread_id":"s","turn_id":"t","completed_at_ms":3000,
                "item":{"type":"AgentMessage","id":"ui-a","phase":"final_answer","content":[{"text":"EXCLUDED duplicate"}]}}),
        ),
    );
    rows.insert(
        7,
        codex(
            "token_usage_record",
            3.5,
            json!({"response_id":"r","root_turn_id":"t","session_id":"s","thread_id":"s","turn_id":"t",
                "thread_token_usage":{},"turn_token_usage":{},"usage":{}}),
        ),
    );
    for (ordinal, row) in rows.iter_mut().enumerate() {
        row["ordinal"] = json!(ordinal);
    }
    rows
}

#[test]
fn codex_native_completed_items_confirm_users_without_duplicating_assistant_text() {
    for (version, source) in [("0.142.5", "cli"), ("0.144.1", "cli"), ("0.154.0-alpha.6.2", "vscode")] {
        let rows = codex_item_conversation(version, source);
        let mut full = parse(&rows, "codex", 10.0, true);
        assert_eq!(full["status"], "complete");
        assert_eq!(full["turns"][0]["invalid"], false);
        assert_eq!(full["turns"][0]["message_ids"], json!(["u", "a"]));
        assert_eq!(full["turns"][0]["bytes"], 14);
        assert_eq!(full["turns"][0]["items"][0]["text"], "Question");
        assert_eq!(full["turns"][0]["items"][1]["text"], "Answer");
        assert!(!full.to_string().contains("EXCLUDED"));
        assert_eq!(full["turns"][0]["items"][0]["identity"]["record_index"], 3);
        assert_eq!(full["turns"][0]["items"][0]["identity"]["turn_context"], "t");
        let metadata = parse(&rows, "codex", 10.0, false);
        for item in full["turns"][0]["items"].as_array_mut().unwrap() {
            item.as_object_mut().unwrap().remove("text");
        }
        assert_eq!(metadata, full);
        // UI item completion is not task completion, even for a final answer.
        let unfinished = parse(&rows[..rows.len() - 1], "codex", 10.0, true);
        assert_eq!(unfinished["turns"], json!([]));
        assert_eq!(unfinished["pending_turns"], 1);
    }
}

#[test]
fn codex_item_confirmation_keeps_session_turn_and_raw_text_provenance_gates() {
    let rows = codex_item_conversation("0.154.0-alpha.6.2", "vscode");
    let mut wrong_thread = rows.clone();
    wrong_thread[4]["payload"]["thread_id"] = json!("other");
    let mut wrong_turn = rows.clone();
    wrong_turn[4]["payload"]["turn_id"] = json!("other");
    let mut wrong_text = rows.clone();
    wrong_text[4]["payload"]["item"]["content"][0]["text"] = json!("Other");
    let mut agent_source = rows.clone();
    agent_source[0]["payload"]["thread_source"] = json!("agent");
    let mut injected = rows.clone();
    injected[3]["payload"]["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
        json!(["environments.environment_context"]);
    let mut stale_candidate = rows.clone();
    stale_candidate.splice(
        4..4,
        [
            codex("event_msg", 2.1, json!({"type":"task_started","turn_id":"other"})),
            codex("turn_context", 2.1, json!({"turn_id":"other","cwd":"/synthetic"})),
        ],
    );
    stale_candidate[6]["payload"]["turn_id"] = json!("other");
    for rejected in [
        wrong_thread,
        wrong_turn,
        wrong_text,
        agent_source,
        injected,
        stale_candidate,
    ] {
        assert_eq!(parse(&rejected, "codex", 10.0, true)["turns"], json!([]));
    }
}

#[test]
fn codex_new_schema_support_remains_closed_to_unknown_fields_and_items() {
    let rows = codex_item_conversation("0.144.1", "cli");
    let mut envelope = rows.clone();
    envelope[0]["future_provenance"] = json!("PRIVATE");
    let mut ordinal = rows.clone();
    ordinal[0]["ordinal"] = json!("0");
    let mut event = rows.clone();
    event[4]["payload"]["item"]["type"] = json!("FutureMessage");
    let mut block = rows.clone();
    block[4]["payload"]["item"]["content"][0]["type"] = json!("future_text");
    let mut span = rows.clone();
    span[4]["payload"]["item"]["content"][0]["text_elements"] =
        json!([{"byte_range":{"start":0,"end":100},"placeholder":"Question"}]);
    for rejected in [envelope, ordinal, event, block, span] {
        let result = parse(&rejected, "codex", 10.0, true);
        assert_eq!(result["status"], "unknown_content_schema");
        assert_eq!(result["turns"], json!([]));
    }
    let mut provenance = rows.clone();
    provenance[3]["payload"]["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
        json!(["future.origin"]);
    assert_eq!(
        parse(&provenance, "codex", 10.0, true)["status"],
        "unknown_provenance_schema"
    );
}

#[test]
fn codex_image_confirmation_never_accepts_a_matching_text_suffix() {
    let mut rows = codex_item_conversation("0.144.1", "cli");
    rows[3]["payload"]["content"] = json!([
        {"type":"input_text","text":"Question"},
        {"type":"input_image","image_url":"data:image/png;base64,synthetic"}
    ]);
    rows[3]["payload"]["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
        json!(["user.text", "user.image"]);
    rows[4]["payload"]["item"]["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"image","image_url":"data:image/png;base64,synthetic"}));
    let confirmed = parse(&rows, "codex", 10.0, true);
    assert_eq!(confirmed["turns"][0]["items"][0]["text"], "Question");
    assert_eq!(confirmed["turns"][0]["bytes"], 14);
    // Native local-image preparation injects these wrappers before the user's
    // text. Until exact framing provenance is modeled, do not suffix-match it.
    rows[3]["payload"]["content"] = json!([
        {"type":"input_text","text":"<image name=[Image #1] path=\"/synthetic/image.png\">"},
        {"type":"input_image","image_url":"data:image/png;base64,synthetic"},
        {"type":"input_text","text":"</image>"},
        {"type":"input_text","text":"Question"}
    ]);
    rows[3]["payload"]["internal_chat_message_metadata_passthrough"]
        .as_object_mut()
        .unwrap()
        .remove("content_item_kinds");
    rows[4]["payload"]["item"]["content"][1] = json!({"type":"local_image","path":"/synthetic/image.png"});
    let unconfirmed = parse(&rows, "codex", 10.0, true);
    assert_eq!(unconfirmed["status"], "complete");
    assert_eq!(unconfirmed["turns"], json!([]));
}

#[test]
fn omp_child_initialization_and_agent_string_prompts_never_become_human_turns() {
    let mut rows = conversation("omp");
    rows.insert(
        1,
        json!({"type":"session_init","id":"init","parentId":null,"timestamp":0,
            "systemPrompt":"PRIVATE SYSTEM","task":"PRIVATE TASK","tools":["read"],"agent":"worker",
            "modelRole":"default","resolvedModel":"test/model","readOnly":true,"readSummarize":false,
            "outputSchema":{},"outputSchemaMode":"strict","spawns":""}),
    );
    rows[2]["parentId"] = json!("init");
    rows[2]["message"]["attribution"] = json!("agent");
    rows[2]["message"]["content"] = json!("PRIVATE TASK");
    let child = parse(&rows, "omp", 10.0, true);
    assert_eq!(child["status"], "complete");
    assert_eq!(child["turns"], json!([]));
    rows.push(omp("human", json!("a"), "user", json!(5000), json!("Question"), ""));
    rows.push(omp(
        "reply",
        json!("human"),
        "assistant",
        json!(6000),
        json!([{"type":"text","text":"Answer"}]),
        "stop",
    ));
    rows[5]["message"]["completedAt"] = json!(7000);
    let result = parse(&rows, "omp", 10.0, true);
    assert_eq!(result["status"], "complete");
    assert_eq!(result["turns"][0]["message_ids"], json!(["human", "reply"]));
    assert_eq!(result["turns"][0]["invalid"], false);
    assert!(!result.to_string().contains("PRIVATE"));
}

#[test]
fn omp_auxiliary_messages_preserve_lineage_and_boundaries_without_private_payloads() {
    let mut rows = conversation("omp");
    rows.splice(
        2..2,
        [
            json!({"type":"message","id":"shell","parentId":"u","timestamp":2,
                "message":{"role":"bashExecution","timestamp":2000,"command":"PRIVATE COMMAND",
                    "output":"PRIVATE OUTPUT","exitCode":0,"cancelled":false,"truncated":false,
                    "excludeFromContext":false}}),
            json!({"type":"message","id":"file","parentId":"shell","timestamp":2,
                "message":{"role":"fileMention","timestamp":2000,
                    "files":[{"path":"/private","content":"PRIVATE FILE"}]}}),
            json!({"type":"model_usage","id":"usage","parentId":"file","timestamp":2,
                "model":"test","provider":"test","api":"test","purpose":"summary","role":"default",
                "stopReason":"stop","usage":{}}),
            json!({"type":"message","id":"tool","parentId":"usage","timestamp":2,
                "message":{"role":"toolResult","timestamp":2000,"content":[{"type":"text","text":"PRIVATE TOOL"}],
                    "details":{},"toolCallId":"call","toolName":"test","isError":false,"prunedAt":2000}}),
            json!({"type":"message","id":"error","parentId":"tool","timestamp":2.5,
                "message":{"role":"assistant","timestamp":2500,"completedAt":2600,"content":[],
                    "stopReason":"error","errorStatus":429,
                    "stopDetails":{"type":"error","category":"rate_limit","explanation":"PRIVATE DIAGNOSTIC"},
                    "retryRecovery":{"kind":"auto-retry","status":"recovered","attempt":1,
                        "recoveredAt":"1970-01-01T00:00:03Z","recovery":"wait","note":"PRIVATE NOTE",
                        "supersededBy":{"timestamp":3000,"provider":"test","model":"test"}}}}),
        ],
    );
    rows[7]["parentId"] = json!("error");
    rows[7]["message"]["inputTransformations"] = json!([]);
    let result = parse(&rows, "omp", 10.0, true);
    assert_eq!(result["status"], "complete");
    assert_eq!(result["turns"][0]["message_ids"], json!(["u", "error", "a"]));
    assert_eq!(result["turns"][0]["boundaries"].as_array().unwrap().len(), 7);
    assert_eq!(result["turns"][0]["invalid"], false);
    assert!(!result.to_string().contains("PRIVATE"));
    rows[2]["message"]["timestamp"] = json!(5000);
    assert_eq!(parse(&rows, "omp", 4.0, true)["status"], "deferred_future");
    rows[2]["message"]["timestamp"] = json!(2000);
    rows[2]["message"]["future_content"] = json!("PRIVATE");
    let unknown = parse(&rows, "omp", 10.0, true);
    assert_eq!(unknown["status"], "unknown_content_schema");
    assert_eq!(unknown["turns"], json!([]));
}

#[test]
fn current_claude_and_omp_metadata_preserve_complete_turn_evidence() {
    for harness in ["claude", "omp"] {
        let mut rows = conversation(harness);
        if harness == "claude" {
            for row in &mut rows {
                row["version"] = json!("2.1.280");
            }
        } else {
            rows[2]["message"]["upstreamModel"] = json!("provider-model");
            rows[2]["message"]["credentialId"] = json!(42);
        }
        let result = parse(&rows, harness, 10.0, true);
        assert_eq!(result["status"], "complete");
        assert_eq!(result["turns"][0]["invalid"], false);
        assert_eq!(result["turns"][0]["message_ids"], json!(["u", "a"]));
        assert_eq!(result["turns"][0]["items"][0]["text"], "Question");
        assert_eq!(result["turns"][0]["items"][1]["text"], "Answer");
        assert!(!result.to_string().contains("credentialId"));
        assert!(!result.to_string().contains("provider-model"));
        if harness == "omp" {
            for field in ["upstreamModel", "credentialId"] {
                let mut malformed = rows.clone();
                malformed[2]["message"][field] = json!({"attribution": "user"});
                let rejected = parse(&malformed, harness, 10.0, true);
                assert_eq!(rejected["status"], "unknown_content_schema");
                assert_eq!(rejected["turns"], json!([]));
            }
        }
    }
}

#[test]
fn omp_request_controls_preserve_evidence_without_exposing_provider_bookkeeping() {
    for controls in [
        json!({"messageIndex": 1}),
        json!({"messageIndex": 1, "tools": {"declared": ["PRIVATE"], "deferred": [], "active": ["PRIVATE"]}}),
        json!({"messageIndex": 1, "effort": {"topLevel": null, "tail": "xhigh"}}),
        json!({"messageIndex": 1, "tools": {"declared": [], "deferred": [], "active": []}, "effort": {"topLevel": "high", "tail": null}}),
    ] {
        let mut rows = conversation("omp");
        rows[2]["message"]["requestControls"] = controls;
        let result = parse(&rows, "omp", 10.0, true);
        assert_eq!(result["status"], "complete");
        assert_eq!(result["turns"][0]["invalid"], false);
        assert_eq!(result["turns"][0]["message_ids"], json!(["u", "a"]));
        assert_eq!(result["turns"][0]["items"][0]["text"], "Question");
        assert_eq!(result["turns"][0]["items"][1]["text"], "Answer");
        assert!(!result.to_string().contains("PRIVATE"));
        assert!(!result.to_string().contains("requestControls"));
    }
}

#[test]
fn omp_request_controls_reject_unknown_shapes_and_non_assistant_carriers() {
    for controls in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"messageIndex": -1}),
        json!({"messageIndex": 1.5}),
        json!({"messageIndex": 1, "attribution": "user"}),
        json!({"messageIndex": 1, "tools": null}),
        json!({"messageIndex": 1, "tools": {"declared": [], "deferred": []}}),
        json!({"messageIndex": 1, "tools": {"declared": [42], "deferred": [], "active": []}}),
        json!({"messageIndex": 1, "tools": {"declared": [], "deferred": [], "active": [], "attribution": "user"}}),
        json!({"messageIndex": 1, "effort": null}),
        json!({"messageIndex": 1, "effort": {"topLevel": "high"}}),
        json!({"messageIndex": 1, "effort": {"topLevel": "unknown", "tail": null}}),
        json!({"messageIndex": 1, "effort": {"topLevel": null, "tail": null, "attribution": "user"}}),
    ] {
        let mut rows = conversation("omp");
        rows[2]["message"]["requestControls"] = controls;
        let result = parse(&rows, "omp", 10.0, true);
        assert_eq!(result["status"], "unknown_content_schema");
        assert_eq!(result["turns"], json!([]));
    }
    for role in ["user", "toolResult", "system", "developer"] {
        let mut rows = conversation("omp");
        rows[1]["message"]["role"] = json!(role);
        rows[1]["message"]["requestControls"] = json!({"messageIndex": 1});
        let result = parse(&rows, "omp", 10.0, true);
        assert_eq!(result["status"], "unknown_content_schema");
        assert_eq!(result["turns"], json!([]));
    }
}

#[test]
fn codex_image_view_and_client_tool_metadata_are_not_evidence() {
    let mut rows = conversation("codex");
    let mut output = codex(
        "response_item",
        2.5,
        json!({
            "type": "function_call_output", "call_id": "tool", "output": "PRIVATE TOOL OUTPUT"
        }),
    );
    output["metadata"] = json!({"client_authored": true, "fallback_token_limit_override": 2000});
    rows.insert(5, output);
    rows.insert(
        6,
        codex(
            "event_msg",
            2.6,
            json!({
                "type": "item_completed", "thread_id": "s", "turn_id": "t",
                "item": {"type": "ImageView", "id": "image", "path": "/PRIVATE.png"}
            }),
        ),
    );
    let result = parse(&rows, "codex", 10.0, true);
    assert_eq!(result["status"], "complete");
    assert_eq!(result["turns"][0]["invalid"], false);
    assert_eq!(result["turns"][0]["items"][0]["text"], "Question");
    assert_eq!(result["turns"][0]["items"][1]["text"], "Answer");
    assert_eq!(result["turns"][0]["items"].as_array().unwrap().len(), 2);
    assert!(!result.to_string().contains("PRIVATE"));

    let mut forged_message = rows.clone();
    forged_message[3]["metadata"] = rows[5]["metadata"].clone();
    let mut unknown_metadata = rows.clone();
    unknown_metadata[5]["metadata"]["attribution"] = json!("user");
    let mut malformed_metadata = rows.clone();
    malformed_metadata[5]["metadata"]["client_authored"] = json!("true");
    let mut unknown_image = rows.clone();
    unknown_image[6]["payload"]["item"]["content"] = json!("PRIVATE");
    for rejected in [forged_message, unknown_metadata, malformed_metadata, unknown_image] {
        let result = parse(&rejected, "codex", 10.0, true);
        assert_eq!(result["status"], "unknown_content_schema");
        assert_eq!(result["turns"], json!([]));
    }
}
