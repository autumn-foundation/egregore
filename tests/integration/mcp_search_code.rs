//! MCP `search_code` tool — issue #182.
//!
//! RED phase: pins the new read-only MCP tool that exposes semantic code
//! search to agents — the natural-language entry point the MCP surface was
//! missing. Covers: tool registration and `tools/list` advertising, argument
//! validation (empty query, bounded limit with documented default), the
//! structured payload shape, per-match field byte-parity with
//! `eg query semantic` in the same ranked order, the miss-path envelopes
//! (no-embeddings store, embedder mismatch, no-match), determinism, and the
//! redaction-safe row contract.
//!
//! Tests that need a live daemon + vector index use synthetic 4-D vectors —
//! the deterministic substrate from the #59 daemon tests — so no embedding
//! model is required. Only the rmcp method's own embed step needs the real
//! model; the miss paths that precede it (empty query, missing daemon) are
//! covered directly against the method.

#![allow(missing_docs)]
#![allow(clippy::doc_markdown)]
#![cfg(feature = "embedded-aletheiadb")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(feature = "embeddings")]
use aletheia_egregore::{
    adapters::{EmbeddedAletheiaSink, GraphSink},
    embeddings::{EmbeddingVectorKey, EmbeddingVectorMap},
};
use aletheia_egregore::{
    daemon::DaemonQueryRejection,
    ir::{GraphRecord, SourceSpan, stable_id},
    mcp::{
        EgregoreMcpServer, SearchCodeArgs, search_code_effective_limit,
        search_code_rejection_error, tool_search_code_from_verb_result,
        tool_search_code_with_vector,
    },
};
use rmcp::{ServerHandler as _, handler::server::wrapper::Parameters};
use serde_json::{Value, json};

// ── Registration (AC7) ──────────────────────────────────────────────────────

/// The tool router must register `search_code` under exactly that name.
#[test]
fn tool_router_registers_search_code() {
    let attr = EgregoreMcpServer::search_code_tool_attr();
    assert_eq!(attr.name, "search_code", "tool name");
    let description = attr.description.as_deref().unwrap_or_default();
    assert!(
        description.contains("semantic"),
        "tool description must advertise semantic search, got: {description}"
    );
    assert!(
        description.contains("query"),
        "tool description must name the query argument, got: {description}"
    );
}

/// The server instructions must mention semantic search (AC7).
#[test]
fn server_instructions_mention_semantic_search() {
    let server = EgregoreMcpServer::new(PathBuf::from(".egregore-mcp-search-code-test"));
    let info = server.get_info();
    let instructions = info.instructions.as_deref().unwrap_or_default();
    assert!(
        instructions.contains("search_code") || instructions.contains("semantic"),
        "server instructions must mention semantic search, got: {instructions}"
    );
}

// ── Argument validation (AC1) ────────────────────────────────────────────────

/// An empty query is rejected with the stable `missing_argument` envelope
/// before any daemon is contacted.
#[test]
fn empty_query_returns_missing_argument_envelope() {
    let server = EgregoreMcpServer::new(PathBuf::from(".egregore-mcp-search-code-test"));
    for query in [None, Some(String::new())] {
        let raw = server.search_code(Parameters(SearchCodeArgs {
            query,
            data_dir: Some("/nonexistent-data-dir".to_owned()),
            limit: None,
            repo_path: None,
        }));
        let payload: Value =
            serde_json::from_str(&raw).expect("search_code must return a JSON payload");
        assert_eq!(
            payload,
            json!({
                "ok": false,
                "error": {
                    "code": "missing_argument",
                    "field": "query",
                    "message": "query is required and must be non-empty",
                }
            }),
            "empty query must return the stable missing_argument envelope"
        );
    }
}

