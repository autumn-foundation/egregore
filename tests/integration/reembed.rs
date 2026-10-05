#![allow(missing_docs)]
//! Issue #167: in-place re-embedding of an `--embed` store under a new local
//! model, without rescanning sources.
//!
//! Fixture: three tiny deterministic BERT models generated at test time —
//! `a-384` (model A, 384 dims), `b-384` (model B at 384 dims, a different
//! seed so its vector space differs), and `b-768` (model B at 768 dims).
//! Seeded weights, a hand-rolled 64-token `WordPiece` `tokenizer.json`, and
//! hand-rolled safetensors files validated by loading through the real
//! `BertEmbedder` path. Model A is staged into an isolated `HF_HOME` so
//! `eg ingest --embed --embed-model` loads it with `HF_ENDPOINT` pointed at
//! a dead address: the suite needs no network and no warm Hugging Face
//! cache.
//!
//! The model generator lives in the shared [`super::embed_fixture`] module so
//! other suites can stage the same deterministic local models.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
use super::embed_fixture::{DEAD_HF_ENDPOINT, generate_fixture_model, stage_hub_model};

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
use assert_cmd::Command;

/// Model A: staged into the isolated HF cache under this hub id for ingest.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
const MODEL_A_ID: &str = "local/test-a-384";
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
const MODEL_A_DIM: usize = 384;

/// `eg` with the network dead and the HF cache isolated: any model load must
/// resolve from `hf_home` or refuse — never download.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn eg_isolated(hf_home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
    cmd.env("HF_HOME", hf_home);
    cmd.env("HF_ENDPOINT", DEAD_HF_ENDPOINT);
    cmd
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

/// Generates model A locally, stages it into an isolated HF cache, scans the
/// fixture repo, and ingests it `--embed --embed-model <model-a>` into a
/// fresh embedded store; returns `(data_dir, hf_home)`. Every `eg` invocation
/// runs with the network dead (`HF_ENDPOINT` → 127.0.0.1:9), so the suite
/// proves no download happens at any step.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn ingest_embedded_store_with_model_a(temp: &Path) -> (PathBuf, PathBuf) {
    let model_a_dir = temp.join("models/a-384");
    generate_fixture_model(&model_a_dir, MODEL_A_DIM, 0xA11C_E254_68BA_DF00);
    let hf_home = temp.join("hf-home");
    stage_hub_model(&hf_home, MODEL_A_ID, &model_a_dir);

    let graph_path = temp.join("graph.jsonl");
    // Write the scanned graph via the CLI surface for the store build itself.
    eg_isolated(&hf_home)
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let data_dir = temp.join("store");
    eg_isolated(&hf_home)
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--embed")
        .arg("--embed-model")
        .arg(MODEL_A_ID)
        .assert()
        .success();
    (data_dir, hf_home)
}

/// Reads every record in the store, keyed by record id, as canonical JSON —
/// the byte-identity snapshot for AC3 (vectors are node properties, not part
/// of the record JSON).
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn snapshot_records(data_dir: &Path) -> BTreeMap<String, String> {
    use aletheia_egregore::adapters::EmbeddedAletheiaSink;
    let sink = EmbeddedAletheiaSink::open(data_dir).expect("store should open");
    let records = sink.read_all_records().expect("records should be readable");
    records
        .into_iter()
        .map(|record| {
            let id = match &record {
                aletheia_egregore::GraphRecord::Node { id, .. }
                | aletheia_egregore::GraphRecord::Edge { id, .. }
                | aletheia_egregore::GraphRecord::Tombstone { id, .. } => id.clone(),
            };
            let json = serde_json::to_string(&record).expect("record should serialize");
            (id, json)
        })
        .collect()
}

/// The fixed identity record id (issue #104); excluded from the AC3
/// byte-identity comparison because re-embed legitimately supersedes it.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn identity_record_id() -> String {
    aletheia_egregore::embeddings::embedding_index_identity_id()
}

/// Reads every persisted embedding vector as (`record_id`, dim, `blake3` of the
/// LE f32 bytes) — the stored-vector evidence for AC5 (no mixed dimensions)
/// and AC8 (deterministic stored bytes).
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn snapshot_vectors(data_dir: &Path) -> BTreeMap<String, (usize, String)> {
    use aletheia_egregore::adapters::EmbeddedAletheiaSink;
    let sink = EmbeddedAletheiaSink::open(data_dir).expect("store should open");
    let vectors = sink
        .read_persisted_vectors()
        .expect("vectors should be readable");
    vectors
        .into_iter()
        .map(|(record_id, vector)| {
            let mut hasher = blake3::Hasher::new();
            for value in &vector {
                hasher.update(&value.to_le_bytes());
            }
            (
                record_id,
                (vector.len(), hasher.finalize().to_hex().to_string()),
            )
        })
        .collect()
}

