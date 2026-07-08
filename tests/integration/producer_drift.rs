//! Integration tests for `eg query producer-drift` (issue #234): flag
//! code-graph records whose producer identity (grammar/extractor versions)
//! differs from the current running binary, without ever re-extracting or
//! mutating the store.

#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::scan_repository_at_with_override;
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Minimal Rust source scanned with the *current* binary so its producer
/// envelope matches the running extractor exactly.
fn write_current_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("src dir");
    fs::write(
        dir.join("src/lib.rs"),
        "pub fn current_fn() -> usize {\n    1\n}\n",
    )
    .expect("lib.rs");
}

/// Scans the fixture with the in-process extractor (same producer identity as
/// the built binary) and returns (`TempDir`, graph path).
fn current_binary_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_current_fixture(temp.path());
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("drift-fixture"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

/// A code-graph node stamped by a hypothetical older binary: both the
/// `egregore_version` and the `tree_sitter_rust` grammar component differ
/// from anything a current build could produce.
const OLD_EXTRACTOR_NODE: &str = concat!(
    r#"{"record_type":"node","id":"codegraph:v4:"#,
    r#"00000000000000000000000000000000000000000000000000000000000000aa","#,
    r#""kind":"Symbol","schema_version":4,"repo_relative_path":"src/old.rs","#,
    r#""span":{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1},"#,
    r#""name":"old_fn","#,
    r#""producer":{"egregore_version":"0.0.1","producer_kind":"code_graph_extractor","#,
    r#""producer_components":{"tree_sitter":"0.1.0","tree_sitter_rust":"0.1.0"},"#,
    r#""producer_started_at":"2025-01-01T00:00:00Z"},"#,
    r#""summary":"fn old_fn"}"#
);

/// A history-replay edge from the same hypothetical older binary. Both
/// endpoints anchor on the old extractor node so the embedded sink's
/// referential-integrity check accepts the fixture.
const OLD_HISTORY_EDGE: &str = concat!(
    r#"{"record_type":"edge","id":"codegraph:v4:"#,
    r#"00000000000000000000000000000000000000000000000000000000000000ab","#,
    r#""schema_version":4,"label":"REFERENCES","#,
    r#""source":"codegraph:v4:00000000000000000000000000000000000000000000000000000000000000aa","#,
    r#""target":"codegraph:v4:00000000000000000000000000000000000000000000000000000000000000aa","#,
    r#""producer":{"egregore_version":"0.0.1","producer_kind":"history_replay","#,
    r#""producer_components":{"tree_sitter":"0.1.0","tree_sitter_rust":"0.1.0"},"#,
    r#""producer_started_at":"2025-01-01T00:00:00Z"},"#,
    r#""summary":"src/old.rs defines old_fn"}"#
);

/// An agent-memory record whose producer must never be compared against
/// grammar identity, no matter how bogus its component versions look.
const OBSERVATION_NODE: &str = concat!(
    r#"{"record_type":"node","id":"agentmem:v1:"#,
    r#"00000000000000000000000000000000000000000000000000000000000000ac","#,
    r#""kind":"Observation","schema_version":1,"#,
    r#""producer":{"egregore_version":"0.0.1","producer_kind":"observation_writer","#,
    r#""producer_components":{"writer_schema":"9.9.9"},"#,
    r#""producer_started_at":"2025-01-01T00:00:00Z"},"#,
    r#""summary":"an observation"}"#
);

/// A pre-producer-envelope record: no `producer` field at all.
const LEGACY_NODE: &str = concat!(
    r#"{"record_type":"node","id":"codegraph:v4:"#,
    r#"00000000000000000000000000000000000000000000000000000000000000ad","#,
    r#""kind":"File","schema_version":4,"repo_relative_path":"src/legacy.rs","#,
    r#""summary":"legacy file","name":"src/legacy.rs"}"#
);

/// Writes a mixed graph: current-binary records plus one drifted extractor
/// node, one drifted history edge, one non-code observation, one legacy record.
fn mixed_graph() -> (tempfile::TempDir, PathBuf) {
    let (temp, graph) = current_binary_graph();
    let mut jsonl = fs::read_to_string(&graph).expect("read scanned graph");
    jsonl.push_str(OLD_EXTRACTOR_NODE);
    jsonl.push('\n');
    jsonl.push_str(OLD_HISTORY_EDGE);
    jsonl.push('\n');
    jsonl.push_str(OBSERVATION_NODE);
    jsonl.push('\n');
    jsonl.push_str(LEGACY_NODE);
    jsonl.push('\n');
    let mixed = temp.path().join("mixed.jsonl");
    fs::write(&mixed, jsonl).expect("write mixed graph");
    (temp, mixed)
}

fn run_producer_drift(graph: &Path) -> Value {
    let output = egregore()
        .args(["query", "producer-drift", "--graph"])
        .arg(graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON")
}

fn groups_in_bucket<'a>(parsed: &'a Value, bucket: &str) -> Vec<&'a Value> {
    parsed["groups"]
        .as_array()
        .expect("groups array")
        .iter()
        .filter(|g| g["bucket"] == bucket)
        .collect()
}

