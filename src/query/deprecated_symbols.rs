use std::collections::{BTreeMap, BTreeSet};

use super::RepositoryIndex;
use super::liveness::Liveness;
use crate::ir::{CallResolution, EdgeLabel, GraphRecord, NodeKind, SourceSpan};

// ---------------------------------------------------------------------------
// deprecated-symbol inventory with the migration worklist (issue #249)
// ---------------------------------------------------------------------------

/// One still-resolvable call site of a deprecated symbol: a migration
/// worklist row.
///
/// The row asserts only that the graph recorded a `CALLS` edge from the
/// caller to the deprecated symbol — never what the call means or what
/// should replace it.
#[derive(Debug, Clone)]
pub struct DeprecatedCallSite<'a> {
    /// Stable record ID of the `CALLS` edge — the citable call-site handle.
    pub call_edge_record_id: &'a str,
    /// Stable record ID of the calling `Symbol` node, when the caller
    /// record is present in the graph.
    pub caller_record_id: Option<&'a str>,
    /// Caller symbol name (qualified where the extractor qualifies it).
    pub caller_name: Option<&'a str>,
    /// Language-specific caller kind (`function`, …).
    pub caller_kind: Option<&'a str>,
    /// Repo-relative file of the caller declaration.
    pub caller_repo_relative_path: Option<&'a str>,
    /// Source span of the caller declaration.
    pub caller_span: Option<SourceSpan>,
    /// Resolution label the graph recorded for the edge: `resolved`,
    /// `ambiguous` (the call may target this deprecated symbol — every
    /// candidate target carries an edge), or `unrecorded` (the edge carried
    /// no resolution status).
    pub resolution: &'static str,
}

/// One deprecated symbol with its migration worklist.
///
/// Code facts only: record IDs, paths, spans, counts, and the verbatim
/// bounded `since` / `note` payloads. No replacement suggestions, no
/// synthesized prose.
#[derive(Debug, Clone)]
pub struct DeprecatedSymbol<'a> {
    /// Stable record ID of the `Symbol` node.
    pub record_id: &'a str,
    /// Record schema version.
    pub schema_version: u32,
    /// Symbol name (qualified where the extractor qualifies it).
    pub name: &'a str,
    /// Language-specific symbol kind (`function`, `struct`, …); `symbol`
    /// for records without a recorded kind.
    pub kind: &'a str,
    /// Repo-relative file of the declaration.
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the declaration.
    pub span: Option<SourceSpan>,
    /// Introducing commit for temporal (history-backed) records.
    pub git_commit: Option<&'a str>,
    /// Verbatim `since` payload of the `#[deprecated]` attribute; `None`
    /// when the attribute did not carry one — the documented absent value,
    /// never fabricated text.
    pub since: Option<&'a str>,
    /// Verbatim `note` payload; `None` when absent — never fabricated text.
    pub note: Option<&'a str>,
    /// Still-resolvable call sites — the migration worklist — sorted by
    /// (`caller_repo_relative_path`, caller span start line,
    /// `call_edge_record_id`).
    pub call_sites: Vec<DeprecatedCallSite<'a>>,
}

/// Deterministic tallies for the deprecated-symbol result.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct DeprecatedCounts {
    /// Live, in-scope `Symbol` records considered.
    pub symbols_considered: usize,
    /// Considered symbols carrying a deprecation mark.
    pub deprecated_symbols: usize,
    /// `CALLS` edges of a recorded resolution targeting a deprecated symbol.
    pub resolved_call_sites: usize,
    /// `CALLS` edges with no resolvable in-repo target (issue #152) —
    /// reported under coverage, never silently dropped and never counted as
    /// zero.
    pub unresolved_call_edges: usize,
}

/// A stable machine-readable condition attached to the result.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeprecatedDiagnostic {
    /// Stable diagnostic code (`no_deprecated_symbols`, `no_code_symbols`,
    /// `unresolved_call_edges_present`).
    pub code: &'static str,
    /// Record the diagnostic is about, when one exists.
    pub record_id: Option<String>,
    /// Bounded human-readable detail (counts and IDs only — never payload).
    pub detail: String,
}

