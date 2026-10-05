//! In-place re-embedding of an `--embed` store under a new local model
//! (issue #167).
//!
//! A store whose vector index was built by model A cannot answer queries
//! embedded by model B — issue #104 refuses those queries rather than ranking
//! across incompatible vector spaces. Re-embedding migrates the store itself:
//! every previously-embedded node is re-embedded with model B, the persisted
//! embedding-model identity is superseded, and the vector index is rebuilt at
//! B's dimension when the dimension changes. No source scan or graph
//! extraction runs; the candidate set is exactly the node set that already
//! carries vectors.
//!
//! Model availability is strictly local: the target model must already exist
//! as a local directory or in the Hugging Face cache. The preflight refuses
//! with [`REEMBED_MODEL_UNAVAILABLE_EXIT_CODE`] before anything loads —
//! Egregore never downloads a model as a side effect of re-embedding.

use std::path::{Path, PathBuf};

/// Exit code when the requested model is not available locally: the stable
/// `embedding_model_unavailable_locally` refusal (never a download).
pub const REEMBED_MODEL_UNAVAILABLE_EXIT_CODE: i32 = 12;

/// Exit code for `--dry-run` when the plan shows work remaining.
pub const REEMBED_DRY_RUN_PENDING_EXIT_CODE: i32 = 13;

/// Exit code when the store carries no embedding identity or no embedded
/// nodes, so there is nothing to migrate.
pub const REEMBED_NOTHING_TO_MIGRATE_EXIT_CODE: i32 = 14;

/// Stable refusal code for a locally-unavailable model.
pub const MODEL_UNAVAILABLE_LOCALLY_CODE: &str = "embedding_model_unavailable_locally";

/// Stable diagnostic code when the store has nothing to migrate.
pub const NOTHING_TO_MIGRATE_CODE: &str = "reembed_nothing_to_migrate";

/// Files `BertEmbedder::new` reads for a BERT model: the weights file has two
/// accepted names (`model.safetensors` preferred, `pytorch_model.bin` legacy).
const REQUIRED_MODEL_FILES: [&str; 2] = ["config.json", "tokenizer.json"];

/// How the operator named the re-embed target model.
#[derive(Debug, Clone)]
pub enum ModelSpec {
    /// A local model directory (must already contain the model files).
    LocalDir(PathBuf),
    /// A Hugging Face model id, resolved from the local HF cache only.
    HubId(String),
}

/// Parses the `--model` argument into a [`ModelSpec`].
///
/// A path-like spec is always a local model directory (present or not — an
/// absent one is the honest refusal); anything else is a Hugging Face id
/// resolved against the local cache. A bare `/` does NOT make a spec
/// path-like: Hugging Face ids are `org/model`, so only absolute paths,
/// explicit `./`/`../`/`~/` anchors, backslashes, or an actually-existing
/// directory count as paths.
#[must_use]
pub fn resolve_model_spec(spec: &str) -> ModelSpec {
    let path = PathBuf::from(spec);
    if path.is_dir() || looks_like_a_path(spec) {
        ModelSpec::LocalDir(path)
    } else {
        ModelSpec::HubId(spec.to_owned())
    }
}

/// A spec that names a filesystem location rather than a hub id: absolute, or
/// explicitly `./`/`../`/`~/`-anchored, or carrying a backslash (which never
/// appears in a Hugging Face id). An existing directory is caught by
/// [`resolve_model_spec`] before this is consulted.
fn looks_like_a_path(spec: &str) -> bool {
    let path = Path::new(spec);
    path.is_absolute()
        || spec.contains('\\')
        || spec == "."
        || spec == ".."
        || spec == "~"
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with("~/")
}

/// The Hugging Face cache root, mirroring `hf-hub`'s `Cache::from_env`:
/// `$HF_HOME/hub` when `HF_HOME` is set, otherwise `~/.cache/huggingface/hub`.
#[must_use]
pub fn hf_hub_cache_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HF_HOME") {
        return PathBuf::from(home).join("hub");
    }
    let mut path = dirs_home_fallback();
    path.push(".cache");
    path.push("huggingface");
    path.push("hub");
    path
}

fn dirs_home_fallback() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("/root"), PathBuf::from)
}