/// Runs `eg query semantic` with the isolated HF env and returns (exit code,
/// parsed stdout JSON lines).
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn query_semantic(
    hf_home: &Path,
    data_dir: &Path,
    query: &str,
    embed_model: Option<&Path>,
) -> (i32, Vec<serde_json::Value>) {
    let mut cmd = eg_isolated(hf_home);
    cmd.arg("query")
        .arg("semantic")
        .arg(query)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--limit")
        .arg("5")
        .arg("--format")
        .arg("json");
    if let Some(model) = embed_model {
        cmd.arg("--embed-model").arg(model);
    }
    let output = cmd.output().expect("query should run");
    let code = output.status.code().unwrap_or(-1);
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line.trim()).ok())
        .collect();
    (code, lines)
}

/// Runs `eg re-embed` with the isolated HF env and returns (exit code, parsed
/// stdout JSON).
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn run_reembed(
    hf_home: &Path,
    data_dir: &Path,
    model: &Path,
    extra_args: &[&str],
) -> (i32, serde_json::Value, String) {
    let mut cmd = eg_isolated(hf_home);
    cmd.arg("re-embed")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--model")
        .arg(model)
        .arg("--format")
        .arg("json");
    for arg in extra_args {
        cmd.arg(arg);
    }
    let output = cmd.output().expect("re-embed should run");
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or(serde_json::Value::Null);
    (
        code,
        parsed,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Full #167 workflow: refuse (model A store, model B query) → re-embed →
/// answer (model B query), with byte-identity, counts, dimension change,
/// no-op rerun, and determinism.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn reembed_refuse_to_answer_transition() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let ctx = transition_setup(temp.path());

    // #104 refuses the model-B query while the store is under model A (AC2,
    // refuse half). Same dimension, different model → exit 10.
    let (code, _) = query_semantic(
        &ctx.hf_home,
        &ctx.data_dir,
        "function",
        Some(&ctx.model_b384),
    );
    assert_eq!(
        code, 10,
        "model-B query against model-A store should refuse with 10"
    );

    let (candidates, top_k_first) = transition_same_dim_reembed(&ctx);
    transition_noop_rerun(&ctx, candidates, &top_k_first);
    transition_dim_change(&ctx, candidates);
}

/// Shared context for the refuse→answer transition phases.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
struct TransitionCtx {
    data_dir: PathBuf,
    hf_home: PathBuf,
    model_b384: PathBuf,
    model_b768: PathBuf,
    before: BTreeMap<String, String>,
    identity_id: String,
}

/// Generates both fixture models, ingests the model-A store, and asserts the
/// starting identity.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn transition_setup(temp: &Path) -> TransitionCtx {
    let model_b384 = temp.join("models/b-384");
    let model_b768 = temp.join("models/b-768");
    generate_fixture_model(&model_b384, 384, 0x9E37_79B9_7F4A_7C15);
    generate_fixture_model(&model_b768, 768, 0x1234_5678_9ABC_DEF0);

    let (data_dir, hf_home) = ingest_embedded_store_with_model_a(temp);
    let before = snapshot_records(&data_dir);
    let identity_id = identity_record_id();
    assert!(
        before.contains_key(&identity_id),
        "ingested store should carry the #104 identity record"
    );
    let identity_before: serde_json::Value =
        serde_json::from_str(&before[&identity_id]).expect("identity should parse");
    assert_eq!(
        identity_before["embedding_model"]["name"],
        serde_json::json!(MODEL_A_ID),
        "store should start under model A"
    );
    assert_eq!(
        identity_before["embedding_model"]["dim"],
        serde_json::json!(MODEL_A_DIM),
    );
    TransitionCtx {
        data_dir,
        hf_home,
        model_b384,
        model_b768,
        before,
        identity_id,
    }
}

