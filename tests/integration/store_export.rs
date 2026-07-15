//! `eg export --data-dir <dir> --out <file>` — full embedded-store export
//! (issue #155).
//!
//! These fixtures prove the acceptance criteria: the inverse of `eg ingest`
//! reads the same physical inventory `eg inspect --data-dir` uses and writes
//! every persisted record as canonical newline-delimited JSON. Coverage:
//! lossless round-trip, re-ingest → inspect count parity, byte-identical
//! output across runs, strict read-only behavior, the `eg forget`
//! body-suppression guarantee (with a `validate`-clean re-ingest), unknown
//! schema-version verbatim round-trip, and stable path-naming diagnostics for
//! missing / empty / record-empty stores that write no output file.

#![allow(missing_docs)]

use assert_cmd::Command;
use predicates::prelude::*;

#[cfg(feature = "embedded-aletheiadb")]
use std::{fs, path::Path};

#[cfg(feature = "embedded-aletheiadb")]
use aletheia_egregore::{
    GraphRecord, NodeKind, SCHEMA_VERSION,
    adapters::records_from_jsonl,
    ir::{AGENT_MEMORY_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION},
};

/// Sentinel body that `eg forget` must keep out of any export.
#[cfg(feature = "embedded-aletheiadb")]
const SECRET_BODY: &str = "SECRET-CUSTOMER-NAME-DO-NOT-EXPORT";

#[cfg(feature = "embedded-aletheiadb")]
fn node_with_version(
    id: &str,
    kind: NodeKind,
    schema_version: u32,
    name: &str,
    summary: &str,
) -> GraphRecord {
    let mut record = GraphRecord::node(
        id.to_owned(),
        kind,
        None,
        None,
        Some(name.to_owned()),
        summary.to_owned(),
    );
    if let GraphRecord::Node {
        schema_version: version,
        ..
    } = &mut record
    {
        *version = schema_version;
    }
    record
}

/// A small graph spanning three domains: codegraph Repository + File,
/// `agent_memory` Observation, project Task.
#[cfg(feature = "embedded-aletheiadb")]
fn mixed_domain_jsonl() -> String {
    let records = vec![
        node_with_version(
            "codegraph:v5:test-repo",
            NodeKind::Repository,
            SCHEMA_VERSION,
            "test-repo",
            "test repo",
        ),
        node_with_version(
            "codegraph:v5:test-file",
            NodeKind::File,
            SCHEMA_VERSION,
            "src/lib.rs",
            "file record",
        ),
        node_with_version(
            "agent_memory:v1:obs-1",
            NodeKind::Observation,
            AGENT_MEMORY_SCHEMA_VERSION,
            "obs1",
            "observation record",
        ),
        node_with_version(
            "project:v1:task-1",
            NodeKind::Task,
            PROJECT_SCHEMA_VERSION,
            "task1",
            "project task record",
        ),
    ];
    let mut jsonl = String::new();
    for record in &records {
        jsonl.push_str(&serde_json::to_string(record).expect("record should serialize"));
        jsonl.push('\n');
    }
    jsonl
}

/// A forgettable agent-memory Observation carrying a sensitive body, plus a
/// harmless one that must survive.
#[cfg(feature = "embedded-aletheiadb")]
fn forgettable_jsonl() -> String {
    let mut secret = node_with_version(
        "agent_memory:v1:obs-forget-me",
        NodeKind::Observation,
        AGENT_MEMORY_SCHEMA_VERSION,
        "secret",
        "Observation by agent:sess",
    );
    if let GraphRecord::Node { text, agent_id, .. } = &mut secret {
        *text = Some(SECRET_BODY.to_owned());
        *agent_id = Some("agent-1".to_owned());
    }
    let keep = node_with_version(
        "agent_memory:v1:obs-keep-me",
        NodeKind::Observation,
        AGENT_MEMORY_SCHEMA_VERSION,
        "keep",
        "Observation by agent:sess",
    );
    let mut jsonl = String::new();
    for record in [&secret, &keep] {
        jsonl.push_str(&serde_json::to_string(record).expect("record should serialize"));
        jsonl.push('\n');
    }
    jsonl
}

