use std::collections::{BTreeMap, BTreeSet};

use super::RepositoryIndex;
use crate::ir::{
    CallResolution, EdgeLabel, GraphRecord, NodeKind, SnapshotHead, SourceSpan, TemporalMetadata,
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
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect();
    let is_owned =
        |id: &str| -> bool { repo_scope.is_none_or(|scope| index.owner_of(id) == Some(scope)) };

    // Stamped HEAD commit per live repository (`source_snapshot`, issue #82).
    // History replay re-emits the full graph at every commit with `temporal`
    // provenance and no tombstones for between-commit removals, so a temporal
    // record is part of the current state only when its commit is its
    // repository's stamped HEAD — the same rule `resolve_head_symbols` uses.
    // Snapshot-less stores (pre-#186 graphs) keep the conservative fallback:
    // nodes resolve by keep-last dedupe and every recorded edge counts.
    let mut repo_heads: BTreeMap<&str, &str> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Repository,
            source_snapshot: Some(snapshot),
            ..
        } = record
            && !tombstoned.contains(id.as_str())
            && let SnapshotHead::Commit { sha } = &snapshot.head
        {
            repo_heads.insert(id.as_str(), sha.as_str());
        }
    }
    // Current-state check for records attributable through the containment
    // topology (symbols; reference edges via their target symbol).
    let owned_record_is_current = |id: &str, temporal: Option<&TemporalMetadata>| -> bool {
        let Some(t) = temporal else {
            return true;
        };
        index
            .owner_of(id)
            .and_then(|owner| repo_heads.get(owner))
            .is_none_or(|head_sha| t.git_commit == *head_sha)
    };
    // Current-state check for records outside the containment topology
    // (Diagnostic markers, unresolved-call edges): no owner is resolvable, so
    // a temporal record is current when its commit is any repository's
    // stamped HEAD. Commit SHAs never collide across repositories in
    // practice, and snapshot-less stores keep everything (fallback).
    let unowned_record_is_current = |temporal: Option<&TemporalMetadata>| -> bool {
        let Some(t) = temporal else {
            return true;
        };
        repo_heads.is_empty() || repo_heads.values().any(|sha| *sha == t.git_commit)
    };

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
                if !is_owned(id) || !owned_record_is_current(id, temporal.as_ref()) {
                    continue;
                }
                // impl blocks are unnameable declaration details, never
                // prune candidates; their methods are considered directly.
                if symbol_kind.as_deref() == Some("impl") {
                    continue;
                }
                symbols.insert(id.as_str(), record);
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
        for (id, path, name, temporal) in &diagnostic_rows {
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
            let current = attributed.map_or_else(
                || unowned_record_is_current(*temporal),
                |repo| {
                    temporal.is_none_or(|t| {
                        repo_heads
                            .get(repo)
                            .is_none_or(|head_sha| t.git_commit == *head_sha)
                    })
                },
            );
            if current {
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
    for record in records {
        let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            resolution,
            temporal,
            ..
        } = record
        else {
            continue;
        };
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
            if is_owned(source.as_str())
                && owned_record_is_current(source.as_str(), temporal.as_ref())
            {
                unresolved_call_edges += 1;
            }
            continue;
        }
        if !owned_record_is_current(target.as_str(), temporal.as_ref()) {
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
