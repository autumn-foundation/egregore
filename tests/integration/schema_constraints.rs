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

/// Store fixtures that simulate a NON-Egregore writer.
///
/// These live in the TEST crate on purpose (issue #486). They plant records and
/// declarations that bypass every Egregore invariant — exactly the class of
/// write the schema-constraint backstop exists to catch — so they must not be
/// reachable from the published library. A Cargo feature could not give that
/// guarantee: features are consumer-selectable, so `adapters::fixtures` behind
/// `test-fixtures` was public API of the crate for anyone who chose to enable
/// it, regardless of `#[doc(hidden)]` or the comment claiming otherwise.
///
/// Nothing crate-private is needed to make them work: the store is opened
/// through `aletheiadb`'s own public `config::durable_config_for_data_dir`, the
/// same call `EmbeddedAletheiaSink::open` uses, with Egregore's public
/// `MAX_INTERNED_STRINGS` pinned so the interner budget matches the adapter's
/// (issue #439) rather than silently inheriting upstream's default.
///
/// Unlike the adapter's own open path these take NO write lease, which is
/// correct here: the fixtures run between CLI subprocess invocations, never
/// alongside one, and a fixture is not a writer the contention contract is
/// written about.
#[cfg(feature = "embedded-aletheiadb")]
mod fixtures {
    use std::path::Path;

    use aletheia_egregore::adapters::preflight::MAX_INTERNED_STRINGS;

    /// Opens the store the way the embedded adapter does, minus the lease.
    fn open(data_dir: &Path) -> Result<aletheiadb::AletheiaDB, String> {
        let mut config = aletheiadb::config::durable_config_for_data_dir(data_dir);
        config.persistence.max_interned_strings =
            usize::try_from(MAX_INTERNED_STRINGS).unwrap_or(usize::MAX);
        aletheiadb::AletheiaDB::with_unified_config(config).map_err(|error| error.to_string())
    }

