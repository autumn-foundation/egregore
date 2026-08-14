use super::*;

// ---------------------------------------------------------------------------
// query symbol --as-of <instant>
// ---------------------------------------------------------------------------

/// Indexes, per record ID, the position of the `symbol_name` record that is part
/// of the snapshot at `instant`.
///
/// Two steps, and BOTH are needed before a package filter may run:
///
/// 1. Each IDENTITY collapses to its newest `valid_time` at or before the
///    instant. This mirrors `query::symbol_as_of_valid_time_by_repo`'s own rule
///    (strictly-greater wins, so an exact tie keeps the earlier position)
///    restricted to one identity, so it can never disagree with the winner that
///    function would pick. It is what makes a manifest RENAME correct: the ID is
///    stable, so there is one identity, and its current version carries the new
///    package.
/// 2. Identities absent from the snapshot are DROPPED. A file MOVE changes the
///    stable ID (the path is part of the ADR-0004 preimage), so the old and new
///    locations are distinct identities; step 1 alone would keep the old one's
///    final version alive forever, and a package filter that removes the current
///    row would then let that stale row win — answering from a file that no
///    longer exists. History replay re-emits every live record at every commit,
///    so an identity present in the snapshot carries that snapshot's
///    `valid_time`; one whose newest version is older was not there.
///
/// The snapshot instant is computed PER REPOSITORY, grouped exactly as
/// `symbol_as_of_valid_time_by_repo` groups (including the shared unattributed
/// group), so one repository's commit timeline never evicts another's current
/// records.
///
/// Deletion semantics are deliberately unchanged: a symbol removed outright
/// still resolves to its last recorded version, exactly as the unscoped lane
/// resolves it. This function only makes the SCOPED answer agree with the
/// snapshot the unscoped lane would already have chosen.
fn current_symbol_versions_at<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    instant: chrono::DateTime<chrono::FixedOffset>,
    index: &query::RepositoryIndex,
) -> std::collections::HashMap<&'records str, usize> {
    let mut best: std::collections::HashMap<&str, (usize, chrono::DateTime<chrono::FixedOffset>)> =
        std::collections::HashMap::new();
    for (position, record) in records.iter().enumerate() {
        let GraphRecord::Node {
            kind: NodeKind::Symbol,
            id,
            name,
            temporal,
            valid_time,
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        let Some(parsed) = temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref())
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        else {
            continue;
        };
        if parsed > instant {
            continue;
        }
        if best
            .get(id.as_str())
            .is_none_or(|(_, previous)| parsed > *previous)
        {
            best.insert(id.as_str(), (position, parsed));
        }
    }
    // Step 2: the snapshot instant per repository — the newest version any
    // identity of this name reached there — then keep only the identities that
    // actually reach it.
    let mut snapshot: std::collections::HashMap<
        Option<&str>,
        chrono::DateTime<chrono::FixedOffset>,
    > = std::collections::HashMap::new();
    for (id, (_, parsed)) in &best {
        let owner = index.owner_of(id);
        snapshot
            .entry(owner)
            .and_modify(|latest| {
                if *parsed > *latest {
                    *latest = *parsed;
                }
            })
            .or_insert(*parsed);
    }
    best.into_iter()
        .filter(|(id, (_, parsed))| snapshot.get(&index.owner_of(id)) == Some(parsed))
        .map(|(id, (position, _))| (id, position))
        .collect()
}

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
    //
    // But the SNAPSHOT is resolved first, per symbol identity. Removing a
    // record from `symbol_as_of_valid_time_by_repo`'s input can PROMOTE another
    // one — it picks a single winner per repository across every record
    // carrying the queried name — so when a symbol keeps its stable ID while
    // its enclosing manifest is renamed, a bare package filter deletes the
    // current version and lets the SUPERSEDED one win, asserting ownership that
    // ended before the queried instant. Collapsing each identity to the version
    // current AT the instant first makes the two concerns independent: the
    // instant decides WHICH version, and the selector then decides whether that
    // version's owner matches.
    let as_of_instant = chrono::DateTime::parse_from_rfc3339(as_of).ok();
    let scoped_records: Option<Vec<GraphRecord>> =
        package.zip(as_of_instant).map(|(selector, instant)| {
            let current = current_symbol_versions_at(records, name, instant, index);
            records
                .iter()
                .enumerate()
                .filter(|(position, record)| {
                    let GraphRecord::Node {
                        kind: NodeKind::Symbol,
                        name: node_name,
                        ..
                    } = record
                    else {
                        return true;
                    };
                    // A same-named symbol survives only as the version current
                    // at the instant; every other version of that identity is
                    // out of view and must not be promoted by the filter.
                    if node_name.as_deref() == Some(name)
                        && current.get(record.id()) != Some(position)
                    {
                        return false;
                    }
                    record
                        .crate_attribution()
                        .and_then(super::CrateAttributionExt::owning_package_name)
                        == Some(selector)
                })
                .map(|(_, record)| record.clone())
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
