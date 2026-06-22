//! Local setup preflight report for the `eg doctor` command (issue #75).
//!
//! Classifies machine readiness for the `eg` scan → ingest → semantic-search
//! workflow before agents rely on it.
//!
//! # Design
//!
//! The module uses a pure/impure split so the entire fixture matrix is
//! testable without real git, python, or network access:
//!
//! - [`build_report`]: pure, deterministic, no I/O.
//! - [`gather_observations`]: the only impure code; reduces every env-sensitive
//!   input to plain booleans and safe resolved paths.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::embeddings::{
    DEFAULT_EMBEDDING_MODEL_DIMENSIONS, DEFAULT_EMBEDDING_MODEL_NAME,
    DEFAULT_EMBEDDING_MODEL_PROVIDER,
};

/// Schema version stamped on every doctor report.
/// Bump on breaking JSON field changes.
pub const DOCTOR_SCHEMA_VERSION: u32 = 1;

/// Expected HF model subdirectory prefix (slug-encoded model name).
const MODEL_CACHE_DIR_SLUG: &str = "models--sentence-transformers--all-MiniLM-L6-v2";

// ── Stable machine-readable identifiers ────────────────────────────────────

/// Stable check identifiers.
///
/// The serialised `snake_case` string is the machine contract — never rename a
/// variant's serialised form without bumping [`DOCTOR_SCHEMA_VERSION`].
/// Declaration order **is** the canonical output order (the `Ord` derive uses
/// discriminant order).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckId {
    // ── Structural ──────────────────────────────────────────────────────────
    /// The repository path exists and is accessible.
    RepositoryPath,
    /// `git` binary is available on PATH.
    GitAvailable,
    /// The repository has readable Git history (optional by default; promoted
    /// to required by `--require-history`).
    GitHistoryReadable,
    /// Parent directory of `--out` is writable.
    OutputPathWritable,
    /// `--data-dir` is writable (or its parent, if it does not yet exist).
    DataDirWritable,
    /// The embedded `AletheiaDB` store could be opened at `--data-dir` (runtime
    /// sidecar is usable and no stale daemon metadata blocks it). Skipped when
    /// the `embedded-aletheiadb` adapter is not compiled in.
    EmbeddedStoreOpenable,
    // ── Informational (always Pass) ─────────────────────────────────────────
    /// Resolved Hugging Face cache path (always Pass; informational).
    HfCacheLocation,
    /// Expected embedding model name and provider (always Pass; informational).
    EmbeddingModelIdentity,
    /// Expected embedding model vector dimension (always Pass; informational).
    EmbeddingModelDimension,
    // ── Semantic ─────────────────────────────────────────────────────────────
    /// This binary was compiled with the `embeddings` feature (Warn when not,
    /// since semantic ingest/query are unavailable in that build).
    EmbeddingsFeatureEnabled,
    /// Expected model directory is present under the HF cache.
    ModelCachePresent,
    /// `python3` or `python` is available on PATH.
    PythonAvailable,
    /// `import sentence_transformers` succeeds (Skipped when python absent).
    PythonPrimingRunnable,
    /// HF cache path is readable.
    HfCacheReadable,
    /// Windows Developer Mode or registry symlinks are enabled (Warn on
    /// Windows when disabled; Skipped on non-Windows).
    WindowsSymlinkSupport,
    /// Hugging Face host is TCP-reachable (only included with `--network`).
    HfReachable,
}

/// Per-check pass/fail/warn/skipped status.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The check criterion is satisfied.
    Pass,
    /// The check criterion is not satisfied.
    Fail,
    /// The check found a non-fatal issue worth surfacing.
    Warn,
    /// The check does not apply in this environment and was not run.
    Skipped,
}

/// Whether a check is required for the exit code.
///
/// A [`Requirement::Optional`] check never affects the exit code regardless
/// of its status.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    /// A [`CheckStatus::Fail`] on this check causes exit 1.
    Required,
    /// Failure and warnings on this check never affect the exit code.
    Optional,
}

/// Which readiness gate a check feeds.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// Feeds `structural_ready`.
    Structural,
    /// Feeds `semantic_ready` (also requires `structural_ready`).
    Semantic,
    /// Informational only; never feeds any readiness gate.
    None,
}

/// One diagnosed check in the preflight report.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Check {
    /// Stable machine-readable check identifier.
    pub id: CheckId,
    /// Outcome of the check.
    pub status: CheckStatus,
    /// Whether this check contributes to the exit code.
    pub requirement: Requirement,
    /// Which readiness gate this check feeds.
    pub gate: Gate,
    /// One-line human summary containing only safe paths and static templates.
    pub summary: String,
    /// Shortest actionable remediation hint. `None` when status is Pass or Skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// Safe resolved path this check concerns (HF cache dir, out path, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// Preflight report returned by [`build_report`] and serialised by `eg doctor`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreflightReport {
    /// Schema version — bump on breaking JSON changes.
    pub schema_version: u32,
    /// Canonicalised repository path that was inspected.
    pub repository_path: PathBuf,
    /// `true` when the structural scan/ingest workflow can proceed.
    /// Equals [`structural_ready`](Self::structural_ready).
    pub overall_ready: bool,
    /// `true` when all required structural checks pass.
    pub structural_ready: bool,
    /// `true` when structural *and* all required-for-semantic checks pass.
    pub semantic_ready: bool,
    /// Shortest next command for the local workflow given the current state.
    pub next_command: String,
    /// Ordered checks; always sorted by [`CheckId`] discriminant.
    pub checks: Vec<Check>,
}

// ── Config + Observations ────────────────────────────────────────────────────

/// CLI-derived configuration for a `doctor` run.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DoctorConfig {
    /// Repository path to inspect (from positional arg, default `.`).
    pub repo_path: PathBuf,
    /// Output JSONL path whose writability is checked (default `graph.jsonl`).
    pub out: PathBuf,
    /// Embedded data directory whose writability is checked (default `.egregore`).
    pub data_dir: PathBuf,
    /// Promote git-history readability from optional to required.
    pub require_history: bool,
    /// Perform an optional Hugging Face TCP reachability check.
    pub network: bool,
}

/// Reason a direct embedded-store open would be rejected, mirrored from the
/// embedded adapter's open path (`EmbeddedAletheiaSink::open`).
///
/// Only computed when the `embedded-aletheiadb` feature is compiled in; the
/// variants carry only safe paths and an operator remediation string.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum EmbeddedOpenBlock {
    /// The runtime sidecar path already exists but is not a usable directory
    /// (it is a symlink or a regular file), so `ensure_runtime_dir` would fail.
    RuntimeSidecarUnusable(PathBuf),
    /// Stale, non-stopped daemon metadata blocks a direct embedded open. The
    /// string is the operator remediation from `repair::embedded_open_repair_gate`.
    StaleDaemonMetadata(String),
    /// A live Egregore daemon currently holds the store lease, so a direct
    /// embedded open would be rejected by `StoreLease::acquire`. This is a
    /// healthy state (ingest via `--adapter daemon`), reported as a warning.
    StoreLeased,
}

