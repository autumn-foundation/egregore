#![allow(missing_docs)]

use std::{collections::BTreeMap, fs};

use assert_cmd::Command;
use predicates::prelude::*;

use aletheia_egregore::ir::{
    EgregoreGit, GraphRecord, NodeKind, Producer, ProducerKind, stable_id,
};

/// PR-1 (docs/prd/0001-codebase-knowledge-graph.md): two records produced by
/// different binary versions over identical input MUST have identical stable IDs.
/// Documented as the non-identity rule in docs/schema/producer-version.md.
#[test]
fn producer_non_identity_rule() {
    // IDs are computed independently from identical inputs so that if stable_id
    // composition ever accidentally started including producer metadata, the two
    // computed IDs would diverge and the assert_eq below would catch it.
    let producer_a = Producer {
        egregore_version: "0.1.0".to_owned(),
        egregore_git: None,
        producer_kind: ProducerKind::CodeGraphExtractor,
        producer_components: BTreeMap::from([
            ("tree_sitter".to_owned(), "0.26.8".to_owned()),
            ("tree_sitter_rust".to_owned(), "0.24.2".to_owned()),
        ]),
        producer_started_at: "2025-01-01T00:00:00Z".to_owned(),
    };

    let producer_b = Producer {
        egregore_version: "0.2.0".to_owned(),
        egregore_git: Some(EgregoreGit {
            commit: "deadbeef1234".to_owned(),
            dirty: true,
        }),
        producer_kind: ProducerKind::CodeGraphExtractor,
        producer_components: BTreeMap::from([
            ("tree_sitter".to_owned(), "0.27.0".to_owned()),
            ("tree_sitter_rust".to_owned(), "0.25.0".to_owned()),
        ]),
        producer_started_at: "2026-01-01T00:00:00Z".to_owned(),
    };

    let record_a = GraphRecord::node(
        stable_id(&["test-repo", "src/lib.rs", "fn", "run"]),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("run".to_owned()),
        "fn run".to_owned(),
    )
    .with_producer(producer_a);

    let record_b = GraphRecord::node(
        stable_id(&["test-repo", "src/lib.rs", "fn", "run"]),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("run".to_owned()),
        "fn run".to_owned(),
    )
    .with_producer(producer_b);

    assert_eq!(
        record_a.id(),
        record_b.id(),
        "stable ID must not depend on the producer envelope"
    );

    assert_ne!(
        record_a.producer(),
        record_b.producer(),
        "producer envelopes from different binary versions must differ"
    );
}

/// Legacy-record policy: a JSONL mixing one producer-stamped record and one
/// legacy record (no `producer` field) MUST inspect cleanly, with the legacy
/// record reported under the `legacy_pre_v1` bucket.
/// Documented in docs/schema/producer-version.md §Legacy-Record Policy.
#[test]
fn legacy_record_inspect_policy() {
    let legacy = concat!(
        r#"{"record_type":"node","id":"codegraph:v4:"#,
        r#"0000000000000000000000000000000000000000000000000000000000000001","#,
        r#""kind":"File","schema_version":4,"summary":"legacy file","name":"src/lib.rs"}"#
    );
    let stamped = concat!(
        r#"{"record_type":"node","id":"codegraph:v4:"#,
        r#"0000000000000000000000000000000000000000000000000000000000000002","#,
        r#""kind":"File","schema_version":4,"#,
        r#""producer":{"egregore_version":"0.1.0","producer_kind":"code_graph_extractor","#,
        r#""producer_components":{"tree_sitter":"0.26.8"},"#,
        r#""producer_started_at":"2026-01-01T00:00:00Z"},"#,
        r#""summary":"stamped file","name":"src/main.rs"}"#
    );

    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("mixed.jsonl");
    fs::write(&path, format!("{legacy}\n{stamped}\n")).expect("write JSONL");

    Command::cargo_bin("egregore")
        .expect("binary")
        .arg("inspect")
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "producer_kind code_graph_extractor: 1",
        ))
        .stdout(predicate::str::contains("producer_kind legacy_pre_v1: 1"));
}
