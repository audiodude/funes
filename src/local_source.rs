//! Local-only source inventory. This module never initializes search or a remote memory.
mod normalize;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 256 * 1024 * 1024;
const MAX_TURN: u64 = 32 * 1024 * 1024;
const MAX_SOURCES: usize = 100_000;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_ROOTS: usize = 256;
const INVENTORY_VERSION: i64 = 1;
type Result<T, E = &'static str> = std::result::Result<T, E>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    protocol: u64,
    op: String,
    corpus: String,
    scope: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    source_id: Option<String>,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    ordinal: Option<usize>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Enrollment {
    version: u64,
    roots: Roots,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Roots {
    claude: Vec<String>,
    codex: Vec<String>,
    omp: Vec<String>,
}

struct Scope {
    path: PathBuf,
    id: String,
    roots: Vec<(String, PathBuf)>,
}

#[derive(Clone)]
struct Source {
    id: String,
    harness: String,
    locator: String,
    revision: String,
    state: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    snapshot: String,
    scope: String,
    harness: Option<String>,
    offset: usize,
}

/// Run exactly one bounded JSON request. All failures are content-free on stdout.
pub fn stdio() -> std::process::ExitCode {
    let mut input = Vec::new();
    let result = match std::io::stdin()
        .lock()
        .take((MAX_REQUEST + 1) as u64)
        .read_to_end(&mut input)
    {
        Ok(_) => execute(&input),
        Err(_) => Err("invalid_request"),
    };
    let successful = result.is_ok();
    let response = match result {
        Ok(result) => json!({"protocol": 1, "ok": true, "result": result}),
        Err(code) => json!({"protocol": 1, "ok": false, "error": {"code": code}}),
    };
    let mut output = BoundedOutput(Vec::new());
    let oversized = serde_json::to_writer(&mut output, &response).is_err();
    if oversized {
        output.0 = b"{\"protocol\":1,\"ok\":false,\"error\":{\"code\":\"oversized_response\"}}".to_vec();
    }
    let mut stdout = std::io::stdout().lock();
    let written = stdout
        .write_all(&output.0)
        .and_then(|_| stdout.write_all(b"\n"))
        .is_ok();
    if successful && !oversized && written {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

struct BoundedOutput(Vec<u8>);
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_RESPONSE.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("oversized_response"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn execute(input: &[u8]) -> Result<Value> {
    if input.len() > MAX_REQUEST {
        return Err("invalid_request");
    }
    let value: Value = serde_json::from_slice(input).map_err(|_| "invalid_request")?;
    if value.get("protocol").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported_protocol");
    }
    let request: Request = serde_json::from_slice(input).map_err(|_| "invalid_request")?;
    validate_request(&request, &value)?;
    let corpus = absolute_path(&request.corpus).map_err(|_| "invalid_scope")?;
    check_components(&corpus, true).map_err(|_| "invalid_scope")?;
    let scope = load_scope(Path::new(&request.scope))?;
    if request.op == "capabilities" {
        return Ok(json!({
            "protocol": 1, "build_revision": env!("FUNES_BUILD_REVISION"),
            "identity": "actomasto-v1", "harnesses": {
                "claude": "claude-schema2", "codex": "codex-0.144.1-schema1", "omp": "omp-session3-schema1"
            }, "local_only": true, "metadata_only": true,
            "revision_bound": true, "snapshot_enumeration": true
        }));
    }
    if request.op == "refresh" {
        return refresh(&corpus, &scope);
    }
    let connection = open_inventory(&corpus, false).map_err(|code| {
        if request.cursor.is_some() && code == "coverage_unavailable" {
            "invalid_cursor"
        } else {
            code
        }
    })?;
    match request.op.as_str() {
        "enumerate" => enumerate(&connection, &scope, &request),
        "turns" | "read" => source_turns(&connection, &scope, &request),
        _ => Err("invalid_request"),
    }
}

fn validate_request(request: &Request, value: &Value) -> Result<()> {
    if request.protocol != 1 {
        return Err("unsupported_protocol");
    }
    let extra: &[&str] = match request.op.as_str() {
        "capabilities" | "refresh" => &[],
        "enumerate" => &["cursor", "limit", "harness"],
        "turns" => &["source_id", "revision", "offset", "limit"],
        "read" => &["source_id", "revision", "ordinal"],
        _ => return Err("invalid_request"),
    };
    for key in value.as_object().ok_or("invalid_request")?.keys() {
        if !["protocol", "op", "corpus", "scope"].contains(&key.as_str()) && !extra.contains(&key.as_str()) {
            return Err("invalid_request");
        }
        if value[key].is_null() && key != "cursor" {
            return Err("invalid_request");
        }
    }
    if request.limit.is_some_and(|limit| !(1..=128).contains(&limit)) {
        return Err("invalid_request");
    }
    if request
        .harness
        .as_deref()
        .is_some_and(|h| !["claude", "codex", "omp"].contains(&h))
    {
        return Err("invalid_request");
    }
    if matches!(request.op.as_str(), "turns" | "read") {
        if !request.source_id.as_deref().is_some_and(is_digest) || !request.revision.as_deref().is_some_and(is_digest) {
            return Err("invalid_request");
        }
        if request.op == "read" && request.ordinal.is_none() {
            return Err("invalid_request");
        }
    }
    Ok(())
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn absolute_path(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || value.contains("://")
        || value.contains('\0')
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err("invalid_scope");
    }
    Ok(path.components().collect())
}

/// Reject symlinks in every component, not only the final source filename.
fn check_components(path: &Path, missing_ok: bool) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err("invalid_scope"),
            Ok(_) => (),
            Err(error) if missing_ok && error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("source_missing"),
            Err(_) => return Err("source_unavailable"),
        }
    }
    Ok(())
}

