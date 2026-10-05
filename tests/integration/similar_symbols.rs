#![allow(missing_docs)]
//! Issue #154 — find symbols similar to a given symbol to surface reusable code.
//!
//! `eg query similar <handle>` accepts a qualified symbol name or a
//! `record_id`, reads the anchor's already-stored embedding (no model load, no
//! re-scan), and returns the top-N most similar OTHER symbol/file nodes as
//! `SemanticResult` rows, ranked by cosine score descending with ties broken
//! by `record_id` ascending.
//!
//! These tests never load the embedding model: the fixture stores hand-made
//! 2-D vectors through `EmbeddedAletheiaSink::open_with_embeddings`, exactly
//! like the existing `semantic_search_returns_latest_live_records_only`
//! fixture.
//!
//! RED PHASE: `eg query similar` does not exist yet, so every test below fails
//! against unmodified code (clap rejects the unknown subcommand with exit 2).

#![cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]

use std::path::PathBuf;

use aletheia_egregore::{
    GraphRecord, NodeKind, SourceSpan,
    adapters::{EmbeddedAletheiaSink, GraphSink},
    embeddings::{EmbeddingVectorKey, EmbeddingVectorMap},
    stable_id,
};
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

const fn span(end_byte: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte,
        start_line: 1,
        end_line: 1,
        start_column: None,
        end_column: None,
    }
}

fn symbol(id: &str, name: &str) -> GraphRecord {
    GraphRecord::symbol(
        id.to_owned(),
        "function",
        "src/lib.rs".to_owned(),
        span(40),
        name.to_owned(),
        format!("symbol {name}"),
    )
}

fn file_node(id: &str, path: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::File,
        Some(path.to_owned()),
        Some(span(200)),
        Some(path.to_owned()),
        format!("file {path}"),
    )
}

/// Handle bundle for the shared similar-symbols fixture.
struct SimilarFixture {
    _temp: tempfile::TempDir,
    data_dir: PathBuf,
    anchor_id: String,
    beta_id: String,
    beta_twin_id: String,
    file_id: String,
    gamma_id: String,
    delta_id: String,
    novec_id: String,
}

/// Builds a small embedded store with hand-made 2-D vectors.
///
/// Geometry (anchor = `[1, 0]`, cosine similarity):
/// - `beta`, `beta2`: `[0.9, 0.4359]` → ≈ 0.9 (tie: ordered by `record_id`)
/// - file `src/other.rs`: `[0.8, 0.6]` → 0.8
/// - `gamma`: `[-0.5, 0.8660254]` → -0.5 (below the similarity floor)
/// - `delta`: `[-1, 0]` → -1.0 (below the similarity floor)
/// - `novec`: no stored vector at all
fn build_similar_fixture() -> SimilarFixture {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("similar-store");

    let anchor_id = stable_id(&["node", "symbol", "src/lib.rs", "alpha"]);
    let beta_id = stable_id(&["node", "symbol", "src/lib.rs", "beta"]);
    let beta_twin_id = stable_id(&["node", "symbol", "src/lib.rs", "beta2"]);
    let file_id = stable_id(&["node", "file", "src/other.rs"]);
    let gamma_id = stable_id(&["node", "symbol", "src/lib.rs", "gamma"]);
    let delta_id = stable_id(&["node", "symbol", "src/lib.rs", "delta"]);
    let novec_id = stable_id(&["node", "symbol", "src/lib.rs", "novec"]);

    let anchor = symbol(&anchor_id, "alpha");
    let beta = symbol(&beta_id, "beta");
    let beta2 = symbol(&beta_twin_id, "beta2");
    let file = file_node(&file_id, "src/other.rs");
    let gamma = symbol(&gamma_id, "gamma");
    let delta = symbol(&delta_id, "delta");
    let novec = symbol(&novec_id, "novec");

    let mut vectors = EmbeddingVectorMap::new();
    let mut put = |record: &GraphRecord, vector: Vec<f32>| {
        vectors.insert(
            EmbeddingVectorKey::from_record(record).expect("record should be embeddable"),
            vector,
        );
    };
    put(&anchor, vec![1.0, 0.0]);
    put(&beta, vec![0.9, 0.4359]);
    put(&beta2, vec![0.9, 0.4359]);
    put(&file, vec![0.8, 0.6]);
    put(&gamma, vec![-0.5, 0.866_025_4]);
    put(&delta, vec![-1.0, 0.0]);
    // `novec` deliberately gets no vector.

    let mut sink = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
        .expect("similar fixture store should open");
    for record in [&anchor, &beta, &beta2, &file, &gamma, &delta, &novec] {
        sink.write_record(record)
            .expect("fixture record should write");
    }
    sink.persist_indexes()
        .expect("fixture indexes should persist");
    drop(sink);

    SimilarFixture {
        _temp: temp,
        data_dir,
        anchor_id,
        beta_id,
        beta_twin_id,
        file_id,
        gamma_id,
        delta_id,
        novec_id,
    }
}

