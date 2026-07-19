use std::collections::{BTreeMap, BTreeSet};

use super::{
    PUBLIC_API_SYMBOL_KINDS, PublicApiDiagnostic, RepositoryIndex, is_library_crate_path,
    public_api_surface,
};
use crate::ir::{GraphRecord, NodeKind, SnapshotHead, SourceSpan, TemporalMetadata};

// ---------------------------------------------------------------------------
// undocumented public API lane (issue #257)
// ---------------------------------------------------------------------------

/// One symbol reported by the undocumented-public-API lane: a doc-auditable
/// symbol whose recorded doc-comment fact is absent.
///
/// For declared items the citation fields point at the declaration; for
/// re-exports (`via_reexport` = `true`) they point at the `pub use` site and
/// `target_record_id` cites the resolved declaration whose doc fact was
/// checked.
#[derive(Debug, Clone)]
pub struct UndocumentedItem<'a> {
    /// Stable record ID of the declaring `Symbol` node, or of the `Import`
    /// node at the re-export site.
    pub record_id: &'a str,
    /// Symbol kind (`function`, `struct`, `enum`, `trait`, `type_alias`,
    /// `const`, `static`, or `method` under `--include-private`).
    pub kind: String,
    /// Crate-relative fully-qualified path (alias-aware for re-exports).
    pub path: String,
    /// Recorded visibility class: `public` for externally-reachable rows;
    /// the declared class for `--include-private` rows.
    pub visibility: &'a str,
    /// Repo-relative file of the declaration or re-export site.
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the declaration or re-export site.
    pub span: Option<SourceSpan>,
    /// Persisted declaration signature (issue #124), joined when present.
    pub signature: Option<&'a str>,
    /// Concrete evidence asserted for this row, as stable markers:
    /// `doc_comment_absent` always, plus `externally_reachable` when the
    /// symbol is on the issue #213 public surface.
    pub evidence: Vec<&'static str>,
    /// `true` when the symbol reaches the surface through a `pub use`.
    pub via_reexport: bool,
    /// Crate-relative use-path the re-export points at (re-exports only).
    pub target: Option<String>,
    /// Record ID of the resolved re-export target whose doc fact was checked.
    pub target_record_id: Option<&'a str>,
}

/// Deterministic tallies for the undocumented-public-API lane.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct UndocumentedCounts {
    /// Doc-auditable candidates whose doc fact was asserted
    /// (`documented + undocumented`).
    pub considered: usize,
    /// Candidates carrying a recorded doc comment — excluded from `items`.
    pub documented: usize,
    /// Candidates with no recorded doc comment — the returned rows (before
    /// any `--limit` truncation).
    pub undocumented: usize,
    /// Undocumented rows contributed by `pub use` re-exports.
    pub reexports: usize,
    /// Surface `module` rows: modules carry no doc-comment fact and are
    /// excluded from the audit, never guessed.
    pub modules_excluded: usize,
    /// Re-export rows whose target did not resolve in-graph: doc presence
    /// cannot be asserted, so they are diagnosed, never reported.
    pub reexports_unresolved: usize,
    /// Doc-auditable symbol records carrying no issue #124 declaration
    /// surface (pre-#124 scan): their doc fact was never captured.
    pub doc_capture_missing: usize,
}

/// The undocumented-public-API report: rows, tallies, and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct UndocumentedReport<'a> {
    /// `true` when the store carries no doc-capture facts at all (pre-#124
    /// scan): the lane reports this verdict instead of treating every symbol
    /// as undocumented.
    pub capability_absent: bool,
    /// Undocumented symbols, sorted by (path, kind, record ID).
    pub items: Vec<UndocumentedItem<'a>>,
    /// Deterministic tallies.
    pub counts: UndocumentedCounts,
    /// Stable diagnostics, sorted and de-duplicated. Reuses the public-api
    /// diagnostic shape; surface diagnostics pass through (except
    /// `empty_surface`, which this lane replaces with its own verdicts).
    pub diagnostics: Vec<PublicApiDiagnostic>,
}