/// The stable model id a resolved spec loads as.
///
/// A hub id loads unchanged; a directory loads as `local/<blake3-of-canonical-path>`.
/// The slug is derived from the canonical path so the identity is stable
/// across working directories and repeated runs.
#[must_use]
pub fn model_id_for_spec(spec: &ModelSpec) -> String {
    match spec {
        ModelSpec::HubId(id) => id.clone(),
        ModelSpec::LocalDir(path) => {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            let hash = blake3::hash(canonical.to_string_lossy().as_bytes());
            format!("local/{}", hash.to_hex().as_str()[..16].to_owned())
        }
    }
}

/// A model that passed the local-availability preflight.
#[derive(Debug, Clone)]
pub struct ResolvedLocalModel {
    /// The original spec.
    pub spec: ModelSpec,
    /// The HF-style id the embedder loads (a real hub id, or the staged
    /// `local/<slug>` id for a directory).
    pub model_id: String,
    /// Directory holding the model's files: the local directory itself, or
    /// the resolved HF-cache snapshot directory for a hub id. Used to read
    /// `config.json` (declared dimension) and to hash the weights (content
    /// hash) without loading the model.
    pub model_dir: PathBuf,
}

/// Refuses unless every file the embedder reads is already present locally.
///
/// For a directory this checks the directory itself; for a hub id it resolves
/// `models--<id>/refs/main` → `snapshots/<hash>/` exactly the way `hf-hub`
/// does. It never touches the network: a missing file is a refusal, not a
/// download.
///
/// # Errors
///
/// Returns [`ReembedError::ModelUnavailableLocally`] when any required file
/// is absent, and [`ReembedError::Io`] when the cache layout cannot be read.
pub fn ensure_model_available_locally(
    spec: &ModelSpec,
) -> Result<ResolvedLocalModel, ReembedError> {
    let model_id = model_id_for_spec(spec);
    match spec {
        ModelSpec::LocalDir(dir) => {
            let mut missing = Vec::new();
            for file in REQUIRED_MODEL_FILES {
                if !dir.join(file).is_file() {
                    missing.push(file.to_owned());
                }
            }
            if !has_weights_file(dir) {
                missing.push("model.safetensors|pytorch_model.bin".to_owned());
            }
            if !missing.is_empty() {
                return Err(ReembedError::ModelUnavailableLocally {
                    spec: spec.clone(),
                    checked: dir.clone(),
                    missing,
                });
            }
            Ok(ResolvedLocalModel {
                spec: spec.clone(),
                model_id,
                model_dir: dir.clone(),
            })
        }
        ModelSpec::HubId(id) => {
            let hub = hf_hub_cache_dir();
            // Mirrors `Repo::folder_name` for a model repo.
            let repo_dir = hub.join(format!("models--{}", id.replace('/', "--")));
            let snapshot = (|| {
                let hash = std::fs::read_to_string(repo_dir.join("refs").join("main"))
                    .ok()?
                    .trim()
                    .to_owned();
                if hash.is_empty() {
                    return None;
                }
                Some(repo_dir.join("snapshots").join(hash))
            })();
            let mut missing = Vec::new();
            match snapshot {
                None => missing.push("refs/main (no cached snapshot)".to_owned()),
                Some(ref dir) => {
                    for file in REQUIRED_MODEL_FILES {
                        if !dir.join(file).is_file() {
                            missing.push(file.to_owned());
                        }
                    }
                    if !has_weights_file(dir) {
                        missing.push("model.safetensors|pytorch_model.bin".to_owned());
                    }
                }
            }
            if !missing.is_empty() {
                return Err(ReembedError::ModelUnavailableLocally {
                    spec: spec.clone(),
                    checked: repo_dir,
                    missing,
                });
            }
            // `missing` is empty, so `snapshot` is `Some`.
            let model_dir = snapshot.unwrap_or_else(|| repo_dir.clone());
            Ok(ResolvedLocalModel {
                spec: spec.clone(),
                model_id,
                model_dir,
            })
        }
    }
}

fn has_weights_file(dir: &Path) -> bool {
    dir.join("model.safetensors").is_file() || dir.join("pytorch_model.bin").is_file()
}