fn private(path: &Path, directory: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "invalid_scope")?;
    let expected = if directory { 0o700 } else { 0o600 };
    if metadata.file_type().is_symlink()
        || metadata.is_dir() != directory
        || (!directory && !metadata.is_file())
        || metadata.permissions().mode() & 0o777 != expected
    {
        return Err("invalid_scope");
    }
    Ok(())
}

fn load_scope(path: &Path) -> Result<Scope> {
    let path = absolute_path(path.to_str().ok_or("invalid_scope")?)?;
    check_components(&path, false).map_err(|_| "invalid_scope")?;
    private(&path, false)?;
    let mut raw = Vec::new();
    File::open(&path)
        .map_err(|_| "invalid_scope")?
        .take((MAX_REQUEST + 1) as u64)
        .read_to_end(&mut raw)
        .map_err(|_| "invalid_scope")?;
    if raw.len() > MAX_REQUEST {
        return Err("invalid_scope");
    }
    let mut enrollment: Enrollment = serde_json::from_slice(&raw).map_err(|_| "invalid_scope")?;
    if enrollment.version != 1 {
        return Err("invalid_scope");
    }
    let mut roots = Vec::new();
    for (harness, values) in [
        ("claude", &mut enrollment.roots.claude),
        ("codex", &mut enrollment.roots.codex),
        ("omp", &mut enrollment.roots.omp),
    ] {
        values.sort();
        values.dedup();
        for value in values {
            let root = absolute_path(value)?;
            check_components(&root, true).map_err(|_| "invalid_scope")?;
            if root.exists() && !root.is_dir() {
                return Err("invalid_scope");
            }
            roots.push((harness.to_owned(), root));
            if roots.len() > MAX_ROOTS {
                return Err("invalid_scope");
            }
        }
    }
    let canonical = serde_json::to_vec(&enrollment).map_err(|_| "invalid_scope")?;
    Ok(Scope {
        path,
        id: digest(&canonical),
        roots,
    })
}

fn verify_scope(scope: &Scope) -> Result<()> {
    if load_scope(&scope.path)?.id != scope.id {
        return Err("invalid_scope");
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| "source_unavailable")?;
    Ok(hex::encode(bytes))
}

