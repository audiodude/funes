//! Local-only structural normalization. Authorization belongs to the consumer.
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;
const MAX_TURN_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
const SCHEMA: &str = "unknown_content_schema";

fn digest(value: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(value.as_ref()))
}

// The legacy digest hashes the lowercase hex STRING, not the original bytes.
fn raw_digest(raw: &[u8]) -> String {
    let mut hash = Sha256::new();
    const HEX: &[u8] = b"0123456789abcdef";
    for chunk in raw.chunks(4096) {
        let mut encoded = [0u8; 8192];
        for (index, byte) in chunk.iter().enumerate() {
            encoded[index * 2] = HEX[(byte >> 4) as usize];
            encoded[index * 2 + 1] = HEX[(byte & 15) as usize];
        }
        hash.update(&encoded[..chunk.len() * 2]);
    }
    hex::encode(hash.finalize())
}

fn truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}

// Normally native identifiers are strings. Null must be Python's None, not JSON null.
fn py_string(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(value) => value.clone(),
        Value::Number(number) if number.is_f64() => {
            let rendered = format!("{:?}", number.as_f64().unwrap());
            if let Some((mantissa, exponent)) = rendered.split_once('e') {
                let exponent: i32 = exponent.parse().unwrap();
                format!("{mantissa}e{exponent:+03}")
            } else {
                rendered
            }
        }
        Value::Array(values) => format!("[{}]", values.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", py_repr(&Value::String(key.clone())), py_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => value.to_string(),
    }
}

fn py_repr(value: &Value) -> String {
    let Some(text) = value.as_str() else {
        return py_string(value);
    };
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut result = String::new();
    result.push(quote);
    for character in text.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c == quote => {
                result.push('\\');
                result.push(c);
            }
            c if c.is_control() || (c.is_whitespace() && c != ' ') => {
                let point = c as u32;
                result.push_str(&if point <= 255 {
                    format!("\\x{point:02x}")
                } else if point <= 65535 {
                    format!("\\u{point:04x}")
                } else {
                    format!("\\U{point:08x}")
                });
            }
            c => result.push(c),
        }
    }
    result.push(quote);
    result
}

// datetime.fromisoformat accepts basic/extended dates, any one-character date
// separator, comma fractions, short times and second-resolution UTC offsets.
// It truncates fractions to microseconds and rejects naive dates/leap seconds.
fn iso_clock(text: &str) -> Option<(u32, u32, u32, u32)> {
    let (whole, fraction) = text
        .find(['.', ','])
        .map_or((text, None), |index| (&text[..index], Some(&text[index + 1..])));
    let mut components = [0u32; 3];
    if whole.contains(':') {
        let parts: Vec<_> = whole.split(':').collect();
        if parts.is_empty() || parts.len() > 3 {
            return None;
        }
        for (index, part) in parts.iter().enumerate() {
            if part.len() != 2 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            components[index] = part.parse().ok()?;
        }
    } else {
        if !matches!(whole.len(), 2 | 4 | 6) || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        for (index, part) in whole.as_bytes().chunks_exact(2).enumerate() {
            components[index] = u32::from(part[0] - b'0') * 10 + u32::from(part[1] - b'0');
        }
    }
    let mut micros = 0;
    if let Some(fraction) = fraction {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        for index in 0..6 {
            micros = micros * 10 + u32::from(fraction.as_bytes().get(index).map_or(0, |byte| byte - b'0'));
        }
    }
    Some((components[0], components[1], components[2], micros))
}

fn iso_timestamp(text: &str) -> Option<f64> {
    let (date, rest) = [(10, "%Y-%m-%d"), (8, "%Y%m%d"), (10, "%G-W%V-%u"), (8, "%GW%V%u")]
        .into_iter()
        .find_map(|(length, format)| {
            let date = NaiveDate::parse_from_str(text.get(..length)?, format).ok()?;
            let suffix = text.get(length..)?;
            let separator = suffix.chars().next()?;
            Some((date, &suffix[separator.len_utf8()..]))
        })?;
    let position = rest.find(['+', '-', 'Z'])?;
    let (hour, minute, second, micros) = iso_clock(&rest[..position])?;
    let naive = date.and_hms_micro_opt(hour, minute, second, micros)?;
    let zone = &rest[position..];
    let offset = if zone == "Z" {
        0.0
    } else {
        let sign = match zone.as_bytes()[0] {
            b'+' => 1.0,
            b'-' => -1.0,
            _ => return None,
        };
        let (hours, minutes, seconds, micros) = iso_clock(&zone[1..])?;
        let whole = f64::from(hours * 3600 + minutes * 60 + seconds);
        let offset = whole
            + if whole == 0.0 {
                0.0
            } else {
                f64::from(micros) / 1_000_000.0
            };
        if offset >= 86400.0 {
            return None;
        }
        sign * offset
    };
    Some(naive.and_utc().timestamp() as f64 + f64::from(micros) / 1_000_000.0 - offset)
}