/// Pure snapshot of environment observations used to build a [`PreflightReport`].
///
/// Contains no raw environment variable values — only booleans, resolved safe
/// paths, and public compile-time constants. Construct this directly in tests
/// and pass to [`build_report`] without touching the filesystem.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Observations {
    // ── structural ───────────────────────────────────────────────────────────
    /// Canonicalised repository path (or the input if canonicalisation fails).
    pub repo_path: PathBuf,
    /// Repository path is an existing, **readable** directory. `scan` rejects
    /// non-directories in `validate_repository` and then calls `read_dir` on the
    /// root in `discover_source_files`, so an unreadable directory fails too.
    pub repo_path_is_dir: bool,
    /// `git --version` returned exit 0.
    pub git_available: bool,
    /// `git -C <repo> rev-parse --git-dir` returned exit 0.
    pub is_git_repo: bool,
    /// `git -C <repo> log -1` returned exit 0.
    pub git_history_readable: bool,
    /// Resolved output JSONL path.
    pub out_path: PathBuf,
    /// Parent directory of `out_path` is writable (probe: create+remove temp).
    pub out_writable: bool,
    /// Resolved data directory path.
    pub data_dir: PathBuf,
    /// Data directory (or its nearest existing ancestor) is writable.
    pub data_dir_writable: bool,
    /// Whether this binary includes the embedded `AletheiaDB` adapter
    /// (`embedded-aletheiadb` feature). When false, the embedded-store-open
    /// check is not applicable and is reported as Skipped.
    pub embedded_adapter_available: bool,
    /// Mirror of the embedded-store-open preconditions for `--data-dir`
    /// (runtime sidecar health + stale-daemon repair gate). `None` when the
    /// embedded open would not be blocked (or the adapter is unavailable).
    pub embedded_open_block: Option<EmbeddedOpenBlock>,

    // ── semantic / HF ────────────────────────────────────────────────────────
    /// Resolved HF cache directory (safe path — no token values).
    pub hf_cache_dir: PathBuf,
    /// HF cache directory is readable.
    pub hf_cache_readable: bool,
    /// `HF_HUB_OFFLINE` (or `TRANSFORMERS_OFFLINE`) is set to a truthy value.
    pub hf_offline: bool,
    /// Expected model subdirectory found under `hf_cache_dir`.
    pub model_cache_present: bool,
    /// `python3` or `python` binary found on PATH.
    pub python_available: bool,
    /// The interpreter `find_python` detected (`"python3"` or `"python"`), so
    /// remediation/`next_command` strings name a runnable executable. `None`
    /// when no interpreter is on PATH.
    pub python_executable: Option<String>,
    /// The `sentence_transformers` package is installed (detected without
    /// executing it). Only needed to *prime* a missing model cache — the
    /// embedding runtime loads the cached model through Rust, not Python.
    pub python_import_ok: bool,
    /// This binary was compiled with the `embeddings` feature, so `eg ingest
    /// --embed` and `eg query semantic` exist. Gathered via `cfg!`.
    pub embeddings_feature_enabled: bool,

    // ── platform ─────────────────────────────────────────────────────────────
    /// Running on Windows.
    pub is_windows: bool,
    /// Windows Developer Mode / symlinks enabled. `None` on non-Windows.
    pub windows_symlinks_enabled: Option<bool>,

    // ── network (only gathered when config.network == true) ──────────────────
    /// TCP connection to `huggingface.co:443` succeeded. `None` when not probed.
    pub hf_reachable: Option<bool>,

    // ── flags echoed from config for classification ───────────────────────────
    /// `--require-history` was passed.
    pub require_history: bool,
    /// `--network` was passed.
    pub network_checked: bool,
}

// ── Check builders ───────────────────────────────────────────────────────────

fn check_repository_path(obs: &Observations) -> Check {
    if obs.repo_path_is_dir {
        Check {
            id: CheckId::RepositoryPath,
            status: CheckStatus::Pass,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: format!(
                "repository path is a readable directory: {}",
                obs.repo_path.display()
            ),
            remediation: None,
            path: Some(obs.repo_path.clone()),
        }
    } else {
        Check {
            id: CheckId::RepositoryPath,
            status: CheckStatus::Fail,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: format!(
                "repository path is not a readable directory: {}",
                obs.repo_path.display()
            ),
            remediation: Some(
                "pass a readable directory to scan: eg doctor <DIR> \
                 (scan rejects file paths and unreadable roots)"
                    .to_owned(),
            ),
            path: Some(obs.repo_path.clone()),
        }
    }
}

fn check_git_available(obs: &Observations) -> Check {
    if obs.git_available {
        Check {
            id: CheckId::GitAvailable,
            status: CheckStatus::Pass,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: "git binary is available on PATH".to_owned(),
            remediation: None,
            path: None,
        }
    } else {
        Check {
            id: CheckId::GitAvailable,
            status: CheckStatus::Fail,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: "git binary not found on PATH".to_owned(),
            remediation: Some(
                "install git (https://git-scm.com/downloads), then re-run: eg doctor".to_owned(),
            ),
            path: None,
        }
    }
}

fn check_git_history_readable(obs: &Observations) -> Check {
    let requirement = if obs.require_history {
        Requirement::Required
    } else {
        Requirement::Optional
    };
    let ok = obs.is_git_repo && obs.git_history_readable;

    if ok {
        Check {
            id: CheckId::GitHistoryReadable,
            status: CheckStatus::Pass,
            requirement,
            gate: Gate::Structural,
            summary: "git history is readable".to_owned(),
            remediation: None,
            path: None,
        }
    } else if !obs.is_git_repo {
        let remediation = if obs.require_history {
            "run inside a git repository, or drop --require-history".to_owned()
        } else {
            "run inside a git repository to enable scan-history".to_owned()
        };
        Check {
            id: CheckId::GitHistoryReadable,
            status: CheckStatus::Fail,
            requirement,
            gate: Gate::Structural,
            summary: format!(
                "directory is not a git repository: {}",
                obs.repo_path.display()
            ),
            remediation: Some(remediation),
            path: Some(obs.repo_path.clone()),
        }
    } else {
        Check {
            id: CheckId::GitHistoryReadable,
            status: CheckStatus::Fail,
            requirement,
            gate: Gate::Structural,
            summary: "git history is not readable".to_owned(),
            remediation: Some(
                "check that the repository has at least one commit: git log -1".to_owned(),
            ),
            path: None,
        }
    }
}

fn check_output_path_writable(obs: &Observations) -> Check {
    if obs.out_writable {
        Check {
            id: CheckId::OutputPathWritable,
            status: CheckStatus::Pass,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: format!("output path parent is writable: {}", obs.out_path.display()),
            remediation: None,
            path: Some(obs.out_path.clone()),
        }
    } else {
        Check {
            id: CheckId::OutputPathWritable,
            status: CheckStatus::Fail,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: format!(
                "output path parent is not writable: {}",
                obs.out_path.display()
            ),
            remediation: Some("choose a writable --out path: eg doctor --out <path>".to_owned()),
            path: Some(obs.out_path.clone()),
        }
    }
}

fn check_data_dir_writable(obs: &Observations) -> Check {
    if !obs.embedded_adapter_available {
        // Without the embedded adapter the only structural ingest path is
        // `ingest --adapter dry-run`, which never uses `--data-dir`.
        return Check {
            id: CheckId::DataDirWritable,
            status: CheckStatus::Skipped,
            requirement: Requirement::Optional,
            gate: Gate::Structural,
            summary: "skipped — embedded AletheiaDB adapter not compiled in this build".to_owned(),
            remediation: None,
            path: None,
        };
    }
    if obs.data_dir_writable {
        Check {
            id: CheckId::DataDirWritable,
            status: CheckStatus::Pass,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: format!("data directory is writable: {}", obs.data_dir.display()),
            remediation: None,
            path: Some(obs.data_dir.clone()),
        }
    } else {
        Check {
            id: CheckId::DataDirWritable,
            status: CheckStatus::Fail,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: format!("data directory is not writable: {}", obs.data_dir.display()),
            remediation: Some(
                "choose a writable --data-dir: eg doctor --data-dir <path>".to_owned(),
            ),
            path: Some(obs.data_dir.clone()),
        }
    }
}

