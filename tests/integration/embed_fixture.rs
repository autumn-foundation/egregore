#![allow(missing_docs)]
//! Shared deterministic local embedding-model fixture for integration tests.
//!
//! Extracted from `reembed.rs` (issue #167) so other suites — e.g. the
//! issue #130 `--embed` re-ingest convergence test — can stage a tiny local
//! BERT without duplicating the generator. Everything is generated at test
//! time: seeded weights, a hand-rolled 64-token `WordPiece` `tokenizer.json`,
//! and hand-rolled safetensors files validated by loading through the real
//! `BertEmbedder` path. Tests stage the model into an isolated `HF_HOME` and
//! point `HF_ENDPOINT` at a dead address, so the suite needs no network and no
//! warm Hugging Face cache: any download attempt fails fast with connection
//! refused instead of hanging.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Dead endpoint for `HF_ENDPOINT`: model loads must resolve from the
/// isolated `HF_HOME` or refuse — never download.
pub const DEAD_HF_ENDPOINT: &str = "http://127.0.0.1:9";

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
pub fn generate_fixture_model(dir: &Path, hidden: usize, seed: u64) {
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
pub fn stage_hub_model(hf_home: &Path, hub_id: &str, model_dir: &Path) {
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

/// Generates a fixture model and stages it into an isolated HF cache under
/// `hub_id`; returns the `hf_home` to point `HF_HOME` at. `eg` invocations
/// should also set `HF_ENDPOINT` to [`DEAD_HF_ENDPOINT`] so the test proves
/// no network is touched.
pub fn stage_local_model(temp: &Path, hub_id: &str, hidden: usize, seed: u64) -> PathBuf {
    let model_dir = temp.join("models").join(hub_id.replace('/', "--"));
    generate_fixture_model(&model_dir, hidden, seed);
    let hf_home = temp.join("hf-home");
    stage_hub_model(&hf_home, hub_id, &model_dir);
    hf_home
}