fn timestamp(value: &Value, milliseconds: bool) -> Option<f64> {
    let result = if let Some(number) = value.as_f64() {
        number / if milliseconds { 1000.0 } else { 1.0 }
    } else {
        iso_timestamp(value.as_str()?)?
    };
    result.is_finite().then_some(result)
}

fn allowed_keys(row: &Value, allowed: &str) -> bool {
    row.as_object()
        .is_some_and(|row| row.keys().all(|key| allowed.split_whitespace().any(|v| v == key)))
}
fn one_of(value: &Value, values: &str) -> bool {
    value
        .as_str()
        .is_some_and(|s| values.split_whitespace().any(|v| v == s))
}
fn whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}
const INJECTED: &[&str] = &[
    "system-reminder",
    "system-directive",
    "environment_context",
    "developer_instructions",
    "permissions instructions",
    "turn_aborted",
    "subagent_notification",
    "task-notification",
    "local-command-caveat",
    "local-command-stdout",
    "command-message",
    "command-name",
    "command-args",
];
// Python re.IGNORECASE matches these four non-ASCII characters against ASCII
// letters. Preserve its framing contract without changing original byte offsets.
fn prefix_ignorecase(text: &str, pattern: &str) -> Option<usize> {
    let mut actual = text.char_indices();
    let mut end = 0;
    for expected in pattern.chars() {
        let (offset, character) = actual.next()?;
        let folded = match character {
            'İ' | 'ı' => 'i',
            'ſ' => 's',
            'K' => 'k',
            other => other.to_ascii_lowercase(),
        };
        if folded != expected.to_ascii_lowercase() {
            return None;
        }
        end = offset + character.len_utf8();
    }
    Some(end)
}
fn opening_end(text: &str, start: usize, tag: &str) -> Option<usize> {
    let rest = text.get(start + 1..)?;
    let length = prefix_ignorecase(rest, tag)?;
    let suffix = &rest[length..];
    match suffix.chars().next()? {
        '>' => Some(start + length + 2),
        c if whitespace(c) => suffix.find('>').map(|end| start + length + 2 + end),
        _ => None,
    }
}
fn strip_framing(text: String) -> String {
    let leading = text.trim_start_matches(whitespace);
    if [
        "# agents.md instructions for ",
        "[request interrupted by user",
        "[this is a continuation of a previous conversation",
        "<task-notification>",
        "<subagent_notification>",
        "<environment_context>",
    ]
    .iter()
    .any(|s| prefix_ignorecase(leading, s).is_some())
    {
        return String::new();
    }
    let mut result = String::with_capacity(text.len());
    let mut copied = 0;
    let mut scan = 0;
    while let Some(relative) = text[scan..].find('<') {
        let start = scan + relative;
        let matched = INJECTED.iter().find_map(|tag| {
            let end = opening_end(&text, start, tag)?;
            let close = format!("</{tag}>");
            text[end..].match_indices('<').find_map(|(position, _)| {
                prefix_ignorecase(&text[end + position..], &close).map(|length| end + position + length)
            })
        });
        if let Some(end) = matched {
            result.push_str(&text[copied..start]);
            copied = end;
            scan = end;
        } else {
            scan = start + 1;
        }
    }
    result.push_str(&text[copied..]);
    for (start, _) in result.match_indices('<') {
        for tag in [
            "system-reminder",
            "system-directive",
            "environment_context",
            "subagent_notification",
            "task-notification",
        ] {
            let rest = &result[start + 1..];
            if let Some(length) = prefix_ignorecase(rest, tag) {
                if rest[length..].chars().next().is_some_and(|c| c == '>' || whitespace(c)) {
                    return String::new();
                }
            }
        }
    }
    result.trim_matches(whitespace).to_owned()
}
fn text_blocks(content: &Value, allowed: &str, excluded: &str) -> Result<String, &'static str> {
    let text = if let Some(text) = content.as_str() {
        text.to_owned()
    } else if let Some(blocks) = content.as_array() {
        let mut text = String::new();
        let mut first = true;
        for block in blocks {
            if !block.is_object() || !block["type"].is_string() {
                return Err(SCHEMA);
            }
            if one_of(&block["type"], allowed) {
                let part = block["text"].as_str().ok_or(SCHEMA)?;
                if !allowed_keys(block, "type text textSignature citations") {
                    return Err(SCHEMA);
                }
                if !first {
                    text.push('\n');
                }
                first = false;
                text.push_str(part);
            } else if !one_of(&block["type"], excluded) {
                return Err(SCHEMA);
            }
        }
        text
    } else {
        return Err(SCHEMA);
    };
    Ok(strip_framing(text))
}