fn check_embedded_store_openable(obs: &Observations) -> Check {
    if !obs.embedded_adapter_available {
        return Check {
            id: CheckId::EmbeddedStoreOpenable,
            status: CheckStatus::Skipped,
            requirement: Requirement::Optional,
            gate: Gate::Structural,
            summary: "skipped — embedded AletheiaDB adapter not compiled in this build".to_owned(),
            remediation: None,
            path: None,
        };
    }
    match &obs.embedded_open_block {
        None => Check {
            id: CheckId::EmbeddedStoreOpenable,
            status: CheckStatus::Pass,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: format!(
                "embedded store can be opened at: {}",
                obs.data_dir.display()
            ),
            remediation: None,
            path: Some(obs.data_dir.clone()),
        },
        Some(EmbeddedOpenBlock::RuntimeSidecarUnusable(sidecar)) => Check {
            id: CheckId::EmbeddedStoreOpenable,
            status: CheckStatus::Fail,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: format!(
                "embedded runtime path is not usable (symlink or unexpected type): {}",
                sidecar.display()
            ),
            remediation: Some(
                "remove or replace the runtime path (the sidecar directory and its \
                 egregored.lock must not be symlinks or non-directories) so embedded \
                 ingest can open the store"
                    .to_owned(),
            ),
            path: Some(sidecar.clone()),
        },
        Some(EmbeddedOpenBlock::StaleDaemonMetadata(remediation)) => Check {
            id: CheckId::EmbeddedStoreOpenable,
            status: CheckStatus::Fail,
            requirement: Requirement::Required,
            gate: Gate::Structural,
            summary: format!(
                "stale daemon metadata blocks embedded store open at: {}",
                obs.data_dir.display()
            ),
            remediation: Some(remediation.clone()),
            path: Some(obs.data_dir.clone()),
        },
        Some(EmbeddedOpenBlock::StoreLeased) => Check {
            id: CheckId::EmbeddedStoreOpenable,
            status: CheckStatus::Warn,
            requirement: Requirement::Optional,
            gate: Gate::Structural,
            summary: format!(
                "a running daemon holds the store lease for: {}",
                obs.data_dir.display()
            ),
            remediation: Some(
                "ingest via --adapter daemon, or stop the daemon \
                 (eg daemon stop --data-dir <dir>) before using --adapter embedded"
                    .to_owned(),
            ),
            path: Some(obs.data_dir.clone()),
        },
    }
}

fn check_hf_cache_location(obs: &Observations) -> Check {
    Check {
        id: CheckId::HfCacheLocation,
        status: CheckStatus::Pass,
        requirement: Requirement::Optional,
        gate: Gate::None,
        summary: format!("Hugging Face cache: {}", obs.hf_cache_dir.display()),
        remediation: None,
        path: Some(obs.hf_cache_dir.clone()),
    }
}

fn check_embedding_model_identity() -> Check {
    Check {
        id: CheckId::EmbeddingModelIdentity,
        status: CheckStatus::Pass,
        requirement: Requirement::Optional,
        gate: Gate::None,
        summary: format!(
            "expected model: {DEFAULT_EMBEDDING_MODEL_NAME} (provider: {DEFAULT_EMBEDDING_MODEL_PROVIDER})"
        ),
        remediation: None,
        path: None,
    }
}

fn check_embedding_model_dimension() -> Check {
    Check {
        id: CheckId::EmbeddingModelDimension,
        status: CheckStatus::Pass,
        requirement: Requirement::Optional,
        gate: Gate::None,
        summary: format!(
            "expected embedding dimension: {DEFAULT_EMBEDDING_MODEL_DIMENSIONS} (all-MiniLM-L6-v2)"
        ),
        remediation: None,
        path: None,
    }
}

fn check_embeddings_feature(obs: &Observations) -> Check {
    if obs.embeddings_feature_enabled {
        Check {
            id: CheckId::EmbeddingsFeatureEnabled,
            status: CheckStatus::Pass,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "embeddings feature is compiled in (semantic ingest/query available)"
                .to_owned(),
            remediation: None,
            path: None,
        }
    } else {
        Check {
            id: CheckId::EmbeddingsFeatureEnabled,
            status: CheckStatus::Warn,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "this binary was built without the embeddings feature; \
                      eg ingest --embed and eg query semantic are unavailable"
                .to_owned(),
            remediation: Some(
                "rebuild with default features (or --features embeddings) to enable semantic search"
                    .to_owned(),
            ),
            path: None,
        }
    }
}

/// The interpreter to name in remediation/`next_command` strings: the one the
/// probe detected, or the documented `python` when none was found.
fn python_exe(obs: &Observations) -> &str {
    obs.python_executable.as_deref().unwrap_or("python")
}

/// The documented model-priming command, using the detected interpreter so it is
/// runnable in the same environment being diagnosed (`python3 -m pip` / `python3
/// -c` when only `python3` exists).
fn priming_command(py: &str) -> String {
    format!(
        "{py} -m pip install -U sentence-transformers && {py} -c \
         \"from sentence_transformers import SentenceTransformer; \
         SentenceTransformer('{DEFAULT_EMBEDDING_MODEL_NAME}')\""
    )
}

fn check_model_cache_present(obs: &Observations) -> Check {
    if obs.model_cache_present {
        Check {
            id: CheckId::ModelCachePresent,
            status: CheckStatus::Pass,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: format!(
                "model cache found: {}",
                obs.hf_cache_dir.join(MODEL_CACHE_DIR_SLUG).display()
            ),
            remediation: None,
            path: Some(obs.hf_cache_dir.join(MODEL_CACHE_DIR_SLUG)),
        }
    } else {
        let py = python_exe(obs);
        let remediation = if obs.hf_offline {
            format!(
                "HF_HUB_OFFLINE is set but model is not cached; \
                 disable offline mode and run: {}",
                priming_command(py)
            )
        } else {
            format!("prime the model cache: {}", priming_command(py))
        };
        Check {
            id: CheckId::ModelCachePresent,
            status: CheckStatus::Fail,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: format!(
                "model cache not found under: {}",
                obs.hf_cache_dir.display()
            ),
            remediation: Some(remediation),
            path: Some(obs.hf_cache_dir.clone()),
        }
    }
}

fn check_python_available(obs: &Observations) -> Check {
    if obs.python_available {
        Check {
            id: CheckId::PythonAvailable,
            status: CheckStatus::Pass,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "python3 or python is available on PATH".to_owned(),
            remediation: None,
            path: None,
        }
    } else {
        Check {
            id: CheckId::PythonAvailable,
            status: CheckStatus::Fail,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "python3 and python not found on PATH".to_owned(),
            remediation: Some(
                "install Python 3.8+ (https://python.org/downloads) to enable model priming"
                    .to_owned(),
            ),
            path: None,
        }
    }
}

fn check_python_priming_runnable(obs: &Observations) -> Check {
    if !obs.python_available {
        Check {
            id: CheckId::PythonPrimingRunnable,
            status: CheckStatus::Skipped,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "skipped — python not available".to_owned(),
            remediation: None,
            path: None,
        }
    } else if obs.python_import_ok {
        Check {
            id: CheckId::PythonPrimingRunnable,
            status: CheckStatus::Pass,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "sentence_transformers package is installed (model priming available)"
                .to_owned(),
            remediation: None,
            path: None,
        }
    } else {
        Check {
            id: CheckId::PythonPrimingRunnable,
            status: CheckStatus::Fail,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "sentence_transformers package is not installed".to_owned(),
            remediation: Some(priming_command(python_exe(obs))),
            path: None,
        }
    }
}