fn open_inventory(corpus: &Path, writable: bool) -> Result<Connection> {
    check_components(corpus, true)?;
    let directory = corpus.join("local-source");
    let database = directory.join("inventory.sqlite3");
    if writable {
        // Only refresh may create files. Never mutate existing permissions on the caller's behalf.
        for path in [corpus, directory.as_path()] {
            if !path.exists() {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(path)
                    .map_err(|_| "source_unavailable")?;
            }
            private(path, true)?;
        }
    } else if !database.exists() {
        return Err("coverage_unavailable");
    }
    check_components(&database, true)?;
    private(corpus, true)?;
    private(&directory, true)?;
    let mut created = false;
    if writable && !database.exists() {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&database)
            .map_err(|_| "source_unavailable")?;
        created = true;
    }
    private(&database, false)?;
    for suffix in ["-journal", "-wal", "-shm"] {
        let sidecar = directory.join(format!("inventory.sqlite3{suffix}"));
        check_components(&sidecar, true)?;
        if sidecar.exists() {
            private(&sidecar, false)?;
        }
    }
    let flags = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let connection = Connection::open_with_flags(&database, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|_| "source_unavailable")?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| "source_unavailable")?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|_| "source_unavailable")?;
    if created {
        let instance = random_id()?;
        connection.execute_batch("PRAGMA journal_mode=DELETE; BEGIN IMMEDIATE;
            CREATE TABLE metadata (instance TEXT NOT NULL);
            CREATE TABLE snapshots (sequence INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, scope TEXT NOT NULL);
            CREATE TABLE sources (snapshot TEXT NOT NULL, id TEXT NOT NULL, harness TEXT NOT NULL, locator TEXT NOT NULL,
                revision TEXT NOT NULL, state TEXT NOT NULL, PRIMARY KEY(snapshot,id));
            CREATE INDEX source_lookup ON sources(id,snapshot);
            PRAGMA user_version=1;") .map_err(|_| "source_unavailable")?;
        connection
            .execute("INSERT INTO metadata(instance) VALUES (?)", [instance])
            .map_err(|_| "source_unavailable")?;
        connection.execute_batch("COMMIT").map_err(|_| "source_unavailable")?;
    } else if version != INVENTORY_VERSION {
        return Err("unsupported_protocol");
    }
    for query in [
        "SELECT instance FROM metadata LIMIT 0",
        "SELECT sequence,id,scope FROM snapshots LIMIT 0",
        "SELECT snapshot,id,harness,locator,revision,state FROM sources LIMIT 0",
    ] {
        connection.prepare(query).map_err(|_| "unsupported_protocol")?;
    }
    let (instances, instance): (i64, Option<String>) = connection
        .query_row("SELECT count(*),min(instance) FROM metadata", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|_| "unsupported_protocol")?;
    if instances != 1 || !instance.as_deref().is_some_and(is_digest) {
        return Err("unsupported_protocol");
    }
    if !writable {
        connection
            .execute_batch("PRAGMA query_only=ON")
            .map_err(|_| "source_unavailable")?;
    }
    Ok(connection)
}

fn metadata_identity(metadata: &fs::Metadata) -> [u64; 7] {
    [
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
        metadata.ctime() as u64,
        metadata.ctime_nsec() as u64,
    ]
}

fn source_path(scope: &Scope, harness: &str, locator: &str) -> Result<PathBuf> {
    let path = absolute_path(locator)?;
    if !scope
        .roots
        .iter()
        .any(|(h, root)| h == harness && path.starts_with(root) && path != *root)
    {
        return Err("invalid_scope");
    }
    check_components(&path, false)?;
    Ok(path)
}

/// Hash every byte in constant memory and reject mutations during the fingerprint itself.
fn pinned_source(scope: &Scope, harness: &str, locator: &str) -> Result<(String, File)> {
    let path = source_path(scope, harness, locator)?;
    let mut file = File::open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "source_missing"
        } else {
            "source_unavailable"
        }
    })?;
    let metadata = file.metadata().map_err(|_| "source_unavailable")?;
    if !metadata.is_file() {
        return Err("source_unavailable");
    }
    let identity = metadata_identity(&metadata);
    let mut hash = Sha256::new();
    for number in identity {
        hash.update(number.to_be_bytes());
    }
    let mut buffer = [0u8; 64 * 1024];
    let mut count = 0u64;
    loop {
        let length = file.read(&mut buffer).map_err(|_| "source_unavailable")?;
        if length == 0 {
            break;
        }
        count = count.checked_add(length as u64).ok_or("oversized_source")?;
        if count > identity[2] {
            return Err("source_changed");
        }
        hash.update(&buffer[..length]);
    }
    let after = file.metadata().map_err(|_| "source_unavailable")?;
    source_path(scope, harness, locator)?;
    let current = fs::metadata(&path).map_err(|_| "source_missing")?;
    if count != identity[2] || metadata_identity(&after) != identity || metadata_identity(&current) != identity {
        return Err("source_changed");
    }
    file.seek(SeekFrom::Start(0)).map_err(|_| "source_unavailable")?;
    Ok((hex::encode(hash.finalize()), file))
}

fn revision(scope: &Scope, harness: &str, locator: &str) -> Result<String> {
    pinned_source(scope, harness, locator).map(|(revision, _)| revision)
}

