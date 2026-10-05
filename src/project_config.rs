//! Checked-in project configuration — `egregore.toml` (issue #261).
//!
//! Egregore's behavior used to be configured entirely through transient CLI
//! flags, so two operators scanning the same source could produce different
//! stores with no record of the configuration that produced them. A
//! repository-root `egregore.toml` pins the per-repo defaults — data
//! directory, repository identity override, embedding model, scan scope, and
//! redaction policy — so every agent, machine, and CI run reproduces the same
//! store from the same source without re-specifying flags.
//!
//! Discovery: [`discover_config_file`] walks up from the working directory; a
//! missing file is not an error (built-in defaults apply). Parsing is strict:
//! malformed TOML, unknown keys/tables, wrong types, an unknown redaction
//! policy, or an unparsable timestamp all fail fast with a machine-readable
//! [`ConfigLoadError`] diagnostic (`code: "config_invalid"`), never a silent
//! ignore.
//!
//! Precedence (see [`resolve_opt`], [`resolve_value`], [`resolve_flag`]):
//! explicit CLI flag > config file value > built-in default.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use serde::Serialize;

/// Name of the checked-in project config file, discovered by walking up from
/// the working directory.
pub const CONFIG_FILE_NAME: &str = "egregore.toml";

/// Machine-readable error code carried by every [`ConfigLoadError`].
pub const CONFIG_INVALID_CODE: &str = "config_invalid";

/// Where one resolved configuration value came from.
///
/// Serialized lowercase (`"cli"`, `"config"`, `"default"`) for the
/// machine-readable `eg config show` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueSource {
    /// An explicit CLI flag.
    Cli,
    /// The checked-in `egregore.toml`.
    Config,
    /// The built-in default (no flag, no config value).
    Default,
}

/// Failure to discover, read, parse, or validate an `egregore.toml`.
///
/// Always renders as a machine-readable diagnostic via [`Self::diagnostic`]:
/// `{"code":"config_invalid","config_file":...,"message":...}`.
#[derive(Debug)]
pub struct ConfigLoadError {
    path: Option<PathBuf>,
    message: String,
}

impl ConfigLoadError {
    const fn new(path: Option<PathBuf>, message: String) -> Self {
        Self { path, message }
    }

    fn io(path: &Path, error: &std::io::Error) -> Self {
        Self::new(
            Some(path.to_path_buf()),
            format!("failed to read config file: {error}"),
        )
    }

    fn parse(path: &Path, error: &toml_edit::TomlError) -> Self {
        Self::new(
            Some(path.to_path_buf()),
            format!("failed to parse config file as TOML: {error}"),
        )
    }

    fn schema(path: &Path, message: String) -> Self {
        Self::new(Some(path.to_path_buf()), message)
    }

    /// The stable machine-readable error code (`"config_invalid"`).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        CONFIG_INVALID_CODE
    }

    /// The machine-readable diagnostic object printed to stderr on failure.
    #[must_use]
    pub fn diagnostic(&self) -> serde_json::Value {
        serde_json::json!({
            "code": self.code(),
            "config_file": self.path.as_ref().map(|p| p.to_string_lossy()),
            "message": self.message,
        })
    }
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {} ({})", self.code(), self.message, path.display()),
            None => write!(f, "{}: {}", self.code(), self.message),
        }
    }
}

impl std::error::Error for ConfigLoadError {}

/// `[embeddings]` table: which embedding model `--embed` runs produce.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbeddingsConfig {
    /// Embedding model identifier (Hugging Face model id), e.g.
    /// `"sentence-transformers/all-MiniLM-L6-v2"`. Unset → the built-in
    /// default ([`default_embedding_model`]).
    pub model: Option<String>,
}

/// `[scan]` table: scan-scope pins and the fixture-reproducibility time knob.
///
/// The include/exclude arrays are presence-tracked (`None` when the key is
/// absent, `Some` — even an empty vec — when pinned) so `eg config show`
/// reports the source honestly: an explicitly set-but-empty array is a
/// config pin, not a default. The scope pins are reserved for the issue #99
/// scoping engine; pinned here so the values are version-controlled,
/// reported by `eg config show`, and fail loudly on typos — but not yet
/// consumed by `eg scan`, which warns when they are set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanConfig {
    /// Scan-scope include patterns (`None` when the key is absent).
    pub include: Option<Vec<String>>,
    /// Scan-scope exclude patterns (`None` when the key is absent).
    pub exclude: Option<Vec<String>>,
    /// Fixed RFC 3339 transaction time stamping the scan (e.g.
    /// `"2026-05-19T00:00:00Z"`). Fixture/CI use: two runs sharing the
    /// checked-in config produce byte-for-byte identical graph JSONL. Unset →
    /// the current wall-clock instant, as before.
    pub transaction_time: Option<String>,
}