/// Stages a local model directory into the HF cache under its
/// `local/<slug>` id so the standard `from_pretrained_hf` loader finds it
/// offline. Idempotent: re-staging the same directory is a no-op.
///
/// The layout mirrors a real cached repo: `models--local--<slug>/refs/main`
/// points at `snapshots/<slug>/`, which holds symlinks (copies on Windows)
/// to the directory's model files.
///
/// # Errors
///
/// Returns [`ReembedError::NotALocalModelId`] when `model_id` is not a
/// staged `local/<slug>` id, and [`ReembedError::Io`] when the cache
/// directory cannot be created or populated.
pub fn stage_local_model_dir(dir: &Path, model_id: &str) -> Result<PathBuf, ReembedError> {
    let slug = model_id
        .strip_prefix("local/")
        .ok_or_else(|| ReembedError::NotALocalModelId(model_id.to_owned()))?;
    let canonical = dir.canonicalize().map_err(|error| ReembedError::Io {
        path: dir.to_path_buf(),
        message: error.to_string(),
    })?;
    let repo_dir = hf_hub_cache_dir().join(format!("models--local--{slug}"));
    let snapshot_dir = repo_dir.join("snapshots").join(slug);
    std::fs::create_dir_all(&snapshot_dir).map_err(|error| ReembedError::Io {
        path: snapshot_dir.clone(),
        message: error.to_string(),
    })?;

    let mut files: Vec<&str> = REQUIRED_MODEL_FILES.to_vec();
    files.push(if canonical.join("model.safetensors").is_file() {
        "model.safetensors"
    } else {
        "pytorch_model.bin"
    });
    for file in files {
        let target = canonical.join(file);
        let link = snapshot_dir.join(file);
        if link.exists() {
            continue;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).map_err(|error| ReembedError::Io {
            path: link.clone(),
            message: error.to_string(),
        })?;
        #[cfg(not(unix))]
        std::fs::copy(&target, &link).map_err(|error| ReembedError::Io {
            path: link.clone(),
            message: error.to_string(),
        })?;
    }

    let refs_dir = repo_dir.join("refs");
    std::fs::create_dir_all(&refs_dir).map_err(|error| ReembedError::Io {
        path: refs_dir.clone(),
        message: error.to_string(),
    })?;
    std::fs::write(refs_dir.join("main"), slug).map_err(|error| ReembedError::Io {
        path: refs_dir.join("main"),
        message: error.to_string(),
    })?;
    Ok(snapshot_dir)
}

/// Reads the model's DECLARED embedding dimension (`hidden_size` in
/// `config.json`) without loading any weights.
///
/// This is the cheap pre-load identity the `#104` gate compares before the
/// expensive embedder load; the measured vector length (see
/// [`embed_texts`]) remains the ground truth and is re-gated after
/// embedding, so a config that lies about its dimension still fails closed.
///
/// # Errors
///
/// Returns [`ReembedError::Io`] when `config.json` cannot be read or parsed,
/// or when it lacks a positive integer `hidden_size`.
pub fn declared_model_dim(resolved: &ResolvedLocalModel) -> Result<usize, ReembedError> {
    let path = resolved.model_dir.join("config.json");
    let raw = std::fs::read_to_string(&path).map_err(|error| ReembedError::Io {
        path: path.clone(),
        message: error.to_string(),
    })?;
    let config: serde_json::Value =
        serde_json::from_str(&raw).map_err(|error| ReembedError::Io {
            path: path.clone(),
            message: format!("config.json is not valid JSON: {error}"),
        })?;
    config
        .get("hidden_size")
        .and_then(serde_json::Value::as_u64)
        .and_then(|dim| usize::try_from(dim).ok())
        .filter(|dim| *dim > 0)
        .ok_or_else(|| ReembedError::Io {
            path,
            message: "config.json has no positive integer hidden_size".to_owned(),
        })
}

/// Weights file precedence, mirroring the embedder loader: `model.safetensors`
/// preferred, `pytorch_model.bin` legacy.
fn weights_file(dir: &Path) -> Option<PathBuf> {
    let safetensors = dir.join("model.safetensors");
    if safetensors.is_file() {
        return Some(safetensors);
    }
    let pytorch = dir.join("pytorch_model.bin");
    if pytorch.is_file() {
        return Some(pytorch);
    }
    None
}