    fn plant_raw_node(
        data_dir: impl AsRef<Path>,
        label: &str,
        properties: &[(&str, &str)],
    ) -> Result<(), String> {
        let db = open(data_dir.as_ref())?;
        let mut builder = aletheiadb::PropertyMapBuilder::new();
        for &(key, value) in properties {
            builder = builder.insert(key, value);
        }
        db.create_node(label, builder.build())
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Writes a raw node under `label` carrying none of the identity spine.
    ///
    /// Returns the store's error when schema constraints are declared and
    /// reject it — which is what the enforcement test asserts on.
    pub fn plant_nonconforming_node(
        data_dir: impl AsRef<Path>,
        label: &str,
        marker: &str,
    ) -> Result<(), String> {
        // Deliberately NO `codegraph_id` / `record_type` / `schema_version`:
        // this is the shape a foreign writer produces, and it is unciteable by
        // construction (there is no handle to resolve).
        plant_raw_node(data_dir, label, &[("planted_marker", marker)])
    }

    /// Writes a raw node that HAS a `codegraph_id` but still violates the spine.
    ///
    /// The distinction matters: `plant_nonconforming_node` produces a record
    /// with no handle at all, so it can only ever land in `unresolved_samples`.
    /// This one is citable, so it exercises the sample-resolution path that
    /// turns an engine-internal entity id back into a record handle.
    pub fn plant_citable_nonconforming_node(
        data_dir: impl AsRef<Path>,
        label: &str,
        codegraph_id: &str,
    ) -> Result<(), String> {
        plant_raw_node(data_dir, label, &[("codegraph_id", codegraph_id)])
    }

    /// Writes a raw node carrying the FULL identity spine at an arbitrary
    /// `schema_version`.
    ///
    /// Exists to test the schema-bump contract at the level the constraint
    /// actually operates. `eg ingest` cannot be used for this: Egregore's own
    /// reader rejects an unknown future `(domain, kind, version)` tuple at the
    /// JSONL parse gate, long before any store write, so it would prove the
    /// reader's behaviour rather than the constraint's.
    pub fn plant_spine_node_at_version(
        data_dir: impl AsRef<Path>,
        label: &str,
        codegraph_id: &str,
        schema_version: i64,
    ) -> Result<(), String> {
        let db = open(data_dir.as_ref())?;
        let properties = aletheiadb::PropertyMapBuilder::new()
            .insert("codegraph_id", codegraph_id)
            .insert("record_type", "node")
            .insert("schema_version", schema_version)
            .insert("summary", "spine node at an arbitrary schema version")
            .build();
        db.create_node(label, properties)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Declares a schema constraint on an arbitrary label, simulating another
    /// tool that shares the data dir.
    ///
    /// Exists so the `--drop` scoping contract is testable: Egregore's own
    /// declaration path only ever touches labels in its inventory, so without
    /// this there is no way to create the foreign declaration that `--drop`
    /// must RETAIN rather than destroy.
    pub fn declare_foreign_constraint(
        data_dir: impl AsRef<Path>,
        label: &str,
        property: &str,
    ) -> Result<(), String> {
        let db = open(data_dir.as_ref())?;
        db.schema_constraint(aletheiadb::EntityKind::Node, label)
            .typed(property, aletheiadb::core::constraint::DeclaredType::String)
            .enable()
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Declares a foreign constraint whose descriptor is deliberately UNLIKE
    /// anything Egregore's own profiles emit: a required `Integer` key and a
    /// dimension-pinned `Vector` key.
    ///
    /// Exists so the before-image contract is testable at full fidelity. Every
    /// Egregore-declared property is a non-required `String`/`Integer`, so a
    /// before-image that silently dropped the type, the required flag, or the
    /// vector dimension would still look correct against Egregore's own labels.
    /// A foreign declaration is the only one `--declare` cannot rebuild, so it
    /// is the only case where the recorded descriptor is the sole route back.
    pub fn declare_foreign_typed_constraint(
        data_dir: impl AsRef<Path>,
        label: &str,
        required_int_property: &str,
        vector_property: &str,
        vector_dim: usize,
    ) -> Result<(), String> {
        let db = open(data_dir.as_ref())?;
        db.schema_constraint(aletheiadb::EntityKind::Node, label)
            .require_typed(
                required_int_property,
                aletheiadb::core::constraint::DeclaredType::Integer,
            )
            .typed(
                vector_property,
                aletheiadb::core::constraint::DeclaredType::Vector {
                    dim: Some(vector_dim),
                },
            )
            .enable()
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

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
    fixtures::plant_nonconforming_node(&data_dir, "Symbol", "PLANTED-NONCONFORMING")
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
    fixtures::plant_nonconforming_node(&data_dir, "Symbol", "PLANTED-NONCONFORMING")
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

    let refusal = fixtures::plant_nonconforming_node(&data_dir, "Symbol", "PLANTED-NONCONFORMING");
    assert!(
        refusal.is_err(),
        "with constraints declared, a spine-violating raw write must be refused"
    );
}

// ---------------------------------------------------------------------------
// AC7 - stable operator-facing error surfaces
// ---------------------------------------------------------------------------

/// Runs the command against a `--data-dir` that does not exist, returning the
/// exit code and stderr. Shared by the two configuration-specific assertions
/// below, which differ in WHICH exit-2 diagnostic is correct.
fn missing_store_run() -> (Option<i32>, String) {
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
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn missing_store_is_a_usage_error() {
    let (code, stderr) = missing_store_run();
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("nope"),
        "the diagnostic must name the path; got {stderr}"
    );
    assert!(
        stderr.contains("store_unreadable"),
        "the diagnostic must carry its stable code; got {stderr}"
    );
}

/// Built without the embedded adapter there is no store to read, so the command
/// must say exactly that — a stable, distinct code — rather than reporting a
/// missing path it never actually looked for. Same exit code, different truth.
#[cfg(not(feature = "embedded-aletheiadb"))]
#[test]
fn missing_embedded_adapter_is_a_distinct_usage_error() {
    let (code, stderr) = missing_store_run();
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("embedded_adapter_unavailable"),
        "the diagnostic must name the absent feature; got {stderr}"
    );
    assert!(
        !stderr.contains("store_unreadable"),
        "must not claim it failed to read a store it never opened; got {stderr}"
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

// ---------------------------------------------------------------------------
// Review round 1 - gaps the four review passes identified
// ---------------------------------------------------------------------------

/// A violating record that HAS a `codegraph_id` must be cited by that handle.
/// The original fixture plants a record with no handle at all, so it could only
/// ever exercise the `unresolved_samples` branch - the resolution path that
/// turns an engine entity id back into a record handle was dead in every test.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn violations_cite_offending_records_by_codegraph_id() {
    let (_temp, data_dir) = seeded_store();
    fixtures::plant_citable_nonconforming_node(&data_dir, "Symbol", "codegraph:v8:planted-citable")
        .expect("planting should succeed");

    let output = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        data_dir.to_str().expect("utf8"),
    ]);
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output.stdout).expect("utf8").trim()).expect("json");

