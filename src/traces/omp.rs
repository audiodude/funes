//! Explicit OMP ingestion: persisted user/assistant text, never the active-context projection.
//! Graph-only sidecars keep controls and excluded messages out of the searchable text schema.

use super::{jsonl, Block, Turn, FORMAT_VERSION};
use crate::memory::dataset;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Coverage {
    pub signature: String,
    pub complete: bool,
    pub issues: Vec<String>,
    pub session_id: String,
    pub messages: usize,
    #[serde(default)]
    pub dependencies: Vec<CoverageDependency>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CoverageDependency {
    pub path: String,
    pub signature: Option<String>,
}

impl Coverage {
    fn issue(&mut self, issue: impl Into<String>) {
        self.complete = false;
        let issue = issue.into();
        if !self.issues.contains(&issue) {
            self.issues.push(issue);
        }
    }
}

/// Shared with the bridge's bigint stat protocol. No parser/dependency prefix is permitted.
pub fn file_signature(path: &Path) -> io::Result<String> {
    let md = fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mtime = i128::from(md.mtime()) * 1_000_000_000 + i128::from(md.mtime_nsec());
        let ctime = i128::from(md.ctime()) * 1_000_000_000 + i128::from(md.ctime_nsec());
        Ok(format!("{}:{mtime}:{ctime}:{}", md.len(), md.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "OMP stat protocol requires Unix",
        ))
    }
}

#[derive(Serialize, Deserialize)]
struct Dependency {
    path: PathBuf,
    signature: Option<String>,
    session_id: Option<String>,
    relation: String,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    id: String,
    parent_id: Option<String>,
    kind: String,
    /// Strict allowlist of graph links and attribution, not arbitrary control/message payloads.
    metadata: Map<String, Value>,
}

#[derive(Serialize, Deserialize)]
struct Provenance {
    version: u32,
    session_id: String,
    source_path: PathBuf,
    signature: String,
    session_version: u64,
    parent_session: Option<String>,
    parent_reference_type: Option<String>,
    previous_session_files: Vec<String>,
    dependencies: Vec<Dependency>,
    entries: Vec<Entry>,
    coverage: Coverage,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn id(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| valid_id(s))
}

fn sidecar_path(session_id: &str) -> Option<PathBuf> {
    valid_id(session_id).then(|| {
        dataset::funes_dir()
            .join("omp-provenance")
            .join(format!("{session_id}.json"))
    })
}

fn load_provenance(session_id: &str) -> Option<Provenance> {
    let p: Provenance = serde_json::from_slice(&fs::read(sidecar_path(session_id)?).ok()?).ok()?;
    (p.version == 1 && p.session_id == session_id).then_some(p)
}

/// Only read the title/header of a dependency, never its transcript content or external blobs.
fn header(path: &Path) -> io::Result<Value> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    for _ in 0..2 {
        line.clear();
        // Header/title metadata is bounded; oversized records are not a reason to read a transcript.
        let mut bounded = (&mut reader).take(1024 * 1024);
        bounded.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            break;
        }
        let record: Value = serde_json::from_slice(&line).map_err(io::Error::other)?;
        match record.get("type").and_then(Value::as_str) {
            Some("title") => continue,
            Some("session") if id(record.get("id")).is_some() => return Ok(record),
            _ => break,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "missing complete OMP session header",
    ))
}

pub fn dependencies_current(path: &Path) -> bool {
    let Ok(h) = header(path) else { return false };
    let Some(sid) = id(h.get("id")) else { return false };
    let Some(p) = load_provenance(sid) else { return false };
    p.source_path == path
        && file_signature(path).ok().as_deref() == Some(p.signature.as_str())
        && p.dependencies
            .iter()
            .all(|d| d.signature.is_some() && d.session_id.is_some() && file_signature(&d.path).ok() == d.signature)
}

fn dependency(path: PathBuf, relation: &str, coverage: &mut Coverage) -> Dependency {
    let signature = file_signature(&path).ok();
    let session_id = header(&path).ok().and_then(|h| id(h.get("id")).map(str::to_owned));
    if signature.is_none() || session_id.is_none() || file_signature(&path).ok() != signature {
        coverage.issue(format!("{relation} dependency unavailable or unstable"));
    }
    Dependency {
        path,
        signature,
        session_id,
        relation: relation.into(),
    }
}