/// The deprecated-symbol inventory plus tallies, diagnostics, and the
/// unresolved-call coverage list.
#[derive(Debug, Clone, Default)]
pub struct DeprecatedSymbols<'a> {
    /// Rows sorted by (`repo_relative_path`, `span.start_line`,
    /// `record_id`) — the documented deterministic ordering.
    pub symbols: Vec<DeprecatedSymbol<'a>>,
    /// Deterministic tallies.
    pub counts: DeprecatedCounts,
    /// Stable diagnostics, sorted and de-duplicated.
    pub diagnostics: Vec<DeprecatedDiagnostic>,
    /// Stable record IDs of the `CALLS` edges the graph could not resolve to
    /// an in-repo target, sorted for determinism. Non-empty exactly when
    /// `counts.unresolved_call_edges > 0`: every unresolved edge is cited —
    /// none is silently dropped and the count is never reported as zero.
    pub unresolved_call_edge_ids: Vec<String>,
}

/// Selects code symbols carrying a `#[deprecated]` attribute mark and, for
/// each, its still-resolvable call sites over the existing `CALLS` edge
/// vocabulary as a migration worklist (issue #249).
///
/// A live `Symbol` node is selected when it carries a
/// [`crate::ir::DeprecationMark`] — the mark's *presence* is the fact, so a
/// bare `#[deprecated]` selects with both payloads absent. Absent `since` /
/// `note` are never synthesized from elsewhere.
///
/// `file_filter`, when `Some`, restricts the symbol inventory (and the
/// `symbols_considered` tally) to declarations in that repo-relative file.
/// The call-site worklists of the selected symbols stay complete — they
/// cite callers wherever they are — because the worklist answers "who still
/// calls this symbol", not "what is declared in this file".
///
/// Current-state view: tombstoned symbols are excluded, and when a stable ID
/// appears more than once (history graphs) the latest record wins
/// deterministically — the same liveness, scoping, and head-anchoring
/// machinery as the sibling triage lanes. Only `CALLS` edges count toward
/// the worklist: `Resolved` and `Ambiguous` edges whose target is a selected
/// symbol (an ambiguous call *might* target it, so it is never dropped),
/// attributed through the caller record. `Unresolved` / `UnresolvedDispatch`
/// edges target a `Diagnostic` marker, not a symbol — they cannot be
/// attributed to any deprecated symbol, so they are reported as a coverage
/// list instead: every edge cited by record ID, counted honestly.
///
/// Deterministic: output ordering depends only on record content, never on
/// map iteration or wall-clock time. Strictly read-only.
#[must_use]
pub fn deprecated_symbols<'a>(
    records: &'a [GraphRecord],
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
    file_filter: Option<&str>,
) -> DeprecatedSymbols<'a> {
    // Latest-write-wins liveness (issues #421/#432): over an append-only
    // `--graph` a node OR reference edge re-ingested AFTER its own tombstone is
    // live again, matching the embedded `--data-dir` current-state read. See
    // `super::liveness` and the sibling unreferenced lane.
    let liveness = Liveness::new(records);
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .filter(|&id| liveness.deleted(id))
        .collect();
    let is_owned =
        |id: &str| -> bool { repo_scope.is_none_or(|scope| index.owner_of(id) == Some(scope)) };
    // Head-anchor the current-state view (issues #82/#427): history replay
    // re-emits the full graph at every commit, so a deprecation mark or call
    // edge absent at HEAD must not shape the answer.
    let non_head_current = super::non_head_current_record_ids(records, index);

    // Live, in-scope, HEAD-current Symbol nodes, keep-last dedupe by stable
    // ID so history graphs resolve to their newest version. Collection is
    // NOT file-filtered: worklist attribution resolves callers wherever
    // they are; the `--file` filter applies to the inventory rows below.
    let mut symbols: BTreeMap<&str, &'a GraphRecord> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node { id, kind, .. } = record else {
            continue;
        };
        if tombstoned.contains(id.as_str()) {
            continue;
        }
        if *kind != NodeKind::Symbol {
            continue;
        }
        if !is_owned(id) || non_head_current.contains(id.as_str()) {
            continue;
        }
        let replace = symbols.get(id.as_str()).is_none_or(|existing| {
            super::file_at_point::version_recency_key(record)
                >= super::file_at_point::version_recency_key(existing)
        });
        if replace {
            symbols.insert(id.as_str(), record);
        }
    }

    // The deprecated population: symbols whose node record carries the mark.
    // The mark is looked up through the accessor so the selection keys on
    // the same field the extractor stamps.
    let deprecated_ids: BTreeSet<&str> = symbols
        .iter()
        .filter(|(_, record)| record.deprecated().is_some())
        .map(|(id, _)| *id)
        .collect();

    // Migration worklist: Resolved/Ambiguous CALLS edges targeting a
    // deprecated symbol, attributed through the caller record. Latest
    // edge version wins; stale and non-current edges never attribute.
    let mut worklists: BTreeMap<&str, Vec<DeprecatedCallSite<'a>>> = BTreeMap::new();
    let mut unresolved_ids: BTreeSet<String> = BTreeSet::new();
    for (position, record) in records.iter().enumerate() {
        let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            resolution,
            ..
        } = record
        else {
            continue;
        };
        if *label != EdgeLabel::Calls {
            continue;
        }
        if !liveness.is_latest_edge_version(id.as_str(), position) {
            continue;
        }
        if tombstoned.contains(id.as_str()) {
            continue;
        }
        if non_head_current.contains(id.as_str()) {
            continue;
        }
        // The edge is attributed through its SOURCE symbol, so a repo-scoped
        // run tallies only its own repository's calls.
        if !is_owned(source.as_str()) {
            continue;
        }
        if matches!(
            resolution,
            Some(CallResolution::Unresolved | CallResolution::UnresolvedDispatch)
        ) {
            // The target is a Diagnostic marker, not a symbol: the callee has
            // no in-repo definition the graph could see. It cannot join any
            // worklist, so it is cited under coverage instead.
            unresolved_ids.insert(id.clone());
            continue;
        }
        if !deprecated_ids.contains(target.as_str()) {
            continue;
        }
        let (caller_record_id, caller_name, caller_kind, caller_path, caller_span) =
            match symbols.get(source.as_str()) {
                Some(GraphRecord::Node {
                    id: caller_id,
                    name,
                    symbol_kind,
                    repo_relative_path,
                    span,
                    ..
                }) => (
                    Some(caller_id.as_str()),
                    name.as_deref(),
                    symbol_kind.as_deref(),
                    repo_relative_path.as_deref(),
                    *span,
                ),
                _ => (None, None, None, None, None),
            };
        let resolution_label = match resolution {
            Some(CallResolution::Resolved) => "resolved",
            Some(CallResolution::Ambiguous) => "ambiguous",
            // The edge carried no resolution status: still attributed, but
            // labeled honestly.
            _ => "unrecorded",
        };
        worklists
            .entry(target.as_str())
            .or_default()
            .push(DeprecatedCallSite {
                call_edge_record_id: id.as_str(),
                caller_record_id,
                caller_name,
                caller_kind,
                caller_repo_relative_path: caller_path,
                caller_span,
                resolution: resolution_label,
            });
    }

    let mut result = DeprecatedSymbols::default();
    // File filter (issue #249 `--file`): the inventory — and the
    // `symbols_considered` tally that drives the none-in-scope diagnostics —
    // is scoped to declarations in this repo-relative file. The caller-side
    // worklist attribution still resolves callers wherever they are, because
    // the worklist answers "who still calls this symbol", not "what is
    // declared in this file".
    let in_file = |record: &GraphRecord| -> bool {
        let GraphRecord::Node {
            repo_relative_path, ..
        } = record
        else {
            return false;
        };
        file_filter.is_none_or(|file| repo_relative_path.as_deref() == Some(file))
    };
    result.counts.symbols_considered = symbols.values().filter(|r| in_file(r)).count();
    result.counts.unresolved_call_edges = unresolved_ids.len();
    result.unresolved_call_edge_ids = unresolved_ids.into_iter().collect();

    for (id, record) in &symbols {
        if !in_file(record) {
            continue;
        }
        if !deprecated_ids.contains(id) {
            continue;
        }
        let GraphRecord::Node {
            schema_version,
            repo_relative_path,
            span,
            name,
            symbol_kind,
            temporal,
            deprecated,
            ..
        } = record
        else {
            continue;
        };
        let Some(name) = name.as_deref() else {
            continue;
        };
        let mark = deprecated.as_ref();
        let mut call_sites: Vec<DeprecatedCallSite<'a>> = worklists.remove(id).unwrap_or_default();
        call_sites.sort_by(|a, b| {
            a.caller_repo_relative_path
                .cmp(&b.caller_repo_relative_path)
                .then_with(|| {
                    a.caller_span
                        .map(|s| s.start_line)
                        .cmp(&b.caller_span.map(|s| s.start_line))
                })
                .then_with(|| a.call_edge_record_id.cmp(b.call_edge_record_id))
        });
        result.counts.resolved_call_sites += call_sites.len();
        result.symbols.push(DeprecatedSymbol {
            record_id: id,
            schema_version: *schema_version,
            name,
            kind: symbol_kind.as_deref().unwrap_or("symbol"),
            repo_relative_path: repo_relative_path.as_deref(),
            span: *span,
            git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
            since: mark.and_then(|m| m.since.as_deref()),
            note: mark.and_then(|m| m.note.as_deref()),
            call_sites,
        });
    }
    result.counts.deprecated_symbols = result.symbols.len();

    // Documented deterministic ordering (issue #249 AC8): by
    // (repo_relative_path, span start line, record ID).
    result.symbols.sort_by(|a, b| {
        a.repo_relative_path
            .cmp(&b.repo_relative_path)
            .then_with(|| {
                a.span
                    .map(|s| s.start_line)
                    .cmp(&b.span.map(|s| s.start_line))
            })
            .then_with(|| a.record_id.cmp(b.record_id))
    });

    if result.counts.symbols_considered == 0 {
        result.diagnostics.push(DeprecatedDiagnostic {
            code: "no_code_symbols",
            record_id: None,
            detail: "graph contains no live code Symbol records in scope; there is \
                     nothing to inventory (the store may be empty or the repository \
                     scope excludes every symbol)"
                .to_owned(),
        });
    } else if result.symbols.is_empty() {
        result.diagnostics.push(DeprecatedDiagnostic {
            code: "no_deprecated_symbols",
            record_id: None,
            detail: "graph contains live code Symbol records but none carries a \
                     #[deprecated] attribute mark; a store that predates issue #249 \
                     deprecation capture reports the same signal — rescan to \
                     distinguish"
                .to_owned(),
        });
    }
    if result.counts.unresolved_call_edges > 0 {
        result.diagnostics.push(DeprecatedDiagnostic {
            code: "unresolved_call_edges_present",
            record_id: None,
            detail: format!(
                "{} call edge(s) in this graph have no resolved in-repo target and \
                 are cited in coverage.unresolved_call_edge_ids; any of them may \
                 call a deprecated symbol, so the worklists above are a lower \
                 bound",
                result.counts.unresolved_call_edges
            ),
        });
    }
    result.diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.record_id.cmp(&b.record_id))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    result.diagnostics.dedup();
    result
}

