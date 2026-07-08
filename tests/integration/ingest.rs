#![allow(missing_docs)]

use std::{fs, path::PathBuf};

#[cfg(feature = "embedded-aletheiadb")]
use std::{
    path::Path,
    process::{Command as ProcessCommand, Stdio},
};

#[cfg(feature = "embedded-aletheiadb")]
use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink, records_from_jsonl};
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
use aletheia_egregore::embeddings::{
    DEFAULT_EMBEDDING_MODEL_DIMENSIONS, EmbeddingVectorKey, EmbeddingVectorMap,
};
#[cfg(feature = "embedded-aletheiadb")]
use aletheia_egregore::{
    EdgeLabel, Graph, GraphRecord, NodeKind, SCHEMA_VERSION, SourceSpan, TemporalMetadata,
    scan_repository_history, stable_id,
};
use aletheia_egregore::{
    adapters::{FakeSink, ingest_records},
    scan_repository,
};
use assert_cmd::Command;
use predicates::prelude::*;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

#[test]
fn adapter_reports_partial_success() {
    let records = scan_repository(fixture_repo())
        .expect("fixture repo should scan")
        .records()
        .to_vec();
    let original = records.clone();
    let mut sink = FakeSink::fail_after(2);

    let report = ingest_records(&records, &mut sink);

    assert_eq!(records, original, "ingest must not mutate retry input");
    assert_eq!(report.attempted, records.len());
    assert_eq!(report.succeeded, 2);
    assert_eq!(report.failed, records.len() - 2);
    assert!(!report.is_success());
    assert!(report.failures[0].message.contains("fake adapter failure"));
}

#[test]
fn dry_run_ingest_preserves_jsonl_for_retry() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let before = fs::read_to_string(&graph_path).expect("scan should write graph JSONL");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains("attempted:"))
        .stdout(predicate::str::contains("succeeded:"))
        .stdout(predicate::str::contains("failed: 0"))
        .stderr(predicate::str::is_empty());

    let after = fs::read_to_string(&graph_path).expect("graph JSONL should still exist");
    assert_eq!(before, after, "dry-run ingest must preserve retry input");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_cli_ingest_accepts_data_dir() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("aletheia-store");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("failed: 0"))
        .stderr(predicate::str::is_empty());
}

#[cfg(feature = "embeddings")]
#[test]
fn embed_flag_is_rejected_for_dry_run_ingest() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("dry-run")
        .arg("--embed")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--embed requires --adapter embedded",
        ));
}

#[cfg(feature = "embeddings")]
#[test]
fn embed_flag_is_rejected_for_daemon_ingest() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("daemon-store");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("embed-daemon-reject")
        .arg("--embed")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--embed requires --adapter embedded",
        ));
}