    let symbol = report["conformance"]["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["label"].as_str() == Some("Symbol"))
        .expect("Symbol row")
        .clone();
    let violations = symbol["violations"].as_array().expect("violations");
    let cited: Vec<&str> = violations
        .iter()
        .flat_map(|v| v["sample_record_ids"].as_array().expect("ids"))
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        cited.contains(&"codegraph:v8:planted-citable"),
        "the offending record must be cited by its handle; got {cited:?}"
    );
    assert!(
        violations
            .iter()
            .all(|v| v["sample_complete"].as_bool() == Some(true)),
        "a sample below the engine bound is provably complete"
    );
}

/// A record with NO handle is counted, never leaked as an engine entity id.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn unciteable_violations_are_counted_not_leaked() {
    let (_temp, data_dir) = seeded_store();
    fixtures::plant_nonconforming_node(&data_dir, "Symbol", "PLANTED-NONCONFORMING")
        .expect("planting should succeed");

    let output = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        data_dir.to_str().expect("utf8"),
    ]);
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    let symbol = report["conformance"]["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["label"].as_str() == Some("Symbol"))
        .expect("Symbol row")
        .clone();
    let violations = symbol["violations"].as_array().expect("violations");
    assert!(
        violations
            .iter()
            .any(|v| v["unresolved_samples"].as_u64().unwrap_or(0) >= 1),
        "a handle-less offender must be counted under unresolved_samples"
    );
    // The planted marker is a record PROPERTY: it must never be echoed, and
    // neither may a bare engine entity id.
    assert!(
        !stdout.contains("PLANTED-NONCONFORMING"),
        "record property values must never reach the report"
    );
    for row in violations {
        for id in row["sample_record_ids"].as_array().expect("ids") {
            let id = id.as_str().expect("string id");
            assert!(
                id.contains(':'),
                "every cited id must be an Egregore record handle, never a raw engine id; got {id:?}"
            );
        }
    }
}

/// A store-controlled value must not be able to drive the operator's terminal
/// or forge report rows in `--format text` (the #104 doctrine, mirroring the
/// `eg audit control-catalog` precedent).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn text_output_neutralizes_control_characters_in_store_controlled_values() {
    let (_temp, data_dir) = seeded_store();
    // A foreign writer controls both the LABEL and the `codegraph_id`; both
    // reach `--format text`.
    fixtures::plant_citable_nonconforming_node(
        &data_dir,
        "Symbol",
        "aa\n    forged - missing required key [codegraph:v8:not-real]\u{1b}[2Jbb",
    )
    .expect("planting should succeed");

    let output = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        data_dir.to_str().expect("utf8"),
        "--format",
        "text",
    ]);
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(
        !stdout.contains('\u{1b}'),
        "an ANSI escape from a store value must be neutralized"
    );
    // The payload's text may still appear INLINE - that is harmless, and
    // truncating a record handle would destroy its value as a citation. What
    // must not happen is a forged ROW: the embedded newline is neutralized, so
    // no output LINE can begin with attacker-authored content.
    assert!(
        stdout
            .lines()
            .all(|line| !line.trim_start().starts_with("forged")),
        "a store value must not be able to start a new report line; got:\n{stdout}"
    );
    // The planted record violates more than one required key, so it is legitimately
    // cited on more than one line. What matters is that EVERY line carrying it is a
    // real, indented violation-detail row emitted by the renderer - never a line the
    // payload manufactured for itself.
    for line in stdout.lines().filter(|line| line.contains("forged")) {
        assert!(
            line.starts_with("    ") && line.contains(" - "),
            "payload appeared outside a violation-detail row: {line:?}"
        );
    }
}

