use super::*;

/// `eg query evidence-path <source_id> <target_id>` (issue #247): trace the
/// deterministic shortest cross-domain evidence witness path between two record
/// handles over the evidence-edge subgraph, or emit an explicit `no_path`
/// verdict.
///
/// Exit codes: `0` on a witness path (>= 1 hop); `1` on identical endpoints or a
/// `no_path` verdict between two live endpoints; `2` when an endpoint is absent
/// (`endpoint_not_found`) or tombstoned (`endpoint_tombstoned`). Load errors and
/// both/neither transport flag surface via the loader (`?`).
pub(crate) fn query_evidence_path_cmd(
    records: &[GraphRecord],
    source_id: &str,
    target_id: &str,
    format: OutputFormat,
) -> Result<()> {
    // Disclosure-only (issue #427): evidence-path is a current-state view over
    // the live evidence-edge subgraph, but over a scan-history store it walks the
    // UNION of all commit snapshots (no head anchoring; temporal versions are not
    // pinned to a HEAD). Disclose that honestly.
    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        query::disclose_corpus(records, query::CorpusMode::Union);
    let corpus = Corpus {
        mode: corpus_mode.as_str(),
        source: corpus_mode_source.as_str(),
        disclaimer: corpus_disclaimer,
    };

    match query::evidence_path(records, source_id, target_id) {
        Ok(path) => {
            match format {
                OutputFormat::Json => print_path_json(&path, &corpus)?,
                OutputFormat::Text => print_path_text(&path, &corpus),
            }
            Ok(())
        }
        Err(err) => {
            match format {
                OutputFormat::Json => print_error_json(&err, &corpus)?,
                OutputFormat::Text => print_error_text(&err),
            }
            std::process::exit(exit_code_for(&err));
        }
    }
}

/// Corpus-disclosure fields (issue #427) threaded onto every evidence-path
/// envelope.
struct Corpus {
    mode: &'static str,
    source: &'static str,
    disclaimer: String,
}

/// Maps a failure mode to its CLI exit code.
const fn exit_code_for(err: &query::EvidencePathError) -> i32 {
    match err {
        query::EvidencePathError::IdenticalEndpoints { .. }
        | query::EvidencePathError::NoPath { .. } => 1,
        query::EvidencePathError::EndpointNotFound { .. }
        | query::EvidencePathError::EndpointTombstoned { .. } => 2,
    }
}

/// Success envelope: a summary line followed by one hop per line (NDJSON).
fn print_path_json(path: &query::EvidencePath, corpus: &Corpus) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Summary<'a> {
        ok: bool,
        source: &'a query::NodeRow,
        target: &'a query::NodeRow,
        hop_count: usize,
        traversed_edge_classes: &'a [String],
        excluded_edge_classes: &'a [String],
        disclaimer: &'static str,
        corpus_mode: &'a str,
        corpus_mode_source: &'a str,
        corpus_disclaimer: &'a str,
    }
    let summary = Summary {
        ok: true,
        source: &path.source,
        target: &path.target,
        hop_count: path.hops.len(),
        traversed_edge_classes: &path.traversed_edge_classes,
        excluded_edge_classes: &path.excluded_edge_classes,
        disclaimer: query::EVIDENCE_PATH_DISCLAIMER,
        corpus_mode: corpus.mode,
        corpus_mode_source: corpus.source,
        corpus_disclaimer: &corpus.disclaimer,
    };
    println!(
        "{}",
        serde_json::to_string(&summary).context("failed to serialize evidence-path summary")?
    );
    for hop in &path.hops {
        println!(
            "{}",
            serde_json::to_string(hop).context("failed to serialize evidence-path hop")?
        );
    }
    Ok(())
}

/// Error envelope: a single JSON line.
fn print_error_json(err: &query::EvidencePathError, corpus: &Corpus) -> Result<()> {
    let error = match err {
        query::EvidencePathError::IdenticalEndpoints { handle } => serde_json::json!({
            "error_type": "identical_endpoints",
            "handle": handle,
            "disclaimer": query::EVIDENCE_PATH_DISCLAIMER,
        }),
        query::EvidencePathError::NoPath {
            source,
            target,
            traversed_edge_classes,
            excluded_edge_classes,
        } => serde_json::json!({
            "error_type": "no_path",
            "source": source,
            "target": target,
            "traversed_edge_classes": traversed_edge_classes,
            "excluded_edge_classes": excluded_edge_classes,
            "disclaimer": query::EVIDENCE_PATH_DISCLAIMER,
            "corpus_mode": corpus.mode,
            "corpus_mode_source": corpus.source,
            "corpus_disclaimer": corpus.disclaimer,
        }),
        query::EvidencePathError::EndpointNotFound { side, handle } => serde_json::json!({
            "error_type": "endpoint_not_found",
            "side": side.as_str(),
            "handle": handle,
            "disclaimer": query::EVIDENCE_PATH_DISCLAIMER,
        }),
        query::EvidencePathError::EndpointTombstoned { side, handle } => serde_json::json!({
            "error_type": "endpoint_tombstoned",
            "side": side.as_str(),
            "handle": handle,
            "disclaimer": query::EVIDENCE_PATH_DISCLAIMER,
        }),
    };
    let envelope = serde_json::json!({ "ok": false, "error": error });
    println!(
        "{}",
        serde_json::to_string(&envelope).context("failed to serialize evidence-path error")?
    );
    Ok(())
}

/// Compact human rendering: header, one line per hop, footer.
fn print_path_text(path: &query::EvidencePath, corpus: &Corpus) {
    println!(
        "evidence-path: {} [{}/{}]  ->  {} [{}/{}]",
        path.source.record_id,
        path.source.domain,
        path.source.kind,
        path.target.record_id,
        path.target.domain,
        path.target.kind,
    );
    for hop in &path.hops {
        println!(
            "{}  {} [{}/{}] --{}({})--> {} [{}/{}]",
            hop.index,
            hop.from.record_id,
            hop.from.domain,
            hop.from.kind,
            hop.edge.label,
            hop.edge.traversal_direction,
            hop.to.record_id,
            hop.to.domain,
            hop.to.kind,
        );
    }
    println!(
        "hops: {}  traversed classes: {}  excluded classes: {}",
        path.hops.len(),
        path.traversed_edge_classes.len(),
        path.excluded_edge_classes.len(),
    );
    println!("corpus: {}", corpus.mode);
    println!("{}", query::EVIDENCE_PATH_DISCLAIMER);
}

/// One-line human error message.
fn print_error_text(err: &query::EvidencePathError) {
    match err {
        query::EvidencePathError::IdenticalEndpoints { handle } => {
            println!("identical_endpoints: source and target are the same handle ({handle})");
        }
        query::EvidencePathError::NoPath { source, target, .. } => {
            println!(
                "no_path: no evidence-edge chain connects {} and {} (excluded classes are not traversed by design)",
                source.record_id, target.record_id
            );
        }
        query::EvidencePathError::EndpointNotFound { side, handle } => {
            println!(
                "endpoint_not_found: {} handle names no record ({handle})",
                side.as_str()
            );
        }
        query::EvidencePathError::EndpointTombstoned { side, handle } => {
            println!(
                "endpoint_tombstoned: {} handle names a retracted record ({handle})",
                side.as_str()
            );
        }
    }
}
