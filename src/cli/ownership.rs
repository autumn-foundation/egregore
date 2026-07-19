use super::*;

/// `eg query ownership` (issue #245): per-file authorship aggregates with a
/// primary owner and bus-factor signal.
///
/// Exit codes follow the `eg query deltas` convention: `0` on success
/// (including an explicit empty surface), `2` when nothing matches (unknown
/// path, missing commit, no commits at the queried time, empty history), and
/// `1` for the remaining stable diagnostics (ambiguous prefix, malformed
/// timestamp, invalid threshold or limit).
pub(crate) fn query_ownership_cmd(
    records: &[GraphRecord],
    options: &query::OwnershipOptions<'_>,
    format: OutputFormat,
) -> Result<()> {
    match query::ownership_map(records, options) {
        Ok(map) => {
            match format {
                OutputFormat::Json => {
                    #[derive(Debug, Clone, serde::Serialize)]
                    struct OwnershipResponse<'a> {
                        ok: bool,
                        #[serde(flatten)]
                        map: query::OwnershipMap<'a>,
                    }
                    let response = OwnershipResponse { ok: true, map };
                    let output = serde_json::to_string(&response)
                        .context("failed to serialize ownership map")?;
                    println!("{output}");
                }
                OutputFormat::Text => print_ownership_text(&map),
            }
            Ok(())
        }
        Err(err) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct OwnershipErrorResponse {
                ok: bool,
                error: query::OwnershipError,
            }
            let response = OwnershipErrorResponse {
                ok: false,
                error: err.clone(),
            };
            let output =
                serde_json::to_string(&response).context("failed to serialize ownership error")?;
            println!("{output}");
            let exit_code = match err {
                query::OwnershipError::EmptyHistory
                | query::OwnershipError::MissingCommit { .. }
                | query::OwnershipError::NoCommitsAtTime { .. }
                | query::OwnershipError::UnknownPath { .. } => 2,
                _ => 1,
            };
            std::process::exit(exit_code);
        }
    }
}

/// Human-readable one-file-per-block form of an ownership map.
pub(crate) fn print_ownership_text(map: &query::OwnershipMap<'_>) {
    let anchors = map
        .anchors
        .iter()
        .map(|a| a.commit_sha)
        .collect::<Vec<_>>()
        .join(", ");
    let truncated = if map.truncated { " (truncated)" } else { "" };
    println!(
        "ownership: {} of {} files, threshold {}%, anchors [{}]{}",
        map.returned_file_count, map.total_file_count, map.threshold_percent, anchors, truncated
    );
    println!("note: {}", map.disclaimer);
    println!("corpus: {}", map.corpus_mode);
    for row in &map.files {
        println!(
            "{} bus_factor={} total_commits={} primary={} share={:.4} ({})",
            row.repo_relative_path,
            row.bus_factor,
            row.total_commits,
            ownership_author_identity(&row.primary_owner),
            row.primary_owner.share,
            row.record_id
        );
        for author in &row.authors {
            println!(
                "  author {} commits={} share={:.4}",
                ownership_author_identity(author),
                author.commits,
                author.share
            );
        }
    }
    for diagnostic in &map.diagnostics {
        println!("diagnostic: {} {}", diagnostic.code, diagnostic.detail);
    }
}

/// Bounded display identity for an ownership author row.
pub(crate) fn ownership_author_identity(author: &query::OwnershipAuthor<'_>) -> String {
    match (author.author_name, author.author_email) {
        (Some(name), Some(email)) => format!("{name} <{email}>"),
        (Some(name), None) => name.to_owned(),
        (None, Some(email)) => format!("<{email}>"),
        (None, None) => "(unrecorded author)".to_owned(),
    }
}
