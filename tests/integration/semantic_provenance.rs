#![allow(missing_docs)]
//! Issue #243 — stamp embedding-model provenance on every semantic query
//! answer.
//!
//! Every `eg query semantic` answer carries one embedding-provenance envelope
//! ahead of its rows: the query model identity, the index model identity, the
//! similarity metric, a stable index fingerprint, and an explicit
//! match/mismatch verdict. `SemanticResult` rows are unchanged.
//!
//! These tests never load the embedding model: the compatibility gate runs
//! before embedding, and the envelope is stamped even on the no-index exit-2
//! path — which is exactly the path most at risk of silently omitting it.

#![cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]

use std::path::Path;

use aletheia_egregore::{
    GraphRecord, SourceSpan,
    adapters::{EmbeddedAletheiaSink, GraphSink},
    embeddings::DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
    stable_id,
};
use assert_cmd::Command;
use serde_json::Value;

/// Builds a store that was never `--embed`ed: plain records, no vector index,
/// no identity records. The CLI must still stamp a provenance envelope on the
/// exit-2 answer (query model named, index absent) without loading the model.
fn build_plain_store(data_dir: &Path) {
    let mut sink = EmbeddedAletheiaSink::open(data_dir).expect("plain store should open");
    let record = GraphRecord::symbol(
        stable_id(&["node", "symbol", "src/lib.rs", "plain_fn"]),
        "function",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line: 1,
            end_line: 1,
            start_column: None,
            end_column: None,
        },
        "plain_fn".to_owned(),
        "a symbol in a store with no embeddings".to_owned(),
    );
    sink.write_record(&record).expect("record should write");
    drop(sink);
}

fn semantic_query(data_dir: &Path, format: &str) -> Command {
    let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
    cmd.arg("query")
        .arg("semantic")
        .arg("request timeout handling")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--format")
        .arg(format);
    cmd
}

/// The first stdout line must be the envelope object; returns it parsed.
fn first_line_envelope(output: &[u8]) -> Value {
    let text = String::from_utf8_lossy(output);
    let mut lines = text.lines();
    let first = lines
        .next()
        .unwrap_or_else(|| panic!("expected stdout lines, got:\n{text}"));
    let envelope: Value =
        serde_json::from_str(first).expect("first stdout line must be the JSON envelope");
    assert!(
        envelope.get("embedding_provenance").is_some(),
        "first line must be the provenance envelope, got: {first}"
    );
    envelope
}

fn provenance_of(envelope: &Value) -> &Value {
    &envelope["embedding_provenance"]
}

/// A never-embedded store still gets a well-formed envelope on the exit-2
/// answer: the query model is named (declared 384-dim default), the index is
/// absent, the metric is cosine, and the fingerprint is the absent-marker
/// hash — 64 lowercase hex chars, distinct from any real model fingerprint.
#[test]
fn empty_store_answer_carries_provenance_envelope() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    build_plain_store(&data_dir);

    let output = semantic_query(&data_dir, "json")
        .assert()
        .code(2)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no results"),
        "stderr keeps the documented no-results message, got: {stderr}"
    );

    let envelope = first_line_envelope(&output.stdout);
    let provenance = provenance_of(&envelope);
    let query = &provenance["query_model"];
    assert_eq!(query["provider"], "aletheiadb_re_export");
    assert_eq!(query["name"], "sentence-transformers/all-MiniLM-L6-v2");
    assert_eq!(query["dim"], DEFAULT_EMBEDDING_MODEL_DIMENSIONS as u64);
    assert!(
        provenance["index_model"].is_null(),
        "no index → null index_model"
    );
    assert_eq!(provenance["metric"], "cosine");
    assert_eq!(provenance["model_match"], false);
    assert_eq!(provenance["mismatch_fields"], Value::Array(Vec::new()));
    let fingerprint = provenance["index_fingerprint"]
        .as_str()
        .expect("fingerprint is a string");
    assert_eq!(fingerprint.len(), 64, "BLAKE3 hex fingerprint");
    assert!(
        fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
        "fingerprint is hex, got: {fingerprint}"
    );
}

/// Re-running an identical query against an unchanged store reproduces the
/// envelope byte-for-byte (AC: deterministic serialization).
#[test]
fn envelope_is_byte_deterministic_across_runs() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    build_plain_store(&data_dir);

    let first = semantic_query(&data_dir, "json")
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let second = semantic_query(&data_dir, "json")
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        first, second,
        "identical query against an unchanged store must reproduce stdout byte-for-byte"
    );
}

/// `--format text` prints the envelope exactly once, as the first line, ahead
/// of any result rows or verdicts.
#[test]
fn text_format_prints_provenance_once_first() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    build_plain_store(&data_dir);

    let output = semantic_query(&data_dir, "text")
        .assert()
        .code(2)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let count = stdout
        .lines()
        .filter(|line| line.starts_with("embedding_provenance:"))
        .count();
    assert_eq!(count, 1, "envelope appears exactly once, got:\n{stdout}");
    let first = stdout.lines().next().expect("stdout has a first line");
    assert!(
        first.starts_with("embedding_provenance:"),
        "envelope is the first line, got: {first}"
    );
    for required in [
        "sentence-transformers/all-MiniLM-L6-v2",
        "dim=384",
        "metric=cosine",
        "model_match=false",
    ] {
        assert!(
            first.contains(required),
            "text envelope must carry {required}, got: {first}"
        );
    }
}
