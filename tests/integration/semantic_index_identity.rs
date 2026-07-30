#![allow(missing_docs)]
//! Issue #104 — refuse semantic queries when the query embedder mismatches the
//! index.
//!
//! Every test here drives a store whose vector index is built from SYNTHETIC
//! vectors and whose identity node is written directly, so no embedding model is
//! ever loaded (no Hugging Face download, no network). That is possible because
//! the compatibility gate runs BEFORE the query is embedded — which is also the
//! point: an incompatible store costs an operator a refusal, not a model load.

#![cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]

use std::{fs, path::Path};

use aletheia_egregore::{
    Graph, GraphRecord, NodeKind, SourceSpan,
    adapters::{EmbeddedAletheiaSink, GraphSink},
    embeddings::{
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS, EmbeddingVectorKey, EmbeddingVectorMap,
        default_embedding_model_identity, embedding_index_identity_record, indexed_identities,
    },
    stable_id,
};
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

/// Builds a store whose vector index holds `dim`-dimensional synthetic vectors,
/// optionally stamped with `identity` records. Returns once the write lease is
/// released so the CLI can reopen the store.
fn build_store(data_dir: &Path, dim: usize, identities: &[aletheia_egregore::EmbeddingModel]) {
    let mut vectors = EmbeddingVectorMap::new();
    let mut records = Vec::new();
    for i in 0..3_usize {
        let id = stable_id(&["node", "symbol", "src/lib.rs", &format!("sym{i:02}")]);
        let record = GraphRecord::symbol(
            id,
            "function",
            "src/lib.rs".to_owned(),
            SourceSpan {
                start_byte: 0,
                end_byte: 10 + i,
                start_line: i + 1,
                end_line: i + 1,
            },
            format!("sym{i:02}"),
            format!("fixture symbol number {i}"),
        );
        #[allow(clippy::cast_precision_loss)]
        let mut vector = vec![0.0_f32; dim];
        vector[i % dim] = 1.0;
        vectors.insert(
            EmbeddingVectorKey::from_record(&record).expect("symbol must be embeddable"),
            vector,
        );
        records.push(record);
    }
    // The identity record ID is FIXED (one vector index → one identity), so
    // writing several through the normal constructor would collapse to the last
    // one. A store carrying several distinct identities is only reachable via
    // foreign/hand-authored records, so the fixture forges distinct IDs for the
    // 2+ case — that is exactly the state the ambiguity verdict fails closed on.
    for (i, identity) in identities.iter().enumerate() {
        let mut record = embedding_index_identity_record(identity);
        if identities.len() > 1
            && let GraphRecord::Node { id, .. } = &mut record
        {
            *id = format!("semantic:v1:fixture-identity-{i:02}");
        }
        records.push(record);
    }
    let mut sink = EmbeddedAletheiaSink::open_with_embeddings(data_dir, vectors, dim)
        .expect("fixture store should open");
    for record in &records {
        sink.write_record(record).expect("fixture record writes");
    }
    sink.persist_indexes().expect("fixture indexes persist");
    drop(sink);
}

fn semantic_query(data_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
    cmd.arg("query")
        .arg("semantic")
        .arg("request timeout handling")
        .arg("--data-dir")
        .arg(data_dir);
    cmd
}

fn stdout_envelope(output: &[u8]) -> Value {
    let text = String::from_utf8_lossy(output);
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("expected a JSON envelope on stdout, got:\n{text}"));
    serde_json::from_str(line).expect("stdout envelope is valid JSON")
}

// ── AC1 / Success Metric: `--embed` stores carry a readable identity ─────────