/// Same-dimension re-embed under model B: JSON counts (AC1, AC4),
/// byte-identity of non-semantic records (AC3), the flipped identity, and the
/// answer half of AC2. Returns the candidate count and the first top-k.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn transition_same_dim_reembed(ctx: &TransitionCtx) -> (u64, Vec<serde_json::Value>) {
    // Re-embed under model B (384 dims): no rescan, JSON counts (AC1, AC4).
    let (code, report, stderr) = run_reembed(&ctx.hf_home, &ctx.data_dir, &ctx.model_b384, &[]);
    assert_eq!(code, 0, "re-embed should succeed: {stderr}");
    assert_eq!(report["ok"], serde_json::json!(true));
    assert_eq!(report["dimension_changed"], serde_json::json!(false));
    assert_eq!(report["failed"], serde_json::json!(0));
    let candidates = report["candidates"].as_u64().expect("candidates count");
    assert!(
        candidates > 0,
        "fixture store should embed at least one node"
    );
    assert_eq!(report["reembedded"], serde_json::json!(candidates));
    assert_eq!(report["skipped"], serde_json::json!(0));
    assert_eq!(
        report["target_model"]["dim"],
        serde_json::json!(384),
        "report should name the target dimension"
    );

    // AC3: every non-semantic record is byte-identical (IDs, spans, everything
    // except the superseded identity record and the vector properties).
    let after = snapshot_records(&ctx.data_dir);
    assert_eq!(
        ctx.before.len(),
        after.len(),
        "re-embed must not add or drop records"
    );
    assert_non_semantic_byte_identical(&ctx.before, &after, &ctx.identity_id);
    // The identity record now reflects model B (AC2, identity half).
    let identity_after: serde_json::Value =
        serde_json::from_str(&after[&ctx.identity_id]).expect("identity should parse");
    assert_eq!(
        identity_after["embedding_model"]["dim"],
        serde_json::json!(384)
    );
    assert_ne!(
        identity_after["embedding_model"]["name"],
        serde_json::json!(MODEL_A_ID),
        "identity should no longer name model A"
    );
    assert_eq!(
        identity_after["id"],
        serde_json::from_str::<serde_json::Value>(&ctx.before[&ctx.identity_id])
            .expect("identity should parse")["id"],
        "identity id is fixed"
    );

    // Answer half: the model-B query now returns a ranked answer (AC2).
    let (code, lines) = query_semantic(
        &ctx.hf_home,
        &ctx.data_dir,
        "function",
        Some(&ctx.model_b384),
    );
    assert_eq!(code, 0, "model-B query should answer after re-embed");
    let provenance = lines
        .iter()
        .find_map(|line| line.get("embedding_provenance"))
        .expect("answer should carry the #243 provenance envelope");
    assert_eq!(provenance["model_match"], serde_json::json!(true));
    assert_eq!(
        provenance["index_model"]["dim"],
        serde_json::json!(384),
        "provenance should show the B index identity"
    );
    let result_rows: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line.get("record_id").is_some())
        .collect();
    assert!(
        !result_rows.is_empty(),
        "model-B query should return ranked rows"
    );
    let top_k_first: Vec<serde_json::Value> =
        result_rows.iter().map(|row| (*row).clone()).collect();

    // The model-A query now refuses: the identity really flipped.
    let (code, _) = query_semantic(&ctx.hf_home, &ctx.data_dir, "function", None);
    assert_eq!(
        code, 10,
        "default-model query against model-B store should refuse with 10"
    );

    (candidates, top_k_first)
}

/// Re-running re-embed is a deterministic no-op (AC8): same counts shape,
/// zero reembedded, the top-k answer is byte-identical, and the stored vector
/// bytes are unchanged.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn transition_noop_rerun(ctx: &TransitionCtx, candidates: u64, top_k_first: &[serde_json::Value]) {
    let vectors_before = snapshot_vectors(&ctx.data_dir);
    let (code, rerun_report, stderr) =
        run_reembed(&ctx.hf_home, &ctx.data_dir, &ctx.model_b384, &[]);
    assert_eq!(code, 0, "re-embed rerun should succeed: {stderr}");
    assert_eq!(rerun_report["reembedded"], serde_json::json!(0));
    assert_eq!(rerun_report["candidates"], serde_json::json!(candidates));
    let vectors_after = snapshot_vectors(&ctx.data_dir);
    assert_eq!(
        vectors_before, vectors_after,
        "no-op re-embed must leave stored vector bytes identical (AC8)"
    );
    let (code, lines) = query_semantic(
        &ctx.hf_home,
        &ctx.data_dir,
        "function",
        Some(&ctx.model_b384),
    );
    assert_eq!(code, 0);
    let top_k_second: Vec<serde_json::Value> = lines
        .iter()
        .filter(|line| line.get("record_id").is_some())
        .map(|row| (*row).clone())
        .collect();
    assert_eq!(
        top_k_first, top_k_second,
        "repeated re-embed must leave top-k bytes identical (AC8)"
    );
}