fn persist(p: &Provenance) -> io::Result<()> {
    let path = sidecar_path(&p.session_id).ok_or_else(|| io::Error::other("invalid OMP session identity"))?;
    let dir = path.parent().expect("sidecar parent");
    fs::create_dir_all(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut temp, p).map_err(io::Error::other)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(&path).map_err(|e| e.error)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn blocks(content: Option<&Value>, coverage: &mut Coverage) -> Vec<Block> {
    fn text(text: &str, blocks: &mut Vec<Block>, coverage: &mut Coverage) {
        if text.contains("[Session persistence truncated large content]") {
            coverage.issue("persisted message text was truncated by OMP");
        }
        if !text.is_empty() {
            blocks.push(Block {
                block_type: "text".into(),
                text: text.into(),
                tool_name: None,
                tool_use_id: None,
            });
        }
    }
    let mut out = Vec::new();
    match content {
        Some(Value::String(s)) => text(s, &mut out, coverage),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => match part.get("text").and_then(Value::as_str) {
                        Some(s) => text(s, &mut out, coverage),
                        None => coverage.issue("unsupported message text representation"),
                    },
                    Some(
                        "thinking" | "redactedThinking" | "fallback" | "anthropicServerTool" | "toolCall" | "image",
                    ) => {}
                    _ => coverage.issue("unsupported message content part"),
                }
            }
        }
        _ => coverage.issue("unsupported message content representation"),
    }
    out
}