/// The limit is bounded with a documented safe default (AC1): absent → 10
/// (the `eg query semantic` default), capped at 100 (the daemon verb ceiling).
#[test]
fn limit_has_documented_default_and_ceiling() {
    assert_eq!(
        search_code_effective_limit(None),
        10,
        "absent limit must default to 10"
    );
    assert_eq!(
        search_code_effective_limit(Some(3)),
        3,
        "an in-range limit must pass through"
    );
    assert_eq!(
        search_code_effective_limit(Some(100)),
        100,
        "the ceiling itself must pass through"
    );
    assert_eq!(
        search_code_effective_limit(Some(10_000)),
        100,
        "an over-ceiling limit must clamp to 100"
    );
    assert_eq!(
        search_code_effective_limit(Some(u64::MAX)),
        100,
        "u64::MAX must clamp to 100, never overflow the daemon's usize"
    );
}

/// A missing daemon surfaces the existing stable daemon envelope (AC8) —
/// and it does so before the model would load, so this path needs no embedder.
#[test]
fn missing_daemon_returns_stable_daemon_envelope() {
    let server = EgregoreMcpServer::new(PathBuf::from(".egregore-mcp-search-code-test"));
    let raw = server.search_code(Parameters(SearchCodeArgs {
        query: Some("where is the retry loop".to_owned()),
        data_dir: Some("/nonexistent-data-dir-search-code".to_owned()),
        limit: None,
        repo_path: None,
    }));
    let payload: Value =
        serde_json::from_str(&raw).expect("search_code must return a JSON payload");
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(
        payload["error"]["code"].as_str(),
        Some("daemon_not_running"),
        "missing daemon must surface the frozen daemon code: {payload}"
    );
}

// ── Rejection mapping (AC3, AC4) ─────────────────────────────────────────────

fn rejection(code: &str) -> DaemonQueryRejection {
    DaemonQueryRejection {
        code: code.to_owned(),
        message: format!("daemon said: {code}"),
        candidates: None,
    }
}