/// `[redaction]` table: which redaction policy applies to scanned graphs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactionConfig {
    /// Redaction policy name. Must be the documented policy
    /// (`docs/schema/redaction.md`), currently `"v1"`; anything else fails
    /// fast so a future policy can never apply silently. Unset → the
    /// built-in default ([`default_redaction_policy`]).
    pub policy: Option<String>,
    /// Mirror of `--raw-literals`: keep source-embedded secrets in raw form
    /// instead of redacting them. Unset → `false`.
    pub raw_literals: Option<bool>,
}

/// A parsed `egregore.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    /// Default `--data-dir`. Relative paths resolve against the directory
    /// containing the config file (the repository root), so `eg ingest` run
    /// from a subdirectory hits the same store as from the root.
    pub data_dir: Option<PathBuf>,
    /// Default `--repo-id-override`: forces `identity_source =
    /// operator_override` for repository identity.
    pub repo_id_override: Option<String>,
    /// Embedding model pinning.
    pub embeddings: EmbeddingsConfig,
    /// Scan-scope and time pinning.
    pub scan: ScanConfig,
    /// Redaction policy pinning.
    pub redaction: RedactionConfig,
}

/// A discovered and parsed config file: its path plus the parsed values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    /// Absolute (or cwd-anchored) path of the discovered `egregore.toml`.
    pub path: PathBuf,
    /// The parsed configuration.
    pub config: ProjectConfig,
}

/// The built-in default `--data-dir` used when neither a flag nor the config
/// pins one: the historical `".egregore"`, resolved against the working
/// directory exactly as before issue #261.
#[must_use]
pub fn default_data_dir() -> PathBuf {
    PathBuf::from(".egregore")
}

/// The built-in default embedding model identifier, shared with the CLI's
/// default embedder so the two can never disagree.
#[must_use]
pub fn default_embedding_model() -> String {
    crate::embeddings::DEFAULT_EMBEDDING_MODEL_NAME.to_owned()
}

/// The built-in default redaction policy: the documented
/// `docs/schema/redaction.md` policy version.
#[must_use]
pub fn default_redaction_policy() -> String {
    crate::redaction::REDACTION_POLICY_VERSION.to_owned()
}

/// Walk up from `start`, returning the nearest `egregore.toml` found.
///
/// A missing file is not an error: returns `None` and the caller falls back
/// to built-in defaults.
#[must_use]
pub fn discover_config_file(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        let candidate = dir.join(CONFIG_FILE_NAME);
        candidate.is_file().then_some(candidate)
    })
}

/// Discover ([`discover_config_file`]) and parse ([`load_config_file`]) the
/// project config, returning `None` when no file exists.
///
/// # Errors
///
/// Returns [`ConfigLoadError`] when a discovered file cannot be read, parsed,
/// or validated.
pub fn discover_and_load(start: &Path) -> Result<Option<LoadedConfig>, ConfigLoadError> {
    let Some(path) = discover_config_file(start) else {
        return Ok(None);
    };
    load_config_file(&path).map(|config| Some(LoadedConfig { path, config }))
}