/// A label Egregore cannot write must be surfaced as unknown, end to end -
/// previously this was only unit-tested against a hand-written vector, which
/// tested set subtraction rather than that the store is actually read.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn a_foreign_label_is_reported_as_unknown_and_is_never_scanned() {
    let (_temp, data_dir) = seeded_store();
    fixtures::plant_nonconforming_node(&data_dir, "FutureForeignKind", "irrelevant")
        .expect("planting should succeed");

    let report = report_json(&data_dir, &[]);
    assert_eq!(
        report["observed"]["unknown_node_labels"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>(),
        vec!["FutureForeignKind"]
    );
    // It is outside the inventory, so it is NOT scanned - and the report must
    // say so rather than let a passing verdict imply the store is all clean.
    assert!(
        !report["conformance"]["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .any(|row| row["label"].as_str() == Some("FutureForeignKind")),
        "a foreign label is never scanned"
    );
    assert_eq!(report["unchecked_unknown_labels"].as_bool(), Some(true));
    assert!(
        report["disclaimer"]
            .as_str()
            .expect("disclaimer")
            .contains("unknown_node_labels"),
        "the disclaimer must name unknown labels as unchecked"
    );
}

/// `--drop` evaluates no profile, so it must not ship a zeroed conformance
/// block that reads as "this store conforms".
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn drop_reports_no_conformance_verdict() {
    let (_temp, data_dir) = seeded_store();
    let dir = data_dir.to_str().expect("utf8");
    let output = run(&["audit", "schema-constraints", "--data-dir", dir, "--drop"]);
    assert_eq!(output.status.code(), Some(0));
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output.stdout).expect("utf8").trim()).expect("json");

    assert_eq!(report["action"].as_str(), Some("drop"));
    assert_eq!(report["conformance_evaluated"].as_bool(), Some(false));
    assert!(
        report.get("conformance").is_none(),
        "a run that evaluated nothing must not publish a conformance block"
    );
    assert!(
        report.get("profile_properties").is_none(),
        "a run that applied no profile must not publish its properties"
    );
    // Idempotent: dropping when nothing is declared is a clean no-op.
    assert_eq!(report["dropped_labels"].as_u64(), Some(0));
}

/// `--drop` is the inverse of `--declare`, and `--declare` only ever touches
/// Egregore's own labels. A declaration on any other label was made by
/// something else and must be retained and reported, not silently destroyed.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn drop_records_a_before_image_so_a_mistake_is_recoverable() {
    let (_temp, data_dir) = seeded_store();
    let dir = data_dir.to_str().expect("utf8");
    run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        dir,
        "--declare",
    ]);

    let output = run(&["audit", "schema-constraints", "--data-dir", dir, "--drop"]);
    assert_eq!(output.status.code(), Some(0));
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output.stdout).expect("utf8").trim()).expect("json");

    let dropped = report["dropped_constraints"].as_array().expect("array");
    assert!(
        !dropped.is_empty(),
        "the drop must record what it removed - upstream rewrites the sidecar,          so this is the only way to re-declare afterwards"
    );
    assert_eq!(
        report["dropped_labels"].as_u64(),
        Some(dropped.len() as u64)
    );
    // Each entry must be enough to reconstruct the declaration - which means the
    // COMPLETE property descriptor, not merely the key names. A name-only entry
    // cannot restore a declaration whose type or optionality is not Egregore's.
    let first = &dropped[0];
    assert!(first["label"].as_str().is_some());
    assert!(first["entity_kind"].as_str().is_some());
    let properties = first["properties"].as_array().expect("props");
    assert!(!properties.is_empty());
    for property in properties {
        assert!(
            property["property"].as_str().is_some(),
            "every descriptor names its key"
        );
        assert!(
            property["declared_type"].is_string() || property["declared_type"].is_null(),
            "the declared type is recorded (null meaning 'any type'), never omitted"
        );
        assert!(
            property["required"].is_boolean(),
            "optionality is part of the declaration and must survive the drop"
        );
        assert!(property["nullable"].is_boolean());
    }
}