#[cfg(test)]
mod tests {
    //! Unit tests for the deprecated-symbol inventory (issue #249).
    use super::*;
    use crate::ir::{DeprecationMark, SCHEMA_VERSION};

    fn sym(id: &str, name: &str, mark: Option<DeprecationMark>) -> GraphRecord {
        let mut record = GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
                start_column: None,
                end_column: None,
            }),
            Some(name.to_owned()),
            format!("symbol {name}"),
        );
        if let Some(mark) = mark {
            record = record.with_deprecated(mark);
        }
        record
    }

    fn mark(since: Option<&str>, note: Option<&str>) -> DeprecationMark {
        DeprecationMark {
            since: since.map(str::to_owned),
            note: note.map(str::to_owned),
        }
    }

    fn calls(source: &str, target: &str, resolution: CallResolution) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Calls,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            "calls".to_owned(),
        )
        .with_resolution(resolution)
    }

    #[test]
    fn selects_only_marked_symbols_with_verbatim_payloads() {
        let records = vec![
            sym("codegraph:v6:old", "old", Some(mark(None, None))),
            sym("codegraph:v6:new", "new", None),
            sym(
                "codegraph:v6:older",
                "older",
                Some(mark(Some("1.2.0"), Some("use new"))),
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = deprecated_symbols(&records, &index, None, None);

        assert_eq!(result.counts.symbols_considered, 3);
        assert_eq!(result.counts.deprecated_symbols, 2);
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["old", "older"]);

        let bare = &result.symbols[0];
        assert_eq!(bare.since, None, "absent since is None, never fabricated");
        assert_eq!(bare.note, None, "absent note is None, never fabricated");
        let meta = &result.symbols[1];
        assert_eq!(meta.since, Some("1.2.0"));
        assert_eq!(meta.note, Some("use new"));
    }

    #[test]
    fn resolved_call_sites_form_the_worklist_with_citable_handles() {
        let records = vec![
            sym("codegraph:v6:old", "old", Some(mark(None, None))),
            sym("codegraph:v6:caller", "caller", None),
            calls(
                "codegraph:v6:caller",
                "codegraph:v6:old",
                CallResolution::Resolved,
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = deprecated_symbols(&records, &index, None, None);

        assert_eq!(result.symbols.len(), 1);
        let row = &result.symbols[0];
        assert_eq!(row.call_sites.len(), 1);
        let site = &row.call_sites[0];
        assert!(!site.call_edge_record_id.is_empty(), "edge ID is cited");
        assert_eq!(site.caller_record_id, Some("codegraph:v6:caller"));
        assert_eq!(site.caller_name, Some("caller"));
        assert_eq!(site.caller_repo_relative_path, Some("src/lib.rs"));
        assert!(site.caller_span.is_some());
        assert_eq!(site.resolution, "resolved");
        assert_eq!(result.counts.resolved_call_sites, 1);
    }

    #[test]
    fn ambiguous_calls_are_kept_as_possible_call_sites() {
        let records = vec![
            sym("codegraph:v6:old", "old", Some(mark(None, None))),
            sym("codegraph:v6:caller", "caller", None),
            calls(
                "codegraph:v6:caller",
                "codegraph:v6:old",
                CallResolution::Ambiguous,
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = deprecated_symbols(&records, &index, None, None);

        assert_eq!(result.symbols[0].call_sites.len(), 1);
        assert_eq!(result.symbols[0].call_sites[0].resolution, "ambiguous");
    }

    #[test]
    fn unresolved_calls_are_cited_under_coverage_never_in_the_worklist() {
        let records = vec![
            sym("codegraph:v6:old", "old", Some(mark(None, None))),
            sym("codegraph:v6:user", "user", None),
            calls(
                "codegraph:v6:user",
                "codegraph:v6:old",
                CallResolution::Resolved,
            ),
            calls(
                "codegraph:v6:user",
                "codegraph:v6:missing",
                CallResolution::Unresolved,
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = deprecated_symbols(&records, &index, None, None);

        assert_eq!(result.counts.unresolved_call_edges, 1);
        assert_eq!(result.unresolved_call_edge_ids.len(), 1);
        assert_eq!(
            result.symbols[0].call_sites.len(),
            1,
            "only the resolved edge joins"
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "unresolved_call_edges_present"),
            "the unresolved coverage diagnostic is raised"
        );
    }

    #[test]
    fn tombstoned_marks_are_not_selected() {
        let id = "codegraph:v6:old";
        let records = vec![
            sym(id, "old", Some(mark(None, None))),
            GraphRecord::Tombstone {
                id: format!("codegraph:v6:tomb_{id}"),
                schema_version: SCHEMA_VERSION,
                deleted_id: id.to_owned(),
                summary: "removed".to_owned(),
                producer: None,
            },
        ];
        let index = RepositoryIndex::build(&records);
        let result = deprecated_symbols(&records, &index, None, None);

        assert!(result.symbols.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "no_code_symbols"),
            "a fully tombstoned store reports no_code_symbols"
        );
    }

    #[test]
    fn zero_deprecated_yields_the_stable_diagnostic() {
        let records = vec![sym("codegraph:v6:new", "new", None)];
        let index = RepositoryIndex::build(&records);
        let result = deprecated_symbols(&records, &index, None, None);

        assert!(result.symbols.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "no_deprecated_symbols"),
            "live symbols with no marks report no_deprecated_symbols"
        );
    }

    #[test]
    fn rows_sort_by_path_span_then_record_id() {
        let records = vec![
            sym("codegraph:v6:zzz", "zzz", Some(mark(None, None))),
            sym("codegraph:v6:aaa", "aaa", Some(mark(None, None))),
        ];
        let index = RepositoryIndex::build(&records);
        let result = deprecated_symbols(&records, &index, None, None);

        let ids: Vec<&str> = result.symbols.iter().map(|s| s.record_id).collect();
        assert_eq!(ids, vec!["codegraph:v6:aaa", "codegraph:v6:zzz"]);
    }

    #[test]
    fn file_filter_scopes_rows_but_keeps_cross_file_callers() {
        fn sym_in(id: &str, name: &str, path: &str, mark: Option<DeprecationMark>) -> GraphRecord {
            let mut record = GraphRecord::node(
                id.to_owned(),
                NodeKind::Symbol,
                Some(path.to_owned()),
                Some(SourceSpan {
                    start_byte: 0,
                    end_byte: 10,
                    start_line: 1,
                    end_line: 2,
                    start_column: None,
                    end_column: None,
                }),
                Some(name.to_owned()),
                format!("symbol {name}"),
            );
            if let Some(mark) = mark {
                record = record.with_deprecated(mark);
            }
            record
        }
        let records = vec![
            sym_in(
                "codegraph:v6:old",
                "old",
                "src/lib.rs",
                Some(mark(None, None)),
            ),
            sym_in(
                "codegraph:v6:old2",
                "old2",
                "src/other.rs",
                Some(mark(None, None)),
            ),
            sym_in("codegraph:v6:caller", "caller", "src/other.rs", None),
            calls(
                "codegraph:v6:caller",
                "codegraph:v6:old",
                CallResolution::Resolved,
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = deprecated_symbols(&records, &index, None, Some("src/lib.rs"));

        assert_eq!(result.counts.symbols_considered, 1);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "old");
        // The caller lives in the other file, but the worklist stays complete.
        assert_eq!(result.symbols[0].call_sites.len(), 1);
        assert_eq!(
            result.symbols[0].call_sites[0].caller_name,
            Some("caller"),
            "caller attribution resolves across the file boundary"
        );

        // A file with no symbols at all yields the no_code_symbols signal.
        let empty = deprecated_symbols(&records, &index, None, Some("src/missing.rs"));
        assert_eq!(empty.counts.symbols_considered, 0);
        assert!(
            empty
                .diagnostics
                .iter()
                .any(|d| d.code == "no_code_symbols")
        );
    }
}
