//! Native OMP coverage stays conservative across bounded passes and rows-first spool indexing.
use funes::memory::dataset;
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

fn index(home: &Path, source: &Path, extra: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_funes"))
        .arg("index")
        .arg(source)
        .args(["--harness", "omp", "--yes"])
        .args(extra)
        .env("FUNES_HOME", home)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn receipt(home: &Path, source: &Path) -> Value {
    let receipts: Value = serde_json::from_slice(&std::fs::read(home.join("omp-coverage.json")).unwrap()).unwrap();
    receipts["units"][source.to_str().unwrap()].clone()
}

#[tokio::test]
async fn bounded_native_passes_publish_only_committed_coverage_and_resume_without_duplicates() {
    let home = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("native.jsonl");
    let rows = [
        json!({"type":"session","version":3,"id":"01990000-0000-7000-8000-000000000001","cwd":"/synthetic"}),
        json!({"type":"message","id":"question","parentId":null,"message":{"role":"user","attribution":"user","timestamp":1000,"content":"Why retain the narwhal archive?"}}),
        json!({"type":"message","id":"answer","parentId":"question","message":{"role":"assistant","timestamp":2000,"completedAt":3000,"stopReason":"stop","content":[{"type":"text","text":"Retain the narwhal archive for provenance, not independent verification."}]}}),
    ];
    let text = rows.iter().map(|row| format!("{row}\n")).collect::<String>();
    std::fs::write(&source, text).unwrap();

    index(home.path(), &source, &["--omp-max-chunks", "1"]);
    let partial = receipt(home.path(), &source);
    assert_eq!(partial["complete"], false);
    assert_eq!(partial["chunks"], 1);
    assert!(partial["issues"].as_array().unwrap().contains(&json!("chunk-budget")));

    index(home.path(), &source, &["--omp-max-chunks", "1"]);
    let complete = receipt(home.path(), &source);
    assert_eq!(complete["complete"], true);
    assert_eq!(complete["chunks"], 2);
    index(home.path(), &source, &[]);
    assert_eq!(receipt(home.path(), &source)["chunks"], 2);

    let uri = home.path().join("memory/chunks.lance");
    let ds = dataset::open(uri.to_str().unwrap(), Default::default()).await.unwrap();
    let batches = dataset::scan_rows(&ds, &["id", "vector"], None, None).await.unwrap();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 2);
    assert!(batches
        .iter()
        .all(|b| b.column_by_name("vector").unwrap().null_count() == 0));

    let out = Command::new(env!("CARGO_BIN_EXE_funes"))
        .args(["get", "01990000-0000-7000-8000-000000000001"])
        .env("FUNES_HOME", home.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let restored = String::from_utf8(out.stdout).unwrap();
    assert!(restored.contains("Why retain the narwhal archive?"));
    assert!(restored.contains("Retain the narwhal archive for provenance, not independent verification."));
    assert!(restored.contains("question") && restored.contains("answer"));
}