fn discover(scope: &Scope) -> Result<BTreeMap<String, Source>> {
    let mut sources = BTreeMap::new();
    let mut visited = BTreeSet::new();
    let mut entries = 0usize;
    for (harness, root) in &scope.roots {
        check_components(root, true)?;
        if !root.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(root).follow_links(false).max_open(32) {
            entries += 1;
            if entries > MAX_ENTRIES {
                return Err("oversized_source");
            }
            let entry = entry.map_err(|_| "source_unavailable")?;
            if entry.file_type().is_symlink() {
                return Err("invalid_scope");
            }
            if !entry.file_type().is_file() || entry.path().extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let locator = entry.path().to_str().ok_or("invalid_scope")?.to_owned();
            if !visited.insert((harness.clone(), locator.clone())) {
                continue;
            }
            if sources.len() >= MAX_SOURCES {
                return Err("oversized_source");
            }
            let id = digest(format!("{harness}\0{locator}").as_bytes());
            let revision = revision(scope, harness, &locator)?;
            sources.insert(
                id.clone(),
                Source {
                    id,
                    harness: harness.clone(),
                    locator,
                    revision,
                    state: "present".to_owned(),
                },
            );
        }
    }
    Ok(sources)
}

fn latest_snapshot(connection: &Connection, scope: &str) -> Result<Option<String>> {
    connection
        .query_row(
            "SELECT id FROM snapshots WHERE scope=? ORDER BY sequence DESC LIMIT 1",
            [scope],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "source_unavailable")
}

fn refresh(corpus: &Path, scope: &Scope) -> Result<Value> {
    let mut sources = discover(scope)?;
    verify_scope(scope)?;
    let mut connection = open_inventory(corpus, true)?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|_| "source_unavailable")?;
    if let Some(previous) = latest_snapshot(&transaction, &scope.id)? {
        let mut statement = transaction
            .prepare("SELECT id,harness,locator,revision FROM sources WHERE snapshot=?")
            .map_err(|_| "source_unavailable")?;
        let rows = statement
            .query_map([previous], |row| {
                Ok(Source {
                    id: row.get(0)?,
                    harness: row.get(1)?,
                    locator: row.get(2)?,
                    revision: row.get(3)?,
                    state: "missing".to_owned(),
                })
            })
            .map_err(|_| "source_unavailable")?;
        for row in rows {
            let source = row.map_err(|_| "source_unavailable")?;
            sources.entry(source.id.clone()).or_insert(source);
            if sources.len() > MAX_SOURCES {
                return Err("oversized_source");
            }
        }
    }
    let snapshot = random_id()?;
    transaction
        .execute(
            "INSERT INTO snapshots(id,scope) VALUES (?,?)",
            params![snapshot, scope.id],
        )
        .map_err(|_| "source_unavailable")?;
    {
        let mut statement = transaction
            .prepare("INSERT INTO sources(snapshot,id,harness,locator,revision,state) VALUES (?,?,?,?,?,?)")
            .map_err(|_| "source_unavailable")?;
        for source in sources.values() {
            statement
                .execute(params![
                    snapshot,
                    source.id,
                    source.harness,
                    source.locator,
                    source.revision,
                    source.state
                ])
                .map_err(|_| "source_unavailable")?;
        }
    }
    // Explicit bounded cursor retention: old pages fail rather than silently jump forward.
    transaction.execute_batch("DELETE FROM sources WHERE snapshot IN (SELECT id FROM snapshots ORDER BY sequence DESC LIMIT -1 OFFSET 32);
        DELETE FROM snapshots WHERE sequence NOT IN (SELECT sequence FROM snapshots ORDER BY sequence DESC LIMIT 32);")
        .map_err(|_| "source_unavailable")?;
    verify_scope(scope)?;
    transaction.commit().map_err(|_| "source_unavailable")?;
    Ok(json!({"snapshot": snapshot, "scope_id": scope.id, "sources": sources.len()}))
}

fn cursor_secret(connection: &Connection) -> Result<String> {
    connection
        .query_row("SELECT instance FROM metadata", [], |row| row.get(0))
        .map_err(|_| "source_unavailable")
}

fn cursor_signature(secret: &str, payload: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(secret.as_bytes());
    hash.update(payload);
    hash.update(secret.as_bytes());
    hex::encode(hash.finalize())
}

fn encode_cursor(secret: &str, cursor: &Cursor) -> Result<String> {
    let payload = serde_json::to_vec(cursor).map_err(|_| "invalid_cursor")?;
    Ok(format!(
        "{}.{}",
        hex::encode(&payload),
        cursor_signature(secret, &payload)
    ))
}

fn decode_cursor(secret: &str, value: &str) -> Result<Cursor> {
    if value.len() > 2048 {
        return Err("invalid_cursor");
    }
    let (payload, signature) = value.split_once('.').ok_or("invalid_cursor")?;
    let payload = hex::decode(payload).map_err(|_| "invalid_cursor")?;
    if cursor_signature(secret, &payload) != signature {
        return Err("invalid_cursor");
    }
    serde_json::from_slice(&payload).map_err(|_| "invalid_cursor")
}