/// A structural-only store (never `--embed`ed) is a stable, distinct error —
/// never an empty success, never a panic (AC3).
#[test]
fn missing_semantic_index_maps_to_stable_error_envelope() {
    let payload = search_code_rejection_error(&rejection("missing_semantic_index"));
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(
        payload["error"]["code"].as_str(),
        Some("missing_semantic_index"),
        "the no-embeddings condition keeps the daemon's stable code: {payload}"
    );
    assert!(
        payload["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("--embed")),
        "the envelope must carry the re-ingest remedy: {payload}"
    );
}

/// A present-but-unreadable index (issue #489) is refused as data loss, never
/// misreported as "never embedded" (AC3).
#[test]
fn unreadable_semantic_index_maps_to_distinct_refusal() {
    let payload = search_code_rejection_error(&rejection("semantic_index_unreadable"));
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(
        payload["error"]["code"].as_str(),
        Some("semantic_index_unreadable"),
        "a damaged index must stay distinct from a missing one: {payload}"
    );
    assert_ne!(
        payload["error"]["code"].as_str(),
        Some("missing_semantic_index"),
        "unreadable must never collapse into missing: {payload}"
    );
}

/// An embedder/index width disagreement is refused under the #104 contract
/// code — parity with the refusal contract, never silently degraded results
/// (AC4).
#[test]
fn incompatible_dimension_maps_to_issue_104_refusal_code() {
    let payload = search_code_rejection_error(&rejection("incompatible_embedding_dimension"));
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(
        payload["error"]["code"].as_str(),
        Some("embedding_dimension_mismatch"),
        "the daemon-path width refusal must use the #104 refusal code: {payload}"
    );
}

/// Rejections the tool does not classify keep the daemon's own stable code —
/// no information is dropped, no new vocabulary is invented for pass-through.
#[test]
fn unknown_rejection_codes_pass_through_unchanged() {
    let payload = search_code_rejection_error(&rejection("query_timeout"));
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(
        payload["error"]["code"].as_str(),
        Some("query_timeout"),
        "unclassified rejections keep the daemon code: {payload}"
    );
}

// ── Payload shaping (AC2, AC5, AC6) ──────────────────────────────────────────

/// Builds a synthetic daemon `semantic_search` verb result: two ranked rows
/// plus the issue #243 provenance envelope and the #221 confidence verdict.
fn synthetic_verb_result() -> Value {
    json!({
        "verb": "semantic_search",
        "snapshot": "2026-09-26T00:00:00Z",
        "records": [
            {
                "record_id": "codegraph:v4:sym-high",
                "name": "retry_loop",
                "repo_relative_path": "src/retry.rs",
                "score": 0.9125,
                "span": {
                    "start_byte": 0, "end_byte": 120,
                    "start_line": 11, "end_line": 20,
                    "start_column": null, "end_column": null,
                },
                "repository_id": "codegraph:v4:repo",
                "repository": "acme/widget",
            },
            {
                "record_id": "codegraph:v4:sym-low",
                "score": 0.3125,
            },
        ],
        "page": { "cursor": null, "has_more": false, "returned": 2 },
        "embedding_provenance": {
            "query_model": {
                "provider": "aletheiadb_re_export",
                "name": "sentence-transformers/all-MiniLM-L6-v2",
                "version": "0.1.0", "dim": 384, "content_hash": "unknown",
            },
            "index_model": {
                "provider": "aletheiadb_re_export",
                "name": "sentence-transformers/all-MiniLM-L6-v2",
                "version": "0.1.0", "dim": 384, "content_hash": "unknown",
            },
            "metric": "cosine",
            "index_fingerprint": "abc123",
            "model_match": true,
            "mismatch_fields": [],
        },
        "confidence": {
            "verdict": "strong",
            "best_score": 0.9125,
            "total_candidates": 2,
            "confident_threshold": 0.75,
            "weak_threshold": 0.5,
            "selection_basis": "corpus_calibrated_confidence_floor",
        },
    })
}

/// The shaped payload echoes the query and effective limit, carries the rows
/// with the `eg query semantic` per-match fields (AC2), forwards the
/// provenance envelope and confidence verdict verbatim, and stamps the
/// compatibility note.
#[test]
fn verb_result_shapes_search_code_payload() {
    let result = synthetic_verb_result();
    let payload = tool_search_code_from_verb_result("where is the retry loop", 10, &result);
    assert_eq!(payload["ok"], Value::from(true));
    assert_eq!(payload["query"], Value::from("where is the retry loop"));
    assert_eq!(payload["limit"], Value::from(10));

    let matches = payload["matches"]
        .as_array()
        .expect("matches must be an array");
    assert_eq!(matches.len(), 2, "both verb rows must be forwarded");

    // AC2: the per-match fields are the CLI's stable row fields.
    let first = &matches[0];
    assert_eq!(first["record_id"], Value::from("codegraph:v4:sym-high"));
    assert_eq!(first["name"], Value::from("retry_loop"));
    assert_eq!(first["repo_relative_path"], Value::from("src/retry.rs"));
    // The row is forwarded verbatim from the daemon verb result: the score
    // the tool emits is byte-identical to the score the verb produced.
    assert_eq!(first["score"], result["records"][0]["score"]);
    assert_eq!(first["span"]["start_line"], Value::from(11));
    assert_eq!(first["repository_id"], Value::from("codegraph:v4:repo"));
    assert_eq!(first["repository"], Value::from("acme/widget"));
    // Absent-when-None CLI rule: the sparse row omits name/path/span.
    let second = &matches[1];
    assert_eq!(second["record_id"], Value::from("codegraph:v4:sym-low"));
    assert!(second.get("name").is_none(), "absent name must stay absent");
    assert!(
        second.get("repo_relative_path").is_none(),
        "absent path must stay absent"
    );
    assert!(second.get("span").is_none(), "absent span must stay absent");

    // Per-row confidence enrichment mirrors the CLI's --daemon row rendering.
    for row in matches {
        assert!(
            row.get("confidence_band").is_some(),
            "every row carries its confidence band: {row}"
        );
        assert!(
            row.get("selection_threshold").is_some(),
            "every row carries the selection threshold: {row}"
        );
        assert!(
            row.get("selection_basis").is_some(),
            "every row carries the selection basis: {row}"
        );
    }
    assert_eq!(
        matches[0]["confidence_band"],
        Value::from("strong"),
        "0.9125 clears the confident threshold"
    );
    assert_eq!(
        matches[1]["confidence_band"],
        Value::from("weak"),
        "0.3125 is below the confident threshold"
    );

    // Provenance + verdict are forwarded verbatim from the verb result.
    assert_eq!(
        payload["embedding_provenance"], result["embedding_provenance"],
        "the #243 envelope must be forwarded verbatim"
    );
    assert_eq!(
        payload["confidence"], result["confidence"],
        "the #221 verdict must be forwarded verbatim"
    );
    assert!(
        payload["compatibility_note"]
            .as_str()
            .is_some_and(|n| n.contains("104")),
        "the daemon-path compatibility caveat must be disclosed in-band: {payload}"
    );
}

/// An empty verb result is a successful, explicitly-empty answer — a stable
/// no-match envelope distinct from every error path, never a fabricated or
/// back-filled record (AC5).
#[test]
fn empty_verb_result_is_explicit_no_match_not_error() {
    let mut result = synthetic_verb_result();
    result["records"] = Value::Array(Vec::new());
    result
        .as_object_mut()
        .expect("result is an object")
        .remove("confidence");
    let payload = tool_search_code_from_verb_result("nothing matches this", 10, &result);
    assert_eq!(
        payload["ok"],
        Value::from(true),
        "no-match is a successful empty answer, not an error: {payload}"
    );
    assert_eq!(
        payload["matches"],
        Value::Array(Vec::new()),
        "no-match carries zero records — never fabricated: {payload}"
    );
    assert!(
        payload["message"]
            .as_str()
            .is_some_and(|m| m.contains("no_semantic_matches")),
        "the no-match envelope names its stable marker: {payload}"
    );
    // The provenance envelope still rides along: the agent can see the query
    // model and the (absent) index identity behind the empty answer.
    assert_eq!(
        payload["embedding_provenance"], result["embedding_provenance"],
        "provenance is stamped even on the empty answer"
    );
}

/// Rows are redaction-safe (AC6): bounded handles only — no raw source text,
/// summaries, or payload bytes may appear in a match row.
#[test]
fn match_rows_carry_only_bounded_handles() {
    const ALLOWED_KEYS: &[&str] = &[
        "record_id",
        "name",
        "repo_relative_path",
        "score",
        "span",
        "repository_id",
        "repository",
        "confidence_band",
        "selection_threshold",
        "selection_basis",
    ];
    let payload =
        tool_search_code_from_verb_result("where is the retry loop", 10, &synthetic_verb_result());
    for row in payload["matches"]
        .as_array()
        .expect("matches must be an array")
    {
        let obj = row.as_object().expect("row must be an object");
        for key in obj.keys() {
            assert!(
                ALLOWED_KEYS.contains(&key.as_str()),
                "row key `{key}` is outside the redaction-safe allow-list: {row}"
            );
        }
    }
}

// ── Live-daemon integration (AC2 ordering, AC9) ───────────────────────────────
// Everything below needs the `embeddings` feature: the daemon's
// `semantic_search` verb only exists there.

/// Allowed JSON keys on a daemon semantic result row (mirrors the #59 AC2/AC6/AC7
/// allow-list plus the CLI's per-row confidence enrichment the tool forwards).
#[cfg(feature = "embeddings")]
const SEMANTIC_FIXTURE_DIM: usize = 4;

/// A deterministic synthetic embedding on two independent 2-D circles, so
/// different records rank differently for different queries without the model.
#[cfg(feature = "embeddings")]
fn semantic_vec_at(theta: f32) -> Vec<f32> {
    vec![
        theta.cos(),
        theta.sin(),
        (2.0 * theta).cos() * 0.5,
        (2.0 * theta).sin() * 0.5,
    ]
}

/// Builds a semantic-enabled fixture store with `count` embeddable symbol
/// records carrying distinct synthetic vectors, persists the vector index,
/// and returns the store's symbol records in creation order. The store is
/// closed (lease released) before returning so a daemon can open it.
#[cfg(feature = "embeddings")]
fn build_semantic_fixture_store(data_dir: &Path, count: usize) -> Vec<GraphRecord> {
    let mut vectors = EmbeddingVectorMap::new();
    let mut records = Vec::new();
    for i in 0..count {
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
                start_column: None,
                end_column: None,
            },
            format!("sym{i:02}"),
            format!("fixture symbol number {i}"),
        );
        #[allow(clippy::cast_precision_loss)]
        let theta = (i as f32) * std::f32::consts::TAU / count as f32;
        vectors.insert(
            EmbeddingVectorKey::from_record(&record).expect("symbol must be embeddable"),
            semantic_vec_at(theta),
        );
        records.push(record);
    }
    let mut sink =
        EmbeddedAletheiaSink::open_with_embeddings(data_dir, vectors, SEMANTIC_FIXTURE_DIM)
            .expect("semantic fixture store should open");
    for record in &records {
        sink.write_record(record)
            .expect("fixture semantic record should write");
    }
    sink.persist_indexes()
        .expect("semantic fixture indexes should persist");
    drop(sink);
    records
}