#[cfg(feature = "embeddings")]
#[test]
fn embedded_embed_ingest_without_candidates_creates_empty_queryable_index() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("repo-only.graph.jsonl");
    let data_dir = temp.path().join("repo-only-store");
    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "repo-only"]),
        NodeKind::Repository,
        None,
        None,
        Some("repo-only".to_owned()),
        "Repository repo-only".to_owned(),
    ));
    fs::write(
        &graph_path,
        graph.to_jsonl().expect("repo-only graph should serialize"),
    )
    .expect("repo-only graph should be written");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--embed")
        .assert()
        .success()
        .stdout(predicate::str::contains("failed: 0"))
        .stderr(predicate::str::is_empty());

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
    let query_vector = vec![0.0; DEFAULT_EMBEDDING_MODEL_DIMENSIONS];
    let matches = sink
        .semantic_search(&query_vector, 10)
        .expect("zero-candidate --embed ingest should still create a queryable vector index");
    assert!(
        matches.is_empty(),
        "zero-candidate semantic stores should return clean empty results"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_ingest_reads_back_and_traverses_repository_file_symbol() {
    let jsonl = scan_repository(fixture_repo())
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = records_from_jsonl(&jsonl).expect("graph JSONL should parse");
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let mut sink = EmbeddedAletheiaSink::open(temp.path()).expect("embedded store should open");

    let report = ingest_records(&records, &mut sink);

    assert!(report.is_success(), "{report:?}");
    let repository = records
        .iter()
        .find(|record| record.node_kind_name() == Some("Repository"))
        .expect("fixture should include repository node");
    assert_eq!(
        sink.read_back(repository.id()).expect("read back"),
        Some(repository.clone())
    );
    assert!(
        sink.has_repository_file_symbol_path(repository.id())
            .expect("embedded traversal should run"),
        "embedded store should contain Repository -> File -> Symbol path"
    );
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn semantic_search_returns_latest_live_records_only() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("semantic-live-store");
    let stable_symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
    let old_symbol = symbol_record_with_span(
        &stable_symbol_id,
        "stable",
        "old stable implementation",
        10,
        temporal("aaaaaaaa", "2026-01-01T00:00:00Z"),
    );
    let latest_symbol = symbol_record_with_span(
        &stable_symbol_id,
        "stable",
        "latest stable implementation",
        40,
        temporal("bbbbbbbb", "2026-01-02T00:00:00Z"),
    );
    let deleted_symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "deleted"]);
    let deleted_symbol = current_symbol_record(&deleted_symbol_id, "deleted", "deleted symbol", 20);
    let tombstone = GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &deleted_symbol_id]),
        schema_version: SCHEMA_VERSION,
        deleted_id: deleted_symbol_id.clone(),
        summary: "deleted symbol tombstone".to_owned(),
        producer: None,
    };
    let superseded_symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "superseded"]);
    let superseding_symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "superseding"]);
    let superseding_symbol =
        current_symbol_record(&superseding_symbol_id, "superseding", "live successor", 30);
    let superseded_symbol = superseded_symbol(
        &superseded_symbol_id,
        "superseded",
        "superseded predecessor",
        &superseding_symbol_id,
    );

    let mut vectors = EmbeddingVectorMap::new();
    vectors.insert(
        EmbeddingVectorKey::from_record(&old_symbol).expect("old symbol should be embeddable"),
        vec![1.0, 0.0],
    );
    vectors.insert(
        EmbeddingVectorKey::from_record(&latest_symbol)
            .expect("latest symbol should be embeddable"),
        vec![0.7, 0.3],
    );
    vectors.insert(
        EmbeddingVectorKey::from_record(&deleted_symbol)
            .expect("deleted symbol should be embeddable"),
        vec![1.0, 0.0],
    );
    vectors.insert(
        EmbeddingVectorKey::from_record(&superseded_symbol)
            .expect("superseded symbol should be embeddable"),
        vec![1.0, 0.0],
    );
    vectors.insert(
        EmbeddingVectorKey::from_record(&superseding_symbol)
            .expect("superseding symbol should be embeddable"),
        vec![0.7, 0.3],
    );

    let mut sink = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
        .expect("embedded semantic store should open");
    for record in [
        &old_symbol,
        &latest_symbol,
        &deleted_symbol,
        &tombstone,
        &superseded_symbol,
        &superseding_symbol,
    ] {
        sink.write_record(record)
            .expect("semantic fixture record should write");
    }

    let matches = sink
        .semantic_search(&[1.0, 0.0], 10)
        .expect("semantic search should succeed");

    let stable_matches = matches
        .iter()
        .filter(|m| m.record_id == stable_symbol_id)
        .collect::<Vec<_>>();
    assert_eq!(
        stable_matches.len(),
        1,
        "semantic search must deduplicate physical versions by stable record_id"
    );
    assert_eq!(
        stable_matches[0].span.map(|span| span.end_byte),
        Some(40),
        "semantic search must return the latest live physical observation"
    );
    assert!(
        matches.iter().all(|m| m.record_id != deleted_symbol_id),
        "semantic search must not return active tombstones"
    );
    assert!(
        matches.iter().all(|m| m.record_id != superseded_symbol_id),
        "semantic search must not return superseded nodes"
    );
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn semantic_ingest_backfills_vectors_for_matched_existing_nodes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("matched-semantic-store");
    let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "matched"]);
    let symbol = current_symbol_record(&symbol_id, "matched", "matched existing symbol", 50);

    {
        let mut structural =
            EmbeddedAletheiaSink::open(&data_dir).expect("structural store should open");
        let report = ingest_records(std::slice::from_ref(&symbol), &mut structural);
        assert!(report.is_success(), "{report:?}");
        structural
            .persist_indexes()
            .expect("structural indexes should persist");
    }

    let mut vectors = EmbeddingVectorMap::new();
    vectors.insert(
        EmbeddingVectorKey::from_record(&symbol).expect("symbol should be embeddable"),
        vec![1.0, 0.0],
    );
    let mut semantic = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
        .expect("semantic store should reopen");

    let report = ingest_records(std::slice::from_ref(&symbol), &mut semantic);
    assert!(report.is_success(), "{report:?}");

    let matches = semantic
        .semantic_search(&[1.0, 0.0], 10)
        .expect("semantic search should succeed");
    assert!(
        matches.iter().any(|m| m.record_id == symbol_id),
        "matched structural nodes must be re-embedded when semantic ingest is requested"
    );
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn non_embed_reingest_preserves_existing_embedding_on_latest_node() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("semantic-preserve-store");
    let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "preserved"]);
    let original = current_symbol_record(&symbol_id, "preserved", "original semantic symbol", 20);
    let updated = current_symbol_record(&symbol_id, "preserved", "updated semantic symbol", 80);

    let mut vectors = EmbeddingVectorMap::new();
    vectors.insert(
        EmbeddingVectorKey::from_record(&original).expect("symbol should be embeddable"),
        vec![1.0, 0.0],
    );
    {
        let mut semantic = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
            .expect("semantic store should open");
        let report = ingest_records(std::slice::from_ref(&original), &mut semantic);
        assert!(report.is_success(), "{report:?}");
        assert!(
            semantic
                .semantic_search(&[1.0, 0.0], 10)
                .expect("semantic search should succeed")
                .iter()
                .any(|m| m.record_id == symbol_id),
            "initial semantic ingest should make the record searchable"
        );
        semantic
            .persist_indexes()
            .expect("semantic indexes should persist");
    }

    let mut structural =
        EmbeddedAletheiaSink::open(&data_dir).expect("structural store should reopen");
    let report = ingest_records(std::slice::from_ref(&updated), &mut structural);
    assert!(report.is_success(), "{report:?}");
    assert_eq!(
        structural.read_back(&symbol_id).expect("read back updated"),
        Some(updated)
    );

    let semantic_matches = structural
        .semantic_search(&[1.0, 0.0], 10)
        .expect("semantic search should succeed after structural reingest");
    let preserved_match = semantic_matches
        .iter()
        .find(|m| m.record_id == symbol_id)
        .expect("non-embed rewrite should preserve semantic coverage for latest node");
    assert_eq!(
        preserved_match.span.map(|span| span.end_byte),
        Some(80),
        "semantic search should return the rewritten latest node"
    );
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn embed_reingest_reopens_existing_semantic_store_without_reenabling_index_error() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("repeat-semantic-store");
    let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "repeat-semantic"]);
    let symbol = current_symbol_record(&symbol_id, "repeat", "repeat semantic symbol", 20);

    let mut vectors = EmbeddingVectorMap::new();
    vectors.insert(
        EmbeddingVectorKey::from_record(&symbol).expect("symbol should be embeddable"),
        vec![1.0, 0.0],
    );

    {
        let mut first = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors.clone(), 2)
            .expect("first semantic store open should succeed");
        let report = ingest_records(std::slice::from_ref(&symbol), &mut first);
        assert!(report.is_success(), "{report:?}");
        first
            .persist_indexes()
            .expect("semantic indexes should persist");
    }

    let mut second = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
        .expect("repeat semantic store open should reuse the existing vector index");
    let report = ingest_records(std::slice::from_ref(&symbol), &mut second);
    assert!(report.is_success(), "{report:?}");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_history_ingest_traverses_commit_change_symbol() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_history_repo(&repo);
    let records = scan_repository_history(&repo)
        .expect("history should scan")
        .records()
        .to_vec();
    let mut sink =
        EmbeddedAletheiaSink::open(temp.path().join("store")).expect("embedded store should open");

    let report = ingest_records(&records, &mut sink);

    assert!(report.is_success(), "{report:?}");
    let commit = records
        .iter()
        .find(|record| record.node_kind_name() == Some("Commit"))
        .expect("history should include a commit");
    assert!(
        sink.has_commit_change_symbol_path(commit.id())
            .expect("embedded traversal should run"),
        "embedded store should contain Commit -> Change -> Symbol path"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_read_back_reconstructs_persisted_node_and_edge_after_reopen() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_history_repo(&repo);
    let records = scan_repository_history(&repo)
        .expect("history should scan")
        .records()
        .to_vec();
    let data_dir = temp.path().join("store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    let report = ingest_records(&records, &mut sink);

    assert!(report.is_success(), "{report:?}");
    sink.persist_indexes()
        .expect("embedded indexes should persist");
    drop(sink);

    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
    let symbol = records
        .iter()
        .find(|record| {
            matches!(
                record,
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name: Some(name),
                    temporal: Some(_),
                    span: Some(_),
                    symbol_kind: Some(_),
                    ..
                } if name == "renamed"
            )
        })
        .expect("history should include renamed symbol");
    let defines_edge = records
        .iter()
        .find(|record| {
            matches!(
                record,
                GraphRecord::Edge {
                    label: EdgeLabel::Defines,
                    target,
                    temporal: Some(_),
                    ..
                } if target == symbol.id()
            )
        })
        .expect("history should include a temporal DEFINES edge");

    assert_eq!(
        reopened.read_back(symbol.id()).expect("node read-back"),
        Some(symbol.clone())
    );
    assert_eq!(
        reopened
            .read_back(defines_edge.id())
            .expect("edge read-back"),
        Some(defines_edge.clone())
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_repeated_current_tree_ingest_replaces_duplicate_records_after_reopen() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("repeated-current-tree-store");
    let file_id = stable_id(&["node", "file", "src/lib.rs"]);
    let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
    let original_records = [
        current_file_record(&file_id, "original current file"),
        current_symbol_record(&symbol_id, "stable", "original current symbol", 20),
        current_defines_edge(&file_id, &symbol_id, "original current edge"),
    ];
    let updated_symbol = current_symbol_record(&symbol_id, "stable", "updated current symbol", 42);
    let updated_edge = current_defines_edge(&file_id, &symbol_id, "updated current edge");
    let updated_records = [
        current_file_record(&file_id, "updated current file"),
        updated_symbol.clone(),
        updated_edge.clone(),
    ];
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    let first_report = ingest_records(&original_records, &mut sink);
    assert!(first_report.is_success(), "{first_report:?}");
    let second_report = ingest_records(&updated_records, &mut sink);
    assert!(second_report.is_success(), "{second_report:?}");
    assert_eq!(
        sink.read_back(&symbol_id)
            .expect("same-process node read-back"),
        Some(updated_symbol.clone())
    );
    assert_eq!(
        sink.read_back(updated_edge.id())
            .expect("same-process edge read-back"),
        Some(updated_edge.clone())
    );
    sink.persist_indexes()
        .expect("embedded indexes should persist");
    drop(sink);

    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");

    assert_eq!(
        reopened.read_back(&symbol_id).expect("node read-back"),
        Some(updated_symbol)
    );
    assert_eq!(
        reopened
            .read_back(updated_edge.id())
            .expect("edge read-back"),
        Some(updated_edge)
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_read_back_after_reopen_uses_latest_temporal_observation() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("temporal-read-back-store");
    let file_id = stable_id(&["node", "file", "src/lib.rs"]);
    let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
    let latest = temporal("aaaaaaaa", "2026-01-01T23:00:00-05:00");
    let older = temporal("zzzzzzzz", "2026-01-02T01:00:00+00:00");
    let latest_file = file_record(&file_id, latest.clone());
    let older_file = file_record(&file_id, older.clone());
    let latest_symbol = symbol_record(&symbol_id, "stable", "latest stable symbol", latest.clone());
    let older_symbol = symbol_record(&symbol_id, "stable", "older stable symbol", older.clone());
    let latest_edge = defines_edge(&file_id, &symbol_id, "latest defines edge", latest);
    let older_edge = defines_edge(&file_id, &symbol_id, "older defines edge", older);
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    // Write newest first so a read-back implementation that falls through to
    // storage iteration or insertion order will reconstruct the wrong commit.
    for record in [
        &latest_file,
        &latest_symbol,
        &latest_edge,
        &older_file,
        &older_symbol,
        &older_edge,
    ] {
        sink.write_record(record).expect("record should write");
    }
    sink.persist_indexes()
        .expect("embedded indexes should persist");
    drop(sink);

    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");

    assert_eq!(
        reopened.read_back(&symbol_id).expect("node read-back"),
        Some(latest_symbol)
    );
    assert_eq!(
        reopened
            .read_back(latest_edge.id())
            .expect("edge read-back"),
        Some(latest_edge)
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_same_process_read_back_uses_latest_temporal_observation() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("same-process-temporal-read-back-store");
    let file_id = stable_id(&["node", "file", "src/lib.rs"]);
    let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
    let latest = temporal("aaaaaaaa", "2026-01-01T23:00:00-05:00");
    let older = temporal("zzzzzzzz", "2026-01-02T01:00:00+00:00");
    let latest_file = file_record(&file_id, latest.clone());
    let older_file = file_record(&file_id, older.clone());
    let latest_symbol = symbol_record(&symbol_id, "stable", "latest stable symbol", latest.clone());
    let older_symbol = symbol_record(&symbol_id, "stable", "older stable symbol", older.clone());
    let latest_edge = defines_edge(&file_id, &symbol_id, "latest defines edge", latest);
    let older_edge = defines_edge(&file_id, &symbol_id, "older defines edge", older);
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    for record in [
        &latest_file,
        &latest_symbol,
        &latest_edge,
        &older_file,
        &older_symbol,
        &older_edge,
    ] {
        sink.write_record(record).expect("record should write");
    }

    assert_eq!(
        sink.read_back(&symbol_id).expect("node read-back"),
        Some(latest_symbol)
    );
    assert_eq!(
        sink.read_back(latest_edge.id()).expect("edge read-back"),
        Some(latest_edge)
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_read_back_breaks_valid_time_ties_by_observed_at() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("temporal-tie-read-back-store");
    let file_id = stable_id(&["node", "file", "src/lib.rs"]);
    let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
    let valid_time = "2026-01-01T00:00:00Z";
    let older = temporal_observed("zzzzzzzz", valid_time, "2026-01-01T00:00:01Z");
    let later = temporal_observed("aaaaaaaa", valid_time, "2026-01-01T00:00:02Z");
    let older_file = file_record(&file_id, older.clone());
    let later_file = file_record(&file_id, later.clone());
    let older_symbol = symbol_record(&symbol_id, "stable", "older observed symbol", older.clone());
    let later_symbol = symbol_record(&symbol_id, "stable", "later observed symbol", later.clone());
    let older_edge = defines_edge(&file_id, &symbol_id, "older observed edge", older);
    let later_edge = defines_edge(&file_id, &symbol_id, "later observed edge", later);
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    for record in [
        &older_file,
        &older_symbol,
        &older_edge,
        &later_file,
        &later_symbol,
        &later_edge,
    ] {
        sink.write_record(record).expect("record should write");
    }
    sink.persist_indexes()
        .expect("embedded indexes should persist");
    drop(sink);

    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");

    assert_eq!(
        reopened.read_back(&symbol_id).expect("node read-back"),
        Some(later_symbol)
    );
    assert_eq!(
        reopened.read_back(later_edge.id()).expect("edge read-back"),
        Some(later_edge)
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_reopened_sink_appends_temporal_edge_for_persisted_nodes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("append-after-reopen-store");
    let file_id = stable_id(&["node", "file", "src/lib.rs"]);
    let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
    let latest = temporal("aaaaaaaa", "2026-01-01T23:00:00-05:00");
    let older = temporal("zzzzzzzz", "2026-01-02T01:00:00+00:00");
    let latest_file = file_record(&file_id, latest.clone());
    let older_file = file_record(&file_id, older);
    let latest_symbol = symbol_record(&symbol_id, "stable", "latest stable symbol", latest.clone());
    let older_symbol = symbol_record(
        &symbol_id,
        "stable",
        "older stable symbol",
        temporal("zzzzzzzz", "2026-01-02T01:00:00+00:00"),
    );
    let appended_edge = defines_edge(&file_id, &symbol_id, "appended after reopen", latest);
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    for record in [&latest_file, &latest_symbol, &older_file, &older_symbol] {
        sink.write_record(record).expect("node record should write");
    }
    sink.persist_indexes()
        .expect("embedded indexes should persist");
    drop(sink);

    let mut reopened = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
    reopened
        .write_record(&appended_edge)
        .expect("edge should resolve persisted endpoint nodes after reopen");
    reopened
        .persist_indexes()
        .expect("appended edge should persist");
    drop(reopened);

    let db = reopen_embedded_db(&data_dir);
    let edge_id = edge_id_by_codegraph_id(&db, appended_edge.id());
    let source_commit = edge_endpoint_git_commit(&db, edge_id, EdgeEndpoint::Source);
    let target_commit = edge_endpoint_git_commit(&db, edge_id, EdgeEndpoint::Target);

    assert_eq!(source_commit.as_deref(), Some("aaaaaaaa"));
    assert_eq!(target_commit.as_deref(), Some("aaaaaaaa"));
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_read_back_after_reopen_matches_latest_history_jsonl_observation() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [_first, second] = seed_stable_symbol_history_repo(&repo);
    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history should serialize");
    let records = records_from_jsonl(&jsonl).expect("history JSONL should parse");
    let latest_symbol = records
        .iter()
        .find(|record| {
            matches!(
                record,
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name: Some(name),
                    temporal: Some(temporal),
                    ..
                } if name == "stable" && temporal.git_commit == second
            )
        })
        .expect("history should include latest stable symbol")
        .clone();
    let latest_defines_edge = records
        .iter()
        .find(|record| {
            matches!(
                record,
                GraphRecord::Edge {
                    label: EdgeLabel::Defines,
                    target,
                    temporal: Some(temporal),
                    ..
                } if target == latest_symbol.id() && temporal.git_commit == second
            )
        })
        .expect("history should include latest temporal DEFINES edge")
        .clone();
    let data_dir = temp.path().join("history-jsonl-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    let report = ingest_records(&records, &mut sink);

    assert!(report.is_success(), "{report:?}");
    sink.persist_indexes()
        .expect("embedded indexes should persist");
    drop(sink);

    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");

    assert_eq!(
        reopened
            .read_back(latest_symbol.id())
            .expect("node read-back"),
        Some(latest_symbol)
    );
    assert_eq!(
        reopened
            .read_back(latest_defines_edge.id())
            .expect("edge read-back"),
        Some(latest_defines_edge)
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_history_edges_attach_to_matching_temporal_symbol_node() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, second] = seed_stable_symbol_history_repo(&repo);
    let records = scan_repository_history(&repo)
        .expect("history should scan")
        .records()
        .to_vec();
    let data_dir = temp.path().join("temporal-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    let report = ingest_records(&records, &mut sink);

    assert!(report.is_success(), "{report:?}");
    sink.persist_indexes()
        .expect("embedded indexes should persist");
    drop(sink);

    let db = reopen_embedded_db(&data_dir);
    // Repo has no remote; identity derived from root commit SHA (= first commit).
    let repository_id = stable_id(&["repository", "local-root-commit", &first]);
    let first_commit_id = stable_id(&["node", "commit", &repository_id, &first]);
    let symbol_observation_commits =
        changed_symbol_git_commits_for_commit(&db, &first_commit_id, "stable");

    assert!(
        symbol_observation_commits.contains(&first),
        "first commit should point at the first temporal stable symbol, got {symbol_observation_commits:?}"
    );
    assert!(
        !symbol_observation_commits.contains(&second),
        "first commit was wired to a later stable symbol observation: {symbol_observation_commits:?}"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_tombstone_read_back_survives_persist_and_reopen() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("tombstone-store");
    let deleted_id = stable_id(&["node", "file", "src/deleted.rs"]);
    let tombstone = GraphRecord::Tombstone {
        id: stable_id(&["tombstone", "file", "src/deleted.rs", &deleted_id]),
        schema_version: SCHEMA_VERSION,
        deleted_id,
        summary: "Removed source file src/deleted.rs".to_owned(),
        producer: None,
    };
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    let report = ingest_records(std::slice::from_ref(&tombstone), &mut sink);

    assert!(report.is_success(), "{report:?}");
    assert_eq!(
        sink.read_back(tombstone.id())
            .expect("same-process read-back"),
        Some(tombstone.clone())
    );
    sink.persist_indexes()
        .expect("embedded indexes should persist");
    drop(sink);

    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");

    assert_eq!(
        reopened
            .read_back(tombstone.id())
            .expect("reopened tombstone read-back"),
        Some(tombstone)
    );
}

#[cfg(feature = "embedded-aletheiadb")]
fn file_record(id: &str, temporal: TemporalMetadata) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        Some("src/lib.rs".to_owned()),
        "Rust source file src/lib.rs".to_owned(),
    )
    .with_temporal(temporal)
}

#[cfg(feature = "embedded-aletheiadb")]
fn current_file_record(id: &str, summary: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        Some("src/lib.rs".to_owned()),
        summary.to_owned(),
    )
}

#[cfg(feature = "embedded-aletheiadb")]
fn symbol_record(id: &str, name: &str, summary: &str, temporal: TemporalMetadata) -> GraphRecord {
    GraphRecord::symbol(
        id.to_owned(),
        "function",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 20,
            start_line: 1,
            end_line: 1,
        },
        name.to_owned(),
        summary.to_owned(),
    )
    .with_temporal(temporal)
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn symbol_record_with_span(
    id: &str,
    name: &str,
    summary: &str,
    end_byte: usize,
    temporal: TemporalMetadata,
) -> GraphRecord {
    GraphRecord::symbol(
        id.to_owned(),
        "function",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte,
            start_line: 1,
            end_line: 1,
        },
        name.to_owned(),
        summary.to_owned(),
    )
    .with_temporal(temporal)
}

#[cfg(feature = "embedded-aletheiadb")]
fn current_symbol_record(id: &str, name: &str, summary: &str, end_byte: usize) -> GraphRecord {
    GraphRecord::symbol(
        id.to_owned(),
        "function",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte,
            start_line: 1,
            end_line: 1,
        },
        name.to_owned(),
        summary.to_owned(),
    )
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn superseded_symbol(id: &str, name: &str, summary: &str, superseded_by_id: &str) -> GraphRecord {
    let mut record = current_symbol_record(id, name, summary, 25);
    let GraphRecord::Node { superseded_by, .. } = &mut record else {
        unreachable!("current_symbol_record must create a node");
    };
    *superseded_by = Some(superseded_by_id.to_owned());
    record
}

#[cfg(feature = "embedded-aletheiadb")]
fn defines_edge(
    source: &str,
    target: &str,
    summary: &str,
    temporal: TemporalMetadata,
) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Defines,
        source.to_owned(),
        target.to_owned(),
        Some("1.0".to_owned()),
        summary.to_owned(),
    )
    .with_temporal(temporal)
}

#[cfg(feature = "embedded-aletheiadb")]
fn current_defines_edge(source: &str, target: &str, summary: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Defines,
        source.to_owned(),
        target.to_owned(),
        Some("1.0".to_owned()),
        summary.to_owned(),
    )
}

