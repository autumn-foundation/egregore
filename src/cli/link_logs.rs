use super::*;

use crate::log_link;

/// Honest-limit disclaimer stamped on every `link-logs` envelope.
pub(crate) const LINK_LOGS_DISCLAIMER: &str = "An EMITTED_DURING edge is a correlation lead, never causation. `content_hash_join` proves the \
     log artifact IS the command's captured output (exact byte equality); `temporal_correlation` \
     means only that the signature's time falls inside a run window — overlapping in time is never \
     proof the run produced the error.";

/// Handles `eg link-logs --graph <graph.jsonl>... --out <path>` (or the
/// `--data-dir` variant).
///
/// Links every `ErrorSignature` to the agent runs / commands that produced it,
/// emitting `EMITTED_DURING` edges (each carrying a closed-set
/// [`crate::ir::CorrelationBasis`]) plus `REFERENCES_TASK` task links, mirrored
/// by evidence links on each signature. Writes the enriched log records to
/// `--out` as JSONL and prints a deterministic summary envelope. Byte-identical
/// across runs; raw log / transcript / command text never enters the graph.
pub(crate) fn link_logs_cmd(
    graphs: &[PathBuf],
    data_dir: Option<&Path>,
    out: &Path,
    tolerance: i64,
    at: Option<&str>,
    as_of: Option<&str>,
) -> Result<()> {
    // `--at` and `--as-of` key the same valid-time axis; combining them is an
    // unsupported workflow (mirrors the query verbs and resolve-frames).
    if at.is_some() && as_of.is_some() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "unsupported_combination",
                "message": "--at cannot be combined with --as-of; pass at most one temporal pin",
            },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(1);
    }

    if tolerance < 0 {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "invalid_tolerance",
                "message": "--tolerance must be a non-negative number of seconds",
            },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(1);
    }

    // Load the union of all --graph files (+ optional --data-dir).
    let records = match (graphs.is_empty(), data_dir) {
        (false, None) => {
            let mut recs = Vec::new();
            for g in graphs {
                recs.extend(load_records_from_jsonl(g)?);
            }
            recs
        }
        (true, Some(dir)) => {
            if at.is_some() || as_of.is_some() {
                load_query_records_history(None, Some(dir))?
            } else {
                load_query_records(None, Some(dir))?
            }
        }
        (false, Some(_)) => {
            anyhow::bail!("provide either --graph paths or --data-dir, not both");
        }
        (true, None) => {
            anyhow::bail!("provide at least one --graph <path>, or --data-dir");
        }
    };

    // Resolve the temporal pin, if any, against the code-graph Commit records.
    let at_commit = if at.is_some() || as_of.is_some() {
        let index = query::RepositoryIndex::build(&records);
        Some(resolve_transitive_commit_view(
            &records, &index, None, at, as_of,
        )?)
    } else {
        None
    };

    let result = log_link::link_logs(
        &records,
        &log_link::LinkLogsOptions {
            tolerance_seconds: tolerance,
            at_commit: at_commit.clone(),
        },
    );

    let graph_out = Graph::from_records(result.records);
    let jsonl = graph_out
        .to_jsonl()
        .context("failed to serialize link-logs JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write link-logs JSONL to {}", out.display()))?;

    let envelope = serde_json::json!({
        "ok": true,
        "command": "link-logs",
        "at_commit": at_commit,
        "as_of": as_of,
        "totals": result.totals,
        "signatures": result.signatures,
        "disclaimer": LINK_LOGS_DISCLAIMER,
    });
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}
