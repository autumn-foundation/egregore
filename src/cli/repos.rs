use super::*;

/// Runs `eg repos` (issue #193): catalog a store's repositories with the
/// selectors that scope #67 repository-scoped queries.
///
/// Reads an embedded `--data-dir` store directly — no daemon, no network, no
/// embeddings — or a `--graph` JSONL file, and lists every live repository
/// with its stable record ID, its `identity_source`, the human-usable scope
/// selector the `--repo` query lanes accept verbatim, a node count, and a
/// recency hint.
///
/// Strictly read-only: the embedded read goes through a throwaway store copy,
/// so the original is never re-persisted or otherwise mutated. Re-running
/// against an unchanged store is byte-identical (canonical ordering, no
/// catalog-time timestamps).
///
/// Output is newline-delimited JSON (one repository object per line) by
/// default, with a `--format text` human-readable mode. An empty store reports
/// an explicit empty roster with the stable `empty_repository_roster`
/// diagnostic; a missing or unreadable `--data-dir` fails with an
/// operator-facing diagnostic naming the path. Neither is silent success.
/// See `docs/cli/repos.md` for the documented field shape.
pub(crate) fn repos_cmd(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    let records = match (graph, data_dir) {
        (Some(path), None) => load_records_from_jsonl(path)?,
        // The throwaway-copy read keeps the strict read-only guarantee:
        // opening the embedded engine in place re-persists its on-disk
        // index files, so the original store is never touched.
        (None, Some(dir)) => load_records_from_data_dir_readonly(dir)
            .with_context(|| format!("failed to read embedded store {}", dir.display()))?,
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => match config_data_dir() {
            Some(dir) => load_records_from_data_dir_readonly(&dir)
                .with_context(|| format!("failed to read embedded store {}", dir.display()))?,
            None => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
        },
    };
    let catalog = query::RepositoryIndex::build(&records).catalog();
    match format {
        OutputFormat::Json => {
            if catalog.is_empty() {
                println!(
                    "{}",
                    serde_json::json!({
                        "repositories": [],
                        "diagnostic": {
                            "code": "empty_repository_roster",
                            "message": "the store contains no live repositories",
                        },
                    })
                );
            } else {
                for entry in &catalog {
                    println!("{}", serde_json::to_string(entry)?);
                }
            }
        }
        OutputFormat::Text => print_repos_text(&catalog),
    }
    Ok(())
}

/// Human-readable `eg repos --format text` rendering, in the existing `eg`
/// style (`repository:` lines with indented detail rows).
fn print_repos_text(catalog: &[query::RepositoryCatalogEntry]) {
    if catalog.is_empty() {
        println!("repositories: none");
        println!("diagnostic: empty_repository_roster (the store contains no live repositories)");
        return;
    }
    for entry in catalog {
        println!("repository: {}", entry.repository_id);
        println!(
            "  identity_source: {}",
            entry
                .identity_source
                .as_ref()
                .map_or("unknown", crate::ir::IdentitySource::as_str)
        );
        println!("  selector: {}", entry.selector);
        if entry.selector_collision {
            println!(
                "  selector_collision: true (human selector '{}' is shared; use the stable repository ID to scope queries)",
                entry.colliding_selector.as_deref().unwrap_or("?")
            );
        }
        println!("  nodes: {}", entry.node_count);
        match (&entry.recency_kind, &entry.recency_handle) {
            (Some(kind), Some(handle)) => {
                println!("  recency: {} {handle}", kind.as_str());
            }
            _ => println!("  recency: none"),
        }
    }
}
