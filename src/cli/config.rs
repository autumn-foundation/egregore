//! Checked-in project configuration (`egregore.toml`) CLI plumbing (issue #261).
//!
//! [`cli_project_config`] discovers the config once per process by walking up
//! from the working directory; a missing file is not an error. A malformed
//! file or unknown key fails fast with the machine-readable
//! `{"code":"config_invalid",...}` diagnostic on stderr and a non-zero exit,
//! never a silent ignore.
//!
//! The scan/ingest/refresh/query entry points resolve their flags against the
//! discovered config with explicit precedence — CLI flag > config file value >
//! built-in default — via [`resolve_scan_args`] and [`resolve_data_dir`], and
//! `eg config show` reports the effective resolved configuration.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use super::*;
use crate::project_config::{LoadedConfig, ValueSource, resolve_flag, resolve_opt, resolve_value};

/// Process-wide discovered config: discovery walks the filesystem, so it runs
/// once and every command entry point shares the result (including the
/// fail-fast behavior on malformed files).
static CLI_CONFIG: OnceLock<Option<LoadedConfig>> = OnceLock::new();

/// Discover the `egregore.toml` for this invocation by walking up from the
/// working directory.
///
/// Returns `None` when no config file exists (built-in defaults apply). A
/// discovered file that cannot be read, parsed, or validated prints the
/// machine-readable `config_invalid` diagnostic to stderr and exits non-zero.
pub(crate) fn cli_project_config() -> Option<LoadedConfig> {
    CLI_CONFIG
        .get_or_init(|| {
            let cwd =
                std::env::current_dir().map_or_else(|_| PathBuf::from("."), std::convert::identity);
            match crate::project_config::discover_and_load(&cwd) {
                Ok(loaded) => loaded,
                Err(error) => {
                    let diagnostic = serde_json::to_string(&error.diagnostic()).map_or_else(
                        |_| r#"{"code":"config_invalid"}"#.to_owned(),
                        std::convert::identity,
                    );
                    eprintln!("{diagnostic}");
                    std::process::exit(1);
                }
            }
        })
        .clone()
}

/// The config-pinned data directory, if any (already resolved against the
/// config file's directory at parse time).
pub(crate) fn config_data_dir() -> Option<PathBuf> {
    cli_project_config().and_then(|loaded| loaded.config.data_dir)
}

/// Resolve the effective store for a query lane: an explicit `--data-dir`
/// wins; the config-pinned dir applies only when the caller passed neither
/// `--graph` nor `--data-dir`, so a `--graph` file read plus a pinned store
/// never reads as "both provided" (issue #261).
pub(crate) fn resolve_query_data_dir(
    graph: Option<&Path>,
    data_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    resolve_query_data_dir_with(graph, data_dir, config_data_dir())
}

/// Pure core of [`resolve_query_data_dir`]: the config fallback for query
/// lanes, testable without filesystem discovery.
fn resolve_query_data_dir_with(
    graph: Option<&Path>,
    data_dir: Option<PathBuf>,
    config: Option<PathBuf>,
) -> Option<PathBuf> {
    match (graph, data_dir) {
        (None, None) => config,
        (_, data_dir) => data_dir,
    }
}

/// Resolve the effective `--data-dir`: explicit CLI flag > config file value >
/// built-in default (`.egregore`, exactly as before issue #261).
pub(crate) fn resolve_data_dir(flag: Option<&Path>) -> PathBuf {
    resolve_opt(flag.map(Path::to_path_buf), config_data_dir())
        .0
        .map_or_else(
            crate::project_config::default_data_dir,
            std::convert::identity,
        )
}

/// Scan inputs after resolving explicit CLI flags against the checked-in config.
///
/// Precedence: explicit CLI flag > config file value > built-in default.
pub(crate) struct ResolvedScanArgs {
    /// Effective `--repo-id-override`.
    pub repo_id_override: Option<String>,
    /// Effective `--raw-literals`.
    pub raw_literals: bool,
    /// Effective scan transaction time: the config-pinned
    /// `scan.transaction_time`, or `None` for the historical wall-clock scan.
    pub transaction_time: Option<String>,
    /// Effective `scan-history --max-commits` (raw; issue #256).
    pub max_commits: Option<String>,
    /// Effective `scan-history --since` (issue #256).
    pub since: Option<String>,
    /// Effective `scan-history --from` (issue #256).
    pub from_rev: Option<String>,
    /// Effective `scan-history --to` (issue #256).
    pub to_rev: Option<String>,
}

/// Resolve `eg scan` / `eg scan-history` flags against the discovered config.
pub(crate) fn resolve_scan_args(
    repo_id_override: Option<String>,
    raw_literals: bool,
) -> ResolvedScanArgs {
    let config = cli_project_config();
    let config = config.as_ref().map(|loaded| &loaded.config);
    let (repo_id_override, _) = resolve_opt(
        repo_id_override,
        config.and_then(|c| c.repo_id_override.clone()),
    );
    let (raw_literals, _) =
        resolve_flag(raw_literals, config.and_then(|c| c.redaction.raw_literals));
    let transaction_time = config.and_then(|c| c.scan.transaction_time.clone());
    ResolvedScanArgs {
        repo_id_override,
        raw_literals,
        transaction_time,
        max_commits: None,
        since: None,
        from_rev: None,
        to_rev: None,
    }
}

