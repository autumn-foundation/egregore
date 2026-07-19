use std::collections::{BTreeMap, BTreeSet};

use super::RepositoryIndex;
use super::liveness::Liveness;
use crate::ir::{
    CallResolution, EdgeLabel, GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    parse_codegraph_id, stable_id,
};

// ---------------------------------------------------------------------------
// unreferenced-symbol prune candidates (issue #113)
// ---------------------------------------------------------------------------

/// Edge labels counted as recorded references when selecting unreferenced-
/// symbol candidates (issue #113 AC2).
///
/// The issue documents the reference classes `CALLS` / `MENTIONS` / `IMPORTS`;
/// this repo's extractor additionally records `REFERENCES` (identifier use
/// that is not call-shaped, e.g. a type named in a body or signature) and
/// `IMPLEMENTS` (an `impl` block binding to its trait or type). Both are
/// recorded uses in the existing edge vocabulary, so both count — excluding
/// them would flatly misreport every used-but-never-called type as
/// unreferenced, exactly the false-candidate class the issue rules out.
///
/// Structural containment (`DEFINES` / `CONTAINS`) is never counted: every
/// symbol has one from its own file or module, so it carries no usage signal.
/// Agent-memory `MENTIONS_SYMBOL` edges are never counted either: an
/// agent-authored observation is not a code fact and must not mark code as
/// referenced (trust separation).
pub const UNREFERENCED_REFERENCE_LABELS: &[EdgeLabel] = &[
    EdgeLabel::Calls,
    EdgeLabel::Implements,
    EdgeLabel::Imports,
    EdgeLabel::Mentions,
    EdgeLabel::References,
];

/// Stable wire strings for [`UNREFERENCED_REFERENCE_LABELS`], sorted.
pub const UNREFERENCED_REFERENCE_CLASS_NAMES: &[&str] =
    &["CALLS", "IMPLEMENTS", "IMPORTS", "MENTIONS", "REFERENCES"];

/// Extraction-completeness caveat attached to a candidate whose file scope
/// contains extractor `Diagnostic` markers (issue #87 semantics).
///
/// A macro-hidden or unparsed reference may exist in that scope, so the
/// candidate's confidence is lower. The caveat is advisory: it never rewrites
/// or hides the code fact it annotates.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnreferencedExtractionCaveat {
    /// Stable caveat code (`diagnostics_in_file_scope`).
    pub code: &'static str,
    /// Number of active `Diagnostic` markers in the candidate's file scope.
    pub diagnostic_count: usize,
    /// Stable record IDs of those markers, sorted for determinism.
    pub diagnostic_record_ids: Vec<String>,
    /// Bounded human-readable detail (paths and counts only — never payload).
    pub detail: String,
}

/// One zero-inbound-reference prune candidate.
///
/// Every row is a LEAD to inspect before deleting — never proof the symbol is
/// dead. See [`unreferenced_symbols`] for the false-positive classes the
/// graph cannot see.
#[derive(Debug, Clone)]
pub struct UnreferencedCandidate<'a> {
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
    /// The inbound-reference count that selected the candidate — always 0.
    pub inbound_reference_count: usize,
    /// Present when the candidate's file scope contains `Diagnostic` markers.
    pub extraction_caveat: Option<UnreferencedExtractionCaveat>,
}

/// Deterministic tallies for the unreferenced-symbol result.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct UnreferencedCounts {
    /// Live, in-scope `Symbol` records considered (impl blocks excluded).
    pub symbols_considered: usize,
    /// Considered symbols with at least one recorded inbound reference.
    pub referenced: usize,
    /// Considered symbols with zero recorded inbound references.
    pub candidates: usize,
    /// Files carrying at least one active extractor `Diagnostic` marker.
    pub files_with_diagnostic_markers: usize,
}

/// A stable machine-readable condition attached to the result.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnreferencedDiagnostic {
    /// Stable diagnostic code (`no_candidates`, `no_symbols`,
    /// `unresolved_call_edges_present`).
    pub code: &'static str,
    /// Record the diagnostic is about, when one exists.
    pub record_id: Option<String>,
    /// Bounded human-readable detail (counts only — never payload).
    pub detail: String,
}

/// The unreferenced-symbol candidate set plus tallies and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct UnreferencedSymbols<'a> {
    /// Candidates sorted by (`repo_relative_path`, `span.start_line`,
    /// `record_id`) — the documented deterministic ordering.
    pub candidates: Vec<UnreferencedCandidate<'a>>,
    /// Deterministic tallies.
    pub counts: UnreferencedCounts,
    /// Stable diagnostics, sorted and de-duplicated.
    pub diagnostics: Vec<UnreferencedDiagnostic>,
}

