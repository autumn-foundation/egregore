//! Integration tests for `eg audit schema-constraints` (issue #486): the
//! `AletheiaDB` 0.2.0 schema-constraint evaluation surface.
//!
//! Phase 1 (the default `report` action) is the inventory of the node labels
//! and edge types the embedded adapter actually writes, reconciled against what
//! a real store contains, plus an upstream `.dry_run()` conformance scan of the
//! candidate constraint profile. Phase 2 (`--declare` / `--drop`) is the opt-in
//! enforcement declaration and its retraction.
//!
//! These fixtures prove: exhaustive inventory, read-only reporting,
//! byte-identical output across runs, allow-list-only output (no raw payload
//! text, no engine-internal entity ids), the non-conformance gate, the
//! declare/drop round trip, and the stable error surfaces.

#![allow(missing_docs)]

#[cfg(feature = "embedded-aletheiadb")]
use std::{collections::BTreeMap, fs, path::Path, path::PathBuf};

use assert_cmd::Command;

#[cfg(feature = "embedded-aletheiadb")]
use aletheia_egregore::{
    GraphRecord, NodeKind, SCHEMA_VERSION,
    ir::{AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, PROJECT_SCHEMA_VERSION},
};

/// Sentinel narrative that must never leak into the constraint report.
#[cfg(feature = "embedded-aletheiadb")]
const RAW_PAYLOAD_SENTINEL: &str = "RAW-OBSERVATION-PAYLOAD-DO-NOT-PRINT";

#[cfg(feature = "embedded-aletheiadb")]
fn node(id: &str, kind: NodeKind, schema_version: u32, name: &str, summary: &str) -> GraphRecord {
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

/// A small multi-domain graph with one edge, so the report exercises both the
/// node-label and the edge-type halves of the inventory.
#[cfg(feature = "embedded-aletheiadb")]
fn mixed_domain_jsonl() -> String {
    let records = vec![
        node(
            "codegraph:v5:test-repo",
            NodeKind::Repository,
            SCHEMA_VERSION,
            "test-repo",
            "test repo",
        ),
        node(
            "codegraph:v5:test-file",
            NodeKind::File,
            SCHEMA_VERSION,
            "src/lib.rs",
            "file record",
        ),
        node(
            "agent_memory:v1:obs-1",
            NodeKind::Observation,
            AGENT_MEMORY_SCHEMA_VERSION,
            "obs1",
            RAW_PAYLOAD_SENTINEL,
        ),
        node(
            "project:v1:task-1",
            NodeKind::Task,
            PROJECT_SCHEMA_VERSION,
            "task1",
            "project task record",
        ),
        GraphRecord::edge(
            EdgeLabel::Contains,
            "codegraph:v5:test-repo".to_owned(),
            "codegraph:v5:test-file".to_owned(),
            None,
            "repo contains file".to_owned(),
        ),
    ];
    let mut jsonl = String::new();
    for record in &records {
        jsonl.push_str(&serde_json::to_string(record).expect("record should serialize"));
        jsonl.push('\n');
    }
    jsonl
}

#[cfg(feature = "embedded-aletheiadb")]
fn seeded_store() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, mixed_domain_jsonl()).expect("write graph");
    let data_dir = temp.path().join("store");
    Command::cargo_bin("egregore")
        .expect("binary should build")
        .arg("ingest")
        .arg(&graph)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();
    (temp, data_dir)
}

#[cfg(feature = "embedded-aletheiadb")]
fn run(args: &[&str]) -> std::process::Output {
    Command::cargo_bin("egregore")
        .expect("binary should build")
        .args(args)
        .output()
        .expect("command should run")
}