fn enumerate(connection: &Connection, scope: &Scope, request: &Request) -> Result<Value> {
    // A read transaction holds one inventory view while refresh commits on another process.
    connection.execute_batch("BEGIN").map_err(|_| "source_unavailable")?;
    let secret = cursor_secret(connection)?;
    let cursor = if let Some(value) = &request.cursor {
        let cursor = decode_cursor(&secret, value)?;
        if cursor.scope != scope.id || cursor.harness != request.harness || cursor.offset > MAX_SOURCES {
            return Err("invalid_cursor");
        }
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM snapshots WHERE id=? AND scope=?)",
                params![cursor.snapshot, scope.id],
                |row| row.get(0),
            )
            .map_err(|_| "source_unavailable")?;
        if !exists {
            return Err("invalid_cursor");
        }
        cursor
    } else {
        Cursor {
            snapshot: latest_snapshot(connection, &scope.id)?.ok_or("coverage_unavailable")?,
            scope: scope.id.clone(),
            harness: request.harness.clone(),
            offset: 0,
        }
    };
    let limit = request.limit.unwrap_or(64);
    let mut statement = connection.prepare("SELECT id,harness,revision,state FROM sources WHERE snapshot=? AND (? IS NULL OR harness=?) ORDER BY id LIMIT ? OFFSET ?")
        .map_err(|_| "source_unavailable")?;
    let rows = statement.query_map(params![cursor.snapshot, cursor.harness, cursor.harness, (limit + 1) as i64, cursor.offset as i64], |row| {
        Ok(json!({"id": row.get::<_, String>(0)?, "harness": row.get::<_, String>(1)?, "revision": row.get::<_, String>(2)?, "state": row.get::<_, String>(3)?}))
    }).map_err(|_| "source_unavailable")?;
    let mut sources = rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| "source_unavailable")?;
    let next_cursor = if sources.len() > limit {
        sources.pop();
        Some(encode_cursor(
            &secret,
            &Cursor {
                snapshot: cursor.snapshot.clone(),
                scope: scope.id.clone(),
                harness: cursor.harness,
                offset: cursor.offset + limit,
            },
        )?)
    } else {
        None
    };
    verify_scope(scope)?;
    Ok(
        json!({"snapshot": cursor.snapshot, "scope_id": scope.id, "sources": sources, "next_cursor": next_cursor, "coverage": "current"}),
    )
}

fn normalization_error(code: &str) -> &'static str {
    [
        "invalid_request",
        "unsupported_client",
        "source_missing",
        "source_unavailable",
        "source_changed",
        "source_capacity",
        "oversized_source",
        "oversized_response",
        "incomplete_write",
        "incomplete_turn",
        "deferred_future",
        "malformed_record",
        "unsupported_version",
        "missing_session_header",
        "unknown_content_schema",
        "unknown_session_schema",
        "unknown_provenance_schema",
        "unknown_project_schema",
        "unknown_branch_lineage",
    ]
    .into_iter()
    .find(|candidate| *candidate == code)
    .unwrap_or("source_unavailable")
}