/// Builds the [`crate::ir::EmbeddingModel`] identity re-embed records.
///
/// The identity captures the provider boundary that supplied the model, the
/// stable model id, the egregore version pin (same precedent as the default
/// identity), the MEASURED dimension the model actually produced, and the
/// BLAKE3 content hash of the weights file.
///
/// Deterministic: the same model directory always yields the same identity,
/// so a repeated re-embed is a byte-identical no-op and `--embed-model`
/// queries build the identical query identity for the `#104` comparison.
#[must_use]
pub fn target_model_identity(
    resolved: &ResolvedLocalModel,
    dim: usize,
) -> crate::ir::EmbeddingModel {
    let provider = match &resolved.spec {
        ModelSpec::LocalDir(_) => "local",
        ModelSpec::HubId(_) => "huggingface",
    }
    .to_owned();
    let content_hash = weights_file(&resolved.model_dir)
        .and_then(|path| std::fs::read(&path).ok())
        .map_or_else(
            || "unknown".to_owned(),
            |bytes| blake3::hash(&bytes).to_hex().to_string(),
        );
    crate::ir::EmbeddingModel {
        provider,
        name: resolved.model_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        dim: u32::try_from(dim).unwrap_or(u32::MAX),
        content_hash,
    }
}

/// Loads the embedder for a resolved model, staging a local directory into
/// the HF cache first so the standard `from_pretrained_hf` loader resolves it
/// offline (issue #167).
///
/// The preflight ([`ensure_model_available_locally`]) already verified every
/// file this loader reads is present locally, so the load cannot silently
/// download: a cache miss here is an error, never a network fetch.
///
/// # Errors
///
/// Returns [`ReembedError::EmbedderLoadFailed`] when the embedder cannot be
/// constructed from the staged files.
pub fn load_embedder(
    resolved: &ResolvedLocalModel,
) -> Result<crate::embeddings::aletheia_embeddings::Embedder, ReembedError> {
    if let ModelSpec::LocalDir(dir) = &resolved.spec {
        stage_local_model_dir(dir, &resolved.model_id)?;
    }
    crate::embeddings::aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(crate::embeddings::DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(resolved.model_id.as_str()))
        .from_pretrained_hf()
        .map_err(|error| ReembedError::EmbedderLoadFailed {
            model_id: resolved.model_id.clone(),
            message: error.to_string(),
        })
}

/// Embeds one text per candidate with a loaded embedder, preserving order.
///
/// Uses the same tokio-runtime + `embed_query` + dense-conversion shape as the
/// `--embed` ingest path so re-embedded vectors are produced by the identical
/// pipeline (deterministic: same model + same texts → same vectors).
///
/// # Errors
///
/// Returns [`ReembedError::EmbeddingFailed`] when batch embedding fails.
pub fn embed_texts(
    embedder: &crate::embeddings::aletheia_embeddings::Embedder,
    texts: &[&str],
) -> Result<Vec<Vec<f32>>, ReembedError> {
    use crate::embeddings::aletheia_embeddings;
    let runtime =
        tokio::runtime::Runtime::new().map_err(|error| ReembedError::EmbeddingFailed {
            message: format!("failed to create tokio runtime: {error}"),
        })?;
    let embed_data = runtime
        .block_on(aletheia_embeddings::embed_query(texts, embedder, None))
        .map_err(|error| ReembedError::EmbeddingFailed {
            message: format!("embedding generation failed: {error}"),
        })?;
    let dense: Vec<Vec<f32>> = aletheia_embeddings::embed_data_to_dense_iter(embed_data, None)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ReembedError::EmbeddingFailed {
            message: format!("embedding result was not dense: {error}"),
        })?
        .into_iter()
        .map(|data| data.embedding)
        .collect();
    if dense.len() != texts.len() {
        return Err(ReembedError::EmbeddingFailed {
            message: format!(
                "embedder returned {} vectors for {} texts",
                dense.len(),
                texts.len()
            ),
        });
    }
    Ok(dense)
}

/// The machine-readable refusal envelope for a locally-unavailable model.
#[must_use]
pub fn model_unavailable_envelope(
    spec: &ModelSpec,
    checked: &Path,
    missing: &[String],
) -> serde_json::Value {
    let (kind, value) = match spec {
        ModelSpec::LocalDir(dir) => ("local_directory", dir.display().to_string()),
        ModelSpec::HubId(id) => ("hub_id", id.clone()),
    };
    serde_json::json!({
        "ok": false,
        "error": {
            "code": MODEL_UNAVAILABLE_LOCALLY_CODE,
            "message": format!(
                "embedding model is not available locally (checked {checked}); \
                 Egregore never downloads models — place the model files locally first",
                checked = checked.display(),
            ),
            "model": { "kind": kind, "value": value },
            "checked": checked.display().to_string(),
            "missing": missing,
            "remedy": "supply a local model directory containing config.json, \
                       tokenizer.json and model.safetensors (or pytorch_model.bin), \
                       or pre-populate the Hugging Face cache (e.g. HF_HOME) with the hub id",
        }
    })
}

