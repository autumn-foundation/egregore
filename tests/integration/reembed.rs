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

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;

/// Model A: staged into the isolated HF cache under this hub id for ingest.
const MODEL_A_ID: &str = "local/test-a-384";
const MODEL_A_DIM: usize = 384;

/// Deterministic xorshift64* PRNG — std-only, no extra deps.
struct XorShift64(u64);

impl XorShift64 {
    const fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    // Test-only deterministic PRNG: the int->float casts are the intended
    // uniform mapping, and the output range is exact by construction.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn next_f32(&mut self) -> f32 {
        // Uniform in [-0.05, 0.05): small deterministic weights.
        ((self.next_u64() as f64 / u64::MAX as f64) as f32).mul_add(0.1, -0.05)
    }
}

/// Hand-rolled safetensors writer (F32 only). The header layout mirrors the
/// `safetensors` crate: u64-LE header length (8-byte aligned), JSON header
/// with tensors flattened beside `__metadata__`, then raw LE f32 data.
/// Loading through the real `VarBuilder::from_mmaped_safetensors` path in the
/// test validates every byte.
fn write_safetensors(path: &Path, tensors: &[(&str, Vec<usize>, Vec<f32>)]) {
    let mut offset: u64 = 0;
    let mut entries = serde_json::Map::new();
    for (name, shape, data) in tensors {
        assert_eq!(
            data.len(),
            shape.iter().product::<usize>(),
            "tensor {name} data length must match shape"
        );
        let len = (data.len() * 4) as u64;
        entries.insert(
            (*name).to_owned(),
            serde_json::json!({
                "dtype": "F32",
                "shape": shape,
                "data_offsets": [offset, offset + len],
            }),
        );
        offset += len;
    }
    let mut header = serde_json::Map::new();
    header.insert("__metadata__".to_owned(), serde_json::json!({}));
    header.extend(entries);
    let mut header_bytes = serde_json::to_vec(&serde_json::Value::Object(header))
        .expect("safetensors header should serialize");
    let pad = (8 - header_bytes.len() % 8) % 8;
    header_bytes.extend(std::iter::repeat_n(b' ', pad));

    let mut out = Vec::new();
    out.extend_from_slice(&(header_bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(&header_bytes);
    for (_, _, data) in tensors {
        for value in data {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    fs::write(path, out).expect("safetensors file should be written");
}

/// The 64-token fixture vocab: 5 specials, then code-ish wordpieces.
fn fixture_vocab() -> Vec<String> {
    let mut vocab = vec![
        "[PAD]".to_owned(),
        "[UNK]".to_owned(),
        "[CLS]".to_owned(),
        "[SEP]".to_owned(),
        "[MASK]".to_owned(),
    ];
    vocab.extend(
        [
            "a", "the", "of", "to", "in", "fn", "struct", "impl", "let", "mut", "pub", "self",
            "return", "if", "else", "for", "while", "use", "mod", "enum", "trait", "where", "as",
            "const", "static", "ref", "type", "match", "##s", "##ed", "##ing", "##er", "##ly",
            "##tion", "file", "function", "variable", "module", "test", "main", "lib", "rs", "src",
            "code", "data", "value", "name", "path", "with", "from", "new", "add", "get", "set",
            "##a", "##e", "##i", "##o", "##0",
        ]
        .iter()
        .map(ToString::to_string),
    );
    assert_eq!(vocab.len(), 64, "fixture vocab must be exactly 64 tokens");
    vocab
}

/// Hand-rolled 64-token `WordPiece` `tokenizer.json` in the canonical
/// `tokenizers` schema (`BertNormalizer` + `BertPreTokenizer` +
/// `TemplateProcessing` + `WordPiece` decoder): no dependency on any cached
/// tokenizer. Loading it through the real `BertEmbedder` path in the test
/// validates every field.
fn fixture_tokenizer_json(vocab: &[String]) -> serde_json::Value {
    let vocab_map: serde_json::Map<String, serde_json::Value> = vocab
        .iter()
        .enumerate()
        .map(|(id, token)| (token.clone(), serde_json::json!(id)))
        .collect();
    let added: Vec<serde_json::Value> = ["[PAD]", "[UNK]", "[CLS]", "[SEP]", "[MASK]"]
        .iter()
        .map(|token| {
            let id = vocab.iter().position(|t| t == token).unwrap();
            serde_json::json!({
                "id": id,
                "content": token,
                "single_word": false,
                "lstrip": false,
                "rstrip": false,
                "normalized": false,
                "special": true,
            })
        })
        .collect();
    serde_json::json!({
        "version": "1.0",
        "truncation": {
            "max_length": 128,
            "strategy": "LongestFirst",
            "stride": 0,
            "direction": "Right",
        },
        "padding": null,
        "added_tokens": added,
        "normalizer": {
            "type": "BertNormalizer",
            "clean_text": true,
            "handle_chinese_chars": true,
            "strip_accents": null,
            "lowercase": true,
        },
        "pre_tokenizer": { "type": "BertPreTokenizer" },
        "post_processor": {
            "type": "TemplateProcessing",
            "single": [
                { "SpecialToken": { "id": "[CLS]", "type_id": 0 } },
                { "Sequence": { "id": "A", "type_id": 0 } },
                { "SpecialToken": {"id": "[SEP]", "type_id": 0 } },
            ],
            "pair": [
                { "SpecialToken": { "id": "[CLS]", "type_id": 0 } },
                { "Sequence": { "id": "A", "type_id": 0 } },
                { "SpecialToken": { "id": "[SEP]", "type_id": 0 } },
                { "Sequence": { "id": "B", "type_id": 1 } },
                { "SpecialToken": { "id": "[SEP]", "type_id": 1 } },
            ],
            "special_tokens": {
                "[CLS]": { "id": "[CLS]", "ids": [2], "tokens": ["[CLS]"] },
                "[SEP]": { "id": "[SEP]", "ids": [3], "tokens": ["[SEP]"] },
            },
        },
        "decoder": { "type": "WordPiece", "prefix": "##", "cleanup": true },
        "model": {
            "type": "WordPiece",
            "unk_token": "[UNK]",
            "continuing_subword_prefix": "##",
            "max_input_chars_per_word": 100,
            "vocab": vocab_map,
        },
    })
}

/// Generates a tiny deterministic BERT model directory: config.json,
/// tokenizer.json, model.safetensors. `hidden` is the embedding dimension;
/// the file layout is exactly what `BertEmbedder::new` reads from the HF
/// cache (`config.json`, `tokenizer.json`, `model.safetensors`).
fn generate_fixture_model(dir: &Path, hidden: usize, seed: u64) {
    assert_eq!(
        hidden % 4,
        0,
        "fixture hidden size must divide into 4 attention heads"
    );
    fs::create_dir_all(dir).expect("fixture model dir should be created");
    let vocab = fixture_vocab();
    let layers = 2usize;
    let intermediate = hidden;

    let config = serde_json::json!({
        "architectures": ["BertModel"],
        "model_type": "bert",
        "hidden_size": hidden,
        "num_hidden_layers": layers,
        "num_attention_heads": 4,
        "intermediate_size": intermediate,
        "hidden_act": "gelu",
        "hidden_dropout_prob": 0.0,
        "attention_probs_dropout_prob": 0.0,
        "max_position_embeddings": 128,
        "type_vocab_size": 2,
        "initializer_range": 0.02,
        "layer_norm_eps": 1e-12,
        "pad_token_id": 0,
        "vocab_size": vocab.len(),
    });
    fs::write(
        dir.join("config.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .expect("config.json should be written");

    let tokenizer = fixture_tokenizer_json(&vocab);
    fs::write(
        dir.join("tokenizer.json"),
        serde_json::to_string(&tokenizer).unwrap(),
    )
    .expect("tokenizer.json should be written");

    let mut rng = XorShift64(seed);
    let mut specs: Vec<(String, Vec<usize>)> = vec![
        (
            "embeddings.word_embeddings.weight".to_owned(),
            vec![vocab.len(), hidden],
        ),
        (
            "embeddings.position_embeddings.weight".to_owned(),
            vec![128, hidden],
        ),
        (
            "embeddings.token_type_embeddings.weight".to_owned(),
            vec![2, hidden],
        ),
        ("embeddings.LayerNorm.weight".to_owned(), vec![hidden]),
        ("embeddings.LayerNorm.bias".to_owned(), vec![hidden]),
    ];
    for layer in 0..layers {
        let p = format!("encoder.layer.{layer}");
        for proj in ["query", "key", "value"] {
            specs.push((
                format!("{p}.attention.self.{proj}.weight"),
                vec![hidden, hidden],
            ));
            specs.push((format!("{p}.attention.self.{proj}.bias"), vec![hidden]));
        }
        specs.push((
            format!("{p}.attention.output.dense.weight"),
            vec![hidden, hidden],
        ));
        specs.push((format!("{p}.attention.output.dense.bias"), vec![hidden]));
        specs.push((
            format!("{p}.attention.output.LayerNorm.weight"),
            vec![hidden],
        ));
        specs.push((format!("{p}.attention.output.LayerNorm.bias"), vec![hidden]));
        specs.push((
            format!("{p}.intermediate.dense.weight"),
            vec![intermediate, hidden],
        ));
        specs.push((format!("{p}.intermediate.dense.bias"), vec![intermediate]));
        specs.push((
            format!("{p}.output.dense.weight"),
            vec![hidden, intermediate],
        ));
        specs.push((format!("{p}.output.dense.bias"), vec![hidden]));
        specs.push((format!("{p}.output.LayerNorm.weight"), vec![hidden]));
        specs.push((format!("{p}.output.LayerNorm.bias"), vec![hidden]));
    }
    let owned: Vec<(String, Vec<usize>, Vec<f32>)> = specs
        .into_iter()
        .map(|(name, shape)| {
            let len: usize = shape.iter().product();
            let data: Vec<f32> = (0..len).map(|_| rng.next_f32()).collect();
            (name, shape, data)
        })
        .collect();
    // Re-borrow as &str for the writer.
    let refs: Vec<(&str, Vec<usize>, Vec<f32>)> = owned
        .iter()
        .map(|(n, s, d)| (n.as_str(), s.clone(), d.clone()))
        .collect();
    write_safetensors(&dir.join("model.safetensors"), &refs);
}

/// Stages a generated fixture model into an isolated HF cache under `hub_id`,
/// mirroring `hf-hub`'s on-disk layout (`refs/main` → `snapshots/<hash>/`
/// with plain files, which `hf-hub` 0.4 serves directly). `eg` commands that
/// load it get `HF_HOME` pointed at `hf_home` plus a dead `HF_ENDPOINT`, so a
/// test proves no network is touched: any download attempt fails fast with
/// connection refused instead of hanging.
fn stage_hub_model(hf_home: &Path, hub_id: &str, model_dir: &Path) {
    let repo_dir = hf_home
        .join("hub")
        .join(format!("models--{}", hub_id.replace('/', "--")));
    let hash = "test-snapshot-167";
    let snapshot = repo_dir.join("snapshots").join(hash);
    fs::create_dir_all(&snapshot).expect("snapshot dir should be created");
    for file in ["config.json", "tokenizer.json", "model.safetensors"] {
        fs::copy(model_dir.join(file), snapshot.join(file)).expect("model file should be staged");
    }
    let refs = repo_dir.join("refs");
    fs::create_dir_all(&refs).expect("refs dir should be created");
    fs::write(refs.join("main"), hash).expect("refs/main should be written");
}

/// `eg` with the network dead and the HF cache isolated: any model load must
/// resolve from `hf_home` or refuse — never download.
fn eg_isolated(hf_home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
    cmd.env("HF_HOME", hf_home);
    cmd.env("HF_ENDPOINT", "http://127.0.0.1:9");
    cmd
}

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

/// Generates model A locally, stages it into an isolated HF cache, scans the
/// fixture repo, and ingests it `--embed --embed-model <model-a>` into a
/// fresh embedded store; returns `(data_dir, hf_home)`. Every `eg` invocation
/// runs with the network dead (`HF_ENDPOINT` → 127.0.0.1:9), so the suite
/// proves no download happens at any step.
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