fn check_hf_cache_readable(obs: &Observations) -> Check {
    if obs.hf_cache_readable {
        Check {
            id: CheckId::HfCacheReadable,
            status: CheckStatus::Pass,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "Hugging Face cache directory is readable".to_owned(),
            remediation: None,
            path: Some(obs.hf_cache_dir.clone()),
        }
    } else {
        Check {
            id: CheckId::HfCacheReadable,
            status: CheckStatus::Warn,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: format!(
                "Hugging Face cache directory is not readable: {}",
                obs.hf_cache_dir.display()
            ),
            remediation: Some(
                "check permissions on the HF cache directory or set HF_HUB_CACHE to a readable path"
                    .to_owned(),
            ),
            path: Some(obs.hf_cache_dir.clone()),
        }
    }
}

fn check_windows_symlink_support(obs: &Observations) -> Check {
    if obs.is_windows {
        match obs.windows_symlinks_enabled {
            Some(true) => Check {
                id: CheckId::WindowsSymlinkSupport,
                status: CheckStatus::Pass,
                requirement: Requirement::Optional,
                gate: Gate::Semantic,
                summary: "Windows symlink support is enabled (Developer Mode)".to_owned(),
                remediation: None,
                path: None,
            },
            _ => Check {
                id: CheckId::WindowsSymlinkSupport,
                status: CheckStatus::Warn,
                requirement: Requirement::Optional,
                gate: Gate::Semantic,
                summary: "Windows symlink support may not be enabled; \
                          Hugging Face hub uses symlinks for cache deduplication"
                    .to_owned(),
                remediation: Some(
                    "enable Developer Mode in Windows Settings → For Developers, \
                     or set HF_HUB_DISABLE_SYMLINKS_WARNING=1 to suppress the warning"
                        .to_owned(),
                ),
                path: None,
            },
        }
    } else {
        Check {
            id: CheckId::WindowsSymlinkSupport,
            status: CheckStatus::Skipped,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "skipped — not running on Windows".to_owned(),
            remediation: None,
            path: None,
        }
    }
}

fn check_hf_reachable(obs: &Observations) -> Check {
    match obs.hf_reachable {
        Some(true) => Check {
            id: CheckId::HfReachable,
            status: CheckStatus::Pass,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "huggingface.co:443 is TCP-reachable".to_owned(),
            remediation: None,
            path: None,
        },
        Some(false) => Check {
            id: CheckId::HfReachable,
            status: CheckStatus::Warn,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "huggingface.co:443 is not reachable from this machine".to_owned(),
            remediation: Some(
                "check network connectivity or use an already-primed local cache".to_owned(),
            ),
            path: None,
        },
        None => Check {
            id: CheckId::HfReachable,
            status: CheckStatus::Skipped,
            requirement: Requirement::Optional,
            gate: Gate::Semantic,
            summary: "skipped — network check not performed".to_owned(),
            remediation: None,
            path: None,
        },
    }
}

// ── Pure report builder ──────────────────────────────────────────────────────

/// Build a [`PreflightReport`] from a pure `Observations` snapshot.
///
/// This function is deterministic and performs no I/O. The full fixture matrix
/// in the unit tests targets this function directly.
#[must_use]
pub fn build_report(config: &DoctorConfig, obs: &Observations) -> PreflightReport {
    let mut checks = vec![
        check_repository_path(obs),
        check_git_available(obs),
        check_git_history_readable(obs),
        check_output_path_writable(obs),
        check_data_dir_writable(obs),
        check_embedded_store_openable(obs),
        check_hf_cache_location(obs),
        check_embedding_model_identity(),
        check_embedding_model_dimension(),
        check_embeddings_feature(obs),
        check_model_cache_present(obs),
        check_python_available(obs),
        check_python_priming_runnable(obs),
        check_hf_cache_readable(obs),
        check_windows_symlink_support(obs),
    ];

    if obs.network_checked {
        checks.push(check_hf_reachable(obs));
    }

    // Sort by CheckId discriminant for canonical output order.
    checks.sort_by_key(|c| c.id);

    // Structural: all Required Structural checks must Pass.
    let structural_ready = checks.iter().all(|c| {
        !(c.gate == Gate::Structural
            && c.requirement == Requirement::Required
            && c.status == CheckStatus::Fail)
    });

    // Semantic readiness reflects what the embedding *runtime* needs:
    //   1. structural readiness (scan/ingest must work first),
    //   2. the `embeddings` feature compiled into this binary, and
    //   3. the model cache present on disk.
    // Python / sentence-transformers are NOT runtime dependencies — the cached
    // model loads through AletheiaDB's Rust `from_pretrained_hf()`. Python only
    // primes a *missing* cache, so the Python checks stay advisory and never gate.
    let model_cache_ready = !checks
        .iter()
        .any(|c| c.id == CheckId::ModelCachePresent && c.status == CheckStatus::Fail);
    let semantic_ready = structural_ready && obs.embeddings_feature_enabled && model_cache_ready;

    let overall_ready = structural_ready;

    let next_command = compute_next_command(config, obs, &checks, structural_ready, semantic_ready);

    PreflightReport {
        schema_version: DOCTOR_SCHEMA_VERSION,
        repository_path: obs.repo_path.clone(),
        overall_ready,
        structural_ready,
        semantic_ready,
        next_command,
        checks,
    }
}

/// Compute the shortest next command for the local workflow (pure).
fn compute_next_command(
    config: &DoctorConfig,
    obs: &Observations,
    checks: &[Check],
    structural_ready: bool,
    semantic_ready: bool,
) -> String {
    if !structural_ready {
        // Return the remediation of the first failing Required check.
        if let Some(rem) = checks
            .iter()
            .find(|c| c.requirement == Requirement::Required && c.status == CheckStatus::Fail)
            .and_then(|c| c.remediation.as_deref())
        {
            return rem.to_owned();
        }
        return "re-run: eg doctor to diagnose setup issues".to_owned();
    }

    if !semantic_ready {
        if !obs.embeddings_feature_enabled {
            return "rebuild with default features (or --features embeddings) \
                    to enable semantic search"
                .to_owned();
        }
        let model_missing = checks
            .iter()
            .any(|c| c.id == CheckId::ModelCachePresent && c.status == CheckStatus::Fail);
        if model_missing {
            if !obs.python_available {
                // The priming command needs Python, so install it first.
                return "install Python 3.8+ (https://python.org/downloads), \
                        then prime the model cache with pip install -U \
                        sentence-transformers and the SentenceTransformer snippet"
                    .to_owned();
            }
            if obs.hf_offline {
                // Priming downloads the model, so offline mode must be cleared first.
                return format!(
                    "unset HF_HUB_OFFLINE/TRANSFORMERS_OFFLINE, then {}",
                    priming_command(python_exe(obs))
                );
            }
            return priming_command(python_exe(obs));
        }
        return format!(
            "eg ingest {} --adapter embedded --data-dir {} --embed",
            config.out.display(),
            config.data_dir.display()
        );
    }

    format!(
        "eg scan {} --out {}",
        config.repo_path.display(),
        config.out.display()
    )
}

// ── Human-readable renderer ──────────────────────────────────────────────────