/// Errors from the re-embed preflight and staging.
#[derive(Debug, Clone)]
pub enum ReembedError {
    /// The model is not available locally; Egregore refuses to download it.
    ModelUnavailableLocally {
        /// The spec as the operator gave it.
        spec: ModelSpec,
        /// The directory (or cache snapshot) that was probed.
        checked: PathBuf,
        /// The required files that were absent.
        missing: Vec<String>,
    },
    /// `stage_local_model_dir` was called with a non-`local/` model id.
    NotALocalModelId(
        /// The model id that was not a staged `local/<slug>` id.
        String,
    ),
    /// Filesystem failure while staging or probing.
    Io {
        /// The path that failed.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },
    /// The model passed the file preflight but the embedder could not load it
    /// (corrupt weights, unreadable config). The store is untouched: this
    /// surfaces before any write.
    EmbedderLoadFailed {
        /// The HF-style id the loader attempted.
        model_id: String,
        /// What went wrong.
        message: String,
    },
    /// Batch embedding failed partway. The store is untouched: vectors are
    /// all computed before the single commit transaction opens.
    EmbeddingFailed {
        /// What went wrong.
        message: String,
    },
}

impl std::fmt::Display for ReembedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelUnavailableLocally {
                checked, missing, ..
            } => write!(
                f,
                "{MODEL_UNAVAILABLE_LOCALLY_CODE}: model not available locally \
                 (checked {}; missing: {})",
                checked.display(),
                missing.join(", "),
            ),
            Self::NotALocalModelId(id) => {
                write!(f, "not a staged local model id: {id}")
            }
            Self::Io { path, message } => {
                write!(f, "I/O error at {}: {message}", path.display())
            }
            Self::EmbedderLoadFailed { model_id, message } => {
                write!(f, "failed to load embedding model {model_id}: {message}")
            }
            Self::EmbeddingFailed { message } => {
                write!(f, "embedding failed: {message}")
            }
        }
    }
}

impl std::error::Error for ReembedError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Hugging Face id (`org/model`) must resolve as a hub id even though
    /// it contains a slash — the slash alone is not path-like.
    #[test]
    fn hub_id_with_slash_is_not_a_path() {
        match resolve_model_spec("sentence-transformers/all-MiniLM-L6-v2") {
            ModelSpec::HubId(id) => {
                assert_eq!(id, "sentence-transformers/all-MiniLM-L6-v2");
            }
            ModelSpec::LocalDir(path) => {
                panic!("hub id misclassified as local dir: {}", path.display());
            }
        }
    }

    /// Absolute paths, explicit relative anchors, and backslashes are
    /// path-like.
    #[test]
    fn explicit_paths_resolve_as_local_dirs() {
        for spec in [
            "/tmp/models/foo",
            "./models/foo",
            "../models/foo",
            "~/models/foo",
            r"models\foo",
            ".",
            "..",
        ] {
            match resolve_model_spec(spec) {
                ModelSpec::LocalDir(_) => {}
                ModelSpec::HubId(id) => {
                    panic!("path {spec} misclassified as hub id {id}");
                }
            }
        }
    }

    /// An existing relative directory resolves as a local dir even without
    /// an explicit anchor.
    #[test]
    fn existing_relative_dir_resolves_as_local_dir() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let dir = temp.path().join("models").join("foo");
        std::fs::create_dir_all(&dir).expect("model dir should be created");
        let original = std::env::current_dir().expect("cwd should be readable");
        std::env::set_current_dir(temp.path()).expect("cwd should be changeable");
        let result = resolve_model_spec("models/foo");
        std::env::set_current_dir(original).expect("cwd should be restored");
        match result {
            ModelSpec::LocalDir(_) => {}
            ModelSpec::HubId(id) => {
                panic!("existing dir misclassified as hub id {id}");
            }
        }
    }
}