/// Dimension change: re-embed under the 768-dim model rebuilds the index
/// without mixing vector spaces (AC5), and AC3 holds again afterwards.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn transition_dim_change(ctx: &TransitionCtx, candidates: u64) {
    let (code, dim_report, stderr) = run_reembed(&ctx.hf_home, &ctx.data_dir, &ctx.model_b768, &[]);
    assert_eq!(code, 0, "dim-change re-embed should succeed: {stderr}");
    assert_eq!(dim_report["dimension_changed"], serde_json::json!(true));
    assert_eq!(dim_report["index_rebuilt"], serde_json::json!(true));
    assert_eq!(dim_report["target_model"]["dim"], serde_json::json!(768));
    assert_eq!(dim_report["reembedded"], serde_json::json!(candidates));

    // Every remaining stored vector is 768-wide: no mixed dimensions survive
    // the dimension change (AC5, direct stored-vector evidence).
    let vectors = snapshot_vectors(&ctx.data_dir);
    assert_eq!(
        vectors.len(),
        usize::try_from(candidates).expect("candidate count fits"),
        "every candidate should still carry a stored vector"
    );
    for (record_id, (dim, _)) in &vectors {
        assert_eq!(
            *dim, 768,
            "stored vector for {record_id} must be 768-wide after dim change"
        );
    }

    // Every remaining vector is 768-wide: the 768-dim query answers (AC5).
    let (code, lines) = query_semantic(
        &ctx.hf_home,
        &ctx.data_dir,
        "function",
        Some(&ctx.model_b768),
    );
    assert_eq!(code, 0, "768-dim query should answer after dim change");
    let provenance = lines
        .iter()
        .find_map(|line| line.get("embedding_provenance"))
        .expect("answer should carry provenance");
    assert_eq!(provenance["model_match"], serde_json::json!(true));
    assert_eq!(provenance["index_model"]["dim"], serde_json::json!(768));
    // The 384-dim model query now refuses on dimension, never ranking across
    // mixed spaces (AC5).
    let (code, _) = query_semantic(
        &ctx.hf_home,
        &ctx.data_dir,
        "function",
        Some(&ctx.model_b384),
    );
    assert_eq!(
        code, 9,
        "384-dim query against 768-dim store should refuse with 9"
    );

    // AC3 again after the dimension change: non-semantic records still
    // byte-identical.
    let after_dim = snapshot_records(&ctx.data_dir);
    assert_eq!(ctx.before.len(), after_dim.len());
    assert_non_semantic_byte_identical(&ctx.before, &after_dim, &ctx.identity_id);
}

/// Asserts every record except the identity record is byte-identical between
/// two snapshots (AC3).
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn assert_non_semantic_byte_identical(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    identity_id: &str,
) {
    for (id, json_before) in before {
        if id == identity_id {
            continue;
        }
        assert_eq!(
            after.get(id),
            Some(json_before),
            "non-semantic record {id} must be byte-identical after re-embed"
        );
    }
}

/// A model that is not available locally is refused BEFORE anything loads or
/// downloads: distinct exit code, stable envelope, store untouched (AC7).
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn reembed_refuses_when_model_not_available_locally() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let (data_dir, hf_home) = ingest_embedded_store_with_model_a(temp.path());
    let before = snapshot_records(&data_dir);

    let missing = temp.path().join("models/does-not-exist");
    let (code, envelope, _) = run_reembed(&hf_home, &data_dir, &missing, &[]);
    assert_eq!(code, 12, "absent local model should exit 12");
    assert_eq!(envelope["ok"], serde_json::json!(false));
    assert_eq!(
        envelope["error"]["code"],
        serde_json::json!("embedding_model_unavailable_locally")
    );

    // Unknown hub ids are refused the same way — never downloaded (the dead
    // HF_ENDPOINT would fail fast instead of hanging if anything tried).
    let (code, _, _) = run_reembed(
        &hf_home,
        &data_dir,
        Path::new("no-such-org/no-such-model-167"),
        &[],
    );
    assert_eq!(code, 12);

    // The store is untouched: identity still model A, records byte-identical.
    let after = snapshot_records(&data_dir);
    assert_eq!(before, after, "refused re-embed must not touch the store");
}

