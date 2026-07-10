use super::*;

use crate::log_resolve;

/// Honest-limit disclaimer stamped on every `resolve-frames` envelope.
pub(crate) const RESOLVE_FRAMES_DISCLAIMER: &str = "A frame binding proves that the backtrace frame NAMES the symbol; it is never proof that \
     the symbol is at fault, that it caused the error, or that it must change.";

/// Handles `eg resolve-frames <log_graph> --graph <code> --out <path>` (or the
/// `--data-dir` variant).
///
/// Resolves every structured backtrace frame on every `ErrorSignature` to a
/// code-graph target, writing the enriched log records (updated signatures, new
/// `Diagnostic` markers, and new `FRAME_RESOLVES_TO` edges) to `--out` as JSONL
/// and printing a deterministic summary envelope with the per-signature
/// external tally to stdout. Byte-identical across runs.
pub(crate) fn resolve_frames_cmd(
    log_graph: Option<&Path>,
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    out: &Path,
    at: Option<&str>,
    as_of: Option<&str>,
) -> Result<()> {
    // `--at` and `--as-of` key the same valid-time axis; combining them is an
    // unsupported workflow (mirrors the query verbs).
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

    // Load the union of code + log records.
    let records = match (log_graph, graph, data_dir) {
        (Some(log_path), Some(code_path), None) => {
            let mut recs = load_records_from_jsonl(code_path)?;
            recs.extend(load_records_from_jsonl(log_path)?);
            recs
        }
        (Some(log_path), None, None) => {
            // A log graph alone can still resolve name-only frames against any
            // code records it happens to contain, but the common case needs
            // --graph; accept it and let resolution proceed over the log set.
            load_records_from_jsonl(log_path)?
        }
        (None, None, Some(dir)) => {
            if at.is_some() || as_of.is_some() {
                load_query_records_history(None, Some(dir))?
            } else {
                load_query_records(None, Some(dir))?
            }
        }
        (_, _, Some(_)) => {
            anyhow::bail!("provide either a log graph path with --graph, or --data-dir, not both");
        }
        (None, Some(_), None) => {
            anyhow::bail!("--graph requires a positional log graph path (or use --data-dir)");
        }
        (None, None, None) => {
            anyhow::bail!("provide a log graph path with --graph, or --data-dir");
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

    let result = log_resolve::resolve_frames(&records, at_commit.as_deref());

    let graph_out = Graph::from_records(result.records);
    let jsonl = graph_out
        .to_jsonl()
        .context("failed to serialize resolved-frame JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write resolved-frame JSONL to {}", out.display()))?;

    let envelope = serde_json::json!({
        "ok": true,
        "command": "resolve-frames",
        "at_commit": at_commit,
        "as_of": as_of,
        "totals": result.totals,
        "signatures": result.signatures,
        "disclaimer": RESOLVE_FRAMES_DISCLAIMER,
    });
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}