#[cfg(feature = "embedded-aletheiadb")]
fn temporal(git_commit: &str, valid_time: &str) -> TemporalMetadata {
    temporal_observed(git_commit, valid_time, valid_time)
}

#[cfg(feature = "embedded-aletheiadb")]
fn temporal_observed(git_commit: &str, valid_time: &str, observed_at: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: git_commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: observed_at.to_owned(),
        valid_time_source: None,
    }
}

#[cfg(feature = "embedded-aletheiadb")]
fn seed_history_repo(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, "src/lib.rs", "pub fn original() -> u32 { 1 }\n");
    commit(repo, "initial symbol", "2026-01-01T00:00:00Z");

    write(repo, "src/lib.rs", "pub fn renamed() -> u32 { 2 }\n");
    commit(repo, "rename symbol", "2026-01-02T00:00:00Z");
}

#[cfg(feature = "embedded-aletheiadb")]
fn seed_stable_symbol_history_repo(repo: &Path) -> [String; 2] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, "src/lib.rs", "pub fn stable() -> u32 { 1 }\n");
    let first = commit_with_sha(repo, "initial stable symbol", "2026-03-01T00:00:00Z");

    write(repo, "src/lib.rs", "pub fn stable() -> u32 { 2 }\n");
    let second = commit_with_sha(repo, "change stable symbol body", "2026-03-02T00:00:00Z");

    [first, second]
}