/// One ranked reference match: `(record_id, score, name, repo_relative_path, span)`.
#[cfg(feature = "embeddings")]
type ReferenceMatch = (
    String,
    f32,
    Option<String>,
    Option<String>,
    Option<SourceSpan>,
);

/// Reference ranking computed against the persisted store via the same
/// embedded `semantic_search` the non-daemon CLI uses, in canonical order
/// (score desc, record id asc), code kinds only, capped at `limit`. Reads
/// through a leased `open`, then releases before any daemon starts.
#[cfg(feature = "embeddings")]
fn embedded_reference_ranking(
    data_dir: &Path,
    query_vector: &[f32],
    limit: usize,
) -> Vec<ReferenceMatch> {
    use aletheia_egregore::adapters::compare_semantic_matches;

    let sink = EmbeddedAletheiaSink::open(data_dir).expect("reference store should reopen");
    // Bounded like the daemon verb's own ceiling: `semantic_search` pre-sizes
    // its result vec, so `usize::MAX` would overflow the capacity.
    let mut matches = sink
        .semantic_search(query_vector, limit.max(100))
        .expect("reference semantic search should succeed");
    // Same code-kind filter the CLI and the daemon verb apply (issue #91).
    matches.retain(|m| {
        m.kind
            .as_deref()
            .is_some_and(|k| k == "File" || k == "Symbol")
    });
    matches.sort_by(compare_semantic_matches);
    matches.truncate(limit);
    drop(sink);
    matches
        .into_iter()
        .map(|m| (m.record_id, m.score, m.name, m.repo_relative_path, m.span))
        .collect()
}