/// The read-only report must leave no copy of the store behind.
///
/// The default action audits a byte-for-byte throwaway copy, and the command
/// terminates via `std::process::exit`, which skips destructors. A `TempDir`
/// guard still live at that point would strand a full copy of the store - graph
/// payloads included - in the system temp dir on a lane whose entire promise is
/// that it touches nothing, and repeated runs would accumulate them.
///
/// Asserts the property directly - the temp dir is empty afterwards - rather
/// than the mechanism, and ignores the exit code so it stays valid whichever
/// way each case resolves.
///
/// COVERAGE LIMIT, stated rather than implied: both cases below exit 0, so this
/// pins the SUCCESS path only. The deep-exit paths the fix is really about (a
/// failed store open, a failed conformance scan) need the store to fail after
/// the copy already succeeded, and there is no fault-injection seam to force
/// that - a store whose root files are overwritten with garbage still restores
/// from `indexes/` and reports cleanly, and a non-store directory is simply
/// initialised as an empty store. Those paths are handled structurally instead:
/// `try_open_store` / `try_collect_conformance` return their diagnostic so the
/// caller drops the guard before calling `usage_exit`, leaving no `process::exit`
/// reachable while a copy is live. Same limitation as the failure-after-first-
/// label declaration case.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn the_report_leaves_no_store_copy_in_the_temp_dir() {
    let (_temp, data_dir) = seeded_store();
    let scratch = tempfile::tempdir().expect("temp dir");

    let residue = |label: &str| {
        let left: Vec<String> = fs::read_dir(scratch.path())
            .expect("scratch readable")
            .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
            .collect();
        assert!(
            left.is_empty(),
            "{label} left {} entrie(s) in the temp dir: {left:?} - a skipped \
             destructor stranded the read-only store copy",
            left.len()
        );
    };

    // The success path.
    let output = Command::cargo_bin("egregore")
        .expect("binary")
        .args([
            "audit",
            "schema-constraints",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
        ])
        .env("TMPDIR", scratch.path())
        .output()
        .expect("command runs");
    assert_eq!(output.status.code(), Some(0));
    residue("a successful report");

    // A `--data-dir` that is non-empty but is not a store. Whichever way this
    // resolves - a clean report or a deep `usage_exit` - the copy must be gone.
    let bogus = tempfile::tempdir().expect("temp dir");
    fs::write(bogus.path().join("not-a-store.txt"), b"junk").expect("write");
    Command::cargo_bin("egregore")
        .expect("binary")
        .args([
            "audit",
            "schema-constraints",
            "--data-dir",
            bogus.path().to_str().expect("utf8"),
        ])
        .env("TMPDIR", scratch.path())
        .output()
        .expect("command runs");
    residue("a report over a non-store directory");
}