#[cfg(feature = "embedded-aletheiadb")]
fn reopen_embedded_db(data_dir: &Path) -> ::aletheiadb::AletheiaDB {
    let config = ::aletheiadb::config::durable_config_for_data_dir(data_dir);
    ::aletheiadb::AletheiaDB::with_unified_config(config)
        .expect("embedded AletheiaDB store should reopen")
}

#[cfg(feature = "embedded-aletheiadb")]
fn changed_symbol_git_commits_for_commit(
    db: &::aletheiadb::AletheiaDB,
    commit_record_id: &str,
    symbol_name: &str,
) -> Vec<String> {
    let commit_node_id = node_id_by_codegraph_id(db, commit_record_id);
    let mut commits = Vec::new();
    for contains_edge_id in db.get_outgoing_edges_with_label(commit_node_id, "CONTAINS") {
        let change_node_id = db
            .get_edge_target(contains_edge_id)
            .expect("CONTAINS edge should have a target");
        let change = db
            .get_node(change_node_id)
            .expect("CONTAINS target should be readable");
        if node_property(&change, "kind") != Some("Change") {
            continue;
        }

        for changed_edge_id in db.get_incoming_edges_with_label(change_node_id, "CHANGED_IN") {
            let source_node_id = db
                .get_edge_source(changed_edge_id)
                .expect("CHANGED_IN edge should have a source");
            let source = db
                .get_node(source_node_id)
                .expect("CHANGED_IN source should be readable");
            if node_property(&source, "kind") == Some("Symbol")
                && node_property(&source, "name") == Some(symbol_name)
                && let Some(git_commit) = node_property(&source, "git_commit")
            {
                commits.push(git_commit.to_owned());
            }
        }
    }
    commits.sort();
    commits.dedup();
    commits
}