#[cfg(feature = "embedded-aletheiadb")]
fn report_json(data_dir: &Path, extra: &[&str]) -> serde_json::Value {
    let dir = data_dir.to_str().expect("utf8 path");
    let mut args = vec!["audit", "schema-constraints", "--data-dir", dir];
    args.extend_from_slice(extra);
    let output = run(&args);
    assert_eq!(
        output.status.code(),
        Some(0),
        "report should succeed on a conforming store; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    serde_json::from_str(stdout.trim()).expect("report should be one JSON line")
}

/// Recursive snapshot of every regular file under `dir`: relative path -> bytes.
#[cfg(feature = "embedded-aletheiadb")]
fn dir_snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).expect("store dir should be readable") {
            let entry = entry.expect("entry should be readable");
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("path should be under root")
                    .to_path_buf();
                out.insert(relative, fs::read(&path).expect("file should be readable"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

// ---------------------------------------------------------------------------
// AC1 - the written-label inventory is exhaustive and derived, never hand-kept
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn report_inventories_every_label_the_adapter_can_write() {
    let (_temp, data_dir) = seeded_store();
    let report = report_json(&data_dir, &[]);

    let inventory = &report["inventory"];
    let node_labels: Vec<&str> = inventory["node_labels"]
        .as_array()
        .expect("node_labels array")
        .iter()
        .map(|v| v.as_str().expect("label string"))
        .collect();
    let edge_types: Vec<&str> = inventory["edge_types"]
        .as_array()
        .expect("edge_types array")
        .iter()
        .map(|v| v.as_str().expect("type string"))
        .collect();

    // Every NodeKind maps 1:1 onto its own store-side label (the finding that
    // decides this issue), plus the literal `Tombstone` label the adapter
    // writes for tombstone records.
    assert_eq!(
        node_labels.len(),
        NodeKind::ALL.len() + 1,
        "one label per NodeKind, plus Tombstone"
    );
    for kind in NodeKind::ALL {
        assert!(
            node_labels.contains(&kind.as_str()),
            "inventory should list {}",
            kind.as_str()
        );
    }
    assert!(node_labels.contains(&"Tombstone"));

    assert_eq!(edge_types.len(), EdgeLabel::ALL.len());
    for label in EdgeLabel::ALL {
        assert!(
            edge_types.contains(&label.as_str()),
            "inventory should list {}",
            label.as_str()
        );
    }

    assert_eq!(
        inventory["label_partition"].as_str(),
        Some("one_label_per_node_kind"),
        "the report must state the partition finding explicitly"
    );
    assert_eq!(
        inventory["writable_node_labels"].as_u64(),
        Some(node_labels.len() as u64)
    );
    assert_eq!(
        inventory["writable_edge_types"].as_u64(),
        Some(edge_types.len() as u64)
    );

    // Sorted, so the inventory is stable and diffable.
    let mut sorted_nodes = node_labels.clone();
    sorted_nodes.sort_unstable();
    assert_eq!(node_labels, sorted_nodes, "node labels must be sorted");
    let mut sorted_edges = edge_types.clone();
    sorted_edges.sort_unstable();
    assert_eq!(edge_types, sorted_edges, "edge types must be sorted");
}

// ---------------------------------------------------------------------------
// AC2 - the dry-run conformance scan runs against a real store
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn report_runs_dry_run_conformance_over_present_labels() {
    let (_temp, data_dir) = seeded_store();
    let report = report_json(&data_dir, &[]);

    let conformance = &report["conformance"];
    assert_eq!(
        conformance["entities_non_conforming"].as_u64(),
        Some(0),
        "an adapter-written store conforms to the spine profile"
    );
    assert!(
        conformance["entities_checked"].as_u64().unwrap_or(0) >= 5,
        "the four nodes and one edge should all be checked"
    );

    let rows = conformance["rows"].as_array().expect("rows array");
    let find = |label: &str| {
        rows.iter()
            .find(|row| row["label"].as_str() == Some(label))
            .unwrap_or_else(|| panic!("row for {label} should be present"))
    };

    for label in ["Repository", "File", "Observation", "Task"] {
        let row = find(label);
        assert_eq!(row["entity_kind"].as_str(), Some("node"));
        assert_eq!(row["status"].as_str(), Some("conforms"), "{label}");
        assert_eq!(row["checked"].as_u64(), Some(1), "{label}");
        assert_eq!(row["non_conforming"].as_u64(), Some(0), "{label}");
    }

    let contains = find("CONTAINS");
    assert_eq!(contains["entity_kind"].as_str(), Some("edge"));
    assert_eq!(contains["status"].as_str(), Some("conforms"));

    // A label the adapter can write but this store has never seen is reported
    // as `not_present`, never as a vacuous "conforms" that overstates coverage.
    let symbol = find("Symbol");
    assert_eq!(symbol["status"].as_str(), Some("not_present"));
    assert_eq!(symbol["checked"].as_u64(), Some(0));

    assert!(
        report["ok"].as_bool().unwrap_or(false),
        "a conforming store passes the gate"
    );
}

// ---------------------------------------------------------------------------
// AC3 - the profile constrains only the universal spine
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn spine_profile_declares_only_universal_identity_and_routing_keys() {
    let (_temp, data_dir) = seeded_store();
    let report = report_json(&data_dir, &[]);

    assert_eq!(report["profile"].as_str(), Some("spine"));
    let props = &report["profile_properties"];

    let names = |kind: &str| -> Vec<String> {
        props[kind]
            .as_array()
            .unwrap_or_else(|| panic!("{kind} spec array"))
            .iter()
            .map(|p| p["property"].as_str().expect("property name").to_owned())
            .collect()
    };

    assert_eq!(
        names("node"),
        vec!["codegraph_id", "record_type", "schema_version"],
        "nodes: identity + routing only, never a per-kind payload field"
    );
    assert_eq!(
        names("edge"),
        vec![
            "codegraph_id",
            "label",
            "record_type",
            "schema_version",
            "source_codegraph_id",
            "target_codegraph_id",
        ],
        "edges: the spine plus the endpoint/label routing keys"
    );

    // `schema_version` must be typed Integer: a per-domain version BUMP changes
    // the value, never the type, so the declaration survives every future bump.
    let schema_version_spec = props["node"]
        .as_array()
        .expect("node specs")
        .iter()
        .find(|p| p["property"].as_str() == Some("schema_version"))
        .expect("schema_version spec");
    assert_eq!(schema_version_spec["declared_type"].as_str(), Some("int"));
    assert_eq!(schema_version_spec["required"].as_bool(), Some(true));
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn full_base_profile_adds_summary_and_types_egregore_seq_as_string() {
    let (_temp, data_dir) = seeded_store();
    let report = report_json(&data_dir, &["--profile", "full-base"]);

    assert_eq!(report["profile"].as_str(), Some("full-base"));
    let edge_specs = report["profile_properties"]["edge"]
        .as_array()
        .expect("edge specs")
        .clone();

    let seq = edge_specs
        .iter()
        .find(|p| p["property"].as_str() == Some("egregore_seq"))
        .expect("egregore_seq spec");
    // The adapter writes the sequence NUMBER as a String; declaring it Integer
    // would make every edge write fail. The profile records reality.
    assert_eq!(
        seq["declared_type"].as_str(),
        Some("string"),
        "egregore_seq is a number stored as a string"
    );

    assert!(
        report["profile_properties"]["node"]
            .as_array()
            .expect("node specs")
            .iter()
            .any(|p| p["property"].as_str() == Some("summary")),
        "full-base adds summary"
    );

    // The stricter profile must still conform on an adapter-written store.
    assert_eq!(
        report["conformance"]["entities_non_conforming"].as_u64(),
        Some(0)
    );
}

// ---------------------------------------------------------------------------
// AC4 - strictly read-only, deterministic, redaction-safe
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn report_is_read_only_and_byte_identical_across_runs() {
    let (_temp, data_dir) = seeded_store();
    let before = dir_snapshot(&data_dir);

    let dir = data_dir.to_str().expect("utf8 path");
    let first = run(&["audit", "schema-constraints", "--data-dir", dir]);
    let second = run(&["audit", "schema-constraints", "--data-dir", dir]);
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(
        first.stdout, second.stdout,
        "report must be byte-identical across runs on an unchanged store"
    );

    let after = dir_snapshot(&data_dir);
    assert_eq!(
        before, after,
        "the report action must not modify a single byte of the store"
    );

    // Specifically: the report path must never declare anything.
    assert!(
        !data_dir.join("schema_constraints.dat").exists(),
        "the report action must not write the constraint sidecar"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn report_never_leaks_payload_text_or_engine_entity_ids() {
    let (_temp, data_dir) = seeded_store();
    let dir = data_dir.to_str().expect("utf8 path");
    let output = run(&["audit", "schema-constraints", "--data-dir", dir]);
    let stdout = String::from_utf8(output.stdout).expect("utf8");

    assert!(
        !stdout.contains(RAW_PAYLOAD_SENTINEL),
        "record summaries must never appear in the report"
    );
    assert!(
        !stdout.contains("sample_ids"),
        "engine-internal entity ids must be resolved to citable record ids, never emitted raw"
    );
}

// ---------------------------------------------------------------------------
// AC5 - a non-conforming store fails the gate with citable, bounded diagnostics
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn non_conforming_store_fails_the_gate_and_cites_offending_records() {
    let (temp, data_dir) = seeded_store();
    // Plant a node that is missing the identity spine, the way a hand-edited or
    // foreign writer could. `eg validate` cannot see this - it gates a JSONL
    // file, not the store - which is exactly the gap this issue is about.
    aletheia_egregore::adapters::fixtures::plant_nonconforming_node(
        &data_dir,
        "Symbol",
        "PLANTED-NONCONFORMING",
    )
    .expect("planting a raw node should succeed");
    let _ = &temp;

    let dir = data_dir.to_str().expect("utf8 path");
    let output = run(&["audit", "schema-constraints", "--data-dir", dir]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "non-conformance is a gate failure, not a hard error"
    );
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output.stdout).expect("utf8").trim())
            .expect("the full report is still printed on gate failure");

    assert_eq!(report["ok"].as_bool(), Some(false));
    let rows = report["conformance"]["rows"]
        .as_array()
        .expect("rows")
        .clone();
    let symbol = rows
        .iter()
        .find(|row| row["label"].as_str() == Some("Symbol"))
        .expect("Symbol row");
    assert_eq!(symbol["status"].as_str(), Some("violates"));
    assert_eq!(symbol["non_conforming"].as_u64(), Some(1));
    let violations = symbol["violations"].as_array().expect("violations");
    assert!(
        !violations.is_empty(),
        "a violating label must enumerate its violations"
    );
    assert!(
        violations
            .iter()
            .any(|v| v["property"].as_str() == Some("codegraph_id")),
        "the missing identity key must be named"
    );
    assert!(
        report["conformance"]["entities_non_conforming"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn declare_refuses_a_non_conforming_store_without_writing_anything() {
    let (_temp, data_dir) = seeded_store();
    aletheia_egregore::adapters::fixtures::plant_nonconforming_node(
        &data_dir,
        "Symbol",
        "PLANTED-NONCONFORMING",
    )
    .expect("planting a raw node should succeed");

    let dir = data_dir.to_str().expect("utf8 path");
    let output = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        dir,
        "--declare",
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "declaring over non-conforming state must refuse"
    );
    assert!(
        !data_dir.join("schema_constraints.dat").exists(),
        "a refused declaration must leave the store with no sidecar"
    );
}

// ---------------------------------------------------------------------------
// AC6 - Phase 2: opt-in declaration is real, enforced, and reversible
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn declare_then_drop_round_trips_and_enforcement_is_live_between() {
    let (temp, data_dir) = seeded_store();
    let dir = data_dir.to_str().expect("utf8 path");

    // A store with nothing declared reports an empty declaration set.
    let before = report_json(&data_dir, &[]);
    assert_eq!(
        before["declared_constraints"]
            .as_array()
            .expect("array")
            .len(),
        0
    );

    // Declare.
    let declared = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        dir,
        "--declare",
    ]);
    assert_eq!(
        declared.status.code(),
        Some(0),
        "declaring over a conforming store should succeed; stderr={}",
        String::from_utf8_lossy(&declared.stderr)
    );
    let declared_report: serde_json::Value =
        serde_json::from_str(String::from_utf8(declared.stdout).expect("utf8").trim())
            .expect("json");
    assert_eq!(declared_report["action"].as_str(), Some("declare"));
    assert!(
        declared_report["declared_labels"].as_u64().unwrap_or(0) > 100,
        "every writable label gets a declaration, including ones with no data yet"
    );
    assert!(
        data_dir.join("schema_constraints.dat").exists(),
        "declaring persists the upstream sidecar"
    );

    // The declaration is visible on the next report.
    let after = report_json(&data_dir, &[]);
    assert!(
        !after["declared_constraints"]
            .as_array()
            .expect("array")
            .is_empty(),
        "a subsequent report surfaces the declared constraints"
    );

    // A legitimate Egregore write still succeeds against the live constraints.
    let more = temp.path().join("more.jsonl");
    fs::write(
        &more,
        format!(
            "{}\n",
            serde_json::to_string(&node(
                "codegraph:v5:test-symbol",
                NodeKind::Symbol,
                SCHEMA_VERSION,
                "do_work",
                "a symbol",
            ))
            .expect("serialize")
        ),
    )
    .expect("write");
    Command::cargo_bin("egregore")
        .expect("binary")
        .arg("ingest")
        .arg(&more)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Drop restores the schemaless posture.
    let dropped = run(&["audit", "schema-constraints", "--data-dir", dir, "--drop"]);
    assert_eq!(dropped.status.code(), Some(0));
    let dropped_report: serde_json::Value =
        serde_json::from_str(String::from_utf8(dropped.stdout).expect("utf8").trim())
            .expect("json");
    assert_eq!(dropped_report["action"].as_str(), Some("drop"));
    assert!(dropped_report["dropped_labels"].as_u64().unwrap_or(0) > 0);

    let final_report = report_json(&data_dir, &[]);
    assert_eq!(
        final_report["declared_constraints"]
            .as_array()
            .expect("array")
            .len(),
        0,
        "drop must retract every Egregore-declared constraint"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn a_declared_store_refuses_a_write_that_violates_the_spine() {
    // The point of Phase 2: a bad write that reaches the store by a path
    // `eg validate` cannot gate is refused at the write boundary itself.
    let (_temp, data_dir) = seeded_store();
    let dir = data_dir.to_str().expect("utf8 path");
    run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        dir,
        "--declare",
    ]);

    let refusal = aletheia_egregore::adapters::fixtures::plant_nonconforming_node(
        &data_dir,
        "Symbol",
        "PLANTED-NONCONFORMING",
    );
    assert!(
        refusal.is_err(),
        "with constraints declared, a spine-violating raw write must be refused"
    );
}

// ---------------------------------------------------------------------------
// AC7 - stable operator-facing error surfaces
// ---------------------------------------------------------------------------

#[test]
fn missing_store_is_a_usage_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("nope");
    let output = Command::cargo_bin("egregore")
        .expect("binary")
        .args([
            "audit",
            "schema-constraints",
            "--data-dir",
            missing.to_str().expect("utf8"),
        ])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nope"),
        "the diagnostic must name the path; got {stderr}"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn declare_and_drop_are_mutually_exclusive() {
    let (_temp, data_dir) = seeded_store();
    let output = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        data_dir.to_str().expect("utf8"),
        "--declare",
        "--drop",
    ]);
    assert_eq!(output.status.code(), Some(2));
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn text_format_renders_the_same_findings() {
    let (_temp, data_dir) = seeded_store();
    let output = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        data_dir.to_str().expect("utf8"),
        "--format",
        "text",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(stdout.contains("profile: spine"));
    assert!(stdout.contains("one_label_per_node_kind"));
    assert!(!stdout.contains(RAW_PAYLOAD_SENTINEL));
}