#[derive(Clone, Default)]
struct Context {
    session: Value,
    cwd: Value,
    turn: Option<String>,
    active_turn: Value,
    source_ok: bool,
}
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Metadata,
    Reset,
    User,
    Assistant,
    Boundary,
    Candidate,
    Confirm,
}
struct Event {
    id: String,
    parent: Value,
    session: Value,
    cwd: Value,
    time: Option<f64>,
    end: Option<Option<f64>>,
    role: Role,
    text: String,
    complete: bool,
    invalid: bool,
    confirmation: String,
    identity: Value,
}
impl Event {
    fn new(row: &Value, context: &Context) -> Self {
        Self {
            id: String::new(),
            parent: row
                .get("parentUuid")
                .or_else(|| row.get("parentId"))
                .cloned()
                .unwrap_or(Value::Null),
            session: context.session.clone(),
            cwd: context.cwd.clone(),
            time: timestamp(&row["timestamp"], false),
            end: None,
            role: Role::Metadata,
            text: String::new(),
            complete: false,
            invalid: false,
            confirmation: String::new(),
            identity: Value::Null,
        }
    }
}

fn claude(row: &Value, context: &mut Context) -> Result<Event, &'static str> {
    if let Some(cwd) = row.get("cwd") {
        context.cwd = cwd.clone();
    }
    let kind = &row["type"];
    if one_of(kind, "last-prompt mode permission-mode atis-latch attachment file-history-snapshot ai-title queue-operation progress summary file-history-delta pr-link cost-state agent-name frame-link bridge-session custom-title") {
        return Ok(Event::new(row, context));
    }
    if !one_of(kind, "user assistant system") {
        return Err(SCHEMA);
    }
    if !one_of(
        &row["version"],
        "2.1.220 2.1.221 2.1.223 2.1.224 2.1.226 2.1.227 2.1.228 2.1.229 2.1.231 2.1.232 2.1.233 2.1.260 2.1.263",
    ) {
        return Err("unsupported_version");
    }
    if ["cwd", "uuid", "parentUuid", "sessionId", "isSidechain"]
        .iter()
        .any(|key| row.get(key).is_none())
    {
        return Err(SCHEMA);
    }
    context.session = row["sessionId"].clone();
    let mut event = Event::new(row, context);
    if [
        "isSidechain",
        "isCompactSummary",
        "attributionAgent",
        "agentId",
        "isVisibleInTranscriptOnly",
        "isAbortedMidStream",
        "supersedesUuids",
        "interruptedMessageId",
    ]
    .iter()
    .any(|key| truth(&row[key]))
    {
        event.role = Role::Reset;
        return Ok(event);
    }
    if kind == "system" {
        if !one_of(&row["subtype"], "away_summary local_command stop_hook_summary turn_duration compact_boundary informational bridge_status model_refusal_fallback") { return Err(SCHEMA); }
        if one_of(&row["subtype"], "compact_boundary model_refusal_fallback") {
            event.role = Role::Reset;
        }
        return Ok(event);
    }
    if !allowed_keys(row, "apiBlockIndex classifierMetaLines cwd effort entrypoint error gitBranch imagePasteIds isApiErrorMessage isMeta isSidechain message origin parentUuid permissionMode promptId promptSource queueSkipAttachments requestId sessionId session_id sourceToolAssistantUUID timestamp toolUseResult turnCompanion type userType uuid version attributionAgent attributionSkill attributionPlugin attributionMcpServer attributionMcpTool isCompactSummary slug sourceToolUseID mcpMeta toolDenialKind userFeedback sessionKind") { return Err(SCHEMA); }
    let message = &row["message"];
    if !message.is_object() || message["role"] != *kind || !allowed_keys(message, "container content context_management diagnostics id model role stop_details stop_reason stop_sequence type usage") { return Err(SCHEMA); }
    let text = text_blocks(
        &message["content"],
        "text",
        "thinking redacted_thinking tool_use tool_result image document server_tool_use web_search_tool_result",
    )?;
    if kind == "user" {
        let human =
            row["origin"] == json!({"kind":"human"}) || (row["origin"].is_null() && row["promptSource"] == "typed");
        let genuine = human
            && !["isMeta", "sourceToolAssistantUUID", "sourceToolUseID", "toolUseResult"]
                .iter()
                .any(|key| truth(&row[key]))
            && row["promptSource"] != "system";
        if genuine {
            event.role = Role::User;
            event.text = text;
        } else if !text.is_empty() && !truth(&row["isMeta"]) && !truth(&row["sourceToolAssistantUUID"]) {
            event.role = Role::Reset;
        }
    } else if !truth(&row["isApiErrorMessage"]) && !truth(&row["isMeta"]) {
        let stop = &message["stop_reason"];
        if !stop.is_null() && !one_of(stop, "end_turn stop_sequence tool_use max_tokens refusal") {
            return Err(SCHEMA);
        }
        event.role = Role::Assistant;
        event.complete = stop == "end_turn" && !text.is_empty();
        event.text = text;
    }
    Ok(event)
}