/// An `--embed` ingest persists the vector index's embedding-model identity as
/// a semantic-domain `EmbeddingModel` record, readable back out of the store.
///
/// Uses a repository-only graph so `generate_embeddings` short-circuits with
/// zero candidates and never loads the model — the identity must still be
/// written, because the index itself is still created.
#[test]
fn embed_ingest_persists_the_vector_index_model_identity() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("repo-only.graph.jsonl");
    let data_dir = temp.path().join("store");
    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "identity-repo"]),
        NodeKind::Repository,
        None,
        None,
        Some("identity-repo".to_owned()),
        "Repository identity-repo".to_owned(),
    ));
    fs::write(&graph_path, graph.to_jsonl().expect("graph serializes")).expect("graph written");

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
        .success();

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("store reopens");
    let records = sink.read_all_records().expect("store reads back");
    let identities = indexed_identities(&records);
    assert_eq!(
        identities,
        vec![default_embedding_model_identity(
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS
        )],
        "an --embed ingest must persist exactly the producing model's identity"
    );
}

/// Re-running the identical `--embed` ingest is idempotent: the identity record
/// ID is fixed, so a repeated write supersedes rather than accumulating a second
/// record that would look "ambiguous" — and, being unremovable, brick the store.
#[test]
fn repeated_embed_ingest_keeps_exactly_one_identity() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("repo-only.graph.jsonl");
    let data_dir = temp.path().join("store");
    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "identity-repo"]),
        NodeKind::Repository,
        None,
        None,
        Some("identity-repo".to_owned()),
        "Repository identity-repo".to_owned(),
    ));
    fs::write(&graph_path, graph.to_jsonl().expect("graph serializes")).expect("graph written");

    for _ in 0..2 {
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
            .success();
    }

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("store reopens");
    let records = sink.read_all_records().expect("store reads back");
    assert_eq!(indexed_identities(&records).len(), 1);
}

// ── AC3: same dimension, different model → refuse ────────────────────────────

/// The silent-failure case this issue exists for: identical dimension, a
/// different model. The query must be refused with a stable machine-readable
/// diagnostic naming BOTH identities and a distinct nonzero exit code, and must
/// NOT return a ranked result list.
#[test]
fn same_dim_different_model_is_refused_naming_both_identities() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mut foreign = default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
    foreign.name = "some-other/384-dim-encoder".to_owned();
    build_store(&data_dir, DEFAULT_EMBEDDING_MODEL_DIMENSIONS, &[foreign]);

    let assert = semantic_query(&data_dir).assert().code(10);
    let out = assert.get_output();
    let envelope = stdout_envelope(&out.stdout);
    assert_eq!(envelope["ok"], false);
    let error = &envelope["error"];
    assert_eq!(error["code"], "embedding_model_mismatch");
    assert_eq!(error["indexed_model"]["name"], "some-other/384-dim-encoder");
    assert_eq!(
        error["query_model"]["name"],
        aletheia_egregore::embeddings::DEFAULT_EMBEDDING_MODEL_NAME
    );
    assert_eq!(error["differing_fields"], serde_json::json!(["name"]));
    assert!(
        error["remedy"]
            .as_str()
            .expect("remedy is a string")
            .contains("--embed"),
        "the remedy must point at re-ingest, not at editing the store"
    );
    // No ranked result list: the only stdout line is the refusal envelope.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "a refused query must not emit result rows, got:\n{stdout}"
    );
}

// ── AC4: dimension mismatch and unrecorded identity are distinct ─────────────

/// A dimension mismatch is its own diagnostic and its own exit code, distinct
/// from the same-dimension model mismatch.
#[test]
fn dimension_mismatch_is_a_distinct_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let small = default_embedding_model_identity(4);
    build_store(&data_dir, 4, &[small]);

    let assert = semantic_query(&data_dir).assert().code(9);
    let envelope = stdout_envelope(&assert.get_output().stdout);
    assert_eq!(envelope["error"]["code"], "embedding_dimension_mismatch");
    assert_eq!(envelope["error"]["index_dimensions"], 4);
    assert_eq!(
        envelope["error"]["query_dimensions"],
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS
    );
    assert_eq!(
        envelope["error"]["query_model"]["dim"],
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS
    );
    assert_eq!(
        envelope["error"]["indexed_models"]
            .as_array()
            .expect("indexed_models is an array")
            .len(),
        1,
        "recorded identities are always disclosed as a list, never a silent first pick"
    );
}

