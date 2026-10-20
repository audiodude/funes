//! Trace sources: where the indexer reads agent sessions from. A [`TraceSource`] enumerates the
//! discrete artifacts it indexes ([`Unit`]s — typically files) and parses one on demand into turns.
//! The indexer ([`crate::commands::index`]) drives any source through one generic loop, so adding a new
//! transcript format is: implement [`TraceSource`] and add a branch to [`open`].
//!
//! A unit is both the incremental-tracking granule (skipped when its [`Unit::signature`] still
//! matches `state.json`) and the single-append granule (all of a unit's turns are written in one
//! commit). A turns file is a unit.

use super::funes_jsonl;
use super::jsonl;
use super::omp;
use super::Turn;

use anyhow::{bail, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// One artifact a source indexes as a unit. `key` identifies and locates it: a turns file's path.
/// A `Some` `signature` is a cheap change-stamp: the unit is skipped when it still matches what
/// was recorded, and recorded after a successful index. `None` means "always read, never recorded",
/// for a file whose idempotency comes from chunk-id dedup, not file stats.
pub struct Unit {
    pub key: String,
    pub signature: Option<String>,
}

/// A source of agent-session transcripts. `units()` is cheap (enumerate + stat, no parsing);
/// `read` parses one unit's turns and is called only for units that aren't skipped.
pub trait TraceSource {
    /// One-line description of what's being indexed (the scan banner's source-kind part).
    fn describe(&self) -> String;

    /// The units to consider, in deterministic order.
    fn units(&self) -> Result<Vec<Unit>>;

    /// Parse one unit into turns (each [`Turn`] already carries its `session_id` and `workdir`).
    fn read(&self, unit: &Unit) -> Result<Vec<Turn>>;

    /// Parse a unit for a dry run, without publishing source-specific metadata.
    fn read_for_check(&self, unit: &Unit) -> Result<Vec<Turn>> {
        self.read(unit)
    }

    fn is_omp(&self) -> bool {
        false
    }

    fn omp_coverage(&self, _unit: &Unit) -> Option<omp::Coverage> {
        None
    }

    /// Dependencies can change without changing the unit's own shared stat signature.
    fn dependencies_current(&self, _unit: &Unit) -> bool {
        true
    }

    /// Whether a `read` error aborts the whole index. Best-effort sources (a directory, where one
    /// unreadable file shouldn't sink the run) return `false`; a single-artifact source (a file
    /// named on its own) returns `true`, so a corrupt file is a hard failure rather than a silent
    /// skip.
    fn fatal_on_read_error(&self) -> bool {
        false
    }

    /// Whether `key` names one of this store's units.
    fn owns(&self, _key: &str) -> bool {
        false
    }

    /// Keys of every unit this store holds, uncapped by the `limit` that trims `units`.
    fn unit_keys(&self) -> Result<Vec<String>>;
}

/// Pick the source for `path`: a `.funes.jsonl` file, or a directory holding them, is a turns
/// store. Anything else is refused: funes reads no agent's own transcripts, its integration converts
/// them. `limit` caps how many sessions are read (`None` = all), used to bound a benchmark's build
/// time.
pub fn open(path: &Path, limit: Option<usize>) -> Result<Box<dyn TraceSource>> {
    // Listed once, here; the source keeps the listing. A directory with nothing to read is an
    // empty turns store: a spool no integration has written into yet indexes as a no-op instead of
    // being refused. A file is read only when it is a turns file.
    let listing = jsonl::iter_jsonl_files(path);
    if (listing.is_empty() && path.is_dir()) || listing.iter().any(|p| funes_jsonl::is_turns_file(p)) {
        return Ok(Box::new(funes_jsonl::FunesJsonl::new(path, listing, limit)));
    }
    bail!(
        "{} holds no turns files — an agent's own transcripts are converted by its integration \
         (`funes add <agent>`); OMP requires explicit `--harness omp`",
        path.display()
    )
}

/// Open explicitly enrolled native OMP transcripts, never as part of spool discovery.
pub fn open_omp(path: &Path, limit: Option<usize>) -> Result<Box<dyn TraceSource>> {
    anyhow::ensure!(path.exists(), "no such path: {}", path.display());
    let listing = jsonl::iter_jsonl_files(path);
    anyhow::ensure!(
        !listing.iter().any(|p| funes_jsonl::is_turns_file(p)),
        "--harness omp requires native transcripts, not turns files"
    );
    Ok(Box::new(OmpTree {
        root: path.to_path_buf(),
        limit,
        listing,
        coverage: RefCell::new(HashMap::new()),
    }))
}

struct OmpTree {
    root: PathBuf,
    limit: Option<usize>,
    listing: Vec<PathBuf>,
    coverage: RefCell<HashMap<String, omp::Coverage>>,
}

impl TraceSource for OmpTree {
    fn describe(&self) -> String {
        format!("scanning omp transcripts under {}", self.root.display())
    }

    fn units(&self) -> Result<Vec<Unit>> {
        let mut files = self.listing.clone();
        files.sort_by_cached_key(|p| {
            std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH))
        });
        if let Some(n) = self.limit {
            files.truncate(n);
        }
        files.sort_by_key(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("agent-"))
        });
        Ok(files
            .into_iter()
            .map(|p| Unit {
                signature: omp::file_signature(&p).ok(),
                key: p.to_string_lossy().into_owned(),
            })
            .collect())
    }

    fn owns(&self, key: &str) -> bool {
        Path::new(key).starts_with(&self.root)
    }

    fn unit_keys(&self) -> Result<Vec<String>> {
        Ok(self.listing.iter().map(|p| p.to_string_lossy().into_owned()).collect())
    }

    fn is_omp(&self) -> bool {
        true
    }

    fn fatal_on_read_error(&self) -> bool {
        true
    }

    fn omp_coverage(&self, unit: &Unit) -> Option<omp::Coverage> {
        self.coverage.borrow().get(&unit.key).cloned()
    }

    fn dependencies_current(&self, unit: &Unit) -> bool {
        omp::dependencies_current(Path::new(&unit.key))
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let path = Path::new(&unit.key);
        let (turns, coverage) = omp::read(path, &omp_workdir(path))?;
        self.coverage.borrow_mut().insert(unit.key.clone(), coverage);
        Ok(turns)
    }

    fn read_for_check(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let path = Path::new(&unit.key);
        Ok(omp::check(path, &omp_workdir(path))?)
    }
}