fn source_turns(connection: &Connection, scope: &Scope, request: &Request) -> Result<Value> {
    connection.execute_batch("BEGIN").map_err(|_| "source_unavailable")?;
    let snapshot = latest_snapshot(connection, &scope.id)?.ok_or("coverage_unavailable")?;
    let source = connection
        .query_row(
            "SELECT id,harness,locator,revision,state FROM sources WHERE snapshot=? AND id=?",
            params![snapshot, request.source_id],
            |row| {
                Ok(Source {
                    id: row.get(0)?,
                    harness: row.get(1)?,
                    locator: row.get(2)?,
                    revision: row.get(3)?,
                    state: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|_| "source_unavailable")?
        .ok_or("source_missing")?;
    connection.execute_batch("COMMIT").map_err(|_| "source_unavailable")?;
    if source.state != "present" {
        return Err("source_missing");
    }
    let wanted = request.revision.as_deref().ok_or("invalid_request")?;
    let (before, original) = pinned_source(scope, &source.harness, &source.locator)?;
    if before != wanted {
        return Err("source_changed");
    }
    let identity = metadata_identity(&original.metadata().map_err(|_| "source_unavailable")?);
    // Reopen the held descriptor, never a mutable pathname: swapping a symlink away
    // and back while parsing must not smuggle another inode's content into a response.
    let path = PathBuf::from(format!("/dev/fd/{}", original.as_raw_fd()));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "source_unavailable")?
        .as_secs_f64();
    let include_text = request.op == "read";
    let offset = if include_text {
        request.ordinal.ok_or("invalid_request")?
    } else {
        request.offset.unwrap_or(0)
    };
    let limit = if include_text { 1 } else { request.limit.unwrap_or(64) };
    let parsed = normalize::parse_page(&path, &source.harness, now, include_text, offset, limit);
    // Check even when normalization failed, so a concurrent rotation cannot be misclassified.
    if revision(scope, &source.harness, &source.locator)? != wanted {
        return Err("source_changed");
    }
    if metadata_identity(&original.metadata().map_err(|_| "source_unavailable")?) != identity {
        return Err("source_changed");
    }
    verify_scope(scope)?;
    let mut parsed = parsed.map_err(|error| normalization_error(&error))?;
    if include_text {
        // Known stream tails do not invalidate an earlier complete turn. Unknown
        // schemas and resource failures found beyond this page still fail closed.
        let status = parsed
            .get("status")
            .and_then(Value::as_str)
            .ok_or("source_unavailable")?;
        if !matches!(
            status,
            "complete"
                | "incomplete_turn"
                | "incomplete_write"
                | "deferred_future"
                | "malformed_record"
                | "oversized_source"
        ) {
            return Err(normalization_error(status));
        }
        let turns = parsed
            .get_mut("turns")
            .and_then(Value::as_array_mut)
            .ok_or("source_unavailable")?;
        let position = turns
            .iter()
            .position(|turn| turn.get("ordinal").and_then(Value::as_u64) == Some(offset as u64))
            .ok_or("source_unavailable")?;
        let turn = turns.swap_remove(position);
        if turn.get("bytes").and_then(Value::as_u64).ok_or("source_unavailable")? > MAX_TURN {
            return Err("oversized_source");
        }
        if !turn.get("items").and_then(Value::as_array).is_some_and(|items| {
            items
                .iter()
                .all(|item| item.get("text").and_then(Value::as_str).is_some())
        }) {
            return Err("source_unavailable");
        }
        return Ok(json!({"turn": turn}));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _temp: tempfile::TempDir,
        corpus: PathBuf,
        scope: PathBuf,
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("sessions");
            fs::create_dir(&root).unwrap();
            let scope = temp.path().join("scope.json");
            let enrollment = json!({"version":1,"roots":{"claude":[],"codex":[],"omp":[root]}});
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&scope)
                .unwrap();
            file.write_all(enrollment.to_string().as_bytes()).unwrap();
            let corpus = temp.path().join("corpus");
            Self {
                _temp: temp,
                corpus,
                scope,
                root,
            }
        }
        fn request(&self, op: &str, fields: Value) -> Result<Value> {
            let mut request = json!({"protocol":1,"op":op,"corpus":self.corpus,"scope":self.scope});
            request
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            execute(&serde_json::to_vec(&request).unwrap())
        }
    }

    #[test]
    fn readonly_operations_do_not_create_inventory_and_unknown_fields_fail_closed() {
        let fixture = Fixture::new();
        assert!(fixture.request("capabilities", json!({})).is_ok());
        assert_eq!(fixture.request("enumerate", json!({})), Err("coverage_unavailable"));
        assert!(!fixture.corpus.exists());
        assert_eq!(
            fixture.request("capabilities", json!({"limit":1})),
            Err("invalid_request")
        );
        assert_eq!(
            fixture.request("refresh", json!({"remote":"hf://private"})),
            Err("invalid_request")
        );
        assert_eq!(fixture.request("enumerate", json!({"limit":0})), Err("invalid_request"));
    }

    #[test]
    fn snapshots_include_unsearchable_sources_and_keep_pagination_stable() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("empty.jsonl"), b"").unwrap();
        fs::write(fixture.root.join("malformed.jsonl"), b"not json\n").unwrap();
        fixture.request("refresh", json!({})).unwrap();
        let first = fixture.request("enumerate", json!({"limit":1})).unwrap();
        let cursor = first["next_cursor"].clone();
        assert!(cursor.is_string());
        fs::write(fixture.root.join("late.jsonl"), b"{}\n").unwrap();
        fs::remove_file(fixture.root.join("empty.jsonl")).unwrap();
        fixture.request("refresh", json!({})).unwrap();
        let second = fixture
            .request("enumerate", json!({"limit":1,"cursor":cursor}))
            .unwrap();
        assert_eq!(first["snapshot"], second["snapshot"]);
        assert_eq!(second["next_cursor"], Value::Null);
        assert_ne!(first["sources"][0]["id"], second["sources"][0]["id"]);
        let current = fixture.request("enumerate", json!({})).unwrap();
        assert_ne!(current["snapshot"], first["snapshot"]);
        let sources = current["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 3);
        assert_eq!(sources.iter().filter(|source| source["state"] == "missing").count(), 1);
        assert!(!current.to_string().contains("sessions"));
        assert!(!current.to_string().contains("not json"));
    }

    #[test]
    fn revision_rejects_append_rotation_and_missing_sources() {
        let fixture = Fixture::new();
        let path = fixture.root.join("stream.jsonl");
        fs::write(&path, b"{}\n").unwrap();
        fixture.request("refresh", json!({})).unwrap();
        let inventory = fixture.request("enumerate", json!({})).unwrap();
        let source = &inventory["sources"][0];
        let fields = json!({"source_id":source["id"],"revision":source["revision"]});
        fs::write(&path, b"{}\n{}\n").unwrap();
        assert_eq!(fixture.request("turns", fields.clone()), Err("source_changed"));
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"{}\n").unwrap();
        assert_eq!(fixture.request("turns", fields.clone()), Err("source_changed"));
        fs::remove_file(&path).unwrap();
        assert_eq!(fixture.request("turns", fields), Err("source_missing"));
    }

    #[test]
    fn scope_changes_and_inventory_rebuild_invalidate_cursors() {
        let fixture = Fixture::new();
        for name in ["a.jsonl", "b.jsonl"] {
            fs::write(fixture.root.join(name), b"").unwrap();
        }
        fixture.request("refresh", json!({})).unwrap();
        let first = fixture.request("enumerate", json!({"limit":1})).unwrap();
        let cursor = json!({"cursor":first["next_cursor"]});
        let db = fixture.corpus.join("local-source/inventory.sqlite3");
        fs::remove_file(&db).unwrap();
        fixture.request("refresh", json!({})).unwrap();
        assert_eq!(fixture.request("enumerate", cursor.clone()), Err("invalid_cursor"));
        let next = fixture.request("enumerate", json!({"limit":1})).unwrap();
        fs::write(
            &fixture.scope,
            b"{\"version\":1,\"roots\":{\"claude\":[],\"codex\":[],\"omp\":[]}}",
        )
        .unwrap();
        assert_eq!(
            fixture.request("enumerate", json!({"cursor":next["next_cursor"]})),
            Err("invalid_cursor")
        );
        assert_eq!(fixture.request("enumerate", json!({})), Err("coverage_unavailable"));
    }

    #[test]
    fn metadata_and_full_read_roundtrip_without_inventory_writes() {
        let fixture = Fixture::new();
        let records = [
            json!({"type":"session","id":"s","version":3,"cwd":"/synthetic"}),
            json!({"type":"message","id":"u","parentId":null,"message":{
                "role":"user","attribution":"user","timestamp":1000,
                "content":[{"type":"text","text":"Question\nwith Unicode: λ"}]}}),
            json!({"type":"message","id":"a","parentId":"u","message":{
                "role":"assistant","timestamp":3000,"completedAt":4000,"stopReason":"stop",
                "content":[{"type":"text","text":"Answer"}]}}),
            json!({"type":"message","id":"u2","parentId":"a","message":{
                "role":"user","attribution":"user","timestamp":5000,
                "content":[{"type":"text","text":"Another question"}]}}),
            json!({"type":"message","id":"a2","parentId":"u2","message":{
                "role":"assistant","timestamp":6000,"completedAt":7000,"stopReason":"stop",
                "content":[{"type":"text","text":"Another answer"}]}}),
        ];
        let raw: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(fixture.root.join("complete.jsonl"), raw).unwrap();
        fixture.request("refresh", json!({})).unwrap();
        let database = fixture.corpus.join("local-source/inventory.sqlite3");
        let before = metadata_identity(&fs::metadata(&database).unwrap());
        let inventory = fixture.request("enumerate", json!({})).unwrap();
        let source = &inventory["sources"][0];
        let metadata = fixture
            .request(
                "turns",
                json!({
                    "source_id":source["id"],"revision":source["revision"],"limit":1
                }),
            )
            .unwrap();
        assert_eq!(metadata["status"], "complete");
        assert_eq!(metadata["turns"].as_array().unwrap().len(), 1);
        assert_eq!(metadata["turns"][0]["ordinal"], 0);
        assert_eq!(metadata["next_offset"], 1);
        let second = fixture
            .request(
                "turns",
                json!({
                    "source_id":source["id"],"revision":source["revision"],
                    "offset":metadata["next_offset"],"limit":1
                }),
            )
            .unwrap();
        assert_eq!(second["status"], "complete");
        assert_eq!(second["turns"].as_array().unwrap().len(), 1);
        assert_eq!(second["turns"][0]["ordinal"], 1);
        assert_eq!(second["next_offset"], Value::Null);
        assert_ne!(metadata["turns"][0]["id"], second["turns"][0]["id"]);
        for (page, question, answer) in [
            (&metadata, "Question\nwith Unicode: λ", "Answer"),
            (&second, "Another question", "Another answer"),
        ] {
            let turn = &page["turns"][0];
            assert!(turn["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item.get("text").is_none()));
            let full = fixture
                .request(
                    "read",
                    json!({
                        "source_id":source["id"],"revision":source["revision"],"ordinal":turn["ordinal"]
                    }),
                )
                .unwrap();
            assert_eq!(full["turn"]["items"][0]["text"], question);
            assert_eq!(full["turn"]["items"][1]["text"], answer);
            let mut identity = full["turn"].clone();
            for item in identity["items"].as_array_mut().unwrap() {
                item.as_object_mut().unwrap().remove("text");
            }
            assert_eq!(&identity, turn);
        }
        assert_eq!(metadata_identity(&fs::metadata(database).unwrap()), before);
    }

    #[test]
    fn complete_prefix_read_allows_pending_tail_but_rejects_later_unknown_schema() {
        let fixture = Fixture::new();
        let path = fixture.root.join("active.jsonl");
        let records = [
            json!({"type":"session","id":"s","version":3,"cwd":"/synthetic"}),
            json!({"type":"message","id":"u","parentId":null,"message":{
                "role":"user","attribution":"user","timestamp":1000,
                "content":[{"type":"text","text":"Question"}]}}),
            json!({"type":"message","id":"a","parentId":"u","message":{
                "role":"assistant","timestamp":2000,"completedAt":3000,"stopReason":"stop",
                "content":[{"type":"text","text":"Answer"}]}}),
            json!({"type":"message","id":"pending","parentId":"a","message":{
                "role":"user","attribution":"user","timestamp":4000,
                "content":[{"type":"text","text":"Unanswered"}]}}),
        ];
        let mut raw: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(&path, &raw).unwrap();
        fixture.request("refresh", json!({})).unwrap();
        let inventory = fixture.request("enumerate", json!({})).unwrap();
        let source = &inventory["sources"][0];
        let metadata = fixture
            .request(
                "turns",
                json!({
                    "source_id":source["id"],"revision":source["revision"],"limit":1
                }),
            )
            .unwrap();
        assert_eq!(metadata["status"], "incomplete_turn");
        assert_eq!(metadata["pending_turns"], 1);
        let complete = fixture
            .request(
                "read",
                json!({
                    "source_id":source["id"],"revision":source["revision"],"ordinal":0
                }),
            )
            .unwrap();
        assert_eq!(complete["turn"]["id"], metadata["turns"][0]["id"]);
        assert_eq!(complete["turn"]["items"][1]["text"], "Answer");
        assert!(fixture
            .request(
                "read",
                json!({
                    "source_id":source["id"],"revision":source["revision"],"ordinal":1
                })
            )
            .is_err());
        raw.push_str("{\"type\":\"future_unknown_record\"}\n");
        fs::write(&path, raw).unwrap();
        fixture.request("refresh", json!({})).unwrap();
        let inventory = fixture.request("enumerate", json!({})).unwrap();
        let source = &inventory["sources"][0];
        let metadata = fixture
            .request(
                "turns",
                json!({
                    "source_id":source["id"],"revision":source["revision"],"limit":1
                }),
            )
            .unwrap();
        assert_eq!(metadata["status"], "unknown_content_schema");
        assert_eq!(metadata["turns"][0]["id"], complete["turn"]["id"]);
        assert_eq!(
            fixture.request(
                "read",
                json!({
                    "source_id":source["id"],"revision":source["revision"],"ordinal":0
                })
            ),
            Err("unknown_content_schema")
        );
    }

    #[test]
    fn symlink_sources_and_incompatible_inventory_fail_closed() {
        let fixture = Fixture::new();
        let outside = fixture._temp.path().join("outside.jsonl");
        fs::write(&outside, b"private").unwrap();
        let link = fixture.root.join("escape.jsonl");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(fixture.request("refresh", json!({})), Err("invalid_scope"));
        assert!(!fixture.corpus.exists());
        fs::remove_file(link).unwrap();
        fixture.request("refresh", json!({})).unwrap();
        let db = Connection::open(fixture.corpus.join("local-source/inventory.sqlite3")).unwrap();
        db.pragma_update(None, "user_version", 2).unwrap();
        drop(db);
        assert_eq!(fixture.request("enumerate", json!({})), Err("unsupported_protocol"));
    }
}