/// A legacy index carrying no recorded identity is reported as UNVERIFIABLE and
/// refused — never silently assumed compatible.
#[test]
fn index_without_recorded_identity_is_refused_as_unverifiable() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    build_store(&data_dir, DEFAULT_EMBEDDING_MODEL_DIMENSIONS, &[]);

    let assert = semantic_query(&data_dir).assert().code(7);
    let envelope = stdout_envelope(&assert.get_output().stdout);
    assert_eq!(envelope["error"]["code"], "embedding_identity_unrecorded");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .expect("message is a string")
            .contains("UNVERIFIABLE"),
        "the diagnostic must say unverifiable, not imply compatibility"
    );
}

/// A store whose index was written by two different models cannot be ranked
/// against either; it gets its own diagnostic rather than an arbitrary winner.
#[test]
fn index_with_two_recorded_identities_is_ambiguous() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mine = default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
    let mut foreign = mine.clone();
    foreign.name = "some-other/384-dim-encoder".to_owned();
    build_store(
        &data_dir,
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        &[mine, foreign],
    );

    let assert = semantic_query(&data_dir).assert().code(8);
    let envelope = stdout_envelope(&assert.get_output().stdout);
    assert_eq!(envelope["error"]["code"], "embedding_identity_ambiguous");
    assert_eq!(
        envelope["error"]["indexed_models"]
            .as_array()
            .expect("indexed_models is an array")
            .len(),
        2
    );
}

// ── AC7: refusal output never leaks payloads ────────────────────────────────

/// The refusal envelope carries identity handles only — never model bytes,
/// vectors, source text, or the operator's query string.
#[test]
fn refusal_output_is_allow_list_only() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mut foreign = default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
    foreign.name = "some-other/384-dim-encoder".to_owned();
    build_store(&data_dir, DEFAULT_EMBEDDING_MODEL_DIMENSIONS, &[foreign]);

    let assert = semantic_query(&data_dir).assert().code(10);
    let envelope = stdout_envelope(&assert.get_output().stdout);
    let keys: Vec<&str> = envelope["error"]
        .as_object()
        .expect("error is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "code",
            "differing_fields",
            "indexed_model",
            "message",
            "query_model",
            "remedy",
        ],
        "refusal envelope must expose exactly the allow-listed keys"
    );
    let rendered = serde_json::to_string(&envelope).expect("envelope serializes");
    assert!(
        !rendered.contains("request timeout handling"),
        "the query text must never appear in the refusal envelope"
    );
    assert!(
        !rendered.contains("fixture symbol number"),
        "indexed source text must never appear in the refusal envelope"
    );
}

// ── AC8: determinism ────────────────────────────────────────────────────────

/// Re-running the identical mismatched query produces byte-identical
/// diagnostics and the identical exit code across 5 consecutive runs.
#[test]
fn mismatched_query_is_byte_identical_across_five_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mut foreign = default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
    foreign.name = "some-other/384-dim-encoder".to_owned();
    build_store(&data_dir, DEFAULT_EMBEDDING_MODEL_DIMENSIONS, &[foreign]);

    let mut seen: Option<(Vec<u8>, Vec<u8>, Option<i32>)> = None;
    for run in 0..5 {
        let output = semantic_query(&data_dir)
            .output()
            .expect("query runs to completion");
        let observed = (
            output.stdout.clone(),
            output.stderr.clone(),
            output.status.code(),
        );
        match &seen {
            None => seen = Some(observed),
            Some(first) => assert_eq!(
                &observed, first,
                "run {run} diverged from the first run's stdout/stderr/exit code"
            ),
        }
    }
    assert_eq!(seen.expect("at least one run").2, Some(10));
}

// ── The gate covers every local embedded semantic lane, not just `semantic` ──