fn omp_workdir(path: &Path) -> String {
    let mut parts = path.iter();
    while let Some(part) = parts.next() {
        if part == "projects" {
            if let Some(project) = parts.next().and_then(|p| p.to_str()) {
                return project.to_string();
            }
        }
    }
    path.parent()
        .and_then(Path::file_name)
        .and_then(|p| p.to_str())
        .unwrap_or("")
        .to_string()
}

/// A file's incremental change-stamp, or `None` if it can't be stat'd.
pub(crate) fn file_sig(p: &Path) -> Option<String> {
    let md = std::fs::metadata(p).ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    // Length, mtime to the nanosecond, inode: a file replaced within the second at the same
    // length still changes one of the last two.
    Some(format!(
        "{}:{}.{:09}:{}",
        md.len(),
        mtime.as_secs(),
        mtime.subsec_nanos(),
        md.ino()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn file_sig_is_len_mtime_to_the_nanosecond_and_inode() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"hello").unwrap();
        f.flush().unwrap();
        let sig = file_sig(f.path()).expect("stat-able file has a signature");
        let parts: Vec<&str> = sig.split(':').collect();
        assert_eq!(parts.len(), 3, "{sig}");
        assert_eq!(parts[0], "5");
        let (secs, nanos) = parts[1].split_once('.').expect("mtime is secs.nanos");
        assert!(
            secs.parse::<u64>().is_ok() && nanos.len() == 9 && nanos.parse::<u32>().is_ok(),
            "{sig}"
        );
        assert!(parts[2].parse::<u64>().is_ok(), "{sig}");

        // Replaced in place within the second, at the same length: a new inode tells them apart.
        let replacement = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(replacement.path(), b"world").unwrap();
        std::fs::rename(replacement.path(), f.path()).unwrap();
        assert_ne!(file_sig(f.path()).unwrap(), sig);
    }

    #[test]
    fn file_sig_is_none_for_missing_file() {
        assert!(file_sig(Path::new("/no/such/file")).is_none());
    }

    #[test]
    fn a_file_that_is_no_turns_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("traces.parquet");
        std::fs::write(&file, b"PAR1").unwrap();
        let err = open(&file, None).err().expect("refused");
        assert!(err.to_string().contains("holds no turns files"), "{err}");
    }

    #[test]
    fn open_routes_turns_files_and_a_directory_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("thread.funes.jsonl");
        std::fs::write(&file, b"").unwrap();
        assert!(open(&file, None).unwrap().describe().contains("funes JSONL"));
        assert!(open(dir.path(), None).unwrap().describe().contains("funes JSONL"));
        let nested = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(nested.path().join("a/b")).unwrap();
        std::fs::write(nested.path().join("a/b/t.funes.jsonl"), b"").unwrap();
        assert!(open(nested.path(), None).unwrap().describe().contains("funes JSONL"));
    }

    #[test]
    fn a_transcript_that_is_no_turns_file_is_refused_and_an_empty_root_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.jsonl"), b"{\"type\":\"session_meta\"}\n").unwrap();
        assert!(open(dir.path(), None).is_err());
        let empty = tempfile::tempdir().unwrap();
        assert!(open(empty.path(), None).unwrap().units().unwrap().is_empty());
    }
}