/// The two tied candidates in canonical order (score tie → `record_id` asc).
fn tied_first(fixture: &SimilarFixture) -> (&str, &str) {
    if fixture.beta_id < fixture.beta_twin_id {
        (&fixture.beta_id, &fixture.beta_twin_id)
    } else {
        (&fixture.beta_twin_id, &fixture.beta_id)
    }
}

fn run_similar(data_dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
    cmd.arg("query").arg("similar");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg("--data-dir").arg(data_dir);
    cmd.output().expect("similar should run")
}

fn json_rows(stdout: &[u8]) -> Vec<Value> {
    let text = String::from_utf8(stdout.to_owned()).expect("stdout should be UTF-8");
    assert!(
        !text.trim().is_empty(),
        "similar should print result rows on success"
    );
    text.lines()
        .map(|line| serde_json::from_str(line).expect("each row should be valid JSON"))
        .collect()
}

#[test]
fn similar_ranks_symbol_and_file_matches_and_excludes_anchor() {
    let fixture = build_similar_fixture();
    let output = run_similar(&fixture.data_dir, &["alpha", "--limit", "10"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "similar with matches should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = json_rows(&output.stdout);
    let (first, second) = tied_first(&fixture);
    let record_ids: Vec<&str> = rows
        .iter()
        .map(|row| row["record_id"].as_str().expect("row needs record_id"))
        .collect();
    assert_eq!(
        record_ids,
        vec![first, second, fixture.file_id.as_str()],
        "beta/beta2 tie on score, ordered by record_id, then the file node"
    );
    assert!(
        !record_ids.contains(&fixture.anchor_id.as_str()),
        "the anchor symbol must never be its own match"
    );
    assert!(
        !record_ids.contains(&fixture.gamma_id.as_str())
            && !record_ids.contains(&fixture.delta_id.as_str()),
        "below-floor matches must be filtered out"
    );
    // Citable-handle contract (AC3): every row carries the five handles.
    for row in &rows {
        for field in ["record_id", "name", "repo_relative_path", "span", "score"] {
            assert!(
                row.get(field).is_some_and(|value| !value.is_null()),
                "row must carry citable field '{field}': {row}"
            );
        }
        assert!(
            row["score"].as_f64().expect("score is a number") > 0.0,
            "every returned row must score above the similarity floor"
        );
    }
}

#[test]
fn similar_accepts_record_id_handle() {
    let fixture = build_similar_fixture();
    let output = run_similar(&fixture.data_dir, &[&fixture.anchor_id, "--limit", "10"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "record_id handle should resolve; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = json_rows(&output.stdout);
    assert_eq!(
        rows.len(),
        3,
        "record_id handle should find the same 3 matches"
    );
}

#[test]
fn similar_limit_truncates_top_n() {
    let fixture = build_similar_fixture();
    let output = run_similar(&fixture.data_dir, &["alpha", "--limit", "1"]);

    assert_eq!(output.status.code(), Some(0));
    let rows = json_rows(&output.stdout);
    assert_eq!(rows.len(), 1, "--limit 1 should return exactly one row");
    let (first, _) = tied_first(&fixture);
    assert_eq!(
        rows[0]["record_id"].as_str(),
        Some(first),
        "the single row should be the top-ranked match"
    );
}

#[test]
fn similar_unknown_handle_fails_with_stable_diagnostic() {
    let fixture = build_similar_fixture();
    let output = run_similar(&fixture.data_dir, &["nope::missing"]);

    assert_eq!(
        output.status.code(),
        Some(1),
        "unknown handle should exit 1; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"code\":\"unknown_symbol_handle\""),
        "unknown handle needs the stable diagnostic code; got: {stdout}"
    );
    assert!(
        stdout.contains("nope::missing"),
        "the diagnostic should echo the handle; got: {stdout}"
    );
}

#[test]
fn similar_ambiguous_name_lists_candidate_record_ids() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("ambiguous-store");
    let first_id = stable_id(&["node", "symbol", "src/a.rs", "dup"]);
    let second_id = stable_id(&["node", "symbol", "src/b.rs", "dup"]);
    let first = symbol(&first_id, "dup");
    let second = symbol(&second_id, "dup");

    let mut vectors = EmbeddingVectorMap::new();
    for (record, vector) in [(&first, vec![1.0, 0.0]), (&second, vec![0.0, 1.0])] {
        vectors.insert(
            EmbeddingVectorKey::from_record(record).expect("record should be embeddable"),
            vector,
        );
    }
    let mut sink = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
        .expect("ambiguous fixture store should open");
    for record in [&first, &second] {
        sink.write_record(record)
            .expect("fixture record should write");
    }
    sink.persist_indexes()
        .expect("fixture indexes should persist");
    drop(sink);

    let output = run_similar(&data_dir, &["dup"]);

    assert_eq!(
        output.status.code(),
        Some(1),
        "ambiguous handle should exit 1; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"code\":\"ambiguous_symbol_handle\""),
        "ambiguous handle needs the stable diagnostic code; got: {stdout}"
    );
    assert!(
        stdout.contains(&first_id) && stdout.contains(&second_id),
        "the diagnostic must list every candidate record_id; got: {stdout}"
    );
}

#[test]
fn similar_anchor_without_stored_embedding_fails() {
    let fixture = build_similar_fixture();

    // By name…
    let output = run_similar(&fixture.data_dir, &["novec"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "anchor without a stored embedding should exit 1, not empty success; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"code\":\"anchor_not_embedded\""),
        "missing anchor embedding needs the stable diagnostic code; got: {stdout}"
    );

    // …and by record_id: the handle form must not change the diagnosis.
    let output = run_similar(&fixture.data_dir, &[&fixture.novec_id]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "record_id handle for an unembedded anchor should exit 1; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"code\":\"anchor_not_embedded\""),
        "missing anchor embedding needs the stable diagnostic code; got: {stdout}"
    );
}