/// `semantic-memory` and `semantic-context` read the SAME shared vector index,
/// so they carry the same cross-vector-space hazard and must refuse identically.
#[test]
fn every_local_semantic_lane_applies_the_gate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mut foreign = default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
    foreign.name = "some-other/384-dim-encoder".to_owned();
    build_store(&data_dir, DEFAULT_EMBEDDING_MODEL_DIMENSIONS, &[foreign]);

    for lane in ["semantic-memory", "semantic-context"] {
        let assert = Command::cargo_bin("egregore")
            .expect("binary should run")
            .arg("query")
            .arg(lane)
            .arg("request timeout handling")
            .arg("--data-dir")
            .arg(&data_dir)
            .assert()
            .code(10);
        let envelope = stdout_envelope(&assert.get_output().stdout);
        assert_eq!(
            envelope["error"]["code"], "embedding_model_mismatch",
            "lane {lane} must apply the same gate"
        );
    }
}

// ── The write path refuses before it can blend two vector spaces ────────────

/// Embedding into a store whose index was built by a DIFFERENT model is refused
/// before any write.
///
/// This is the fix for the trap the natural in-place remedy would otherwise
/// spring: after an `eg` upgrade an operator reaches for `--embed` again, and
/// without this the index would silently end up holding vectors from two models
/// while reporting success.
#[test]
fn embed_into_a_foreign_model_store_is_refused_before_writing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mut foreign = default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
    foreign.name = "some-other/384-dim-encoder".to_owned();
    build_store(
        &data_dir,
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        &[foreign.clone()],
    );

    let graph_path = temp.path().join("repo-only.graph.jsonl");
    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "conflict-repo"]),
        NodeKind::Repository,
        None,
        None,
        Some("conflict-repo".to_owned()),
        "Repository conflict-repo".to_owned(),
    ));
    fs::write(&graph_path, graph.to_jsonl().expect("graph serializes")).expect("graph written");

    let assert = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--embed")
        .assert()
        .failure();
    let envelope = stdout_envelope(&assert.get_output().stdout);
    assert_eq!(
        envelope["error"]["code"],
        "embedding_index_identity_conflict"
    );
    assert!(
        envelope["error"]["remedy"]
            .as_str()
            .expect("remedy is a string")
            .contains("--data-dir"),
        "the remedy must name a fresh data dir, not an in-place mutation"
    );

    // Nothing was written: the store still records exactly the original identity.
    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("store reopens");
    let records = sink.read_all_records().expect("store reads back");
    assert_eq!(indexed_identities(&records), vec![foreign]);
}

/// Re-embedding with the SAME model is allowed and supersedes the identity at
/// its fixed record ID, so the store never accumulates a second one.
#[test]
fn re_embedding_with_the_same_model_supersedes_rather_than_accumulates() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mine = default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
    build_store(
        &data_dir,
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        std::slice::from_ref(&mine),
    );

    let graph_path = temp.path().join("repo-only.graph.jsonl");
    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "same-model-repo"]),
        NodeKind::Repository,
        None,
        None,
        Some("same-model-repo".to_owned()),
        "Repository same-model-repo".to_owned(),
    ));
    fs::write(&graph_path, graph.to_jsonl().expect("graph serializes")).expect("graph written");

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
        .success();

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("store reopens");
    let records = sink.read_all_records().expect("store reads back");
    assert_eq!(indexed_identities(&records), vec![mine]);
}

