use super::*;

/// `eg query deltas` (issue #118): symbol- and file-level deltas between two
/// commit handles, grouped by stable change class.
///
/// Exit codes follow the `eg query changes` convention: `0` on success
/// (including a resolved range with no deltas), `2` when a commit handle
/// resolves to nothing or the history is empty (no match), and `1` for the
/// remaining stable diagnostics (ambiguous prefix, identical endpoints,
/// reversed range, no ancestor path).
pub(crate) fn query_deltas_cmd(
    records: &[GraphRecord],
    base: &str,
    head: &str,
    repo: Option<&str>,
) -> Result<()> {
    let index = query::RepositoryIndex::build(records);
    let repo_scope = resolve_repo_scope(&index, repo);
    match query::range_deltas(records, base, head, repo_scope.as_deref()) {
        Ok(deltas) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct DeltasResponse<'a> {
                ok: bool,
                #[serde(flatten)]
                deltas: query::RangeDeltas<'a>,
            }
            let response = DeltasResponse { ok: true, deltas };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize range deltas")?;
            println!("{output}");
            Ok(())
        }
        Err(err) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct DeltasErrorResponse {
                ok: bool,
                error: query::RangeDeltasError,
            }
            let response = DeltasErrorResponse {
                ok: false,
                error: err.clone(),
            };
            let output = serde_json::to_string(&response)
                .context("failed to serialize range-deltas error")?;
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
