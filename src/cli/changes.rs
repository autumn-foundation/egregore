use super::*;

pub(crate) fn query_changes_cmd(
    records: &[GraphRecord],
    base: &str,
    head: &str,
    repo: Option<&str>,
) -> Result<()> {
    let index = query::RepositoryIndex::build(records);
    let repo_scope = resolve_repo_scope(&index, repo);
    match query::changes_context(records, base, head, repo_scope.as_deref()) {
        Ok(ctx) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct ChangesResponse<'a> {
                ok: bool,
                #[serde(flatten)]
                context: query::ChangesContext<'a>,
            }
            let response = ChangesResponse {
                ok: true,
                context: ctx,
            };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize changes context")?;
            println!("{output}");
            Ok(())
        }
        Err(err) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct ChangesErrorResponse {
                ok: bool,
                error: query::ChangesError,
            }
            let response = ChangesErrorResponse {
                ok: false,
                error: err.clone(),
            };
            let output =
                serde_json::to_string(&response).context("failed to serialize changes error")?;
            println!("{output}");
            let exit_code = match err {
                query::ChangesError::MissingCommit { .. } | query::ChangesError::EmptyHistory => 2,
                _ => 1,
            };
            std::process::exit(exit_code);
        }
    }
}