/// Re-ingesting an EXPORT that carries a foreign identity into a fresh dir with
/// `--embed` leaves exactly ONE identity — the model that actually produced the
/// vectors — and the store stays queryable.
///
/// `eg export` re-emits the identity node as ordinary graph data, so a graph
/// exported from another store can carry a stale identity. With an
/// identity-derived record ID that stale record would coexist with the real one
/// and refuse every query forever, with the documented remedy ("re-ingest into a
/// fresh --data-dir") reproducing the very state it claims to fix. The fixed ID
/// makes the real write supersede it.
#[test]
fn re_ingesting_an_export_with_a_foreign_identity_keeps_only_the_real_one() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("with-foreign-identity.graph.jsonl");
    let data_dir = temp.path().join("store");

    let mut foreign = default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
    foreign.name = "some-other/384-dim-encoder".to_owned();
    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "roundtrip-repo"]),
        NodeKind::Repository,
        None,
        None,
        Some("roundtrip-repo".to_owned()),
        "Repository roundtrip-repo".to_owned(),
    ));
    graph.push(embedding_index_identity_record(&foreign));
    fs::write(&graph_path, graph.to_jsonl().expect("graph serializes")).expect("graph written");

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
        .success();

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("store reopens");
    let records = sink.read_all_records().expect("store reads back");
    assert_eq!(
        indexed_identities(&records),
        vec![default_embedding_model_identity(
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS
        )],
        "the real producing model must supersede a stale exported identity, not \
         coexist with it in an unrecoverable ambiguity"
    );
}

// ── AC7: a crafted identity cannot poison the diagnostic ────────────────────

/// A hand-authored identity carrying control characters and megabytes of text
/// cannot forge output lines, drive the terminal, or produce an unbounded
/// diagnostic — identity VALUES are attacker-controlled even though the field
/// NAMES are allow-listed.
#[test]
fn crafted_identity_cannot_poison_the_refusal_output() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let hostile = aletheia_egregore::EmbeddingModel {
        provider: "p\n\rHACKED: forged line".to_owned(),
        name: "\u{1b}[2Kmalicious".to_owned(),
        version: "v".repeat(10_000),
        dim: u32::try_from(DEFAULT_EMBEDDING_MODEL_DIMENSIONS).expect("fits"),
        content_hash: "h\tx".to_owned(),
    };
    build_store(&data_dir, DEFAULT_EMBEDDING_MODEL_DIMENSIONS, &[hostile]);

    let output = semantic_query(&data_dir).output().expect("query runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stdout.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "a crafted identity must not be able to forge extra stdout lines:\n{stdout}"
    );
    for stream in [&stdout, &stderr] {
        assert!(
            !stream.contains('\u{1b}'),
            "ANSI escapes must never reach a terminal: {stream}"
        );
    }
    // The crafted text may survive as INERT characters — what must not happen is
    // it becoming its own line, which is how injected text impersonates a real
    // diagnostic. The single-line assertion above already proves that; this
    // pins the mechanism: the embedded newline was neutralized, not passed on.
    assert!(
        !stdout.lines().any(|l| l.starts_with("HACKED")),
        "an embedded newline must not split into a forged line: {stdout}"
    );
    assert!(
        stdout.contains("p..HACKED"),
        "control characters must be replaced in place, keeping the value inspectable: {stdout}"
    );
    assert!(
        stdout.len() < 8192,
        "the refusal envelope must stay bounded, got {} bytes",
        stdout.len()
    );
}

// ── AC5 / Success Metric: the compatible path is never falsely refused ──────

/// A store whose recorded identity matches the query embedder is NOT refused.
///
/// This is the "0% of compatible cases are falsely refused" half of the success
/// metric, and it runs offline: it asserts only that the gate let the query
/// through, which is observable without the embedding model. If the model is not
/// locally available the run then fails at model load (exit `1`) — still a pass
/// here, because exit `1` is not one of the gate's refusal codes and no refusal
/// envelope was emitted.
#[test]
fn matching_identity_is_not_refused() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    build_store(
        &data_dir,
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        std::slice::from_ref(&default_embedding_model_identity(
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        )),
    );

    let output = semantic_query(&data_dir).output().expect("query runs");
    let code = output.status.code();
    assert!(
        !matches!(code, Some(7..=10)),
        "a compatible store must never be refused by the identity gate, got exit {code:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("embedding_model_mismatch")
            && !stdout.contains("embedding_identity_unrecorded")
            && !stdout.contains("embedding_identity_ambiguous")
            && !stdout.contains("embedding_dimension_mismatch"),
        "a compatible store must emit no refusal envelope, got:\n{stdout}"
    );
}