/// Returns the recorded (`doc`, `visibility`) facts of a symbol record.
fn symbol_doc_facts(record: &GraphRecord) -> (Option<&str>, Option<&str>) {
    if let GraphRecord::Node {
        doc, visibility, ..
    } = record
    {
        (doc.as_deref(), visibility.as_deref())
    } else {
        (None, None)
    }
}

/// Lists externally-reachable public symbols whose captured doc-comment fact
/// is absent (issue #257).
///
/// The result is the issue #213 public surface minus the has-doc set from
/// issue #124 — a graph-native join, never a `pub`-token grep.
///
/// The reachability rule is `public_api_surface`'s, reused verbatim: items
/// not externally reachable are excluded by default. A re-export counts as
/// documented when either the `pub use` site or the resolved target carries
/// a doc fact — rustdoc exposes site docs on the public item.
/// `include_private` widens the audit to every doc-auditable symbol (adding
/// `method` declarations) regardless of visibility, for whole-crate doc
/// audits; such rows carry their declared visibility class and never claim
/// `externally_reachable`.
///
/// Soundness boundary: the lane asserts the **presence or absence of a
/// recorded doc comment** (`///`, `/** */`, or `#[doc = "..."]`) — never doc
/// quality, accuracy, or completeness. When the store predates issue #124
/// doc capture, the report carries `capability_absent = true` and a
/// `doc_capture_unavailable` diagnostic instead of silently treating every
/// symbol as undocumented. Deterministic: output ordering depends only on
/// record content. `limit` truncates the sorted rows and adds a
/// `results_truncated` diagnostic; it never changes row order.
#[must_use]
pub fn undocumented_public_api<'a>(
    records: &'a [GraphRecord],
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
    include_private: bool,
    limit: Option<usize>,
) -> UndocumentedReport<'a> {
    let surface = public_api_surface(records, index, repo_scope);

    // Doc-auditable symbol records in the current-state view, mirroring the
    // surface's scope: the Rust library crate, minus tombstones, within the
    // repo scope, and — like `public_api_surface` — anchored to each
    // repository's stamped HEAD so a symbol deleted at HEAD never resurfaces in
    // the `--include-private` widening (issue #431). Reachability itself is
    // *not* re-derived here — it comes from `public_api_surface` above.
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
    // provenance and mints no tombstone for a between-commit removal, so a
    // temporal record is part of the current state only when its commit is its
    // repository's stamped HEAD — the same rule `resolve_head_symbols` and
    // `public_api_surface` use (issues #428/#430). Without this anchor the
    // `--include-private` widening below resurrects a private symbol deleted at
    // HEAD (issue #431). Snapshot-less stores (plain current-tree `scan`) keep
    // the conservative fallback: nodes resolve by keep-last dedupe by stable
    // ID.
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
    // A temporal code record is current only when its commit is its owning
    // repository's stamped HEAD. Fallbacks: no temporal provenance => current;
    // owner not in `repo_heads` (snapshot-less store) => keep everything.
    let owned_record_is_current = |id: &str, temporal: Option<&TemporalMetadata>| -> bool {
        let Some(t) = temporal else {
            return true;
        };
        index
            .owner_of(id)
            .and_then(|owner| repo_heads.get(owner))
            .is_none_or(|head_sha| t.git_commit == *head_sha)
    };

    let mut symbols: BTreeMap<&str, &'a GraphRecord> = BTreeMap::new();
    // Doc facts recorded at `pub use` sites: rustdoc exposes a doc comment
    // written above the re-export on the public item, so a site doc counts
    // as documentation for the re-exported symbol.
    let mut import_docs: BTreeMap<&str, &'a str> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            language,
            repo_relative_path,
            symbol_kind,
            doc,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        if language.as_deref() != Some("rust")
            || tombstoned.contains(id.as_str())
            || !is_owned(id)
            || !owned_record_is_current(id, temporal.as_ref())
            || !repo_relative_path
                .as_deref()
                .is_some_and(is_library_crate_path)
        {
            continue;
        }
        match kind {
            NodeKind::Symbol => {
                let auditable = symbol_kind
                    .as_deref()
                    .is_some_and(|k| PUBLIC_API_SYMBOL_KINDS.contains(&k) || k == "method");
                if !auditable {
                    continue;
                }
                symbols.insert(id.as_str(), record);
            }
            NodeKind::Import => {
                if let Some(doc) = doc.as_deref() {
                    import_docs.insert(id.as_str(), doc);
                }
            }
            _ => {}
        }
    }

    let mut report = UndocumentedReport::default();

    // Capability check: a store where no doc-auditable symbol carries the
    // issue #124 declaration surface never captured doc facts. Report that
    // verdict — never treat "no doc field" in a pre-#124 store as "no docs".
    let mut facts_recorded = 0usize;
    for record in symbols.values() {
        let (_, visibility) = symbol_doc_facts(record);
        if visibility.is_some() {
            facts_recorded += 1;
        } else {
            report.counts.doc_capture_missing += 1;
        }
    }
    report.capability_absent = facts_recorded == 0 && report.counts.doc_capture_missing > 0;

    // Surface diagnostics pass through so glob re-exports and unknown module
    // visibility stay visible; `empty_surface` is replaced by this lane's own
    // explicit verdicts.
    report.diagnostics.extend(
        surface
            .diagnostics
            .iter()
            .filter(|d| d.code != "empty_surface")
            .cloned(),
    );

    if report.capability_absent {
        report.diagnostics.push(PublicApiDiagnostic {
            code: "doc_capture_unavailable",
            record_id: None,
            detail: format!(
                "{} doc-auditable symbol record(s) carry no issue #124 declaration \
                 surface; doc-comment facts were never captured. Re-scan with a \
                 current build to audit documentation",
                report.counts.doc_capture_missing
            ),
        });
        sort_undocumented(&mut report);
        return report;
    }

    // IDs whose doc fact was already asserted via the surface, so the
    // `--include-private` widening never double-reports a symbol.
    let mut asserted: BTreeSet<&str> = BTreeSet::new();

    for item in surface.items {
        if item.kind == "module" {
            report.counts.modules_excluded += 1;
            continue;
        }
        // A doc comment at the `pub use` site documents the re-exported item
        // (rustdoc attaches it to the public name), regardless of whether the
        // target declaration carries its own doc.
        if item.via_reexport && import_docs.contains_key(item.record_id) {
            report.counts.considered += 1;
            report.counts.documented += 1;
            continue;
        }
        if item.kind == "reexport" {
            report.counts.reexports_unresolved += 1;
            report.diagnostics.push(PublicApiDiagnostic {
                code: "reexport_target_unresolved",
                record_id: Some(item.record_id.to_owned()),
                detail: format!(
                    "pub use target `{}` does not resolve in-graph; doc presence \
                     cannot be asserted for it",
                    item.target.as_deref().unwrap_or("")
                ),
            });
            continue;
        }
        // The record whose doc fact backs this row: the declaration itself,
        // or the resolved target for a re-export row. A re-export row without
        // a target record ID is unresolved; count it, never guess.
        let fact_id = match (item.via_reexport, item.target_record_id) {
            (false, _) => item.record_id,
            (true, Some(target_id)) => target_id,
            (true, None) => {
                report.counts.reexports_unresolved += 1;
                continue;
            }
        };
        let Some(record) = symbols.get(fact_id) else {
            // Resolved to a non-auditable record (e.g. a module alias row is
            // already handled above); never guess a doc fact.
            report.counts.reexports_unresolved += 1;
            continue;
        };
        let (doc, visibility) = symbol_doc_facts(record);
        if visibility.is_none() {
            // Pre-#124 record in a mixed store: already tallied in
            // `doc_capture_missing`; its doc fact cannot be asserted.
            continue;
        }
        asserted.insert(fact_id);
        report.counts.considered += 1;
        if doc.is_some() {
            report.counts.documented += 1;
            continue;
        }
        report.counts.undocumented += 1;
        if item.via_reexport {
            report.counts.reexports += 1;
        }
        report.items.push(UndocumentedItem {
            record_id: item.record_id,
            kind: item.kind,
            path: item.path,
            visibility: "public",
            repo_relative_path: item.repo_relative_path,
            span: item.span,
            signature: item.signature,
            evidence: vec!["externally_reachable", "doc_comment_absent"],
            via_reexport: item.via_reexport,
            target: item.target,
            target_record_id: item.target_record_id,
        });
    }

    if include_private {
        for (id, record) in &symbols {
            if asserted.contains(id) {
                continue;
            }
            let GraphRecord::Node {
                name: Some(name),
                repo_relative_path,
                span,
                symbol_kind: Some(symbol_kind),
                signature,
                ..
            } = record
            else {
                continue;
            };
            let (doc, visibility) = symbol_doc_facts(record);
            let Some(visibility) = visibility else {
                continue; // pre-#124 record: already tallied, never guessed.
            };
            report.counts.considered += 1;
            if doc.is_some() {
                report.counts.documented += 1;
                continue;
            }
            report.counts.undocumented += 1;
            report.items.push(UndocumentedItem {
                record_id: id,
                kind: symbol_kind.clone(),
                path: name.clone(),
                visibility,
                repo_relative_path: repo_relative_path.as_deref(),
                span: *span,
                signature: signature.as_deref(),
                evidence: vec!["doc_comment_absent"],
                via_reexport: false,
                target: None,
                target_record_id: None,
            });
        }
    }

    if report.items.is_empty() {
        // `no_undocumented_items` certifies the audit clean, so it requires
        // an audit with no blind spots. When unresolved re-exports or
        // missing doc capture left symbols unasserted, the empty result gets
        // an honest distinct verdict instead — still exit 0, never an error.
        let blind_spots =
            report.counts.reexports_unresolved > 0 || report.counts.doc_capture_missing > 0;
        report.diagnostics.push(if blind_spots {
            PublicApiDiagnostic {
                code: "empty_result_with_blind_spots",
                record_id: None,
                detail: format!(
                    "no undocumented symbols found, but the audit has blind spots \
                     ({} unresolved re-export(s), {} symbol record(s) without doc \
                     capture); this is not a certified-clean claim",
                    report.counts.reexports_unresolved, report.counts.doc_capture_missing
                ),
            }
        } else {
            PublicApiDiagnostic {
                code: "no_undocumented_items",
                record_id: None,
                detail: format!(
                    "every doc-auditable symbol in scope carries a recorded doc \
                     comment ({} considered)",
                    report.counts.considered
                ),
            }
        });
    }

    sort_undocumented(&mut report);

    if let Some(limit) = limit {
        if report.items.len() > limit {
            let total = report.items.len();
            report.items.truncate(limit);
            report.diagnostics.push(PublicApiDiagnostic {
                code: "results_truncated",
                record_id: None,
                detail: format!(
                    "showing {limit} of {total} undocumented rows; raise --limit \
                     to see the rest"
                ),
            });
            // Re-sort so the appended diagnostic keeps the stable order.
            sort_undocumented_diagnostics(&mut report.diagnostics);
        }
    }

    report
}

/// Sorts an undocumented report's rows and diagnostics deterministically.
fn sort_undocumented(report: &mut UndocumentedReport<'_>) {
    report.items.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.record_id.cmp(b.record_id))
    });
    sort_undocumented_diagnostics(&mut report.diagnostics);
}

/// Sorts and de-duplicates a diagnostics list deterministically.
fn sort_undocumented_diagnostics(diagnostics: &mut Vec<PublicApiDiagnostic>) {
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.record_id.cmp(&b.record_id))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    diagnostics.dedup();
}