#[cfg(feature = "embedded-aletheiadb")]
fn node_id_by_codegraph_id(db: &::aletheiadb::AletheiaDB, record_id: &str) -> ::aletheiadb::NodeId {
    db.get_all_node_ids()
        .into_iter()
        .find(|node_id| {
            db.get_node(*node_id)
                .expect("node should be readable")
                .get_property("codegraph_id")
                .and_then(|value| value.as_str())
                == Some(record_id)
        })
        .unwrap_or_else(|| panic!("missing embedded node for {record_id}"))
}

#[cfg(feature = "embedded-aletheiadb")]
#[derive(Debug, Clone, Copy)]
enum EdgeEndpoint {
    Source,
    Target,
}

#[cfg(feature = "embedded-aletheiadb")]
fn edge_id_by_codegraph_id(db: &::aletheiadb::AletheiaDB, record_id: &str) -> ::aletheiadb::EdgeId {
    db.get_all_node_ids()
        .into_iter()
        .flat_map(|node_id| db.get_outgoing_edges(node_id))
        .find(|edge_id| {
            db.get_edge(*edge_id)
                .expect("edge should be readable")
                .get_property("codegraph_id")
                .and_then(|value| value.as_str())
                == Some(record_id)
        })
        .unwrap_or_else(|| panic!("missing embedded edge for {record_id}"))
}