/// Parse and validate the `egregore.toml` at `path`.
///
/// Strict: unknown keys or tables, wrong value types, an unknown redaction
/// policy, and an unparsable `scan.transaction_time` are all errors — never
/// silently ignored.
///
/// # Errors
///
/// Returns [`ConfigLoadError`] when the file cannot be read, is not valid
/// TOML, or fails validation.
pub fn load_config_file(path: &Path) -> Result<ProjectConfig, ConfigLoadError> {
    let text = fs::read_to_string(path).map_err(|error| ConfigLoadError::io(path, &error))?;
    let document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error| ConfigLoadError::parse(path, &error))?;
    let table = document.as_table();

    check_keys(
        table,
        &[
            "data_dir",
            "repo_id_override",
            "embeddings",
            "scan",
            "redaction",
        ],
        "",
        path,
    )?;

    let config_dir = path
        .parent()
        .map_or_else(|| Path::new("."), std::convert::identity);
    let data_dir = get_string(table, "data_dir", "", path)?.map(|raw| {
        let raw_path = PathBuf::from(raw);
        if raw_path.is_absolute() {
            raw_path
        } else {
            config_dir.join(raw_path)
        }
    });
    let repo_id_override = get_string(table, "repo_id_override", "", path)?.map(str::to_owned);

    let embeddings = {
        let sub = get_table(table, "embeddings", "", path)?;
        if let Some(sub) = sub {
            check_keys(sub, &["model"], "embeddings.", path)?;
            EmbeddingsConfig {
                model: get_string(sub, "model", "embeddings.", path)?.map(str::to_owned),
            }
        } else {
            EmbeddingsConfig::default()
        }
    };

    let scan = {
        let sub = get_table(table, "scan", "", path)?;
        if let Some(sub) = sub {
            check_keys(
                sub,
                &["include", "exclude", "transaction_time"],
                "scan.",
                path,
            )?;
            let include = get_string_array(sub, "include", "scan.", path)?;
            let exclude = get_string_array(sub, "exclude", "scan.", path)?;
            let transaction_time =
                get_string(sub, "transaction_time", "scan.", path)?.map(str::to_owned);
            if let Some(stamp) = &transaction_time {
                validate_transaction_time(stamp, path)?;
            }
            ScanConfig {
                include,
                exclude,
                transaction_time,
            }
        } else {
            ScanConfig::default()
        }
    };

    let redaction = {
        let sub = get_table(table, "redaction", "", path)?;
        if let Some(sub) = sub {
            check_keys(sub, &["policy", "raw_literals"], "redaction.", path)?;
            let policy = get_string(sub, "policy", "redaction.", path)?.map(str::to_owned);
            if let Some(name) = &policy
                && name != crate::redaction::REDACTION_POLICY_VERSION
            {
                return Err(ConfigLoadError::schema(
                    path,
                    format!(
                        "unknown redaction policy {name:?}; known policies: {:?}",
                        crate::redaction::REDACTION_POLICY_VERSION
                    ),
                ));
            }
            let raw_literals = get_bool(sub, "raw_literals", "redaction.", path)?;
            RedactionConfig {
                policy,
                raw_literals,
            }
        } else {
            RedactionConfig::default()
        }
    };

    Ok(ProjectConfig {
        data_dir,
        repo_id_override,
        embeddings,
        scan,
        redaction,
    })
}

/// Reject any key outside `allowed`, naming the offending key (with its table
/// prefix, e.g. `scan.bogus`) in the diagnostic.
fn check_keys(
    table: &toml_edit::Table,
    allowed: &[&str],
    prefix: &str,
    path: &Path,
) -> Result<(), ConfigLoadError> {
    for (key, _) in table {
        if !allowed.contains(&key) {
            return Err(ConfigLoadError::schema(
                path,
                format!(
                    "unknown config key `{prefix}{key}`: {CONFIG_FILE_NAME} is strictly validated; \
                     fix the key or remove it (see docs/cli/config.md)"
                ),
            ));
        }
    }
    Ok(())
}

/// Fetch an optional table, rejecting non-table values.
fn get_table<'a>(
    table: &'a toml_edit::Table,
    key: &str,
    prefix: &str,
    path: &Path,
) -> Result<Option<&'a toml_edit::Table>, ConfigLoadError> {
    table.get(key).map_or_else(
        || Ok(None),
        |item| {
            item.as_table().map(Some).ok_or_else(|| {
                ConfigLoadError::schema(path, format!("config key `{prefix}{key}` must be a table"))
            })
        },
    )
}

/// Fetch an optional string, rejecting non-string values.
fn get_string<'a>(
    table: &'a toml_edit::Table,
    key: &str,
    prefix: &str,
    path: &Path,
) -> Result<Option<&'a str>, ConfigLoadError> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => {
            let value = item.as_str().ok_or_else(|| {
                ConfigLoadError::schema(
                    path,
                    format!("config key `{prefix}{key}` must be a string"),
                )
            })?;
            Ok(Some(value))
        }
    }
}

/// Fetch an optional boolean, rejecting non-boolean values.
fn get_bool(
    table: &toml_edit::Table,
    key: &str,
    prefix: &str,
    path: &Path,
) -> Result<Option<bool>, ConfigLoadError> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => {
            let value = item.as_bool().ok_or_else(|| {
                ConfigLoadError::schema(
                    path,
                    format!("config key `{prefix}{key}` must be a boolean"),
                )
            })?;
            Ok(Some(value))
        }
    }
}