/// `--dry-run` plans without mutating: exit 13 while work remains, and the
/// store is byte-identical afterwards.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn reembed_dry_run_plans_without_mutating() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let model_b384 = temp.path().join("models/b-384");
    generate_fixture_model(&model_b384, 384, 0x9E37_79B9_7F4A_7C15);
    let (data_dir, hf_home) = ingest_embedded_store_with_model_a(temp.path());
    let before = snapshot_records(&data_dir);

    let (code, plan, _) = run_reembed(&hf_home, &data_dir, &model_b384, &["--dry-run"]);
    assert_eq!(code, 13, "dry-run with pending work should exit 13");
    assert_eq!(plan["dry_run"], serde_json::json!(true));
    assert!(plan["candidates"].as_u64().unwrap() > 0);

    let after = snapshot_records(&data_dir);
    assert_eq!(before, after, "dry-run must not mutate the store");
}

/// AC8, vector half: the same model embedding the same texts twice yields
/// byte-identical vectors, through the real `BertEmbedder` load path.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn reembed_vectors_are_deterministic() {
    use aletheia_egregore::reembed;

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let model_b384 = temp.path().join("models/b-384");
    generate_fixture_model(&model_b384, 384, 0x9E37_79B9_7F4A_7C15);

    let spec = reembed::resolve_model_spec(&model_b384.to_string_lossy());
    let resolved =
        reembed::ensure_model_available_locally(&spec).expect("fixture model should resolve");
    let embedder = reembed::load_embedder(&resolved).expect("fixture model should load");
    let texts = ["fn alpha() {}", "struct Beta { x: i32 }", "fn alpha() {}"];
    let first = reembed::embed_texts(&embedder, &texts).expect("embed should succeed");
    let second = reembed::embed_texts(&embedder, &texts).expect("embed should succeed");
    assert_eq!(first.len(), 3);
    assert!(
        first.iter().all(|vector| vector.len() == 384),
        "fixture model should embed at 384 dims"
    );
    assert_eq!(
        first, second,
        "same model + same texts must yield byte-identical vectors"
    );
    assert_eq!(first[0], first[2], "same text must embed identically");
    assert_ne!(first[0], first[1], "different texts must embed differently");
}

/// AC8, store half: two independent ingest → re-embed runs produce
/// byte-identical top-k answers.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn reembed_deterministic_across_independent_runs() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let model_b384 = temp.path().join("models/b-384");
    generate_fixture_model(&model_b384, 384, 0x9E37_79B9_7F4A_7C15);

    let (data_dir_a, hf_home_a) = ingest_embedded_store_with_model_a(temp.path());
    let (code, report_a, stderr) = run_reembed(&hf_home_a, &data_dir_a, &model_b384, &[]);
    assert_eq!(code, 0, "first re-embed should succeed: {stderr}");
    assert_eq!(report_a["ok"], serde_json::json!(true));

    // A second, fully independent store from the same fixture repo.
    let temp_b = tempfile::tempdir().expect("temp dir should be created");
    let (data_dir_b, hf_home_b) = ingest_embedded_store_with_model_a(temp_b.path());
    let (code, report_b, stderr) = run_reembed(&hf_home_b, &data_dir_b, &model_b384, &[]);
    assert_eq!(code, 0, "second re-embed should succeed: {stderr}");
    assert_eq!(
        report_a["candidates"], report_b["candidates"],
        "independent runs must agree on the candidate set"
    );

    let top_k = |hf_home: &Path, data_dir: &Path| -> Vec<serde_json::Value> {
        let (code, lines) = query_semantic(hf_home, data_dir, "function", Some(&model_b384));
        assert_eq!(code, 0, "query should answer");
        lines
            .into_iter()
            .filter(|line| line.get("record_id").is_some())
            .collect()
    };
    assert_eq!(
        top_k(&hf_home_a, &data_dir_a),
        top_k(&hf_home_b, &data_dir_b),
        "independent re-embed runs must produce byte-identical top-k"
    );
}