fn codex(row: &Value, context: &mut Context) -> Result<Event, &'static str> {
    let payload = &row["payload"];
    if !payload.is_object() || !allowed_keys(row, "type timestamp payload") {
        return Err(SCHEMA);
    }
    let mut event = Event::new(row, context);
    let kind = &row["type"];
    if kind == "session_meta" {
        if payload["cli_version"] != "0.144.1" {
            return Err("unsupported_version");
        }
        if !allowed_keys(payload, "base_instructions cli_version context_window cwd git history_mode id model_provider originator session_id source thread_source timestamp") { return Err("unknown_session_schema"); }
        context.session = payload["id"].clone();
        context.cwd = payload["cwd"].clone();
        context.source_ok = payload["source"] == "cli" && payload["thread_source"] == "user";
        if !context.session.is_string() || !context.cwd.is_string() {
            return Err(SCHEMA);
        }
        return Ok(event);
    }
    if !truth(&context.session) {
        return Err("missing_session_header");
    }
    if kind == "world_state" {
        return Ok(event);
    }
    if kind == "turn_context" {
        if !payload["cwd"].is_string() || !payload["turn_id"].is_string() {
            return Err(SCHEMA);
        }
        context.cwd = payload["cwd"].clone();
        context.turn = payload["turn_id"].as_str().map(str::to_owned);
        event.cwd = context.cwd.clone();
        event.role = Role::Boundary;
        return Ok(event);
    }
    if kind == "event_msg" {
        let subtype = &payload["type"];
        if subtype == "task_started" { context.active_turn = payload["turn_id"].clone(); }
        else if subtype == "user_message" {
            event.confirmation = digest(payload["message"].as_str().ok_or(SCHEMA)?);
            event.role = Role::Confirm;
        } else if subtype == "task_complete" {
            event.role = Role::Boundary;
            event.complete = true;
            event.invalid = payload["turn_id"] != context.active_turn;
        } else if subtype == "turn_aborted" { event.role = Role::Reset; }
        else if !one_of(subtype, "agent_message token_count web_search_end patch_apply_end thread_settings_applied agent_reasoning exec_command_begin exec_command_end patch_apply_begin warning error context_compacted") { return Err(SCHEMA); }
        return Ok(event);
    }
    if kind != "response_item" {
        return Err(SCHEMA);
    }
    let subtype = &payload["type"];
    if one_of(subtype, "reasoning function_call function_call_output custom_tool_call custom_tool_call_output web_search_call compaction") { return Ok(event); }
    if subtype != "message" || !one_of(&payload["role"], "user assistant system developer") {
        return Err(SCHEMA);
    }
    if !allowed_keys(
        payload,
        "type role content id phase internal_chat_message_metadata_passthrough",
    ) {
        return Err(SCHEMA);
    }
    if one_of(&payload["role"], "system developer") {
        return Ok(event);
    }
    let blocks = payload["content"].as_array().ok_or(SCHEMA)?;
    let text = text_blocks(&payload["content"], "input_text output_text", "input_image image")?;
    let metadata = &payload["internal_chat_message_metadata_passthrough"];
    if metadata.is_object() && !allowed_keys(metadata, "turn_id") {
        return Err("unknown_provenance_schema");
    }
    let associated = metadata.is_object()
        && metadata["turn_id"].is_string()
        && metadata["turn_id"].as_str() == context.turn.as_deref()
        && metadata["turn_id"] == context.active_turn;
    if context.source_ok && associated {
        event.role = if payload["role"] == "user" {
            Role::Candidate
        } else {
            Role::Assistant
        };
        event.text = text;
        if payload["role"] == "user" {
            let mut hash = Sha256::new();
            let mut first = true;
            for block in blocks.iter().filter(|b| b["type"] == "input_text") {
                if !first {
                    hash.update(b"\n");
                }
                first = false;
                hash.update(block["text"].as_str().ok_or(SCHEMA)?.as_bytes());
            }
            event.confirmation = hex::encode(hash.finalize());
        } else if !one_of(&payload["phase"], "commentary final_answer") {
            event.role = Role::Reset;
        }
    } else if payload["role"] == "user" {
        event.role = Role::Reset;
    }
    Ok(event)
}

