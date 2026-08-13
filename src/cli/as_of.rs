use super::*;

// ---------------------------------------------------------------------------
// query symbol --as-of <instant>
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(crate) fn query_symbol_as_of(
    records: &[GraphRecord],
    name: &str,
    as_of: &str,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
    package: Option<&str>,
    freshness_code: Option<&(String, &'static str)>,
) -> Result<()> {
    // Package scope narrows the candidate records BEFORE the one-best-per-repo
    // selection, for the same reason as the `--at` lane (issue #117): filtering
    // afterwards would report "no match" for a symbol that exists in the
    // requested package but lost the per-repository pick to a sibling.
    let scoped_records: Option<Vec<GraphRecord>> = package.map(|selector| {
        records
            .iter()
            .filter(|record| {
                !matches!(
                    record,
                    GraphRecord::Node {
                        kind: NodeKind::Symbol,
                        ..
                    }
                ) || record
                    .crate_attribution()
                    .and_then(super::CrateAttributionExt::owning_package_name)
                    == Some(selector)
            })
            .cloned()
            .collect()
    });
    let records = scoped_records.as_deref().unwrap_or(records);
    match query::symbol_as_of_valid_time_by_repo(records, name, as_of, index, selected_repo) {
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
        Ok(results) if results.is_empty() => {
            eprintln!("error: no match found for symbol `{name}` at or before `{as_of}`");
            std::process::exit(2);
        }
        Ok(results) => {
            // One best record per repository (plus one for any unattributed
            // legacy group): a single-result time view must never pick one
            // group implicitly on a collision (issue #67).
            if selected_repo.is_none() {
                let groups: std::collections::BTreeSet<Option<&str>> =
                    results.iter().map(|r| index.owner_of(r.id())).collect();
                if groups.len() > 1 {
                    exit_ambiguous_repository(&groups);
                }
            }
            let deleted = current_deleted_ids(records);
            let mut symbol_results: Vec<SymbolResult<'_>> = results
                .iter()
                .filter_map(|r| symbol_result(r, name, index, records, &deleted))
                .collect();
            // Package scope (issue #117), applied to the recorded attribution AT
            // the resolved instant.
            retain_package_scope(&mut symbol_results, package);
            if symbol_results.is_empty() {
                eprintln!("error: no match found for symbol `{name}` at or before `{as_of}`");
                std::process::exit(2);
            }
            stamp_freshness(&mut symbol_results, freshness_code);
            // `--as-of` pins a single valid-time instant: the corpus is
            // commit-pinned, chosen by the selector (issue #427).
            stamp_symbol_corpus(
                &mut symbol_results,
                query::CorpusMode::CommitPinned,
                query::CorpusModeSource::Selector,
            );
            for result in &symbol_results {
                print_result(result, format)?;
            }
        }
    }
    Ok(())
}