/// Warn once when the config pins `[scan] include`/`exclude`: the issue #99
/// scoping engine does not exist yet, so the values are version-controlled and
/// reported but not consumed — a loud warning, never a silent ignore.
pub(crate) fn warn_on_unconsumed_scope_pins() {
    let config = cli_project_config();
    let Some(loaded) = config.as_ref() else {
        return;
    };
    if loaded.config.scan.include.is_none() && loaded.config.scan.exclude.is_none() {
        return;
    }
    eprintln!(
        "warning: [scan] include/exclude pinned in {} are not yet consumed by the \
         issue #99 scoping engine; the scan runs unscoped",
        loaded.path.display()
    );
}

/// The config-pinned embedding model, if any.
pub(crate) fn config_embedding_model() -> Option<String> {
    cli_project_config().and_then(|loaded| loaded.config.embeddings.model)
}

/// Resolve the effective embedding model: explicit `--embed-model` >
/// `[embeddings].model` > built-in default (issue #261).
pub(crate) fn resolve_embed_model(flag: Option<String>) -> (String, ValueSource) {
    resolve_value(
        flag,
        config_embedding_model(),
        crate::project_config::default_embedding_model(),
    )
}

/// One `{value, source}` pair for the `eg config show` JSON contract.
fn valued<T: serde::Serialize>(value: T, source: ValueSource) -> serde_json::Value {
    serde_json::json!({"value": value, "source": source})
}

/// Print the effective resolved configuration as JSON (`eg config show`):
/// the discovered `egregore.toml` (or `null`), every value, and where each
/// value came from (`"config"` or `"default"`; `show` takes no flags, so
/// `"cli"` never appears here).
pub(crate) fn config_show() -> Result<()> {
    let loaded = cli_project_config();
    let config = loaded.as_ref().map(|l| &l.config);

    let (data_dir, data_dir_source) =
        resolve_opt(None::<PathBuf>, config.and_then(|c| c.data_dir.clone()));
    let data_dir = data_dir.map_or_else(
        crate::project_config::default_data_dir,
        std::convert::identity,
    );
    let (repo_id_override, repo_id_source) = resolve_opt(
        None::<String>,
        config.and_then(|c| c.repo_id_override.clone()),
    );
    let (model, model_source) = resolve_embed_model(None);
    let (include, include_source) = resolve_opt(
        None::<Vec<String>>,
        config.and_then(|c| c.scan.include.clone()),
    );
    let (exclude, exclude_source) = resolve_opt(
        None::<Vec<String>>,
        config.and_then(|c| c.scan.exclude.clone()),
    );
    let (transaction_time, transaction_time_source) = resolve_opt(
        None::<String>,
        config.and_then(|c| c.scan.transaction_time.clone()),
    );
    let (policy, policy_source) = resolve_value(
        None::<String>,
        config.and_then(|c| c.redaction.policy.clone()),
        crate::project_config::default_redaction_policy(),
    );
    let (raw_literals, raw_literals_source) = resolve_value(
        None::<bool>,
        config.and_then(|c| c.redaction.raw_literals),
        false,
    );

    let report = serde_json::json!({
        "config_file": loaded.as_ref().map(|l| l.path.to_string_lossy()),
        "data_dir": valued(data_dir.to_string_lossy(), data_dir_source),
        "repo_id_override": valued(repo_id_override, repo_id_source),
        "embeddings": {
            "model": valued(model, model_source),
        },
        "scan": {
            "include": valued(
                include.map_or_else(Vec::new, std::convert::identity),
                include_source,
            ),
            "exclude": valued(
                exclude.map_or_else(Vec::new, std::convert::identity),
                exclude_source,
            ),
            "transaction_time": valued(transaction_time, transaction_time_source),
        },
        "redaction": {
            "policy": valued(policy, policy_source),
            "raw_literals": valued(raw_literals, raw_literals_source),
        },
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("failed to serialize config report")?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config-pinned store answers only when the caller passed no
    /// explicit source: an explicit `--graph` is never shadowed by the
    /// config (and never misread as "both provided"), and an explicit
    /// `--data-dir` always wins.
    #[test]
    fn query_data_dir_falls_back_to_config_only_without_explicit_sources() {
        let config = || Some(PathBuf::from("cfg-store"));
        let flag = || Some(PathBuf::from("flag-store"));
        // No explicit source: the config-pinned store answers.
        assert_eq!(resolve_query_data_dir_with(None, None, config()), config());
        // An explicit --graph is never shadowed by the config store...
        assert_eq!(
            resolve_query_data_dir_with(Some(Path::new("g.jsonl")), None, config()),
            None
        );
        // ...and an explicit --data-dir always wins.
        assert_eq!(resolve_query_data_dir_with(None, flag(), config()), flag());
        assert_eq!(
            resolve_query_data_dir_with(Some(Path::new("g.jsonl")), flag(), config()),
            flag()
        );
        // No config either: unchanged downstream error-path behavior.
        assert_eq!(resolve_query_data_dir_with(None, None, None), None);
    }
}