fn omp(row: &Value, context: &mut Context) -> Result<Event, &'static str> {
    let mut event = Event::new(row, context);
    let kind = &row["type"];
    if kind == "title" {
        return Ok(event);
    }
    if kind == "session" {
        if row["version"].as_f64() != Some(3.0) {
            return Err("unsupported_version");
        }
        if !row["id"].is_string() || !row["cwd"].is_string() {
            return Err(SCHEMA);
        }
        context.session = row["id"].clone();
        context.cwd = row["cwd"].clone();
        return Ok(event);
    }
    if !truth(&context.session) {
        return Err("missing_session_header");
    }
    if one_of(kind, "reset_boundary compaction branch_summary") {
        event.role = Role::Reset;
        return Ok(event);
    }
    if one_of(kind, "title title_change model_change thinking_level_change custom custom_message credential_pin label session_info ttsr_injection") { return Ok(event); }
    if kind != "message" || ["id", "parentId", "message"].iter().any(|key| row.get(key).is_none()) {
        return Err(SCHEMA);
    }
    let message = &row["message"];
    if !message.is_object() || !one_of(&message["role"], "user assistant toolResult system developer") {
        return Err(SCHEMA);
    }
    event.time = timestamp(&message["timestamp"], true);
    if message.get("cwd").is_some() || row.get("cwd").is_some() {
        return Err("unknown_project_schema");
    }
    if !allowed_keys(row, "type id parentId timestamp message") || !allowed_keys(message, "api attribution completedAt content contextSnapshot details duration errorId errorMessage isError model provider providerPayload responseId role steering stopReason timestamp toolCallId toolName ttft usage useless") { return Err(SCHEMA); }
    if one_of(&message["role"], "toolResult system developer") {
        return Ok(event);
    }
    if !message["content"].is_array() {
        return Err(SCHEMA);
    }
    let text = text_blocks(&message["content"], "text", "thinking toolCall image")?;
    if message["role"] == "user" {
        if message["attribution"] == "user" {
            event.role = Role::User;
            event.text = text;
        } else {
            event.role = Role::Reset;
        }
    } else {
        if !one_of(&message["stopReason"], "stop toolUse aborted error length") {
            return Err(SCHEMA);
        }
        event.role = Role::Assistant;
        event.text = text;
        event.complete = message["stopReason"] == "stop";
        if event.complete {
            event.end = Some(timestamp(&message["completedAt"], true));
        }
    }
    Ok(event)
}

#[derive(Clone, Serialize, Deserialize)]
struct Turn {
    user: String,
    session: Value,
    start: Option<f64>,
    end: Option<f64>,
    invalid: bool,
    tail: Option<i64>,
    boundary: Option<i64>,
}

// Scratch contains metadata only. Ordinary text is extracted transiently from
// one source record, discarded, and reread only for a requested complete unit.
#[derive(Serialize, Deserialize)]
struct Reference {
    id: String,
    time: Option<f64>,
    user: bool,
    bytes: usize,
    total_bytes: usize,
    previous: Option<i64>,
    offset: u64,
    length: usize,
    hash: String,
    identity: Value,
}
#[derive(Serialize, Deserialize)]
struct Boundary {
    value: Value,
    previous: Option<i64>,
}
struct Candidate {
    event: Event,
    bytes: usize,
    offset: u64,
    length: usize,
    hash: String,
}
struct Scratch {
    // Drop the connection before unlinking its private 0600 tempfile.
    db: Connection,
    _file: tempfile::NamedTempFile,
}
fn capacity(_: impl std::fmt::Display) -> String {
    "source_capacity".into()
}
fn encode(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(capacity)
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_str(value).map_err(|_| "unknown_branch_lineage".into())
}
impl Scratch {
    fn new() -> Result<Self, String> {
        let file = tempfile::NamedTempFile::new().map_err(capacity)?;
        let db = Connection::open(file.path()).map_err(capacity)?;
        db.execute_batch(
            "PRAGMA journal_mode=OFF;
             PRAGMA synchronous=OFF;
             PRAGMA cache_size=-2048;
             PRAGMA mmap_size=0;
             PRAGMA temp_store=FILE;
             CREATE TABLE nodes (key TEXT PRIMARY KEY, turn TEXT, unit_id TEXT);
             CREATE INDEX nodes_units ON nodes(unit_id);
             CREATE TABLE refs (id INTEGER PRIMARY KEY, key TEXT UNIQUE, metadata TEXT NOT NULL);
             CREATE TABLE boundaries (id INTEGER PRIMARY KEY, metadata TEXT NOT NULL);
             CREATE TABLE emitted (id TEXT PRIMARY KEY);
             BEGIN;",
        )
        .map_err(capacity)?;
        Ok(Self { db, _file: file })
    }
    fn node(&self, key: &str) -> Result<Option<Turn>, String> {
        let value: Option<Option<String>> = self
            .db
            .query_row("SELECT turn FROM nodes WHERE key=?", [key], |row| row.get(0))
            .optional()
            .map_err(capacity)?;
        value.flatten().map(|value| decode(&value)).transpose()
    }
    fn put_node(&self, key: &str, turn: Option<&Turn>, harness: &str) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO nodes(key,turn,unit_id) VALUES (?,?,?)
             ON CONFLICT(key) DO UPDATE SET turn=excluded.turn,unit_id=excluded.unit_id",
                params![
                    key,
                    turn.map(encode).transpose()?,
                    turn.map(|turn| unit_id(turn, harness))
                ],
            )
            .map_err(capacity)?;
        Ok(())
    }
    fn reference(&self, id: i64) -> Result<Reference, String> {
        let metadata: String = self
            .db
            .query_row("SELECT metadata FROM refs WHERE id=?", [id], |row| row.get(0))
            .map_err(capacity)?;
        decode(&metadata)
    }
    fn boundary(&self, id: i64) -> Result<Boundary, String> {
        let metadata: String = self
            .db
            .query_row("SELECT metadata FROM boundaries WHERE id=?", [id], |row| row.get(0))
            .map_err(capacity)?;
        decode(&metadata)
    }
    fn bytes(&self, tail: Option<i64>) -> Result<usize, String> {
        tail.map(|id| self.reference(id).map(|reference| reference.total_bytes))
            .transpose()
            .map(|bytes| bytes.unwrap_or(0))
    }
    fn add_reference(&self, key: &str, mut reference: Reference) -> Result<i64, String> {
        if let Some(id) = self
            .db
            .query_row("SELECT id FROM refs WHERE key=?", [key], |row| row.get(0))
            .optional()
            .map_err(capacity)?
        {
            return Ok(id);
        }
        reference.total_bytes = self.bytes(reference.previous)?.saturating_add(reference.bytes);
        self.db
            .execute(
                "INSERT INTO refs(key,metadata) VALUES (?,?)",
                params![key, encode(&reference)?],
            )
            .map_err(capacity)?;
        Ok(self.db.last_insert_rowid())
    }
    fn emit(&self, id: &str) -> Result<bool, String> {
        self.db
            .execute("INSERT OR IGNORE INTO emitted VALUES (?)", [id])
            .map(|changed| changed != 0)
            .map_err(capacity)
    }
    fn pending(&self) -> Result<usize, String> {
        self.db
            .query_row(
                "SELECT count(DISTINCT unit_id) FROM nodes
             WHERE unit_id IS NOT NULL AND NOT EXISTS
             (SELECT 1 FROM emitted WHERE emitted.id=nodes.unit_id)",
                [],
                |row| row.get(0),
            )
            .map_err(capacity)
    }
}
fn unit_id(turn: &Turn, harness: &str) -> String {
    digest(format!("{harness}:{}:{}", py_string(&turn.session), turn.user))
}
fn touch(turn: &mut Turn, event: &Event, scratch: &Scratch) -> Result<(), String> {
    let mut invalid = event.invalid;
    if event.time.is_none() || matches!((event.time, turn.end), (Some(time), Some(end)) if time < end) {
        invalid = true;
    } else {
        turn.end = event.time;
    }
    if let Some(end) = event.end {
        if !matches!((end, event.time), (Some(end), Some(time)) if end >= time) {
            invalid = true;
        } else {
            turn.end = end;
        }
    }
    turn.invalid |= invalid;
    let boundary = Boundary {
        value: json!({"cwd":event.cwd,"time":event.time,"end":event.end.flatten(),"invalid":invalid}),
        previous: turn.boundary,
    };
    scratch
        .db
        .execute("INSERT INTO boundaries(metadata) VALUES (?)", [encode(&boundary)?])
        .map_err(capacity)?;
    turn.boundary = Some(scratch.db.last_insert_rowid());
    Ok(())
}

struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn response_size(value: &Value) -> Result<usize, String> {
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_err(capacity)?;
    Ok(counter.0)
}
fn charge(budget: &mut usize, value: &Value) -> Result<(), String> {
    *budget = budget.saturating_add(response_size(value)?.saturating_add(1));
    if *budget > MAX_RESPONSE_BYTES {
        return Err("oversized_response".into());
    }
    Ok(())
}
fn reread_text(stream: &mut BufReader<File>, reference: &Reference, harness: &str) -> Result<String, String> {
    stream
        .seek(SeekFrom::Start(reference.offset))
        .map_err(|_| "source_unavailable")?;
    let mut raw = vec![0; reference.length];
    stream.read_exact(&mut raw).map_err(|_| "source_changed")?;
    if digest(&raw) != reference.hash {
        return Err("source_changed".into());
    }
    let row: Value = serde_json::from_slice(&raw).map_err(|_| "source_changed")?;
    let text = match harness {
        "claude" => text_blocks(
            &row["message"]["content"],
            "text",
            "thinking redacted_thinking tool_use tool_result image document server_tool_use web_search_tool_result",
        ),
        "codex" => text_blocks(
            &row["payload"]["content"],
            "input_text output_text",
            "input_image image",
        ),
        _ => text_blocks(&row["message"]["content"], "text", "thinking toolCall image"),
    }
    .map_err(str::to_owned)?;
    if text.len() != reference.bytes {
        return Err("source_changed".into());
    }
    Ok(text)
}
fn finish(
    turn: &Turn,
    harness: &str,
    scratch: &Scratch,
    stream: &mut BufReader<File>,
    include_text: bool,
    ordinal: usize,
    response_bytes: usize,
) -> Result<Value, String> {
    let bytes = scratch.bytes(turn.tail)?;
    let oversized = bytes > MAX_TURN_BYTES;
    let session = py_string(&turn.session);
    let mut items = Vec::new();
    let mut message_ids = Vec::new();
    let mut chain = Vec::new();
    // Count each contribution before retaining it. No unbounded ancestry vector,
    // including for enormous chains of zero-text/tool-only records.
    let mut budget = response_bytes;
    let mut value = json!({"ordinal":ordinal,"id":unit_id(turn,harness),"session_id":turn.session,"user_id":turn.user,"start":turn.start,"end":turn.end,"invalid":turn.invalid,"boundaries":[],"items":[],"message_ids":[],"bytes":bytes});
    charge(&mut budget, &value)?;
    let mut tail = turn.tail;
    while let Some(index) = tail {
        let reference = scratch.reference(index)?;
        if reference.previous.is_some_and(|previous| previous >= index) {
            return Err("unknown_branch_lineage".into());
        }
        let mut item = json!({"id":digest(format!("{harness}:{session}:{}", reference.id)),"message_id":reference.id,"role":if reference.user {"user"} else {"assistant"},"time":reference.time});
        item["identity"] = reference.identity.clone();
        if include_text && !oversized {
            item["text"] = Value::String(reread_text(stream, &reference, harness)?);
        }
        let message_id = Value::String(reference.id);
        charge(&mut budget, &item)?;
        charge(&mut budget, &message_id)?;
        message_ids.push(message_id);
        items.push(item);
        tail = reference.previous;
    }
    items.reverse();
    message_ids.reverse();
    let mut boundary = turn.boundary;
    while let Some(index) = boundary {
        let item = scratch.boundary(index)?;
        if item.previous.is_some_and(|previous| previous >= index) {
            return Err("unknown_branch_lineage".into());
        }
        charge(&mut budget, &item.value)?;
        chain.push(item.value);
        boundary = item.previous;
    }
    chain.reverse();
    let invalid = turn.invalid
        || !turn.session.as_str().is_some_and(|session| !session.is_empty())
        || !matches!((turn.start, turn.end), (Some(start), Some(end)) if start <= end)
        || items.first().is_none_or(|item| item["role"] != "user");
    value["invalid"] = json!(invalid);
    value["items"] = Value::Array(items);
    value["message_ids"] = Value::Array(message_ids);
    value["boundaries"] = Value::Array(chain);
    Ok(value)
}