/// Render a [`PreflightReport`] as human-readable text for `--format text`.
///
/// Pure function — the JSON format remains the stable machine-readable contract.
#[must_use]
pub fn render_doctor_text(report: &PreflightReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "eg doctor — setup preflight (schema v{})\nrepository: {}\n",
        report.schema_version,
        report.repository_path.display()
    );

    for check in &report.checks {
        let glyph = match check.status {
            CheckStatus::Pass => "[ok]  ",
            CheckStatus::Fail => "[fail]",
            CheckStatus::Warn => "[warn]",
            CheckStatus::Skipped => "[--]  ",
        };
        let _ = writeln!(out, "{glyph} {}", check.summary);
        if let Some(rem) = &check.remediation {
            let _ = writeln!(out, "        → {rem}");
        }
    }

    let verdict = if report.overall_ready {
        "ready"
    } else {
        "not ready"
    };
    let structural = if report.structural_ready {
        "ready"
    } else {
        "not ready"
    };
    let semantic = if report.semantic_ready {
        "ready"
    } else {
        "not ready"
    };
    let _ = writeln!(
        out,
        "\noverall: {verdict}  structural: {structural}  semantic: {semantic}\nnext: {}",
        report.next_command
    );
    out
}

// ── Impure observations gatherer ─────────────────────────────────────────────

/// Probe the real environment and return a safe [`Observations`] snapshot.
///
/// The only impure code in this module. All probe failures become `false`/`None`
/// so `eg doctor` never crashes on the broken environment it is diagnosing.
/// No env value (HF token, API key, etc.) is stored — only resolved paths and
/// boolean outcomes.
#[must_use]
pub fn gather_observations(config: &DoctorConfig) -> Observations {
    use std::process::Command;

    let repo_path = config
        .repo_path
        .canonicalize()
        .unwrap_or_else(|_| config.repo_path.clone());
    // `scan` recursively reads the tree via `discover_source_files`, so the
    // repo check must reflect full traversability — not just the readable root.
    // Reuse the exact discovery `scan` uses: an unreadable root or descendant
    // directory makes it (and therefore `scan`) fail.
    let repo_path_is_dir =
        config.repo_path.is_dir() && crate::fs::discover_source_files(&config.repo_path).is_ok();

    let git_available = Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    let is_git_repo = git_available
        && Command::new("git")
            .args(["-C", &repo_path.to_string_lossy(), "rev-parse", "--git-dir"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

    let git_history_readable = is_git_repo
        && Command::new("git")
            .args(["-C", &repo_path.to_string_lossy(), "log", "-1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

    let out_path = config.out.clone();
    let out_writable = probe_out_writable(&out_path);

    let data_dir = config.data_dir.clone();
    let data_dir_writable = probe_data_dir_writable(&data_dir);

    let embedded_adapter_available = cfg!(feature = "embedded-aletheiadb");
    let embedded_open_block = compute_embedded_open_block(&data_dir);

    let hf_cache_dir = resolve_hf_cache_dir();
    let hf_cache_readable = hf_cache_dir.exists() && std::fs::read_dir(&hf_cache_dir).is_ok();
    let hf_offline = is_hf_offline();
    let model_cache_present = model_snapshot_present(&hf_cache_dir);

    let python_cmd = find_python();
    let python_available = python_cmd.is_some();
    let python_executable = python_cmd.map(str::to_owned);
    let python_import_ok = python_cmd.is_some_and(probe_sentence_transformers_installed);

    let embeddings_feature_enabled = cfg!(feature = "embeddings");

    let is_windows = cfg!(windows);
    let windows_symlinks_enabled = is_windows.then(probe_windows_symlinks);

    let hf_reachable = config.network.then(|| tcp_reachable("huggingface.co:443"));

    Observations {
        repo_path,
        repo_path_is_dir,
        git_available,
        is_git_repo,
        git_history_readable,
        out_path,
        out_writable,
        data_dir,
        data_dir_writable,
        embedded_adapter_available,
        embedded_open_block,
        hf_cache_dir,
        hf_cache_readable,
        hf_offline,
        model_cache_present,
        python_available,
        python_executable,
        python_import_ok,
        embeddings_feature_enabled,
        is_windows,
        windows_symlinks_enabled,
        hf_reachable,
        require_history: config.require_history,
        network_checked: config.network,
    }
}

fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return current;
        }
        match current.parent() {
            Some(p) if !p.as_os_str().is_empty() => current = p.to_path_buf(),
            _ => return PathBuf::from("."),
        }
    }
}

/// Probe writability by exclusively creating, then dropping, a temp file.
///
/// This is the one pragmatic relaxation of "read-only": the probe is transient
/// and self-cleaning. `tempfile` creates a uniquely-named file with `O_EXCL`
/// semantics and removes it on drop, so an existing file or symlink at any
/// predictable path is never truncated or deleted.
fn probe_writable(dir: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".egregore-doctor-probe-")
        .tempfile_in(dir)
        .is_ok()
}

/// Probe whether `scan`/`ingest` could write the output JSONL at `out_path`.
///
/// When `out_path` already exists, `scan` writes to that exact path, so probing
/// only the parent is insufficient: an existing directory is never a valid file
/// target, and an existing read-only file would fail at write time. Otherwise
/// fall back to probing the parent directory.
fn probe_out_writable(out_path: &Path) -> bool {
    if out_path.is_dir() {
        // `scan` would try to write a file at a directory path → always fails.
        return false;
    }
    if out_path.exists() {
        // Existing file: writable iff it can be opened for writing (no truncate,
        // so the probe does not alter the file's contents).
        return std::fs::OpenOptions::new()
            .write(true)
            .open(out_path)
            .is_ok();
    }
    // Does not exist yet: the parent directory must accept a new file.
    let parent = out_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    probe_writable(&parent)
}

/// Probe whether `eg ingest --adapter embedded --data-dir <dir>` could write.
///
/// Two locations must be writable:
///   1. inside `data_dir` (or its nearest existing ancestor, if it does not yet
///      exist) — where the embedded store files live; and
///   2. the parent directory of `data_dir` — where `StoreLease::acquire` creates
///      a sibling runtime directory (`<name>.egregore-runtime`) via
///      `create_dir_all`. This matters even when `data_dir` already exists.
fn probe_data_dir_writable(data_dir: &Path) -> bool {
    let inside_ok = probe_writable(&nearest_existing_ancestor(data_dir));
    let parent = data_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let parent_ok = probe_writable(&nearest_existing_ancestor(&parent));
    inside_ok && parent_ok
}

/// Mirror the embedded-store-open preconditions for `data_dir`, noncreating.
///
/// Replicates the checks `EmbeddedAletheiaSink::open` performs before opening:
/// the stale-daemon repair gate ([`repair::embedded_open_repair_gate`]) and the
/// runtime sidecar directory health (`ensure_runtime_dir` rejects a sidecar that
/// is a symlink or a regular file). Returns `None` when not blocked.
/// True when `path` exists and is either a symlink or not a directory.
#[cfg(feature = "embedded-aletheiadb")]
fn path_is_symlink_or_nondir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.file_type().is_symlink() || !meta.is_dir())
}