/// AC6: a crash between the vector commit and the index rebuild leaves an
/// explicit, stable incomplete state — never a ranked answer across mixed
/// spaces — and re-running `eg re-embed` heals it.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn reembed_heals_interrupted_index_rebuild() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let model_b768 = temp.path().join("models/b-768");
    generate_fixture_model(&model_b768, 768, 0x1234_5678_9ABC_DEF0);
    let (data_dir, hf_home) = ingest_embedded_store_with_model_a(temp.path());

    // Commit the dimension change fully, then simulate a crash between the
    // vector commit and the index rebuild by deleting the vector-index
    // artifacts: identity B + B vectors on the nodes, no usable index.
    let (code, report, stderr) = run_reembed(&hf_home, &data_dir, &model_b768, &[]);
    assert_eq!(code, 0, "dim-change re-embed should succeed: {stderr}");
    assert_eq!(report["dimension_changed"], serde_json::json!(true));
    aletheia_egregore::adapters::remove_persisted_vector_index(&data_dir)
        .expect("index artifacts should be removable");

    // The incomplete state is explicit and stable: the query reports the
    // documented no-index diagnostic (exit 2), never a ranked answer.
    let mut cmd = eg_isolated(&hf_home);
    cmd.arg("query")
        .arg("semantic")
        .arg("function")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--embed-model")
        .arg(&model_b768)
        .arg("--format")
        .arg("json");
    let output = cmd.output().expect("query should run");
    assert_eq!(
        output.status.code(),
        Some(2),
        "query against the index-less store should exit 2, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("semantic_index_absent"),
        "the incomplete state must carry the stable diagnostic"
    );

    // Re-running re-embed heals it: the identity already matches, the missing
    // index is rebuilt from the committed vectors, and the query answers.
    let (code, heal_report, stderr) = run_reembed(&hf_home, &data_dir, &model_b768, &[]);
    assert_eq!(code, 0, "healing re-embed should succeed: {stderr}");
    assert_eq!(heal_report["ok"], serde_json::json!(true));
    assert_eq!(heal_report["index_rebuilt"], serde_json::json!(true));
    let (code, lines) = query_semantic(&hf_home, &data_dir, "function", Some(&model_b768));
    assert_eq!(code, 0, "query should answer after the heal");
    let provenance = lines
        .iter()
        .find_map(|line| line.get("embedding_provenance"))
        .expect("answer should carry provenance");
    assert_eq!(provenance["model_match"], serde_json::json!(true));
}

/// AC6 (other interruption point): a crash between the old-index deletion and
/// the vector commit leaves identity A over A vectors with no usable index —
/// still a model-A store, just index-less. Re-running `eg re-embed --model B`
/// completes the migration from that state.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn reembed_heals_interrupted_before_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let model_b768 = temp.path().join("models/b-768");
    generate_fixture_model(&model_b768, 768, 0x1234_5678_9ABC_DEF0);
    let (data_dir, hf_home) = ingest_embedded_store_with_model_a(temp.path());

    // Simulate a crash after the old-dimension index deletion but before the
    // vector commit: identity A + A vectors on the nodes, no usable index.
    aletheia_egregore::adapters::remove_persisted_vector_index(&data_dir)
        .expect("index artifacts should be removable");

    // The store still answers model-A queries? No — without an index the
    // query reports the explicit no-index diagnostic (exit 2), never a stale
    // or mixed ranking.
    let (code, _) = query_semantic(&hf_home, &data_dir, "function", None);
    assert_eq!(
        code, 2,
        "query against the index-less model-A store should exit 2"
    );

    // Re-running re-embed under model B completes the migration: the
    // pre-commit state (A identity, A vectors) is a valid starting point.
    let (code, report, stderr) = run_reembed(&hf_home, &data_dir, &model_b768, &[]);
    assert_eq!(
        code, 0,
        "re-embed from the interrupted state should succeed: {stderr}"
    );
    assert_eq!(report["ok"], serde_json::json!(true));
    assert_eq!(report["dimension_changed"], serde_json::json!(true));
    assert_eq!(report["index_rebuilt"], serde_json::json!(true));
    assert_eq!(report["target_model"]["dim"], serde_json::json!(768));

    // The healed store answers under model B with all vectors at 768 dims.
    let (code, _) = query_semantic(&hf_home, &data_dir, "function", Some(&model_b768));
    assert_eq!(code, 0, "query should answer after the heal");
    let vectors = snapshot_vectors(&data_dir);
    assert!(!vectors.is_empty(), "healed store should carry vectors");
    for (record_id, (dim, _)) in &vectors {
        assert_eq!(*dim, 768, "stored vector for {record_id} must be 768-wide");
    }
}