#[test]
fn similar_zero_matches_above_floor_exits_2() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("lonely-store");
    let lonely_id = stable_id(&["node", "symbol", "src/lib.rs", "lonely"]);
    let opposite_id = stable_id(&["node", "symbol", "src/lib.rs", "opposite"]);
    let lonely = symbol(&lonely_id, "lonely");
    let opposite = symbol(&opposite_id, "opposite");

    let mut vectors = EmbeddingVectorMap::new();
    for (record, vector) in [(&lonely, vec![1.0, 0.0]), (&opposite, vec![-1.0, 0.0])] {
        vectors.insert(
            EmbeddingVectorKey::from_record(record).expect("record should be embeddable"),
            vector,
        );
    }
    let mut sink = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
        .expect("lonely fixture store should open");
    for record in [&lonely, &opposite] {
        sink.write_record(record)
            .expect("fixture record should write");
    }
    sink.persist_indexes()
        .expect("fixture indexes should persist");
    drop(sink);

    let output = run_similar(&data_dir, &["lonely"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "resolvable anchor with zero above-floor matches should exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "exit-2 answers leave stdout empty (semantic-lane convention)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no_similar_matches"),
        "exit 2 should carry the stable diagnostic code on stderr; got: {stderr}"
    );
}

#[test]
fn similar_store_without_embed_index_fails() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("structural-store");
    let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "plain"]);
    let plain = symbol(&symbol_id, "plain");

    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("structural store should open");
    sink.write_record(&plain).expect("record should write");
    sink.persist_indexes().expect("indexes should persist");
    drop(sink);

    let output = run_similar(&data_dir, &["plain"]);

    assert_ne!(
        output.status.code(),
        Some(0),
        "a store ingested without --embed must not succeed"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"code\":\"semantic_index_absent\""),
        "missing vector index needs the stable diagnostic code; got: {stdout}"
    );
}

#[test]
fn similar_repeated_queries_are_byte_identical() {
    let fixture = build_similar_fixture();
    let first = run_similar(&fixture.data_dir, &["alpha", "--limit", "10"]);
    let second = run_similar(&fixture.data_dir, &["alpha", "--limit", "10"]);

    assert_eq!(first.status.code(), Some(0));
    assert_eq!(
        first.stdout, second.stdout,
        "repeated identical queries must produce byte-identical output"
    );
}

#[test]
fn similar_text_format_renders_rows() {
    let fixture = build_similar_fixture();
    let output = run_similar(&fixture.data_dir, &["alpha", "--format", "text"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "text format should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.lines().count(),
        3,
        "text format should render one line per row"
    );
    assert!(
        stdout.contains("beta"),
        "text rows should name the matched symbols; got: {stdout}"
    );
    // The CLI surface contract: this lane must be discoverable.
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("query")
        .arg("similar")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("similar"));
}