/// Scan the pinned original into disposable metadata, materializing only the
/// requested complete-unit window. The caller verifies revision before/after.
/// Structural stream failures retain the complete prefix; scratch I/O failures
/// are retryable errors, never cumulative transcript-length exclusions.
pub fn parse_page(
    path: &Path,
    harness: &str,
    now: f64,
    include_text: bool,
    offset: usize,
    limit: usize,
) -> Result<Value, String> {
    if !matches!(harness, "claude" | "codex" | "omp") {
        return Err("unsupported_client".into());
    }
    if !now.is_finite() {
        return Err("invalid_request".into());
    }
    let file = File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "source_missing"
        } else {
            "source_unavailable"
        }
        .to_owned()
    })?;
    let mut stream = BufReader::new(file);
    let scratch = Scratch::new()?;
    let mut raw = Vec::new();
    let mut context = Context::default();
    let mut linear: Option<Turn> = None;
    let mut candidate: Option<Candidate> = None;
    let mut turns = Vec::new();
    let mut status = "complete";
    let mut oversized_turn = false;
    let mut response_bytes = 0usize;
    let mut source_offset = 0u64;
    let mut record_index = 0usize;
    let mut ordinal = 0usize;
    let linked = harness != "codex";
    loop {
        raw.clear();
        let record_offset = source_offset;
        let count = stream
            .by_ref()
            .take((MAX_RECORD_BYTES + 1) as u64)
            .read_until(b'\n', &mut raw)
            .map_err(|_| "source_unavailable".to_owned())?;
        if count == 0 {
            break;
        }
        source_offset = source_offset.checked_add(count as u64).ok_or("source_capacity")?;
        if count > MAX_RECORD_BYTES {
            status = "oversized_source";
            break;
        }
        if raw.last() != Some(&b'\n') {
            status = "incomplete_write";
            break;
        }
        let row: Value = match serde_json::from_slice::<Value>(&raw) {
            Ok(row) if row.is_object() => row,
            _ => {
                status = "malformed_record";
                break;
            }
        };
        let mut next_context = context.clone();
        let parsed = match harness {
            "claude" => claude(&row, &mut next_context),
            "codex" => codex(&row, &mut next_context),
            _ => omp(&row, &mut next_context),
        };
        let mut event = match parsed {
            Ok(event) => event,
            Err(error) => {
                status = error;
                break;
            }
        };
        if event.time.is_some_and(|time| time > now) || event.end.flatten().is_some_and(|end| end > now) {
            status = "deferred_future";
            break;
        }
        context = next_context;
        if !truth(&event.session) {
            event.session = context.session.clone();
        }
        let session = event.session.clone();
        let session_string = py_string(&session);
        let native = if harness == "codex" && row["type"] == "response_item" {
            row["payload"].get("id")
        } else {
            row.get("uuid").or_else(|| row.get("id"))
        };
        let raw_record_digest = if native.filter(|value| !value.is_null()).is_none()
            || matches!(event.role, Role::User | Role::Assistant | Role::Candidate)
        {
            raw_digest(&raw)
        } else {
            String::new()
        };
        event.id = match native.filter(|value| !value.is_null()) {
            Some(native) => py_string(native),
            None => digest(format!(
                "{session_string}:{}:{record_index}:{}",
                context.turn.as_deref().unwrap_or(""),
                raw_record_digest
            )),
        };
        event.identity = json!({"native_id":native,"record_index":record_index,
            "turn_context":context.turn.as_deref().unwrap_or(""),"raw_record_digest":raw_record_digest});
        record_index = record_index.checked_add(1).ok_or("source_capacity")?;
        let node_key = format!("{session_string}:{}", event.id);
        let mut turn = if linked {
            scratch.node(&format!("{session_string}:{}", py_string(&event.parent)))?
        } else {
            linear.take()
        };
        let mut text_bytes = event.text.len();
        let mut text_offset = record_offset;
        let mut text_length = count;
        let mut text_hash = if matches!(event.role, Role::User | Role::Assistant | Role::Candidate) {
            digest(&raw)
        } else {
            String::new()
        };
        // A Codex candidate keeps provenance/hash/offset metadata, never text.
        event.text.clear();
        event.text.shrink_to_fit();
        let mut role = event.role;
        if role == Role::Candidate {
            candidate = Some(Candidate {
                event,
                bytes: text_bytes,
                offset: text_offset,
                length: text_length,
                hash: text_hash,
            });
            linear = turn;
            continue;
        } else if role == Role::Confirm {
            match candidate.take() {
                Some(mut confirmed) if confirmed.event.confirmation == event.confirmation => {
                    confirmed.event.role = Role::User;
                    event = confirmed.event;
                    text_bytes = confirmed.bytes;
                    text_offset = confirmed.offset;
                    text_length = confirmed.length;
                    text_hash = confirmed.hash;
                    role = Role::User;
                }
                _ => role = Role::Reset,
            }
        }
        let mut completed = Vec::new();
        if role == Role::User {
            if let Some(mut previous) = turn.take() {
                touch(&mut previous, &event, &scratch)?;
                completed.push(previous);
            }
            turn = Some(Turn {
                user: event.id.clone(),
                session: session.clone(),
                start: event.time,
                end: event.time,
                invalid: text_bytes == 0,
                tail: None,
                boundary: None,
            });
        } else if role == Role::Reset {
            turn = None;
        }
        if let Some(turn) = turn.as_mut() {
            if matches!(role, Role::User | Role::Assistant | Role::Boundary)
                || (role == Role::Metadata
                    && (event.time.is_some() || one_of(&row["type"], "user assistant message response_item")))
            {
                touch(turn, &event, &scratch)?;
            }
            if matches!(role, Role::User | Role::Assistant) {
                turn.tail = Some(scratch.add_reference(
                    &format!("{session_string}:{}", event.id),
                    Reference {
                        id: event.id.clone(),
                        time: event.time,
                        user: role == Role::User,
                        bytes: text_bytes,
                        total_bytes: 0,
                        previous: turn.tail,
                        offset: text_offset,
                        length: text_length,
                        hash: text_hash,
                        identity: event.identity.clone(),
                    },
                )?);
            }
        }
        if event.complete {
            if let Some(turn) = turn.take() {
                completed.push(turn);
            }
        }
        if linked {
            scratch.put_node(&node_key, turn.as_ref(), harness)?;
        } else {
            linear = turn;
        }
        for turn in completed {
            if !scratch.emit(&unit_id(&turn, harness))? {
                continue;
            }
            oversized_turn |= scratch.bytes(turn.tail)? > MAX_TURN_BYTES;
            if ordinal >= offset && turns.len() < limit {
                let value = finish(
                    &turn,
                    harness,
                    &scratch,
                    &mut stream,
                    include_text,
                    ordinal,
                    response_bytes,
                )?;
                response_bytes = response_bytes.saturating_add(response_size(&value)?.saturating_add(1));
                if response_bytes > MAX_RESPONSE_BYTES {
                    return Err("oversized_response".into());
                }
                turns.push(value);
                // Rereads use this same open descriptor, never a reopened path.
                stream
                    .seek(SeekFrom::Start(source_offset))
                    .map_err(|_| "source_unavailable")?;
            }
            ordinal = ordinal.checked_add(1).ok_or("source_capacity")?;
        }
    }
    let pending_turns = if linked {
        scratch.pending()?
    } else {
        usize::from(linear.is_some())
    };
    if status == "complete" && pending_turns > 0 {
        status = "incomplete_turn";
    }
    if status == "complete" && oversized_turn {
        status = "oversized_source";
    }
    if offset > ordinal {
        return Err("invalid_request".into());
    }
    let next = offset.saturating_add(turns.len());
    let next_offset = (next < ordinal).then_some(next);
    Ok(json!({"turns":turns,"status":status,"pending_turns":pending_turns,"next_offset":next_offset}))
}