/// AC5 + AC8 end-to-end: an identical compatible query against an unchanged
/// store returns byte-identical results across 5 consecutive runs, and the rows
/// still carry the documented `record_id` / `score` contract unchanged.
///
/// Requires the embedding model in the local Hugging Face cache, so it follows
/// the repository's established offline-incompatible convention. The offline
/// halves of the same guarantees are covered by `matching_identity_is_not_refused`
/// above, the `scope_and_rank_semantic_matches` ordering unit tests, and the
/// classifier's own determinism test.
#[test]
#[ignore = "requires a locally available embedding model (offline-incompatible)"]
fn compatible_query_is_byte_identical_across_five_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    build_store(
        &data_dir,
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        std::slice::from_ref(&default_embedding_model_identity(
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        )),
    );

    let mut first: Option<(Vec<u8>, Option<i32>)> = None;
    for run in 0..5 {
        let output = semantic_query(&data_dir).output().expect("query runs");
        let observed = (output.stdout.clone(), output.status.code());
        match &first {
            None => {
                assert_eq!(observed.1, Some(0), "a compatible query must succeed");
                let text = String::from_utf8_lossy(&observed.0);
                let row: Value =
                    serde_json::from_str(text.lines().next().expect("at least one result row"))
                        .expect("result rows are JSON");
                assert!(row.get("record_id").is_some(), "row keeps `record_id`");
                assert!(row.get("score").is_some(), "row keeps `score`");
                first = Some(observed);
            }
            Some(first) => assert_eq!(
                &observed, first,
                "run {run} diverged from the first run's stdout/exit code"
            ),
        }
    }
}

// ── Non-regression: a never-embedded store keeps its existing outcome ────────

/// A store that was never ingested with `--embed` has no vector index at all.
/// That is the pre-existing "no semantic index" outcome (exit 2), NOT an
/// identity failure — reporting "unverifiable identity" here would be a false
/// refusal against an operator who simply never embedded.
#[test]
fn store_without_any_vector_index_keeps_the_no_embeddings_outcome() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("repo-only.graph.jsonl");
    let data_dir = temp.path().join("store");
    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "plain-repo"]),
        NodeKind::Repository,
        None,
        None,
        Some("plain-repo".to_owned()),
        "Repository plain-repo".to_owned(),
    ));
    fs::write(&graph_path, graph.to_jsonl().expect("graph serializes")).expect("graph written");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    semantic_query(&data_dir)
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "store may not have embeddings (re-run ingest with --embed)",
        ));
}

// ── AC6: the indexed identity is readable through an `eg` workflow ───────────

/// `eg inspect --data-dir` surfaces the indexed model identity so an operator
/// can read what a store was embedded with before deciding to re-ingest.
#[test]
fn inspect_reports_the_indexed_model_identity() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mine = default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
    build_store(
        &data_dir,
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        std::slice::from_ref(&mine),
    );

    let assert = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();
    let report = stdout_envelope(&assert.get_output().stdout);
    let block = &report["semantic_index"];
    assert_eq!(block["identity_recorded"], true);
    assert_eq!(
        block["index_dimensions"],
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS
    );
    let models = block["indexed_models"]
        .as_array()
        .expect("indexed_models is an array");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["name"], mine.name);
    assert_eq!(models[0]["provider"], mine.provider);
    assert_eq!(models[0]["version"], mine.version);
    assert_eq!(models[0]["dim"], mine.dim);
    assert_eq!(models[0]["content_hash"], mine.content_hash);
}

/// `eg inspect --data-dir` stays byte-identical across runs with the new block.
#[test]
fn inspect_semantic_index_block_is_byte_identical_across_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    build_store(
        &data_dir,
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        &[default_embedding_model_identity(
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        )],
    );

    let run = || {
        Command::cargo_bin("egregore")
            .expect("binary should run")
            .arg("inspect")
            .arg("--data-dir")
            .arg(&data_dir)
            .output()
            .expect("inspect runs")
            .stdout
    };
    let first = run();
    for _ in 0..4 {
        assert_eq!(run(), first);
    }
}