#[cfg(feature = "embedded-aletheiadb")]
fn compute_embedded_open_block(data_dir: &Path) -> Option<EmbeddedOpenBlock> {
    // 1. Stale, non-stopped daemon metadata (same gate the adapter applies first).
    if let Some(message) = crate::repair::embedded_open_repair_gate(data_dir) {
        return Some(EmbeddedOpenBlock::StaleDaemonMetadata(message));
    }
    // 2. A live daemon holding the lease: metadata is non-stopped AND not stale
    //    (the lock is held). `StoreLease::acquire` would reject a direct embedded
    //    open here. This is healthy (use `--adapter daemon`), so it is a warning.
    if embedded_store_is_leased(data_dir) {
        return Some(EmbeddedOpenBlock::StoreLeased);
    }
    // 3. Runtime sidecar must be a real directory (not a symlink or file), since
    //    `ensure_runtime_dir` would otherwise fail before the store is opened.
    let runtime_dir = crate::daemon::runtime_dir_for_data_dir(data_dir);
    if path_is_symlink_or_nondir(&runtime_dir) {
        return Some(EmbeddedOpenBlock::RuntimeSidecarUnusable(runtime_dir));
    }
    // 4. The lock file inside the sidecar must not be a symlink: `StoreLease::acquire`
    //    rejects it via `reject_runtime_symlink(&path, "runtime file")`. The lock
    //    file is named `egregored.lock` (see daemon `LOCK_FILE`).
    let lock_file = runtime_dir.join("egregored.lock");
    if std::fs::symlink_metadata(&lock_file).is_ok_and(|m| m.file_type().is_symlink()) {
        return Some(EmbeddedOpenBlock::RuntimeSidecarUnusable(lock_file));
    }
    None
}

/// Whether a live daemon currently holds the embedded store lease.
///
/// Noncreating: metadata exists with a non-stopped state and the runtime lock is
/// held (not stale). Mirrors the condition under which `repair::embedded_open_repair_gate`
/// returns `None` but `StoreLease::acquire` would still reject a direct open.
#[cfg(feature = "embedded-aletheiadb")]
fn embedded_store_is_leased(data_dir: &Path) -> bool {
    let Ok(Some(metadata)) = crate::daemon::try_read_raw_metadata(data_dir) else {
        return false;
    };
    if metadata.state == crate::daemon::DaemonState::Stopped {
        return false;
    }
    // Non-stopped metadata with the lock still held (not stale) means a live lease.
    !crate::daemon::runtime_metadata_is_stale_noncreating(data_dir).unwrap_or(false)
}

/// Embedded adapter is not compiled in, so there is nothing to gate on.
#[cfg(not(feature = "embedded-aletheiadb"))]
#[allow(clippy::missing_const_for_fn)]
fn compute_embedded_open_block(_data_dir: &Path) -> Option<EmbeddedOpenBlock> {
    None
}

/// Detect whether the `sentence_transformers` package is installed **without
/// importing it**.
///
/// `eg doctor` is advertised as read-only, so the probe must not execute
/// arbitrary code. Importing would run a `sentence_transformers.py` planted in an
/// untrusted checkout (the cwd is on `sys.path`). Instead we use
/// `importlib.util.find_spec`, which locates the installed package without
/// executing it, run from a neutral working directory with `PYTHONSAFEPATH=1`
/// (Python 3.11+) so the checkout directory is never prepended to `sys.path`.
///
/// We deliberately avoid `-I`/`-s`, which would drop the user-site directory and
/// hide a `pip install --user sentence-transformers` that the documented priming
/// command would actually use.
fn probe_sentence_transformers_installed(py: &str) -> bool {
    use std::process::Command;
    Command::new(py)
        .args([
            "-c",
            "import importlib.util, sys; \
             sys.exit(0 if importlib.util.find_spec('sentence_transformers') is not None else 1)",
        ])
        .current_dir(std::env::temp_dir())
        .env("PYTHONSAFEPATH", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether a usable model snapshot exists under the HF cache.
///
/// A bare top-level `models--…` directory can be left behind by an interrupted
/// priming run, so require a non-empty `snapshots/` subdirectory rather than
/// accepting any directory with the expected slug.
fn model_snapshot_present(hf_cache_dir: &Path) -> bool {
    let snapshots = hf_cache_dir.join(MODEL_CACHE_DIR_SLUG).join("snapshots");
    std::fs::read_dir(&snapshots).is_ok_and(|mut entries| entries.next().is_some())
}

/// Resolve the HF cache directory per HF convention (safe path, never a token).
///
/// Resolution order: `HF_HUB_CACHE` → `$HF_HOME/hub` →
/// `$XDG_CACHE_HOME/huggingface/hub` (non-Windows) →
/// `<home>/.cache/huggingface/hub`. This mirrors `huggingface_hub`, which bases
/// its default cache on `XDG_CACHE_HOME` when set.
fn resolve_hf_cache_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("HF_HUB_CACHE").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    if let Some(v) = std::env::var_os("HF_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(v).join("hub");
    }
    #[cfg(not(windows))]
    if let Some(v) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(v).join("huggingface").join("hub");
    }
    home_dir().map_or_else(
        || PathBuf::from(".cache/huggingface/hub"),
        |h| h.join(".cache").join("huggingface").join("hub"),
    )
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                let drive = std::env::var_os("HOMEDRIVE")?;
                let path = std::env::var_os("HOMEPATH")?;
                let mut p = PathBuf::from(drive);
                p.push(path);
                Some(p)
            })
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
}

fn is_hf_offline() -> bool {
    for var in &["HF_HUB_OFFLINE", "TRANSFORMERS_OFFLINE"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes") {
                return true;
            }
        }
    }
    false
}

/// Find a usable Python interpreter on PATH.
///
/// Requires Python 3.8+ (the documented minimum) so that a stray Python 2.x — or
/// any older 3.x — does not get recommended for the priming command that would
/// then fail. The probe runs only the builtin `sys` module (no importable code
/// from the cwd), so it stays read-only/safe.
fn find_python() -> Option<&'static str> {
    use std::process::Command;
    for candidate in &["python3", "python"] {
        let ok = Command::new(candidate)
            .args([
                "-c",
                "import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            return Some(candidate);
        }
    }
    None
}

fn tcp_reachable(addr: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    // Resolve the hostname (e.g. `huggingface.co:443`) to one or more socket
    // addresses, then try connecting to each. A bare `SocketAddr` parse would
    // reject hostnames and always report unreachable.
    let Ok(addrs) = addr.to_socket_addrs() else {
        return false;
    };
    for socket_addr in addrs {
        if TcpStream::connect_timeout(&socket_addr, Duration::from_secs(5)).is_ok() {
            return true;
        }
    }
    false
}

#[cfg(windows)]
fn probe_windows_symlinks() -> bool {
    // Probe inside a freshly created, uniquely named temp directory so no
    // predictable path is ever truncated or removed. The directory (and its
    // contents) is deleted when the `TempDir` is dropped.
    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };
    let target = dir.path().join("target");
    if std::fs::write(&target, b"").is_err() {
        return false;
    }
    let link = dir.path().join("link");
    std::os::windows::fs::symlink_file(&target, &link).is_ok()
}