/// The before-image must be able to restore a declaration Egregore's own
/// `--declare` can NEVER rebuild.
///
/// For a label in Egregore's inventory a bare key list would be enough - a
/// re-run of `--declare <profile>` regenerates type and optionality from the
/// profile. `--drop --include-foreign` is the case with no such fallback: the
/// retracted declaration belongs to another tool, upstream rewrites the sidecar
/// atomically, and `dropped_constraints` is the only surviving record of it.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn the_before_image_fully_describes_a_dropped_foreign_constraint() {
    let (_temp, data_dir) = seeded_store();
    let dir = data_dir.to_str().expect("utf8");

    fixtures::declare_foreign_typed_constraint(
        &data_dir,
        "SomeOtherToolsLabel",
        "their_required_count",
        "their_embedding",
        384,
    )
    .expect("a foreign tool declares a typed constraint");

    // What the store holds BEFORE the drop, as the operator would read it.
    let before = report_json(&data_dir, &[]);
    let declared = before["declared_constraints"]
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["label"].as_str() == Some("SomeOtherToolsLabel"))
        .expect("the foreign declaration is visible before the drop")
        .clone();

    let output = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        dir,
        "--drop",
        "--include-foreign",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output.stdout).expect("utf8").trim()).expect("json");

    let dropped = report["dropped_constraints"]
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["label"].as_str() == Some("SomeOtherToolsLabel"))
        .expect("the foreign declaration was retracted and recorded");

    assert_eq!(
        dropped, &declared,
        "the before-image must match what the store held, field for field - \
         anything less cannot restore the declaration"
    );

    // Pin the specific fields a name-only before-image would have lost, so the
    // equality above cannot pass vacuously with both sides degraded.
    let properties = dropped["properties"].as_array().expect("props");
    let by_key = |key: &str| {
        properties
            .iter()
            .find(|property| property["property"].as_str() == Some(key))
            .unwrap_or_else(|| panic!("descriptor for {key}"))
            .clone()
    };

    let counted = by_key("their_required_count");
    assert_eq!(counted["declared_type"].as_str(), Some("int"));
    assert_eq!(
        counted["required"].as_bool(),
        Some(true),
        "a required foreign key must not come back optional"
    );

    let embedding = by_key("their_embedding");
    assert_eq!(embedding["declared_type"].as_str(), Some("vector"));
    assert_eq!(
        embedding["vector_dim"].as_u64(),
        Some(384),
        "the pinned dimension is lost by type_name() alone, so it is captured \
         separately - without it a restored constraint silently widens to accept \
         any dimension"
    );

    // And the store really is clear of it now.
    let after = report_json(&data_dir, &[]);
    assert!(
        after["declared_constraints"]
            .as_array()
            .expect("array")
            .iter()
            .all(|entry| entry["label"].as_str() != Some("SomeOtherToolsLabel"))
    );
}

/// The declared count must be the whole writable surface, not merely "a lot".
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn declare_covers_the_entire_writable_inventory() {
    let (_temp, data_dir) = seeded_store();
    let dir = data_dir.to_str().expect("utf8");
    let output = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        dir,
        "--declare",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output.stdout).expect("utf8").trim()).expect("json");

    let expected = NodeKind::ALL.len() + 1 + EdgeLabel::ALL.len();
    assert_eq!(
        report["declared_labels"].as_u64(),
        Some(expected as u64),
        "every writable label - including ones the store holds nothing of yet"
    );
    assert_eq!(report["declaration_refusal"], serde_json::Value::Null);
    assert_eq!(
        report["declared_constraints"]
            .as_array()
            .expect("array")
            .len(),
        expected
    );
}

/// Open question 3, EXECUTED rather than asserted: a declared store must still
/// accept a record whose per-domain `schema_version` has been BUMPED, because
/// the constraint declares the TYPE (`Integer`) and a bump only changes the
/// VALUE. This is the one executable proof of the schema-versioning table's
/// first row.
///
/// It writes through the raw fixture rather than `eg ingest` deliberately:
/// Egregore's own reader rejects an unknown future `(domain, kind, version)`
/// tuple at the JSONL parse gate, so an ingest would prove the READER's
/// behaviour, not the constraint's.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn a_declared_store_accepts_a_bumped_schema_version() {
    let (_temp, data_dir) = seeded_store();
    let dir = data_dir.to_str().expect("utf8");
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
        "declaration must succeed before the bump can be tested"
    );

    fixtures::plant_spine_node_at_version(
        &data_dir,
        "Symbol",
        "codegraph:v999:future-symbol",
        i64::from(SCHEMA_VERSION) + 1,
    )
    .expect("a bumped schema_version must not violate a constraint on its TYPE");

    // And the store still reports clean afterwards.
    let report = report_json(&data_dir, &[]);
    assert_eq!(
        report["conformance"]["entities_non_conforming"].as_u64(),
        Some(0)
    );
}

/// An unknown `--profile` is a usage error naming the known profiles.
#[test]
fn unknown_profile_is_a_usage_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let output = Command::cargo_bin("egregore")
        .expect("binary")
        .args([
            "audit",
            "schema-constraints",
            "--data-dir",
            temp.path().to_str().expect("utf8"),
            "--profile",
            "not-a-profile",
        ])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown_profile"), "got {stderr}");
    assert!(stderr.contains("spine"), "must list the known profiles");
}