#[cfg(feature = "embeddings")]
struct RunningDaemon {
    child: Option<Child>,
}

#[cfg(feature = "embeddings")]
impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"))
            .arg("daemon")
            .arg("stop")
            .arg("--data-dir")
            .arg(
                self.child
                    .as_ref()
                    .map(|_| PathBuf::from("unused"))
                    .unwrap_or_default(),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawns `eg daemon run` on the fixture store and waits for the running
/// state, mirroring `tests/integration/daemon.rs`.
#[cfg(feature = "embeddings")]
fn start_daemon(data_dir: &Path) -> RunningDaemon {
    use aletheia_egregore::daemon::runtime_dir_for_data_dir;

    fs::create_dir_all(data_dir).expect("should create data dir");
    let mut command = ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"));
    command
        .arg("daemon")
        .arg("run")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg("0")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("daemon should spawn");
    let metadata_path = runtime_dir_for_data_dir(data_dir).join("egregored.json");
    let start = Instant::now();
    loop {
        if let Ok(contents) = fs::read_to_string(&metadata_path)
            && let Ok(metadata) = serde_json::from_str::<Value>(&contents)
            && metadata["state"] == "running"
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "daemon metadata should reach running at {}",
            metadata_path.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
    RunningDaemon { child: Some(child) }
}

#[cfg(feature = "embeddings")]
fn query_theta(index: usize, count: usize) -> Vec<f32> {
    #[allow(clippy::cast_precision_loss)]
    let theta = (index as f32 + 0.37) * std::f32::consts::TAU / count as f32;
    semantic_vec_at(theta)
}

/// AC9(a): the tool returns the same ranked match set as the embedded
/// reference — per-match `record_id` + `name` + `repo_relative_path` + `span`
/// exactly, in the same ranked order, with scores inside the documented
/// daemon-vs-embedded tolerance (AC2 parity at the value level).
#[cfg(feature = "embeddings")]
#[test]
fn tool_matches_embedded_reference_ranking_and_fields() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("semantic-parity-store");
    let count = 12;
    build_semantic_fixture_store(&data_dir, count);
    let query_vector = query_theta(3, count);
    let reference = embedded_reference_ranking(&data_dir, &query_vector, 10);

    let daemon = start_daemon(&data_dir);
    let payload = tool_search_code_with_vector(
        &data_dir,
        Path::new("."),
        "fixture query",
        search_code_effective_limit(None),
        &query_vector,
    );
    drop(daemon);

    assert_eq!(payload["ok"], Value::from(true), "payload: {payload}");
    assert_eq!(payload["query"], Value::from("fixture query"));
    assert_eq!(payload["limit"], Value::from(10));
    let matches = payload["matches"]
        .as_array()
        .expect("matches must be an array");
    assert_eq!(
        matches.len(),
        reference.len(),
        "tool must return the full reference set: {payload}"
    );
    for (row, (id, score, name, path, span)) in matches.iter().zip(reference.iter()) {
        assert_eq!(
            row["record_id"].as_str(),
            Some(id.as_str()),
            "ranked order must match the reference: {payload}"
        );
        // Documented score tolerance (see the #59 daemon parity test):
        // daemon and embedded read the same persisted index, so scores
        // agree within a tight epsilon rather than bit-identically.
        #[allow(clippy::cast_possible_truncation)]
        let tool_score = row["score"].as_f64().expect("score must be a number") as f32;
        assert!(
            (tool_score - score).abs() <= 1e-4,
            "score drift beyond the documented tolerance: {tool_score} vs {score}"
        );
        assert_eq!(
            row["name"].as_str(),
            name.as_deref(),
            "name must match the reference"
        );
        assert_eq!(
            row["repo_relative_path"].as_str(),
            path.as_deref(),
            "path must match the reference"
        );
        let expected_span = serde_json::to_value(span).expect("span must serialize");
        assert_eq!(
            row.get("span").unwrap_or(&Value::Null),
            &expected_span,
            "span must match the reference"
        );
    }
    // The freshness trust signal rides along on success, like every read tool.
    assert!(
        payload.get("freshness").is_some(),
        "successful responses carry the freshness stamp: {payload}"
    );
}

/// AC9(a), limit lane: an explicit limit bounds the ranked set; the default
/// (10) applies when the arg is absent.
#[cfg(feature = "embeddings")]
#[test]
fn tool_respects_explicit_and_default_limits() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("semantic-limit-store");
    let count = 12;
    build_semantic_fixture_store(&data_dir, count);
    let query_vector = query_theta(0, count);

    let daemon = start_daemon(&data_dir);
    let limited = tool_search_code_with_vector(
        &data_dir,
        Path::new("."),
        "fixture query",
        search_code_effective_limit(Some(3)),
        &query_vector,
    );
    let defaulted = tool_search_code_with_vector(
        &data_dir,
        Path::new("."),
        "fixture query",
        search_code_effective_limit(None),
        &query_vector,
    );
    drop(daemon);

    assert_eq!(
        limited["matches"].as_array().expect("array").len(),
        3,
        "explicit limit=3 must return exactly 3 matches"
    );
    assert_eq!(limited["limit"], Value::from(3));
    assert_eq!(
        defaulted["matches"].as_array().expect("array").len(),
        10,
        "absent limit must default to 10 matches"
    );
    // The limited set is the prefix of the defaulted set: same ranking.
    let limited_ids: Vec<&str> = limited["matches"]
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["record_id"].as_str().expect("record_id"))
        .collect();
    let defaulted_ids: Vec<&str> = defaulted["matches"]
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["record_id"].as_str().expect("record_id"))
        .collect();
    assert_eq!(
        &limited_ids,
        &defaulted_ids[..3],
        "the limited set must be the ranking prefix"
    );
}