// ---------------------------------------------------------------------------
// AC1 + AC2: drifted records are flagged, grouped by producer signature, and
// listed with record IDs plus repo-relative file/span handles where available.
// ---------------------------------------------------------------------------

#[test]
fn drift_flags_records_from_older_grammar_and_binary() {
    let (_temp, graph) = mixed_graph();
    let parsed = run_producer_drift(&graph);

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["counts"]["drifted"], 2, "old node + old edge drift");

    let drifted = groups_in_bucket(&parsed, "drifted");
    assert_eq!(
        drifted.len(),
        2,
        "distinct (producer_kind, version, components) signatures stay separate groups"
    );

    let kinds: Vec<&str> = drifted
        .iter()
        .map(|g| g["producer_kind"].as_str().expect("producer_kind"))
        .collect();
    assert_eq!(kinds, ["code_graph_extractor", "history_replay"]);

    let extractor = drifted[0];
    assert_eq!(extractor["egregore_version"], "0.0.1");
    assert_eq!(extractor["record_count"], 1);

    // Every recorded component that differs from the running binary is named,
    // and the binary-version mismatch is named too.
    let mismatch_fields: Vec<&str> = extractor["mismatches"]
        .as_array()
        .expect("mismatches array")
        .iter()
        .map(|m| m["field"].as_str().expect("field"))
        .collect();
    assert!(mismatch_fields.contains(&"egregore_version"));
    assert!(mismatch_fields.contains(&"tree_sitter_rust"));

    // The affected record is listed with its stable ID and file/span handle.
    let records = extractor["records"].as_array().expect("records array");
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["record_id"],
        "codegraph:v4:00000000000000000000000000000000000000000000000000000000000000aa"
    );
    assert_eq!(records[0]["repo_relative_path"], "src/old.rs");
    assert_eq!(records[0]["span"]["start_line"], 1);

    // The current binary's own identity is reported so the comparison basis
    // is citable.
    assert_eq!(
        parsed["current_producer"]["egregore_version"],
        env!("CARGO_PKG_VERSION")
    );
    assert!(
        parsed["current_producer"]["producer_components"]["tree_sitter_rust"].is_string(),
        "current grammar component set must be reported"
    );
}

// ---------------------------------------------------------------------------
// AC5: zero false positives — a store written entirely by one binary version
// yields an empty drift result.
// ---------------------------------------------------------------------------

#[test]
fn single_binary_store_yields_zero_drift() {
    let (_temp, graph) = current_binary_graph();
    let parsed = run_producer_drift(&graph);

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["counts"]["drifted"], 0, "zero false positives");
    assert!(groups_in_bucket(&parsed, "drifted").is_empty());
    assert!(
        parsed["counts"]["current"].as_u64().expect("current") > 0,
        "scanned records must land in the current bucket"
    );

    // The empty drift result is an explicit machine-readable answer.
    assert!(
        parsed["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["code"] == "no_drift")
    );
}

// ---------------------------------------------------------------------------
// AC3: only code-graph-extraction producers are compared against grammar and
// binary identity; other producers land in a separate, never-flagged bucket.
// ---------------------------------------------------------------------------

#[test]
fn non_code_producers_are_never_flagged_as_drifted() {
    let (_temp, graph) = mixed_graph();
    let parsed = run_producer_drift(&graph);

    assert_eq!(parsed["counts"]["non_code_producer"], 1);
    let non_code = groups_in_bucket(&parsed, "non_code_producer");
    assert_eq!(non_code.len(), 1);
    assert_eq!(non_code[0]["producer_kind"], "observation_writer");
    assert!(
        non_code[0].get("mismatches").is_none(),
        "non-code producers are never compared, so no mismatch list may appear"
    );

    // The observation's bogus versions must not leak into the drifted bucket.
    for group in groups_in_bucket(&parsed, "drifted") {
        assert_ne!(group["producer_kind"], "observation_writer");
    }
}

// ---------------------------------------------------------------------------
// AC4: legacy_pre_v1 records keep their own bucket and are never merged into
// the current or drifted buckets.
// ---------------------------------------------------------------------------