/// Fetch an optional string array, rejecting non-arrays and non-string items.
///
/// Returns `None` when the key is absent so callers can distinguish "not
/// pinned" from "pinned to an empty list" (which `eg config show` reports
/// with source `"config"`).
fn get_string_array(
    table: &toml_edit::Table,
    key: &str,
    prefix: &str,
    path: &Path,
) -> Result<Option<Vec<String>>, ConfigLoadError> {
    let Some(item) = table.get(key) else {
        return Ok(None);
    };
    let Some(array) = item.as_array() else {
        return Err(ConfigLoadError::schema(
            path,
            format!("config key `{prefix}{key}` must be an array of strings"),
        ));
    };
    array
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                ConfigLoadError::schema(
                    path,
                    format!("config key `{prefix}{key}` must be an array of strings"),
                )
            })
        })
        .collect::<Result<Vec<String>, _>>()
        .map(Some)
}

/// A pinned `scan.transaction_time` must be RFC 3339 so the scan entry point
/// can stamp it verbatim.
fn validate_transaction_time(stamp: &str, path: &Path) -> Result<(), ConfigLoadError> {
    if chrono::DateTime::parse_from_rfc3339(stamp).is_err() {
        return Err(ConfigLoadError::schema(
            path,
            format!(
                "config key `scan.transaction_time` must be RFC 3339 (e.g. \
                 \"2026-05-19T00:00:00Z\"), got {stamp:?}"
            ),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Precedence: explicit CLI flag > config file value > built-in default
// ---------------------------------------------------------------------------

/// Resolve an optional setting: the CLI flag wins when present, else the
/// config value, else `None` (the built-in default applies downstream).
#[must_use]
pub fn resolve_opt<T: Clone>(cli: Option<T>, config: Option<T>) -> (Option<T>, ValueSource) {
    match (cli, config) {
        (Some(value), _) => (Some(value), ValueSource::Cli),
        (None, Some(value)) => (Some(value), ValueSource::Config),
        (None, None) => (None, ValueSource::Default),
    }
}

/// Resolve a setting with a built-in default: CLI flag > config > default.
#[must_use]
pub fn resolve_value<T: Clone>(cli: Option<T>, config: Option<T>, default: T) -> (T, ValueSource) {
    match (cli, config) {
        (Some(value), _) => (value, ValueSource::Cli),
        (None, Some(value)) => (value, ValueSource::Config),
        (None, None) => (default, ValueSource::Default),
    }
}

/// Resolve a boolean CLI flag against an optional config boolean.
///
/// A `bool` flag cannot distinguish "passed" from "default false", but the
/// precedence still holds exactly: the only way the flag differs from the
/// built-in default (`false`) is an explicit `--flag`, which always wins.
#[must_use]
pub fn resolve_flag(flag: bool, config: Option<bool>) -> (bool, ValueSource) {
    if flag {
        (true, ValueSource::Cli)
    } else {
        config.map_or((false, ValueSource::Default), |value| {
            (value, ValueSource::Config)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp_config(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("temp dir should be created");
        let path = dir.path().join(CONFIG_FILE_NAME);
        fs::write(&path, body).expect("config should be writable");
        (dir, path)
    }

    #[test]
    fn unknown_key_error_names_key_and_code() {
        let (_dir, path) = write_temp_config("bogus = 1\n");
        let error = load_config_file(&path).expect_err("unknown key must fail");
        assert_eq!(error.code(), "config_invalid");
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic["code"], "config_invalid");
        assert!(
            diagnostic["message"]
                .as_str()
                .unwrap_or_default()
                .contains("bogus")
        );
    }

    #[test]
    fn data_dir_resolves_against_config_directory() {
        let (dir, path) = write_temp_config("data_dir = \"store\"\n");
        let config = load_config_file(&path).expect("config should parse");
        assert_eq!(config.data_dir, Some(dir.path().join("store")));
    }

    #[test]
    fn resolve_flag_precedence() {
        assert_eq!(resolve_flag(true, Some(false)), (true, ValueSource::Cli));
        assert_eq!(resolve_flag(false, Some(true)), (true, ValueSource::Config));
        assert_eq!(resolve_flag(false, None), (false, ValueSource::Default));
    }

    #[test]
    fn value_source_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(ValueSource::Config).expect("serializes"),
            serde_json::Value::String("config".to_owned())
        );
    }
}