#[cfg(feature = "embedded-aletheiadb")]
fn edge_endpoint_git_commit(
    db: &::aletheiadb::AletheiaDB,
    edge_id: ::aletheiadb::EdgeId,
    endpoint: EdgeEndpoint,
) -> Option<String> {
    let node_id = match endpoint {
        EdgeEndpoint::Source => db
            .get_edge_source(edge_id)
            .expect("edge source should be readable"),
        EdgeEndpoint::Target => db
            .get_edge_target(edge_id)
            .expect("edge target should be readable"),
    };
    db.get_node(node_id)
        .expect("edge endpoint node should be readable")
        .get_property("git_commit")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}

#[cfg(feature = "embedded-aletheiadb")]
fn node_property<'a>(node: &'a ::aletheiadb::Node, key: &str) -> Option<&'a str> {
    node.get_property(key).and_then(|value| value.as_str())
}

#[cfg(feature = "embedded-aletheiadb")]
fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

#[cfg(feature = "embedded-aletheiadb")]
fn commit(repo: &Path, message: &str, date: &str) {
    git(repo, ["add", "."]);
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        output.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "embedded-aletheiadb")]
fn commit_with_sha(repo: &Path, message: &str, date: &str) -> String {
    commit(repo, message, date);
    git_output(repo, ["rev-parse", "HEAD"])
}

#[cfg(feature = "embedded-aletheiadb")]
fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "embedded-aletheiadb")]
fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}
