use super::*;

use crate::graph_index::{GraphIndex, index_path_for};

/// Handles `eg index <graph>` (issue #447): builds the persistent sidecar index
/// for a graph JSONL file and writes it atomically to `<graph>.idx`.
///
/// This is the ONLY writer of the sidecar index. The build streams the file
/// once and refuses (exit 2) any graph a cold load would also reject (a parse
/// error or unknown schema version), so a graph that cold-loads cleanly is the
/// only graph that gets an index. Query lanes never write it and transparently
/// cold-scan when it is absent or stale.
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
    index.write_atomic(&index_path).with_context(|| {
        format!(
            "failed to write sidecar index to {}",
            index_path.display()
        )
    })?;

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
