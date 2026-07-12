use super::*;

/// `eg query error-context <handle>` (issue #324): resolve an `ErrorSignature`
/// handle and emit one deterministic, trust-separated cross-domain envelope.
///
/// Exit codes: `0` on a resolved bundle (even when some sections are empty);
/// `1` on an ambiguous fingerprint prefix or a graph carrying a protected handle
/// while `--protected-store` is set; `2` on an unmatched handle (a `no_match`
/// envelope on stdout). Load errors surface via `?` (exit 2 through the loader).
#[allow(clippy::too_many_arguments)]
pub(crate) fn query_error_context_cmd(
    records: &[GraphRecord],
    handle: &str,
    repo_scope: Option<&str>,
    at_commit: Option<&str>,
    as_of: Option<&str>,
    supersession: crate::temporal_status::SupersessionMode,
    protected_store: Option<&Path>,
    embedded_source: bool,
) -> Result<()> {
    match query::error_context(
        records,
        handle,
        repo_scope,
        at_commit,
        as_of,
        supersession,
        protected_store,
        embedded_source,
    ) {
        Ok(ctx) => {
            #[derive(serde::Serialize)]
            struct Resp<'a> {
                ok: bool,
                #[serde(flatten)]
                ctx: &'a query::ErrorContext,
            }
            let output = serde_json::to_string(&Resp {
                ok: true,
                ctx: &ctx,
            })
            .context("failed to serialize error-context envelope")?;
            println!("{output}");
            Ok(())
        }
        Err(query::ErrorContextError::Ambiguous { candidates }) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": { "code": "ambiguous", "candidates": candidates },
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(1);
        }
        Err(query::ErrorContextError::NoMatch { handle }) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": { "code": "no_match", "handle": handle },
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(2);
        }
        Err(query::ErrorContextError::ProtectedHandleInGraph) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "protected_handle_in_graph",
                    "message": "the graph carries a protected handle; protected payloads are \
                                resolved only at read time and must never enter the graph",
                },
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(1);
        }
        Err(query::ErrorContextError::ProtectedStoreUnreadable { message }) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": { "code": "store_io_error", "detail": { "message": message } },
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(1);
        }
    }
}
