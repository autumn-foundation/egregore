use super::*;

/// `eg query log-deltas` (issue #326): classify runtime error-signatures across
/// a commit range into new / ceased / continuing groups.
///
/// Exit codes follow the `eg query deltas` (issue #118) convention exactly: `0`
/// on success (including a resolved range with no signatures in any class), `2`
/// when a commit handle resolves to nothing or the history is empty (no match),
/// and `1` for the remaining stable diagnostics (ambiguous prefix, identical
/// endpoints, reversed range, no ancestor path).
///
/// `embedded_source` is `true` when the records came from the embedded
/// (`--data-dir`) read path; it is threaded into [`query::log_deltas`] so the
/// response can disclose the embedded-store last-write-wins retention limitation
/// (issue #363) when log records are present. It never affects classification.
pub(crate) fn query_log_deltas_cmd(
    records: &[GraphRecord],
    base: &str,
    head: &str,
    repo: Option<&str>,
    embedded_source: bool,
) -> Result<()> {
    let index = query::RepositoryIndex::build(records);
    let repo_scope = resolve_repo_scope(&index, repo);
    match query::log_deltas(records, base, head, repo_scope.as_deref(), embedded_source) {
        Ok(deltas) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct LogDeltasResponse {
                ok: bool,
                #[serde(flatten)]
                deltas: query::LogDeltas,
            }
            let response = LogDeltasResponse { ok: true, deltas };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize log deltas")?;
            println!("{output}");
            Ok(())
        }
        Err(err) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct LogDeltasErrorResponse {
                ok: bool,
                error: query::RangeDeltasError,
            }
            let response = LogDeltasErrorResponse {
                ok: false,
                error: err.clone(),
            };
            let output =
                serde_json::to_string(&response).context("failed to serialize log-deltas error")?;
            println!("{output}");
            let exit_code = match err {
                query::RangeDeltasError::MissingCommit { .. }
                | query::RangeDeltasError::EmptyHistory => 2,
                _ => 1,
            };
            std::process::exit(exit_code);
        }
    }
}