/// AC9(b): a structural-only store (no vector index) yields the stable
/// `missing_semantic_index` envelope — never an empty success, never a panic.
#[cfg(feature = "embeddings")]
#[test]
fn structural_only_store_returns_missing_semantic_index() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("structural-store");
    {
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("structural store should open");
        let record = GraphRecord::symbol(
            stable_id(&["node", "symbol", "src/lib.rs", "plain"]),
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
            "plain".to_owned(),
            "fixture symbol".to_owned(),
        );
        sink.write_record(&record).expect("record should write");
    }

    let daemon = start_daemon(&data_dir);
    let payload = tool_search_code_with_vector(
        &data_dir,
        Path::new("."),
        "fixture query",
        10,
        &semantic_vec_at(0.5),
    );
    drop(daemon);

    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(
        payload["error"]["code"].as_str(),
        Some("missing_semantic_index"),
        "structural-only store must surface the stable code: {payload}"
    );
}

/// AC9(c): an index that exists but holds no vectors is a successful,
/// explicitly-empty answer — the stable no-match envelope, never an error.
#[cfg(feature = "embeddings")]
#[test]
fn indexed_but_empty_store_returns_no_match_envelope() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("empty-index-store");
    {
        // The vector index EXISTS (Loaded, dim 4) but no record carries a
        // vector, so the search legitimately returns nothing.
        let vectors = EmbeddingVectorMap::new();
        let mut sink =
            EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, SEMANTIC_FIXTURE_DIM)
                .expect("empty-index store should open");
        let record = GraphRecord::symbol(
            stable_id(&["node", "symbol", "src/lib.rs", "unembedded"]),
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
            "unembedded".to_owned(),
            "fixture symbol without a vector".to_owned(),
        );
        sink.write_record(&record).expect("record should write");
        sink.persist_indexes().expect("indexes should persist");
    }

    let daemon = start_daemon(&data_dir);
    let payload = tool_search_code_with_vector(
        &data_dir,
        Path::new("."),
        "fixture query",
        10,
        &semantic_vec_at(0.5),
    );
    drop(daemon);

    assert_eq!(
        payload["ok"],
        Value::from(true),
        "empty index is a successful empty answer: {payload}"
    );
    assert_eq!(
        payload["matches"],
        Value::Array(Vec::new()),
        "no-match carries zero records: {payload}"
    );
    assert!(
        payload["message"]
            .as_str()
            .is_some_and(|m| m.contains("no_semantic_matches")),
        "the no-match marker must be present: {payload}"
    );
}