/// Selects code symbols with **no recorded inbound reference edges** as
/// prune-triage candidates (issue #113).
///
/// A live `Symbol` record is a candidate when it has zero inbound edges of
/// the reference classes in [`UNREFERENCED_REFERENCE_LABELS`]. Structural
/// containment (`DEFINES` / `CONTAINS`) never counts — every symbol has one.
/// `impl`-block symbols are excluded from the candidate population: they are
/// unnameable declaration details, so a zero inbound count carries no pruning
/// signal (their methods are considered individually).
///
/// Current-state view: tombstoned symbols are excluded, and when a stable ID
/// appears more than once (history graphs) the latest record wins
/// deterministically. Ambiguous call edges count as references — a symbol
/// that *might* be called is never reported as unreferenced.
///
/// Every candidate is a LEAD, never proof of dead code. The graph cannot see:
/// public API consumed outside this repository, trait-method dynamic
/// dispatch, macro-generated call sites, FFI / `#[no_mangle]` /
/// `#[export_name]` consumers, derive-generated use, or crate entry points
/// (`main`, `#[test]`). Candidates in a file scope containing extractor
/// `Diagnostic` markers additionally carry an extraction-completeness caveat
/// (issue #87): a macro-hidden reference may exist there.
///
/// Deterministic: output ordering depends only on record content, never on
/// map iteration or wall-clock time. Strictly read-only.
#[must_use]
pub fn unreferenced_symbols<'a>(
    records: &'a [GraphRecord],
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> UnreferencedSymbols<'a> {
    // Latest-write-wins liveness (issues #421/#432): over an append-only
    // `--graph` a node OR reference edge re-ingested AFTER its own tombstone is
    // live again, matching the embedded `--data-dir` current-state read. The
    // `tombstoned` set below therefore retains a deleted_id only while its
    // tombstone is still the id's most recent write, and the inbound-reference
    // scan additionally selects only the latest EDGE version per id so a stale
    // earlier version cannot supply reference metadata. Orthogonal to the
    // `non_head_current` corpus gate (#82/#427), which still applies below. See
    // `super::liveness`.
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

    // Default current-state view: the set of record IDs NOT current at their
    // repository's stamped HEAD (`source_snapshot`, issue #82/#427), computed by
    // the shared head-anchor gate rather than an inlined `repo_heads` /
    // `owned_record_is_current` / `unowned_record_is_current` copy. History
    // replay re-emits the full graph at every commit with `temporal` provenance
    // and no tombstone for a between-commit removal, so a symbol/edge absent at
    // HEAD would otherwise resurface. The drop-set groups by stable ID (an ID
    // with any HEAD-current version is retained whole) and the recency keep-last
    // below selects its HEAD version — matching the prior inline per-record
    // gate; snapshot-less stores yield an empty drop-set (keep-last dedupe, every
    // recorded edge counts). Records outside the containment topology (Diagnostic
    // markers, unresolved-call edges) are head-anchored by the shared gate's
    // any-head rule — commit SHAs never collide across repositories in practice,
    // so it agrees with the removed owner-specific check.
    let non_head_current = super::non_head_current_record_ids(records, index);

    // Candidate population: live, in-scope Symbol nodes, keep-last dedupe by
    // stable ID so history graphs resolve to their newest version.
    let mut symbols: BTreeMap<&str, &'a GraphRecord> = BTreeMap::new();
    // Active extractor Diagnostic markers, collected raw here and attributed
    // to repositories below. Code-domain only: trajectory/importer records
    // reuse `NodeKind::Diagnostic` with a `domain` marker and must not lower
    // confidence in code extraction.
    let mut diagnostic_rows: Vec<(&str, &str, Option<&str>, Option<&TemporalMetadata>)> =
        Vec::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            repo_relative_path,
            symbol_kind,
            name,
            domain,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        if tombstoned.contains(id.as_str()) {
            continue;
        }
        match kind {
            NodeKind::Symbol => {
                if !is_owned(id) || non_head_current.contains(id.as_str()) {
                    continue;
                }
                // impl blocks are unnameable declaration details, never
                // prune candidates; their methods are considered directly.
                if symbol_kind.as_deref() == Some("impl") {
                    continue;
                }
                // Keep-last dedupe by recency so a retained ID's HEAD
                // (newest-valid-time) version wins — the shared drop-set keeps
                // every version of a HEAD-current ID, and this selects the same
                // version the removed per-record HEAD gate did.
                let replace = symbols.get(id.as_str()).is_none_or(|existing| {
                    super::file_at_point::version_recency_key(record)
                        >= super::file_at_point::version_recency_key(existing)
                });
                if replace {
                    symbols.insert(id.as_str(), record);
                }
            }
            NodeKind::Diagnostic if domain.is_none() => {
                if let Some(path) = repo_relative_path.as_deref() {
                    diagnostic_rows.push((id.as_str(), path, name.as_deref(), temporal.as_ref()));
                }
            }
            _ => {}
        }
    }

    // ── Diagnostic attribution, scoping, and currency ───────────────────────
    // Extractor Diagnostic markers are not attached to the containment
    // topology, so `owner_of` cannot attribute them (an ownership pre-filter
    // would silently drop every caveat from a repo-scoped run). Their stable
    // IDs embed the producing repository's record ID, so attribution is
    // recomputed from the two extractor ID schemes (unsupported macro
    // invocation; unresolved call target). A marker with an unrecognized
    // scheme falls back to the path-owner rule used by the file-at-point
    // lanes — kept when its path is recorded by the selected repository —
    // because the caveat is advisory and dropping a real marker would hide
    // lower extraction confidence.
    //
    // The map is keyed by (attributed repository, path) so an unscoped run
    // over a merged store never blurs the repository boundary: two
    // repositories recording the same repo-relative path keep separate
    // marker sets, and each candidate matches only its own repository's
    // markers (plus unattributable `None`-keyed markers, kept conservatively
    // for every path-matching candidate). Repository keys are canonical ID
    // suffixes so a schema-version bump never splits one repository.
    let canonical_repo = |repo: &str| -> String {
        parse_codegraph_id(repo).map_or_else(|| repo.to_owned(), |(_, suffix)| suffix.to_owned())
    };
    let mut file_diagnostics: BTreeMap<(Option<String>, &str), BTreeSet<&str>> = BTreeMap::new();
    if !diagnostic_rows.is_empty() {
        let repository_ids = index.repository_ids();
        // Macro-scheme disambiguators are per-(path, invocation) ordinals, so
        // the instance count bounds the recomputation search.
        let mut name_counts: BTreeMap<(&str, &str), u64> = BTreeMap::new();
        for (_, path, name, _) in &diagnostic_rows {
            if let Some(name) = name {
                *name_counts.entry((*path, *name)).or_default() += 1;
            }
        }
        // Live File/Symbol owners per path: the fallback attribution rule.
        let mut path_owners: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node {
                id,
                kind: NodeKind::File | NodeKind::Symbol,
                repo_relative_path: Some(path),
                ..
            } = record
                && !tombstoned.contains(id.as_str())
                && let Some(owner) = index.owner_of(id)
            {
                path_owners.entry(path.as_str()).or_default().insert(owner);
            }
        }
        // Repository record IDs can differ in schema version across records;
        // compare by canonical suffix so a version bump never splits a repo.
        let same_repo = |a: &str, b: &str| -> bool {
            a == b
                || matches!(
                    (parse_codegraph_id(a), parse_codegraph_id(b)),
                    (Some((_, sa)), Some((_, sb))) if sa == sb
                )
        };
        for (id, path, name, _temporal) in &diagnostic_rows {
            let attributed = name.and_then(|name| {
                let ordinal_bound = name_counts.get(&(*path, name)).copied().unwrap_or(0);
                repository_ids.iter().copied().find(|repo| {
                    stable_id(&["node", "diagnostic", repo, path, "unresolved-call", name]) == *id
                        || (0..ordinal_bound).any(|ordinal| {
                            let ordinal = ordinal.to_string();
                            stable_id(&["node", "diagnostic", repo, path, name, &ordinal]) == *id
                        })
                })
            });
            let in_scope = match (repo_scope, attributed) {
                (None, _) => true,
                (Some(scope), Some(repo)) => same_repo(repo, scope),
                (Some(scope), None) => path_owners
                    .get(path)
                    .is_some_and(|owners| owners.iter().any(|owner| same_repo(owner, scope))),
            };
            if !in_scope {
                continue;
            }
            // Head-anchor the marker through the shared drop-set. A Diagnostic
            // is outside the containment topology (`owner_of` cannot attribute
            // it), so the shared gate applies its any-head rule; commit SHAs do
            // not collide across repositories, so this agrees with the removed
            // attributed-repo-specific HEAD check for both attribution branches.
            if !non_head_current.contains(*id) {
                file_diagnostics
                    .entry((attributed.map(&canonical_repo), path))
                    .or_default()
                    .insert(id);
            }
        }
    }

    // Inbound reference counting over live, current-state edges of the
    // reference classes. A stale edge — replayed from an older commit and
    // absent at its repository's stamped HEAD — must not mark its target as
    // referenced, or a symbol whose last caller was removed would silently
    // vanish from the candidate set.
    let mut referenced_ids: BTreeSet<&str> = BTreeSet::new();
    let mut unresolved_call_edges = 0usize;
    for (index, record) in records.iter().enumerate() {
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
        // Latest-write-wins for edge metadata (#421/#432): a stable edge id may
        // be re-ingested with changed label/resolution over an append-only
        // `--graph`. Only the latest EDGE write for the id is live, mirroring the
        // embedded `latest_edge_versions` read, so a superseded earlier version
        // never double-counts or mis-classifies an inbound reference.
        if !liveness.is_latest_edge_version(id.as_str(), index) {
            continue;
        }
        if tombstoned.contains(id.as_str()) {
            continue;
        }
        if !UNREFERENCED_REFERENCE_LABELS.contains(label) {
            continue;
        }
        if *label == EdgeLabel::Calls && *resolution == Some(CallResolution::Unresolved) {
            // The target is a Diagnostic marker, not a symbol: the callee has
            // no in-repo definition the graph could see. The edge is
            // attributed through its SOURCE symbol (a caller in the
            // containment topology), so a repo-scoped run tallies only its
            // own repository's unresolved calls — never another repository's
            // noise — and currency is checked against the source
            // repository's stamped HEAD.
            // The edge is head-anchored by its own record ID (unowned → the
            // shared gate's any-head rule, which equals the source repository's
            // stamped-HEAD check since the edge's commit lives in that repo).
            if is_owned(source.as_str()) && !non_head_current.contains(id.as_str()) {
                unresolved_call_edges += 1;
            }
            continue;
        }
        // A stale reference edge (present at an older commit, absent at HEAD)
        // must not mark its target referenced; drop it by its own record ID.
        if non_head_current.contains(id.as_str()) {
            continue;
        }
        referenced_ids.insert(target.as_str());
    }

    let mut result = UnreferencedSymbols::default();
    result.counts.symbols_considered = symbols.len();
    result.counts.files_with_diagnostic_markers = file_diagnostics.len();

    for (id, record) in &symbols {
        if referenced_ids.contains(id) {
            result.counts.referenced += 1;
            continue;
        }
        let GraphRecord::Node {
            schema_version,
            repo_relative_path,
            span,
            name,
            symbol_kind,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        let Some(name) = name.as_deref() else {
            continue;
        };
        // Markers matched through the candidate's own repository: its repo's
        // key plus the unattributable `None` key. A candidate the topology
        // cannot attribute (legacy graphs) conservatively matches every
        // marker at its path.
        let mut marker_ids: BTreeSet<&str> = BTreeSet::new();
        if let Some(path) = repo_relative_path.as_deref() {
            match index.owner_of(id).map(&canonical_repo) {
                Some(candidate_repo) => {
                    for key in [Some(candidate_repo), None] {
                        if let Some(ids) = file_diagnostics.get(&(key, path)) {
                            marker_ids.extend(ids.iter().copied());
                        }
                    }
                }
                None => {
                    for ((_, marker_path), ids) in &file_diagnostics {
                        if *marker_path == path {
                            marker_ids.extend(ids.iter().copied());
                        }
                    }
                }
            }
        }
        let extraction_caveat = (!marker_ids.is_empty()).then(|| {
            let diagnostic_record_ids: Vec<String> =
                marker_ids.iter().map(|m| (*m).to_owned()).collect();
            UnreferencedExtractionCaveat {
                code: "diagnostics_in_file_scope",
                diagnostic_count: diagnostic_record_ids.len(),
                diagnostic_record_ids,
                detail: format!(
                    "file scope contains {} extraction Diagnostic marker(s); a \
                     macro-hidden or unparsed reference may exist, so this \
                     candidate's confidence is lower",
                    marker_ids.len()
                ),
            }
        });
        result.candidates.push(UnreferencedCandidate {
            record_id: id,
            schema_version: *schema_version,
            name,
            kind: symbol_kind.as_deref().unwrap_or("symbol"),
            repo_relative_path: repo_relative_path.as_deref(),
            span: *span,
            git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
            inbound_reference_count: 0,
            extraction_caveat,
        });
    }
    result.counts.candidates = result.candidates.len();

    // Documented deterministic ordering (issue #113 AC8).
    result.candidates.sort_by(|a, b| {
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
        result.diagnostics.push(UnreferencedDiagnostic {
            code: "no_symbols",
            record_id: None,
            detail: "graph contains no live code Symbol records in scope; there is \
                     nothing to triage (the store may predate code extraction or the \
                     repository scope excludes every symbol)"
                .to_owned(),
        });
    } else if result.candidates.is_empty() {
        result.diagnostics.push(UnreferencedDiagnostic {
            code: "no_candidates",
            record_id: None,
            detail: "every considered symbol carries at least one recorded inbound \
                     reference edge; no prune candidates"
                .to_owned(),
        });
    }
    if unresolved_call_edges > 0 {
        result.diagnostics.push(UnreferencedDiagnostic {
            code: "unresolved_call_edges_present",
            record_id: None,
            detail: format!(
                "{unresolved_call_edges} call edge(s) in this graph have no resolved \
                 in-repo target; an unrecorded reference to a listed candidate may \
                 exist, so treat candidates as leads only"
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
mod liveness_parity_tests {
    //! Transport-parity regression (issues #421/#432): over an append-only
    //! `--graph`, a symbol OR reference edge re-ingested AFTER its own tombstone
    //! is live again, and only the latest EDGE version supplies reference
    //! metadata — matching the embedded `--data-dir` current-state read.
    use super::*;
    use crate::ir::SCHEMA_VERSION;

    fn sym(id: &str, name: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
            }),
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
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

    fn tomb(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v6:tomb_{deleted_id}"),
            schema_version: SCHEMA_VERSION,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    #[test]
    fn symbol_reingested_after_tombstone_is_a_candidate() {
        let id = "codegraph:v6:lonely";
        let records = vec![sym(id, "lonely"), tomb(id), sym(id, "lonely")];
        let index = RepositoryIndex::build(&records);
        let result = unreferenced_symbols(&records, &index, None);
        assert!(
            result.candidates.iter().any(|c| c.record_id == id),
            "a symbol re-ingested after its tombstone must be a live candidate"
        );
    }

    #[test]
    fn symbol_tombstone_without_reingest_is_not_a_candidate() {
        let id = "codegraph:v6:lonely";
        let records = vec![sym(id, "lonely"), tomb(id)];
        let index = RepositoryIndex::build(&records);
        let result = unreferenced_symbols(&records, &index, None);
        assert!(
            !result.candidates.iter().any(|c| c.record_id == id),
            "a tombstone with no later re-ingest still deletes the symbol"
        );
    }

    #[test]
    fn reference_edge_reingested_after_tombstone_marks_target_referenced() {
        let from_id = "codegraph:v6:caller";
        let to_id = "codegraph:v6:callee";
        let e1 = calls(from_id, to_id, CallResolution::Resolved);
        let edge_id = e1.id().to_owned();
        let e2 = calls(from_id, to_id, CallResolution::Resolved);
        let records = vec![
            sym(from_id, "caller"),
            sym(to_id, "callee"),
            e1,
            tomb(&edge_id),
            e2,
        ];
        let index = RepositoryIndex::build(&records);
        let result = unreferenced_symbols(&records, &index, None);
        assert!(
            !result.candidates.iter().any(|c| c.record_id == to_id),
            "a reference edge re-ingested after its tombstone must mark its target referenced"
        );
    }

    #[test]
    fn only_latest_edge_version_supplies_reference_metadata() {
        // Two versions of one stable CALLS edge id: v1 `unresolved`, v2
        // `resolved`. Only the latest EDGE write (v2) is read, so the target is
        // referenced and no stale `unresolved_call_edges_present` diagnostic is
        // raised from the superseded v1.
        let from_id = "codegraph:v6:caller";
        let to_id = "codegraph:v6:callee";
        let records = vec![
            sym(from_id, "caller"),
            sym(to_id, "callee"),
            calls(from_id, to_id, CallResolution::Unresolved),
            calls(from_id, to_id, CallResolution::Resolved),
        ];
        let index = RepositoryIndex::build(&records);
        let result = unreferenced_symbols(&records, &index, None);
        assert!(
            !result.candidates.iter().any(|c| c.record_id == to_id),
            "the latest resolved edge version must mark the target referenced"
        );
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.code == "unresolved_call_edges_present"),
            "a superseded unresolved edge version must not raise the unresolved diagnostic"
        );
    }
}