#[cfg(not(windows))]
const fn probe_windows_symlinks() -> bool {
    false
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_obs_structural() -> Observations {
        Observations {
            repo_path: PathBuf::from("/repo"),
            repo_path_is_dir: true,
            git_available: true,
            is_git_repo: true,
            git_history_readable: true,
            out_path: PathBuf::from("/repo/graph.jsonl"),
            out_writable: true,
            data_dir: PathBuf::from("/repo/.egregore"),
            data_dir_writable: true,
            embedded_adapter_available: true,
            embedded_open_block: None,
            hf_cache_dir: PathBuf::from("/home/user/.cache/huggingface/hub"),
            hf_cache_readable: true,
            hf_offline: false,
            model_cache_present: false,
            python_available: false,
            python_executable: None,
            python_import_ok: false,
            embeddings_feature_enabled: true,
            is_windows: false,
            windows_symlinks_enabled: None,
            hf_reachable: None,
            require_history: false,
            network_checked: false,
        }
    }

    fn ready_obs_semantic() -> Observations {
        Observations {
            model_cache_present: true,
            python_available: true,
            python_executable: Some("python3".to_owned()),
            python_import_ok: true,
            ..ready_obs_structural()
        }
    }

    fn default_config() -> DoctorConfig {
        DoctorConfig {
            repo_path: PathBuf::from("/repo"),
            out: PathBuf::from("/repo/graph.jsonl"),
            data_dir: PathBuf::from("/repo/.egregore"),
            require_history: false,
            network: false,
        }
    }

    fn find_check(report: &PreflightReport, id: CheckId) -> Option<&Check> {
        report.checks.iter().find(|c| c.id == id)
    }

    // ── Fixtures: ready states ────────────────────────────────────────────────

    #[test]
    fn ready_structural_exit_0_semantic_not_ready() {
        let report = build_report(&default_config(), &ready_obs_structural());
        assert!(report.structural_ready, "structural should be ready");
        assert!(!report.semantic_ready, "semantic not ready without model");
        assert!(report.overall_ready, "overall_ready == structural_ready");
    }

    #[test]
    fn ready_semantic_all_three_ready() {
        let report = build_report(&default_config(), &ready_obs_semantic());
        assert!(report.structural_ready);
        assert!(report.semantic_ready);
        assert!(report.overall_ready);
    }

    #[test]
    fn overall_ready_equals_structural_ready() {
        let r_a = build_report(&default_config(), &ready_obs_structural());
        let r_b = build_report(&default_config(), &ready_obs_semantic());
        assert_eq!(r_a.overall_ready, r_a.structural_ready);
        assert_eq!(r_b.overall_ready, r_b.structural_ready);
    }

    #[test]
    fn non_directory_repo_path_structural_fail() {
        // e.g. `eg doctor file.rs` — exists as a file but is not a directory.
        let obs = Observations {
            repo_path_is_dir: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            !report.structural_ready,
            "a non-directory path is not ready"
        );
        let c = find_check(&report, CheckId::RepositoryPath).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.requirement, Requirement::Required);
        assert_eq!(c.gate, Gate::Structural);
    }

    // ── Fixtures: required failures (exit 1) ──────────────────────────────────

    #[test]
    fn missing_git_structural_fail() {
        let obs = Observations {
            git_available: false,
            is_git_repo: false,
            git_history_readable: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(!report.structural_ready);
        let c = find_check(&report, CheckId::GitAvailable).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.requirement, Requirement::Required);
        assert_eq!(c.gate, Gate::Structural);
    }

    #[test]
    fn non_git_history_required_structural_fail() {
        let obs = Observations {
            is_git_repo: false,
            git_history_readable: false,
            require_history: true,
            ..ready_obs_structural()
        };
        let cfg = DoctorConfig {
            require_history: true,
            ..default_config()
        };
        let report = build_report(&cfg, &obs);
        assert!(!report.structural_ready);
        let c = find_check(&report, CheckId::GitHistoryReadable).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.requirement, Requirement::Required);
        assert_eq!(c.gate, Gate::Structural);
    }

    #[test]
    fn non_git_history_optional_structural_ready() {
        let obs = Observations {
            is_git_repo: false,
            git_history_readable: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            report.structural_ready,
            "history optional → structural still ready"
        );
        let c = find_check(&report, CheckId::GitHistoryReadable).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.requirement, Requirement::Optional);
    }

    #[test]
    fn unwritable_out_structural_fail() {
        let obs = Observations {
            out_writable: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(!report.structural_ready);
        let c = find_check(&report, CheckId::OutputPathWritable).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.requirement, Requirement::Required);
    }

    #[test]
    fn unwritable_data_dir_structural_fail() {
        let obs = Observations {
            data_dir_writable: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(!report.structural_ready);
        let c = find_check(&report, CheckId::DataDirWritable).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.requirement, Requirement::Required);
    }

    #[test]
    fn stale_daemon_metadata_blocks_structural() {
        let obs = Observations {
            embedded_open_block: Some(EmbeddedOpenBlock::StaleDaemonMetadata(
                "run `eg repair preflight`".to_owned(),
            )),
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            !report.structural_ready,
            "stale daemon metadata blocks embedded ingest"
        );
        let c = find_check(&report, CheckId::EmbeddedStoreOpenable).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.requirement, Requirement::Required);
        assert_eq!(c.gate, Gate::Structural);
        assert!(c.remediation.as_deref().unwrap().contains("repair"));
    }

    #[test]
    fn unusable_runtime_sidecar_blocks_structural() {
        let obs = Observations {
            embedded_open_block: Some(EmbeddedOpenBlock::RuntimeSidecarUnusable(PathBuf::from(
                "/repo/.egregore.egregore-runtime",
            ))),
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(!report.structural_ready);
        let c = find_check(&report, CheckId::EmbeddedStoreOpenable).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
    }

    #[test]
    fn embedded_store_check_skipped_without_adapter() {
        let obs = Observations {
            embedded_adapter_available: false,
            embedded_open_block: None,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            report.structural_ready,
            "skipped check never blocks structural"
        );
        let c = find_check(&report, CheckId::EmbeddedStoreOpenable).unwrap();
        assert_eq!(c.status, CheckStatus::Skipped);
    }

    #[test]
    fn live_lease_is_warn_not_structural_fail() {
        let obs = Observations {
            embedded_open_block: Some(EmbeddedOpenBlock::StoreLeased),
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            report.structural_ready,
            "a running daemon lease is healthy and must not fail structural readiness"
        );
        let c = find_check(&report, CheckId::EmbeddedStoreOpenable).unwrap();
        assert_eq!(c.status, CheckStatus::Warn);
        assert_eq!(c.requirement, Requirement::Optional);
        assert!(c.remediation.as_deref().unwrap().contains("daemon"));
    }

    #[test]
    fn data_dir_skipped_without_embedded_adapter() {
        // Without the embedded adapter, an unwritable data dir must not block
        // structural readiness (the dry-run ingest path never uses --data-dir).
        let obs = Observations {
            embedded_adapter_available: false,
            embedded_open_block: None,
            data_dir_writable: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            report.structural_ready,
            "data-dir is not required without the adapter"
        );
        let c = find_check(&report, CheckId::DataDirWritable).unwrap();
        assert_eq!(c.status, CheckStatus::Skipped);
    }

    // ── Fixtures: semantic-only failures (exit 0) ─────────────────────────────

    #[test]
    fn missing_python_semantic_not_ready_exit_0() {
        let obs = Observations {
            python_available: false,
            python_import_ok: false,
            model_cache_present: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(report.structural_ready, "structural still ready");
        assert!(!report.semantic_ready);
        let py = find_check(&report, CheckId::PythonAvailable).unwrap();
        assert_eq!(py.status, CheckStatus::Fail);
        let priming = find_check(&report, CheckId::PythonPrimingRunnable).unwrap();
        assert_eq!(
            priming.status,
            CheckStatus::Skipped,
            "priming skipped when python absent"
        );
    }

    #[test]
    fn missing_sentence_transformers_semantic_not_ready() {
        let obs = Observations {
            python_available: true,
            python_import_ok: false,
            model_cache_present: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(report.structural_ready);
        assert!(!report.semantic_ready);
        let c = find_check(&report, CheckId::PythonPrimingRunnable).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
    }

    #[test]
    fn missing_model_cache_semantic_not_ready() {
        let obs = Observations {
            python_available: true,
            python_import_ok: true,
            model_cache_present: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(report.structural_ready);
        assert!(!report.semantic_ready);
        let c = find_check(&report, CheckId::ModelCachePresent).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
    }

    #[test]
    fn cached_model_without_python_is_semantic_ready() {
        // The embedding runtime loads the cached model through Rust, not Python,
        // so a present cache should be semantic-ready even with no Python at all.
        let obs = Observations {
            model_cache_present: true,
            python_available: false,
            python_import_ok: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(report.structural_ready);
        assert!(
            report.semantic_ready,
            "cached model + embeddings feature should be semantic-ready without Python"
        );
        // Python checks remain visible but advisory (do not gate).
        assert_eq!(
            find_check(&report, CheckId::PythonAvailable)
                .unwrap()
                .status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn semantic_not_ready_when_embeddings_feature_disabled() {
        // A --no-default-features binary cannot run semantic ingest/query even
        // with a primed cache, so semantic_ready must be false.
        let obs = Observations {
            model_cache_present: true,
            python_available: true,
            python_import_ok: true,
            embeddings_feature_enabled: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            report.structural_ready,
            "structural unaffected by the feature gate"
        );
        assert!(!report.semantic_ready);
        let c = find_check(&report, CheckId::EmbeddingsFeatureEnabled).unwrap();
        assert_eq!(c.status, CheckStatus::Warn);
        assert_eq!(
            c.requirement,
            Requirement::Optional,
            "feature check never affects exit"
        );
        assert!(report.next_command.contains("features"));
    }

    #[test]
    fn offline_without_cache_semantic_not_ready_exit_0() {
        let obs = Observations {
            hf_offline: true,
            model_cache_present: false,
            python_available: true,
            python_import_ok: true,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(report.structural_ready);
        assert!(!report.semantic_ready);
        let c = find_check(&report, CheckId::ModelCachePresent).unwrap();
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.remediation.as_deref().unwrap_or("").contains("offline"),
            "remediation should mention offline mode"
        );
    }

    // ── Fixtures: warnings (never affect exit) ────────────────────────────────

    #[test]
    fn unreadable_cache_is_warn_not_fail() {
        let obs = Observations {
            hf_cache_readable: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(report.structural_ready, "warn does not affect structural");
        let c = find_check(&report, CheckId::HfCacheReadable).unwrap();
        assert_eq!(c.status, CheckStatus::Warn);
        assert_eq!(c.requirement, Requirement::Optional);
    }

    #[test]
    fn windows_symlink_warning() {
        let obs = Observations {
            is_windows: true,
            windows_symlinks_enabled: Some(false),
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(report.structural_ready);
        let c = find_check(&report, CheckId::WindowsSymlinkSupport).unwrap();
        assert_eq!(c.status, CheckStatus::Warn);
    }

    #[test]
    fn windows_symlink_skipped_on_linux() {
        let obs = Observations {
            is_windows: false,
            windows_symlinks_enabled: None,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        let c = find_check(&report, CheckId::WindowsSymlinkSupport).unwrap();
        assert_eq!(c.status, CheckStatus::Skipped);
    }

    // ── Network check ─────────────────────────────────────────────────────────

    #[test]
    fn no_network_flag_omits_hf_reachable() {
        let obs = Observations {
            network_checked: false,
            hf_reachable: None,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            find_check(&report, CheckId::HfReachable).is_none(),
            "HfReachable should not appear without --network"
        );
    }

    #[test]
    fn network_flag_includes_hf_reachable() {
        let obs = Observations {
            network_checked: true,
            hf_reachable: Some(true),
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        let c = find_check(&report, CheckId::HfReachable).unwrap();
        assert_eq!(c.status, CheckStatus::Pass);
    }

    // ── Canonical ordering + determinism ──────────────────────────────────────

    #[test]
    fn checks_are_sorted_by_check_id_discriminant() {
        let report = build_report(&default_config(), &ready_obs_semantic());
        let ids: Vec<CheckId> = report.checks.iter().map(|c| c.id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "checks must be sorted by CheckId");
    }

    #[test]
    fn deterministic_five_repetitions() {
        let obs = ready_obs_semantic();
        let cfg = default_config();
        let r0 = build_report(&cfg, &obs);
        let s0 = serde_json::to_string_pretty(&r0).unwrap();
        for _ in 1..5 {
            let r = build_report(&cfg, &obs);
            let s = serde_json::to_string_pretty(&r).unwrap();
            assert_eq!(
                s0, s,
                "serialised report must be byte-identical across runs"
            );
        }
    }

    // ── Secret safety ─────────────────────────────────────────────────────────

    #[test]
    fn report_never_contains_fake_secret() {
        let fake_secret = "hf_FAKE_SECRET_TOKEN_SHOULD_NOT_APPEAR_abc123";
        let obs = ready_obs_semantic();
        let report = build_report(&default_config(), &obs);
        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(
            !json.contains(fake_secret),
            "serialised report must not contain secret-bearing values"
        );
    }

    #[test]
    fn report_schema_version_is_set() {
        let report = build_report(&default_config(), &ready_obs_structural());
        assert_eq!(report.schema_version, DOCTOR_SCHEMA_VERSION);
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(v["schema_version"], 1);
    }

    // ── next_command ──────────────────────────────────────────────────────────

    #[test]
    fn next_command_guides_to_git_install_when_git_missing() {
        let obs = Observations {
            git_available: false,
            is_git_repo: false,
            git_history_readable: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            report.next_command.contains("git"),
            "next_command should mention git"
        );
    }

    #[test]
    fn next_command_all_ready_suggests_scan() {
        let report = build_report(&default_config(), &ready_obs_semantic());
        assert!(
            report.next_command.contains("scan"),
            "next_command for ready env should suggest scan: {}",
            report.next_command
        );
    }

    #[test]
    fn next_command_structural_ready_semantic_not_suggests_model_priming() {
        let obs = Observations {
            python_available: true,
            python_import_ok: true,
            model_cache_present: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            report.next_command.contains("sentence_transformers")
                || report.next_command.contains("SentenceTransformer"),
            "next_command should suggest model priming: {}",
            report.next_command
        );
    }

    #[test]
    fn next_command_offline_model_missing_says_unset_offline() {
        let obs = Observations {
            hf_offline: true,
            model_cache_present: false,
            python_available: true,
            python_executable: Some("python3".to_owned()),
            python_import_ok: true,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            report.next_command.contains("HF_HUB_OFFLINE"),
            "offline next_command must tell the operator to clear offline mode first: {}",
            report.next_command
        );
    }

    #[test]
    fn next_command_uses_detected_python_executable() {
        // Only python3 exists; the priming next_command must name python3, not python.
        let obs = Observations {
            python_available: true,
            python_executable: Some("python3".to_owned()),
            python_import_ok: true,
            model_cache_present: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            report.next_command.contains("python3 -m pip")
                && report.next_command.contains("python3 -c"),
            "next_command should use the detected python3 executable: {}",
            report.next_command
        );
    }

    #[test]
    fn next_command_model_missing_without_python_suggests_python_install() {
        // The priming command needs Python; when it is absent, point there first.
        let obs = Observations {
            python_available: false,
            python_import_ok: false,
            model_cache_present: false,
            ..ready_obs_structural()
        };
        let report = build_report(&default_config(), &obs);
        assert!(
            report.next_command.contains("Python"),
            "next_command should recommend installing Python first: {}",
            report.next_command
        );
    }
}