#[cfg(feature = "embedded-aletheiadb")]
fn ingest_embedded(graph_path: &Path, data_dir: &Path) {
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
fn export_to(data_dir: &Path, out: &Path) {
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("export")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--out")
        .arg(out)
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
fn inspect_json(data_dir: &Path) -> serde_json::Value {
    let out = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(String::from_utf8(out).expect("UTF-8").trim())
        .expect("inspect output must be JSON")
}

/// Canonical, byte-comparable line set: parse the JSONL to records and
/// re-serialize each via the same `serde_json::to_string` path `eg export`
/// and `Graph::to_jsonl` use, then sort.
#[cfg(feature = "embedded-aletheiadb")]
fn canonical_lines(jsonl: &str) -> Vec<String> {
    let mut lines: Vec<String> = records_from_jsonl(jsonl)
        .expect("records should parse")
        .iter()
        .map(|record| serde_json::to_string(record).expect("record should serialize"))
        .collect();
    lines.sort_unstable();
    lines
}

/// Sorted `(relative path, bytes)` fingerprint of every file under `root`
/// (copied from `tests/integration/log_deltas.rs`).
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

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn export_round_trips_a_known_graph_losslessly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    let out = temp.path().join("export.jsonl");
    let jsonl = mixed_domain_jsonl();
    fs::write(&graph_path, &jsonl).expect("fixture writes");
    ingest_embedded(&graph_path, &data_dir);
    export_to(&data_dir, &out);

    let exported = fs::read_to_string(&out).expect("export file readable");
    assert_eq!(
        canonical_lines(&jsonl),
        canonical_lines(&exported),
        "export must reproduce the ingested record set, canonically"
    );
    assert!(exported.ends_with('\n'), "single trailing newline");
    assert!(
        !exported.trim_end_matches('\n').is_empty(),
        "export must not be empty"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn re_ingest_reproduces_inspect_counts_with_full_parity() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("graph.jsonl");
    let store_a = temp.path().join("A");
    let store_b = temp.path().join("B");
    let export_a = temp.path().join("A.export.jsonl");
    fs::write(&graph_path, mixed_domain_jsonl()).expect("fixture writes");

    ingest_embedded(&graph_path, &store_a);
    export_to(&store_a, &export_a);
    ingest_embedded(&export_a, &store_b);

    let mut a = inspect_json(&store_a);
    let mut b = inspect_json(&store_b);
    // The `source.data_dir` descriptor legitimately differs between stores.
    a.as_object_mut().unwrap().remove("source");
    b.as_object_mut().unwrap().remove("source");
    assert_eq!(
        a, b,
        "re-ingesting an export must reproduce inspect --data-dir counts at full parity"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn export_is_byte_identical_across_five_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    fs::write(&graph_path, mixed_domain_jsonl()).expect("fixture writes");
    ingest_embedded(&graph_path, &data_dir);

    let first = temp.path().join("run-0.jsonl");
    export_to(&data_dir, &first);
    let baseline = fs::read(&first).expect("read run 0");
    for run in 1..5 {
        let out = temp.path().join(format!("run-{run}.jsonl"));
        export_to(&data_dir, &out);
        assert_eq!(
            baseline,
            fs::read(&out).expect("read run"),
            "run {run} must be byte-identical to run 0"
        );
    }
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn export_is_strictly_read_only() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    fs::write(&graph_path, mixed_domain_jsonl()).expect("fixture writes");
    ingest_embedded(&graph_path, &data_dir);

    let before = dir_fingerprint(&data_dir);
    export_to(&data_dir, &temp.path().join("export.jsonl"));
    let after = dir_fingerprint(&data_dir);
    assert_eq!(before, after, "export must not modify any store byte");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn export_omits_forget_retracted_body_but_keeps_audit_trail() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    let out = temp.path().join("export.jsonl");
    fs::write(&graph_path, forgettable_jsonl()).expect("fixture writes");
    ingest_embedded(&graph_path, &data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("forget")
        .arg("agent_memory:v1:obs-forget-me")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--reason")
        .arg("leaked customer name")
        .assert()
        .success();

    export_to(&data_dir, &out);
    let exported = fs::read_to_string(&out).expect("export readable");

    // The retracted body never resurfaces.
    assert!(
        !exported.contains(SECRET_BODY),
        "forget-retracted body must never appear in an export: {exported}"
    );
    // The original Observation record is gone (its ID survives only inside the
    // audit trail's `source_handle` / `deleted_id`, never as a live record).
    let records = records_from_jsonl(&exported).expect("export parses");
    assert!(
        records
            .iter()
            .all(|r| r.id() != "agent_memory:v1:obs-forget-me"),
        "the retracted original record must be absent"
    );
    // The harmless record survives.
    assert!(
        records
            .iter()
            .any(|r| r.id() == "agent_memory:v1:obs-keep-me"),
        "non-retracted records must survive"
    );
    // The audit trail — the Retraction event and its tombstone — is preserved.
    assert!(
        records.iter().any(|r| matches!(
            r,
            GraphRecord::Node {
                kind: NodeKind::Retraction,
                ..
            }
        )),
        "the retraction event must be preserved"
    );
    assert!(
        records.iter().any(|r| matches!(
            r,
            GraphRecord::Tombstone { deleted_id, .. } if deleted_id == "agent_memory:v1:obs-forget-me"
        )),
        "the retraction tombstone must be preserved"
    );

    // Re-ingesting the export is referentially clean: dropping the retracted
    // original while keeping its tombstone raises no dangling-reference defect.
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("validate")
        .arg(&out)
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn export_preserves_unknown_schema_version_verbatim() {
    // A well-formed record with a KNOWN kind, all required properties, and a
    // bumped/future schema version: exactly what a future producer would write
    // and this binary cannot interpret. `eg ingest` would reject the version,
    // so it is injected directly into a raw AletheiaDB store (mirrors
    // tests/integration/inspect_store.rs).
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let out = temp.path().join("export.jsonl");

    let future_version = SCHEMA_VERSION + 1;
    let future_id = "codegraph:v6:future-repo";
    {
        let config = aletheiadb::config::durable_config_for_data_dir(&data_dir);
        let db = aletheiadb::AletheiaDB::with_unified_config(config).expect("raw db opens");
        let properties = aletheiadb::PropertyMapBuilder::new()
            .insert("codegraph_id", future_id)
            .insert("record_type", "node")
            .insert("kind", "Repository")
            .insert("schema_version", i64::from(future_version))
            .insert("domain", "codegraph")
            .insert("name", "future-repo")
            .insert("summary", "future repository record")
            .build();
        db.create_node("Repository", properties)
            .expect("raw node created");
    }

    export_to(&data_dir, &out);
    let exported = fs::read_to_string(&out).expect("export readable");

    // The unknown-version record survives verbatim as its own JSON line — never
    // reprocessed through the current `GraphRecord` reader (which rejects it).
    let line = exported
        .lines()
        .find(|l| l.contains(future_id))
        .expect("unknown-version record must appear in the export");
    let value: serde_json::Value = serde_json::from_str(line).expect("line is JSON");
    assert_eq!(value["id"], future_id);
    assert_eq!(value["kind"], "Repository");
    assert_eq!(value["schema_version"], future_version);
    assert_eq!(value["summary"], "future repository record");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn export_missing_store_fails_naming_the_path_and_writes_no_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("does-not-exist");
    let out = temp.path().join("export.jsonl");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("export")
        .arg("--data-dir")
        .arg(&missing)
        .arg("--out")
        .arg(&out)
        .assert()
        .failure()
        .stderr(predicate::str::contains("embedded store not found"))
        .stderr(predicate::str::contains(missing.to_string_lossy().as_ref()));
    assert!(!out.exists(), "no output file on a missing store");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn export_empty_dir_fails_and_writes_no_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let empty = temp.path().join("empty-store");
    fs::create_dir_all(&empty).expect("empty dir created");
    let out = temp.path().join("export.jsonl");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("export")
        .arg("--data-dir")
        .arg(&empty)
        .arg("--out")
        .arg(&out)
        .assert()
        .failure()
        .stderr(predicate::str::contains("is empty"))
        .stderr(predicate::str::contains(empty.to_string_lossy().as_ref()));
    assert!(!out.exists(), "no output file on an empty dir");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn export_record_empty_store_fails_naming_the_path_and_writes_no_file() {
    // A non-empty store directory holding zero Egregore records (after an empty
    // ingest) is a wrong-store diagnostic, never a valid empty export.
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("empty.jsonl");
    let data_dir = temp.path().join("store");
    let out = temp.path().join("export.jsonl");
    fs::write(&graph_path, "").expect("empty fixture writes");
    ingest_embedded(&graph_path, &data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("export")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--out")
        .arg(&out)
        .assert()
        .failure()
        .stderr(predicate::str::contains("contains no Egregore records"))
        .stderr(predicate::str::contains(
            data_dir.to_string_lossy().as_ref(),
        ));
    assert!(!out.exists(), "no output file on a record-empty store");
}

#[cfg(not(feature = "embedded-aletheiadb"))]
#[test]
fn export_requires_embedded_feature() {
    let temp = tempfile::tempdir().expect("temp dir");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("export")
        .arg("--data-dir")
        .arg(temp.path())
        .arg("--out")
        .arg(temp.path().join("export.jsonl"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("embedded-aletheiadb"));
}