/// Parse completed physical records, retaining usable text even when coverage is incomplete.
fn parse(bytes: &[u8], path: &Path, signature: String, fallback: &str) -> (Vec<Turn>, Coverage, Option<Provenance>) {
    let mut coverage = Coverage {
        signature,
        complete: true,
        issues: Vec::new(),
        session_id: String::new(),
        messages: 0,
        dependencies: Vec::new(),
    };
    let mut records = Vec::new();
    for (n, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
        if line.last() != Some(&b'\n') {
            coverage.issue("incomplete final JSONL record");
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            coverage.issue(format!("empty complete record at line {}", n + 1));
            continue;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(v) if v.is_object() => records.push(v),
            _ => coverage.issue(format!("malformed complete record at line {}", n + 1)),
        }
    }
    let mut iter = records.iter();
    let first = iter.next();
    let header = if first.and_then(|v| v.get("type")).and_then(Value::as_str) == Some("title") {
        if first.and_then(|v| v.get("v")).and_then(Value::as_u64) != Some(1) {
            coverage.issue("unsupported title preamble version");
        }
        iter.next()
    } else {
        first
    };
    let Some(header) = header.filter(|v| v.get("type").and_then(Value::as_str) == Some("session")) else {
        coverage.issue("missing session header");
        return (Vec::new(), coverage, None);
    };
    let Some(sid) = id(header.get("id")) else {
        coverage.issue("invalid session identity");
        return (Vec::new(), coverage, None);
    };
    coverage.session_id = sid.into();
    let version = header.get("version").and_then(Value::as_u64).unwrap_or(1);
    // OMP's v1 migration creates random IDs. Inventing a parallel migration breaks native citations.
    if !matches!(version, 2 | 3) {
        coverage.issue("unsupported session version; v1 requires OMP-persisted native migration");
        return (Vec::new(), coverage, None);
    }
    let cwd = header.get("cwd").and_then(Value::as_str).map(str::to_owned);
    let workdir = cwd
        .as_deref()
        .and_then(jsonl::workdir_of_cwd)
        .unwrap_or_else(|| fallback.into());
    let mut provenance = Provenance {
        version: 1,
        session_id: sid.into(),
        source_path: path.into(),
        signature: coverage.signature.clone(),
        session_version: version,
        parent_session: None,
        parent_reference_type: None,
        previous_session_files: Vec::new(),
        dependencies: Vec::new(),
        entries: Vec::new(),
        coverage: coverage.clone(),
    };
    if let Some(value) = header.get("previousSessionFiles") {
        match value.as_array() {
            Some(paths) if paths.iter().all(Value::is_string) => {
                provenance.previous_session_files = paths.iter().filter_map(Value::as_str).map(str::to_owned).collect()
            }
            _ => coverage.issue("invalid previous-session paths"),
        }
    }
    if let Some(value) = header.get("parentSession") {
        if let Some(reference) = value.as_str().filter(|s| !s.is_empty()) {
            provenance.parent_session = Some(reference.into());
            if valid_id(reference) {
                provenance.parent_reference_type = Some("session_id".into());
            } else if reference.contains('/') || reference.ends_with(".jsonl") {
                provenance.parent_reference_type = Some("path".into());
                let parent = Path::new(reference);
                let parent = if parent.is_absolute() {
                    parent.to_path_buf()
                } else {
                    path.parent().unwrap_or(Path::new(".")).join(parent)
                };
                provenance
                    .dependencies
                    .push(dependency(parent, "header_parent", &mut coverage));
            } else {
                coverage.issue("unsupported parent-session reference");
            }
        } else {
            coverage.issue("invalid parent-session reference");
        }
    }
    let mut turns = Vec::new();
    let mut seen: HashMap<&str, &Value> = HashMap::new();
    let mut structural_child = path
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("__advisor"));
    for record in iter {
        let Some(kind) = record.get("type").and_then(Value::as_str) else {
            coverage.issue("entry type missing");
            continue;
        };
        let Some(eid) = id(record.get("id")) else {
            coverage.issue("entry identity missing or invalid");
            continue;
        };
        if let Some(previous) = seen.get(eid) {
            if *previous != record {
                coverage.issue("conflicting duplicate entry identity");
            }
            continue;
        }
        let parent = match record.get("parentId") {
            Some(Value::Null) => None,
            Some(value) if id(Some(value)).is_some() => id(Some(value)).map(str::to_owned),
            _ => {
                coverage.issue("entry parent identity missing or invalid");
                None
            }
        };
        if parent.as_deref().is_some_and(|p| !seen.contains_key(p)) {
            coverage.issue("entry parent is missing or not earlier in persisted graph");
        }
        seen.insert(eid, record);
        let mut metadata = Map::new();
        for key in ["fromId", "firstKeptEntryId", "providerReplayThroughEntryId", "targetId"] {
            if let Some(value) = record.get(key) {
                if id(Some(value)).is_none() {
                    coverage.issue("invalid control graph reference");
                } else {
                    metadata.insert(key.into(), value.clone());
                }
            }
        }
        if kind == "compaction" && id(record.get("firstKeptEntryId")).is_none()
            || kind == "branch_summary" && id(record.get("fromId")).is_none()
        {
            coverage.issue("missing control graph reference");
        }
        structural_child |= kind == "session_init";
        let mut entry = Entry {
            id: eid.into(),
            parent_id: parent.clone(),
            kind: kind.into(),
            metadata,
        };
        match kind {
            "message" => {
                if let Some(message) = record.get("message").filter(|m| m.is_object()) {
                    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
                    if role.is_empty() {
                        coverage.issue("message role missing");
                    }
                    entry.metadata.insert("role".into(), Value::String(role.into()));
                    if let Some(synthetic) = message.get("synthetic").and_then(Value::as_bool) {
                        entry.metadata.insert("synthetic".into(), Value::Bool(synthetic));
                    }
                    if let Some(attribution) = message
                        .get("attribution")
                        .and_then(Value::as_str)
                        .filter(|s| matches!(*s, "user" | "agent"))
                    {
                        entry
                            .metadata
                            .insert("attribution".into(), Value::String(attribution.into()));
                    }
                    if let Some(reason) = message
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .filter(|s| matches!(*s, "stop" | "length" | "toolUse" | "error" | "aborted"))
                    {
                        entry.metadata.insert("stopReason".into(), Value::String(reason.into()));
                    }
                    if let Some(status) = message
                        .get("retryRecovery")
                        .and_then(|r| r.get("status"))
                        .and_then(Value::as_str)
                        .filter(|s| matches!(*s, "recovered" | "superseded"))
                    {
                        entry
                            .metadata
                            .insert("retryRecoveryStatus".into(), Value::String(status.into()));
                    }
                    if matches!(role, "user" | "assistant") {
                        coverage.messages += 1;
                        let retained = blocks(message.get("content"), &mut coverage);
                        if !retained.is_empty() {
                            turns.push(Turn {
                                format: FORMAT_VERSION,
                                session_id: sid.into(),
                                cwd: cwd.clone(),
                                workdir: workdir.clone(),
                                turn_uuid: eid.into(),
                                parent_uuid: parent,
                                seq: turns.len() as i64,
                                ts: record.get("timestamp").and_then(Value::as_str).unwrap_or("").into(),
                                role: role.into(),
                                blocks: retained,
                                source_path: path.to_string_lossy().into_owned(),
                                harness: "omp".into(),
                            });
                        }
                    }
                } else {
                    coverage.issue("message payload missing");
                }
            }
            "compaction"
            | "branch_summary"
            | "reset_boundary"
            | "session_init"
            | "model_change"
            | "thinking_level_change"
            | "service_tier_change"
            | "custom"
            | "label"
            | "title_change"
            | "ttsr_injection"
            | "credential_pin"
            | "model_usage"
            | "mode_change"
            | "custom_message" => {}
            _ => coverage.issue("unsupported entry type"),
        }
        provenance.entries.push(entry);
    }
    for entry in &provenance.entries {
        for key in ["fromId", "firstKeptEntryId", "providerReplayThroughEntryId", "targetId"] {
            if entry.metadata.get(key).and_then(Value::as_str).is_some_and(|v| {
                !(entry.kind == "branch_summary" && key == "fromId" && v == "root" || seen.contains_key(v))
            }) {
                coverage.issue("control graph reference is missing from retained archive");
            }
        }
    }
    if let Some(dir) = path.parent() {
        // Artifact directories are the complete owning filename with only the final .jsonl removed.
        let owner = PathBuf::from(format!("{}.jsonl", dir.display()));
        if structural_child || owner.is_file() {
            provenance
                .dependencies
                .push(dependency(owner, "structural_owner", &mut coverage));
        }
    }
    coverage.dependencies = provenance
        .dependencies
        .iter()
        .map(|d| CoverageDependency {
            path: d.path.to_string_lossy().into_owned(),
            signature: d.signature.clone(),
        })
        .collect();
    provenance.coverage = coverage.clone();
    (turns, coverage, Some(provenance))
}

