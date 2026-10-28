use super::*;

use crate::graph_index::{GraphIndex, index_path_for};

/// Builds the sidecar index for `graph` and writes it atomically to
/// `<graph>.idx`, returning the built index.
///
/// This is the one build+write path every sidecar writer shares: the
/// explicit `eg index` command and the automatic producer-side builds in
/// `eg scan`, `eg scan-history`, and `eg ingest`. The build streams the
/// file once and refuses any graph a cold load would also reject (a parse
/// error or unknown schema version), so a graph that cold-loads cleanly is
/// the only graph that gets an index. Query lanes never write the index
/// and transparently cold-scan when it is absent or stale.
pub(crate) fn build_and_write_sidecar_index(graph: &Path) -> Result<GraphIndex> {
    let index = GraphIndex::build(graph)?;
    let index_path = index_path_for(graph);
    index
        .write_atomic(&index_path)
        .with_context(|| format!("failed to write sidecar index to {}", index_path.display()))?;
    Ok(index)
}

/// Best-effort automatic sidecar build for the producer commands
/// (`eg scan`, `eg scan-history`, `eg ingest`).
///
/// The index is a pure access-path optimization — it never changes a query
/// answer — so a build failure must never fail a scan or ingest that
/// otherwise succeeded (for `eg ingest` the store may already be
/// persisted; reporting failure there would invite a misleading retry).
/// Instead the failure is loud but non-fatal: a single-line warning on
/// stderr naming the remedy (`eg index`). On the success path this prints
/// nothing, preserving the producers' existing stdout/stderr contracts.
pub(crate) fn auto_build_sidecar_index(graph: &Path) {
    if let Err(error) = build_and_write_sidecar_index(graph) {
        eprintln!(
            "warning: failed to build sidecar index for {}: {error:#}; \
             targeted --graph queries will cold-scan until it is built \
             (run `eg index {}` to retry)",
            graph.display(),
            graph.display()
        );
    }
}

/// [`auto_build_sidecar_index`] unless the producer was asked to skip the
/// sidecar (`--no-index`): the one-line call producers use at their write
/// sites.
pub(crate) fn auto_build_sidecar_index_unless(graph: &Path, no_index: bool) {
    if !no_index {
        auto_build_sidecar_index(graph);
    }
}

/// Handles `eg index <graph>` (issue #447): builds the persistent sidecar index
/// for a graph JSONL file and writes it atomically to `<graph>.idx`.
///
/// The explicit command: a build failure here IS the command's failure, so
/// an unindexable graph refuses with exit 2 (mirroring the cold load) and a
/// write failure propagates as a normal error. The automatic producer-side
/// builds (`eg scan`, `eg scan-history`, `eg ingest`) share the same
/// build+write path via [`build_and_write_sidecar_index`] but downgrade a
/// failure to a warning — see [`auto_build_sidecar_index`].
///
/// Prints a deterministic one-line confirmation of the index key counts.
pub(crate) fn index_cmd(graph: &Path) -> Result<()> {
    let index = match GraphIndex::build(graph) {
        Ok(index) => index,
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "code": "index_build_error",
                    "path": graph.display().to_string(),
                    "message": error.to_string(),
                })
            );
            std::process::exit(2);
        }
    };

    let index_path = index_path_for(graph);
    index
        .write_atomic(&index_path)
        .with_context(|| format!("failed to write sidecar index to {}", index_path.display()))?;

    let summary = serde_json::json!({
        "ok": true,
        "index_path": index_path.display().to_string(),
        "graph_len": index.graph_len,
        "ids": index.body.by_id.len(),
        "deleted_ids": index.body.by_deleted_id.len(),
        "names": index.body.by_name.len(),
        "paths": index.body.by_path.len(),
        "kinds": index.body.by_kind.len(),
        "adjacency_nodes": index.body.adjacency.len(),
    });
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}