/// `--declare` and `--drop` must be byte-identical across repeated runs too -
/// previously only the report action was pinned.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn declare_and_drop_output_is_byte_identical_across_runs() {
    // One store, two full cycles: the report echoes `data_dir`, so two DIFFERENT
    // temp stores would differ for an uninteresting reason.
    let (_temp, data_dir) = seeded_store();
    let dir = data_dir.to_str().expect("utf8").to_owned();
    let cycle = || {
        let declared = run(&[
            "audit",
            "schema-constraints",
            "--data-dir",
            &dir,
            "--declare",
        ]);
        let dropped = run(&["audit", "schema-constraints", "--data-dir", &dir, "--drop"]);
        assert_eq!(declared.status.code(), Some(0));
        assert_eq!(dropped.status.code(), Some(0));
        (declared.stdout, dropped.stdout)
    };
    let (first_declare, first_drop) = cycle();
    let (second_declare, second_drop) = cycle();
    assert_eq!(
        first_declare, second_declare,
        "declare output must be stable across runs"
    );
    assert_eq!(
        first_drop, second_drop,
        "drop output must be stable across runs"
    );
}

/// `--drop` must not destroy a declaration Egregore did not make. It is the
/// inverse of `--declare`, and `--declare` only ever touches labels in
/// Egregore's own inventory; another tool sharing the data dir owns its own.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn drop_retains_a_foreign_declaration_unless_explicitly_widened() {
    let (_temp, data_dir) = seeded_store();
    let dir = data_dir.to_str().expect("utf8");

    run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        dir,
        "--declare",
    ]);
    fixtures::declare_foreign_constraint(&data_dir, "SomeOtherToolsLabel", "their_property")
        .expect("a foreign tool declares its own constraint");

    // Default drop: Egregore's own go, the foreign one stays and is reported.
    let output = run(&["audit", "schema-constraints", "--data-dir", dir, "--drop"]);
    assert_eq!(output.status.code(), Some(0));
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output.stdout).expect("utf8").trim()).expect("json");

    let retained: Vec<&str> = report["foreign_constraints_retained"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|entry| entry["label"].as_str())
        .collect();
    assert_eq!(
        retained,
        vec!["SomeOtherToolsLabel"],
        "a declaration outside Egregore's inventory must be retained and reported"
    );
    assert!(
        report["dropped_constraints"]
            .as_array()
            .expect("array")
            .iter()
            .all(|entry| entry["label"].as_str() != Some("SomeOtherToolsLabel"))
    );
    // It really is still declared.
    let after = report_json(&data_dir, &[]);
    assert!(
        after["declared_constraints"]
            .as_array()
            .expect("array")
            .iter()
            .any(|entry| entry["label"].as_str() == Some("SomeOtherToolsLabel"))
    );

    // `--include-foreign` widens the retraction to everything.
    let widened = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        dir,
        "--drop",
        "--include-foreign",
    ]);
    assert_eq!(widened.status.code(), Some(0));
    let widened_report: serde_json::Value =
        serde_json::from_str(String::from_utf8(widened.stdout).expect("utf8").trim())
            .expect("json");
    assert!(
        widened_report["dropped_constraints"]
            .as_array()
            .expect("array")
            .iter()
            .any(|entry| entry["label"].as_str() == Some("SomeOtherToolsLabel")),
        "--include-foreign must retract it"
    );
    let final_report = report_json(&data_dir, &[]);
    assert!(
        final_report["declared_constraints"]
            .as_array()
            .expect("array")
            .is_empty()
    );
}

/// `--include-foreign` is meaningless without `--drop` and must be refused
/// rather than silently ignored.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn include_foreign_without_drop_is_a_usage_error() {
    let (_temp, data_dir) = seeded_store();
    let output = run(&[
        "audit",
        "schema-constraints",
        "--data-dir",
        data_dir.to_str().expect("utf8"),
        "--include-foreign",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unsupported_combination"),
        "must carry the stable code"
    );
}