fn read_unpersisted(path: &Path, fallback: &str) -> io::Result<(Vec<Turn>, Coverage, Option<Provenance>)> {
    let signature = file_signature(path)?;
    let bytes = fs::read(path)?;
    let (turns, mut coverage, provenance) = parse(&bytes, path, signature, fallback);
    if file_signature(path)? != coverage.signature {
        coverage.issue("source changed during read");
    }
    Ok((turns, coverage, provenance))
}

/// Validate an OMP transcript without writing its graph sidecar.
pub fn check(path: &Path, fallback: &str) -> io::Result<Vec<Turn>> {
    let (turns, coverage, _) = read_unpersisted(path, fallback)?;
    if !coverage.complete {
        return Err(io::Error::new(io::ErrorKind::InvalidData, coverage.issues.join("; ")));
    }
    Ok(turns)
}

pub fn read(path: &Path, fallback: &str) -> io::Result<(Vec<Turn>, Coverage)> {
    let (turns, coverage, provenance) = read_unpersisted(path, fallback)?;
    if let Some(mut provenance) = provenance {
        provenance.coverage = coverage.clone();
        persist(&provenance)?;
    }
    Ok((turns, coverage))
}

/// Rendering uses only Funes-owned metadata, never reopens a transcript to serve a citation.
pub fn provenance_note(session_id: &str, turn_ids: &[&str]) -> String {
    let mut note = format!("OMP historical archive {session_id}: archived branches retained; branch relation is not a current recommendation; supersession unknown. File-order neighbors are not necessarily on the same branch. Citations report user/assistant statements, not independent verification of omitted tool output.\n");
    let Some(p) = load_provenance(session_id) else {
        note.push_str("OMP graph/source provenance unavailable; coverage cannot be established.\n");
        return note;
    };
    note.push_str(&format!(
        "Source: {} (snapshot {}; parse complete: {}).\n",
        p.source_path.display(),
        p.signature,
        p.coverage.complete
    ));
    if let Some(parent) = p.parent_session {
        note.push_str(&format!(
            "Parent session reference ({}) = {parent:?}.\n",
            p.parent_reference_type.as_deref().unwrap_or("unknown")
        ));
    }
    for dep in p.dependencies {
        note.push_str(&format!(
            "{}: {} (session {}). Structural ownership does not establish the immediate spawning agent.\n",
            dep.relation,
            dep.path.display(),
            dep.session_id.as_deref().unwrap_or("unresolved")
        ));
    }
    if !p.coverage.complete {
        note.push_str(&format!(
            "Incomplete source coverage: {}.\n",
            p.coverage.issues.join("; ")
        ));
    }
    if !p.previous_session_files.is_empty() {
        note.push_str(&format!(
            "Recorded previous source locations: {:?}.\n",
            p.previous_session_files
        ));
    }
    let by_id: HashMap<&str, &Entry> = p.entries.iter().map(|e| (e.id.as_str(), e)).collect();
    for entry in &p.entries {
        if matches!(entry.kind.as_str(), "compaction" | "branch_summary" | "reset_boundary") {
            note.push_str(&format!(
                "Archive control {} type={} parent={} links={}.\n",
                entry.id,
                entry.kind,
                entry.parent_id.as_deref().unwrap_or("root"),
                Value::Object(entry.metadata.clone())
            ));
        }
    }
    for entry in &p.entries {
        if turn_ids.contains(&entry.id.as_str()) {
            note.push_str(&format!(
                "Entry {} parent={} metadata={}.\n",
                entry.id,
                entry.parent_id.as_deref().unwrap_or("root"),
                Value::Object(entry.metadata.clone())
            ));
            let mut parent = entry.parent_id.as_deref();
            let mut hops = 0;
            while let Some(eid) = parent {
                let Some(ancestor) = by_id.get(eid) else { break };
                if ancestor.kind != "message" {
                    note.push_str(&format!(
                        "  Graph ancestor {} type={} metadata={}.\n",
                        ancestor.id,
                        ancestor.kind,
                        Value::Object(ancestor.metadata.clone())
                    ));
                }
                parent = ancestor.parent_id.as_deref();
                hops += 1;
                if hops >= p.entries.len() {
                    break;
                }
            }
        }
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(records: Vec<Value>) -> Vec<u8> {
        records
            .into_iter()
            .map(|v| format!("{v}\n"))
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn message_boundary_preserves_graph_and_native_identity() {
        let bytes = fixture(vec![
            json!({"type":"title","v":1,"title":"not indexed"}),
            json!({"type":"session","version":3,"id":"native-session","cwd":"/tmp"}),
            json!({"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":"retained user"}}),
            json!({"type":"message","id":"t1","parentId":"u1","message":{"role":"toolResult","content":"secret tool"}}),
            json!({"type":"reset_boundary","id":"r1","parentId":"t1"}),
            json!({"type":"message","id":"a1","parentId":"r1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret thinking"},{"type":"text","text":"retained assistant"},{"type":"toolCall","arguments":{"secret":"tool"}}]}}),
            json!({"type":"message","id":"b1","parentId":"u1","message":{"role":"user","content":"archived fork"}}),
        ]);
        let (turns, c, p) = parse(&bytes, Path::new("/nonexistent/child.jsonl"), "sig".into(), "fallback");
        assert!(c.complete, "{:?}", c.issues);
        assert_eq!(
            turns.iter().map(|t| t.turn_uuid.as_str()).collect::<Vec<_>>(),
            ["u1", "a1", "b1"]
        );
        assert!(turns
            .iter()
            .all(|t| t.session_id == "native-session" && t.blocks.iter().all(|b| b.block_type == "text")));
        assert_eq!(turns[1].parent_uuid.as_deref(), Some("r1"));
        let encoded = serde_json::to_string(&p.unwrap()).unwrap();
        assert!(!encoded.contains("secret") && !encoded.contains("retained assistant"));
        assert!(encoded.contains("reset_boundary"));
        let (renamed, _, _) = parse(&bytes, Path::new("/elsewhere/renamed.jsonl"), "sig".into(), "fallback");
        assert_eq!(renamed[0].session_id, turns[0].session_id);
    }

    #[test]
    fn incomplete_or_unsupported_content_never_acknowledges_source() {
        let mut bytes = fixture(vec![
            json!({"type":"session","version":2,"id":"s","cwd":"/tmp"}),
            json!({"type":"message","id":"a","parentId":null,"message":{"role":"assistant","content":"completed"}}),
        ]);
        bytes.extend_from_slice(b"{\"type\":\"message\"");
        let (turns, c, _) = parse(&bytes, Path::new("/nonexistent/s.jsonl"), "sig".into(), "");
        assert_eq!(turns[0].blocks[0].text, "completed");
        assert!(!c.complete);
        let bytes = fixture(vec![
            json!({"type":"session","version":3,"id":"s","cwd":"/tmp"}),
            json!({"type":"message","id":"a","parentId":null,"message":{"role":"user","content":[{"type":"text","text":{"blob":"unavailable"}}]}}),
        ]);
        let (turns, c, _) = parse(&bytes, Path::new("/nonexistent/s.jsonl"), "sig".into(), "");
        assert!(turns.is_empty());
        assert!(!c.complete);
    }

    #[test]
    fn children_use_header_identity_and_require_structural_owner() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent.jsonl");
        fs::write(
            &parent,
            fixture(vec![json!({"type":"session","version":3,"id":"parent"})]),
        )
        .unwrap();
        let child = dir.path().join("parent/worker.jsonl");
        let records = |sid: &str| {
            fixture(vec![
                json!({"type":"session","version":3,"id":sid,"cwd":"/tmp"}),
                json!({"type":"session_init","id":"init","parentId":null,"systemPrompt":"excluded","task":"excluded"}),
                json!({"type":"message","id":"m","parentId":"init","message":{"role":"user","content":"child text","synthetic":true,"attribution":"agent"}}),
            ])
        };
        let (one, c, p) = parse(&records("one"), &child, "sig".into(), "");
        assert!(c.complete, "{:?}", c.issues);
        assert_eq!(p.unwrap().dependencies[0].session_id.as_deref(), Some("parent"));
        let (two, c, _) = parse(
            &records("two"),
            &dir.path().join("missing/worker.jsonl"),
            "sig".into(),
            "",
        );
        assert!(!c.complete);
        assert_ne!(one[0].session_id, two[0].session_id);
        assert_eq!(two[0].blocks[0].text, "child text");
    }

    #[test]
    fn malformed_middle_and_truncated_text_keep_completed_records_retryable() {
        let mut bytes = fixture(vec![json!({"type":"session","version":3,"id":"s","cwd":"/tmp"})]);
        bytes.extend_from_slice(b"{invalid}\n");
        bytes.extend(fixture(vec![json!({"type":"message","id":"m","parentId":null,"message":{"role":"user","content":"prefix\n[Session persistence truncated large content]"}})]));
        let (turns, c, _) = parse(&bytes, Path::new("/nonexistent/s.jsonl"), "sig".into(), "");
        assert_eq!(turns[0].turn_uuid, "m");
        assert_eq!(c.issues.len(), 2);
        assert!(!c.complete);
    }

    #[test]
    fn branch_summary_root_sentinel_is_not_a_missing_entry() {
        let bytes = fixture(vec![
            json!({"type":"session","version":3,"id":"s","cwd":"/tmp"}),
            json!({"type":"branch_summary","id":"b","parentId":null,"fromId":"root","summary":"excluded"}),
            json!({"type":"message","id":"m","parentId":"b","message":{"role":"user","content":"retained"}}),
        ]);
        let (turns, coverage, provenance) = parse(&bytes, Path::new("/nonexistent/s.jsonl"), "sig".into(), "");
        assert!(coverage.complete, "{:?}", coverage.issues);
        assert_eq!(turns[0].parent_uuid.as_deref(), Some("b"));
        assert_eq!(provenance.unwrap().entries[0].metadata["fromId"], "root");
    }

    #[test]
    fn custom_message_and_mode_controls_keep_graph_without_indexing_payloads() {
        let bytes = fixture(vec![
            json!({"type":"session","version":3,"id":"s","cwd":"/tmp"}),
            json!({"type":"mode_change","id":"mode","parentId":null,"mode":"plan","data":{"text":"excluded mode payload"}}),
            json!({"type":"custom_message","id":"custom","parentId":"mode","content":"excluded custom content","display":true}),
            json!({"type":"message","id":"m","parentId":"custom","message":{"role":"user","content":"retained"}}),
        ]);
        let (turns, coverage, provenance) = parse(&bytes, Path::new("/nonexistent/s.jsonl"), "sig".into(), "");
        assert!(coverage.complete, "{:?}", coverage.issues);
        assert_eq!(turns.iter().map(|t| t.turn_uuid.as_str()).collect::<Vec<_>>(), ["m"]);
        assert_eq!(turns[0].parent_uuid.as_deref(), Some("custom"));
        let provenance = provenance.unwrap();
        assert_eq!(provenance.entries[1].parent_id.as_deref(), Some("mode"));
        assert!(!serde_json::to_string(&provenance).unwrap().contains("excluded"));
    }
}