/// AC4 against a live daemon: a query vector whose width disagrees with the
/// index is refused under the #104 contract code — never silently degraded.
#[cfg(feature = "embeddings")]
#[test]
fn width_mismatched_vector_is_refused_live() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("semantic-width-store");
    build_semantic_fixture_store(&data_dir, 8);

    let daemon = start_daemon(&data_dir);
    // The fixture index is 4-D; a 5-wide vector cannot share its space.
    let payload = tool_search_code_with_vector(
        &data_dir,
        Path::new("."),
        "fixture query",
        10,
        &[0.1, 0.2, 0.3, 0.4, 0.5],
    );
    drop(daemon);

    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(
        payload["error"]["code"].as_str(),
        Some("embedding_dimension_mismatch"),
        "width disagreement must refuse under the #104 code: {payload}"
    );
}

/// AC9(d): repeated runs of the same query on an unchanged store return
/// byte-identical results.
#[cfg(feature = "embeddings")]
#[test]
fn repeated_queries_are_byte_identical() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("semantic-determinism-store");
    let count = 12;
    build_semantic_fixture_store(&data_dir, count);
    let query_vector = query_theta(5, count);

    let daemon = start_daemon(&data_dir);
    let first = serde_json::to_string(&tool_search_code_with_vector(
        &data_dir,
        Path::new("."),
        "fixture query",
        10,
        &query_vector,
    ))
    .expect("payload must serialize");
    let second = serde_json::to_string(&tool_search_code_with_vector(
        &data_dir,
        Path::new("."),
        "fixture query",
        10,
        &query_vector,
    ))
    .expect("payload must serialize");
    drop(daemon);

    assert_eq!(
        first, second,
        "repeated runs on an unchanged store must be byte-identical"
    );
}