#[test]
fn legacy_records_stay_in_their_own_bucket() {
    let (_temp, graph) = mixed_graph();
    let parsed = run_producer_drift(&graph);

    assert_eq!(parsed["counts"]["legacy_pre_v1"], 1);
    let legacy = groups_in_bucket(&parsed, "legacy_pre_v1");
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0]["producer_kind"], "legacy_pre_v1");
    assert_eq!(legacy[0]["record_count"], 1);

    // Never merged into current or drifted.
    for group in groups_in_bucket(&parsed, "current") {
        assert_ne!(group["producer_kind"], "legacy_pre_v1");
    }
    for group in groups_in_bucket(&parsed, "drifted") {
        assert_ne!(group["producer_kind"], "legacy_pre_v1");
    }
}

// ---------------------------------------------------------------------------
// AC6: deterministic output — byte-identical across repeated runs — plus a
// `--format text` mode.
// ---------------------------------------------------------------------------

#[test]
fn output_is_byte_identical_across_runs() {
    let (_temp, graph) = mixed_graph();
    let run = || {
        egregore()
            .args(["query", "producer-drift", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    assert_eq!(run(), run(), "stdout must be byte-identical across runs");
}

#[test]
fn text_format_renders_deterministic_summary() {
    let (_temp, graph) = mixed_graph();
    let run = || {
        egregore()
            .args(["query", "producer-drift", "--format", "text", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let first = run();
    assert_eq!(first, run(), "text output must be byte-identical too");

    let text = String::from_utf8(first).expect("utf8");
    assert!(text.contains("producer-drift:"), "summary line: {text}");
    assert!(text.contains("2 drifted"), "drift tally: {text}");
    assert!(
        text.contains("code_graph_extractor"),
        "drifted signature: {text}"
    );
    assert!(
        text.contains("mismatch tree_sitter_rust"),
        "component mismatch line: {text}"
    );
    assert!(text.contains("legacy_pre_v1"), "legacy bucket: {text}");
}

// ---------------------------------------------------------------------------
// AC6: the verb runs against an embedded --data-dir store with the same
// classification as the JSONL path.
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_store_reports_the_same_drift() {
    let (_temp, graph) = mixed_graph();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let output = egregore()
        .args(["query", "producer-drift", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value = serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON");

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["counts"]["drifted"], 2);
    assert_eq!(parsed["counts"]["non_code_producer"], 1);
    assert_eq!(parsed["counts"]["legacy_pre_v1"], 1);
}

// ---------------------------------------------------------------------------
// AC6: --repo scopes a shared multi-repo store; unknown selectors are
// rejected with the stable machine-readable diagnostic.
// ---------------------------------------------------------------------------

#[test]
fn repo_selector_scopes_a_multi_repo_store() {
    let temp = tempfile::tempdir().expect("temp dir");

    let repo_a = temp.path().join("a");
    write_current_fixture(&repo_a);
    let jsonl_a = scan_repository_at_with_override(&repo_a, FIXED_TIME, Some("drift-repo-a"))
        .expect("scan a")
        .to_jsonl()
        .expect("serialize a");

    let repo_b = temp.path().join("b");
    fs::create_dir_all(repo_b.join("src")).expect("src dir");
    fs::write(repo_b.join("src/lib.rs"), "pub fn other_fn() {}\n").expect("lib.rs");
    let jsonl_b = scan_repository_at_with_override(&repo_b, FIXED_TIME, Some("drift-repo-b"))
        .expect("scan b")
        .to_jsonl()
        .expect("serialize b");

    let graph = temp.path().join("multi.jsonl");
    fs::write(&graph, format!("{jsonl_a}{jsonl_b}")).expect("write multi graph");

    let unscoped = run_producer_drift(&graph);
    let total_unscoped = unscoped["counts"]["total"].as_u64().expect("total");

    let output = egregore()
        .args([
            "query",
            "producer-drift",
            "--repo",
            "drift-repo-a",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let scoped: Value = serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON");

    assert!(scoped["repo_scope"].as_str().is_some());
    let total_scoped = scoped["counts"]["total"].as_u64().expect("total");
    assert!(
        total_scoped < total_unscoped,
        "scoping to one repo must exclude the other repo's records \
         (scoped {total_scoped} vs unscoped {total_unscoped})"
    );
    assert_eq!(
        scoped["counts"]["drifted"], 0,
        "both repos were scanned by this binary: still zero false positives"
    );

    egregore()
        .args([
            "query",
            "producer-drift",
            "--repo",
            "no-such-repo",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unknown_repository_selector"));
}

/// A `--repo`-scoped run must keep tombstones whose `deleted_id` names an
/// *edge* record. `RepositoryIndex` owns node IDs only, so resolving the
/// deleted edge ID directly always fails; attribution must go through the
/// deleted edge's recorded source node. An ingested incremental stream keeps
/// the superseded edge record alongside its cache-invalidation tombstone, so
/// the source is resolvable — dropping the tombstone silently undercounts the
/// scoped drift buckets.
#[test]
fn repo_scoped_run_keeps_edge_tombstones_attributable_via_edge_source() {
    let temp = tempfile::tempdir().expect("temp dir");

    let repo_a = temp.path().join("a");
    write_current_fixture(&repo_a);
    let jsonl_a = scan_repository_at_with_override(&repo_a, FIXED_TIME, Some("drift-repo-a"))
        .expect("scan a")
        .to_jsonl()
        .expect("serialize a");

    let repo_b = temp.path().join("b");
    fs::create_dir_all(repo_b.join("src")).expect("src dir");
    fs::write(repo_b.join("src/lib.rs"), "pub fn other_fn() {}\n").expect("lib.rs");
    let jsonl_b = scan_repository_at_with_override(&repo_b, FIXED_TIME, Some("drift-repo-b"))
        .expect("scan b")
        .to_jsonl()
        .expect("serialize b");

    // Pick one of repo A's edges: its ID is the tombstone's deleted_id.
    let deleted_edge_id = jsonl_a
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v["record_type"] == "edge")
        .expect("repo A scan must emit at least one edge")["id"]
        .as_str()
        .expect("edge id")
        .to_owned();

    // A cache-invalidation tombstone for that edge, stamped by an older
    // incremental-cache binary so it lands in the drifted bucket.
    let tombstone = serde_json::json!({
        "record_type": "tombstone",
        "id": "codegraph:v4:00000000000000000000000000000000000000000000000000000000000000ae",
        "schema_version": 4,
        "deleted_id": deleted_edge_id,
        "summary": "Invalidated stale cached record from src/lib.rs",
        "producer": {
            "egregore_version": "0.0.1",
            "producer_kind": "incremental_cache",
            "producer_components": {"tree_sitter": "0.1.0", "tree_sitter_rust": "0.1.0"},
            "producer_started_at": "2025-01-01T00:00:00Z"
        }
    })
    .to_string();

    let graph = temp.path().join("multi-tombstone.jsonl");
    fs::write(&graph, format!("{jsonl_a}{jsonl_b}{tombstone}\n")).expect("write graph");

    let run_scoped = |repo: &str| -> Value {
        let output = egregore()
            .args(["query", "producer-drift", "--repo", repo, "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
            .expect("stdout must be valid JSON")
    };

    let unscoped = run_producer_drift(&graph);
    assert_eq!(
        unscoped["counts"]["drifted"], 1,
        "the edge tombstone drifts in the unscoped run"
    );

    // Scoped to the owning repo, the edge tombstone must still be counted and
    // listed: its deleted edge's source node belongs to drift-repo-a.
    let scoped_a = run_scoped("drift-repo-a");
    assert_eq!(
        scoped_a["counts"]["drifted"], 1,
        "an attributable edge tombstone must not vanish from its own repo's scoped run"
    );
    let drifted = groups_in_bucket(&scoped_a, "drifted");
    assert_eq!(drifted.len(), 1);
    let records = drifted[0]["records"].as_array().expect("records array");
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["record_id"],
        "codegraph:v4:00000000000000000000000000000000000000000000000000000000000000ae"
    );
    assert_eq!(records[0]["record_type"], "tombstone");

    // Scoped to the sibling repo, the tombstone stays excluded: attribution
    // through the edge source must not leak another repo's deletions.
    let scoped_b = run_scoped("drift-repo-b");
    assert_eq!(
        scoped_b["counts"]["drifted"], 0,
        "another repo's edge tombstone must stay out of a sibling scope"
    );
}

// ---------------------------------------------------------------------------
// AC7: the verb is read-only — the graph input is never modified.
// ---------------------------------------------------------------------------

#[test]
fn query_is_read_only() {
    let (_temp, graph) = mixed_graph();
    let before = fs::read(&graph).expect("read graph before");
    run_producer_drift(&graph);
    let after = fs::read(&graph).expect("read graph after");
    assert_eq!(before, after, "producer-drift must never mutate its input");
}

/// Sorted `(relative path, bytes)` fingerprint of every file under `root`
/// (mirrors `tests/integration/asof_file_symbols.rs`).
#[cfg(feature = "embedded-aletheiadb")]
fn dir_fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let ft = entry.file_type().unwrap();
            let path = entry.path();
            if ft.is_dir() {
                walk(&path, base, out);
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// AC7 for `--data-dir`: opening the embedded engine in place re-persists its
/// on-disk index files, so the audit must read a throwaway copy and leave the
/// live store byte-for-byte untouched (mirrors the evidence-freshness,
/// point-in-time, and citation-audit read-only lanes).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_query_is_strictly_read_only() {
    let (_temp, graph) = mixed_graph();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let before = dir_fingerprint(&data_dir);
    egregore()
        .args(["query", "producer-drift", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "producer-drift must not modify any store file when reading --data-dir"
    );
}
