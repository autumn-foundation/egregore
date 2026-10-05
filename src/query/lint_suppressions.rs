use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::{RepositoryIndex, path_is_under_prefix};
use crate::ir::{GraphRecord, LintSuppressionScope, NodeKind};

// ── Lint-suppression inventory (issue #227) ─────────────────────────────────

/// Why the lint-suppression lane rejected its scope selectors.
///
/// Every variant carries a stable machine-readable code so an out-of-store
/// path, unknown commit, or malformed prefix is a documented diagnostic and
/// never a silent empty result (issue #196 honesty contract).
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum LintSuppressionScopeError {
    /// The path prefix is empty after stripping trailing slashes.
    MalformedPrefix {
        /// The prefix as supplied by the caller.
        prefix: String,
    },
    /// The path prefix matches no file in the selected store slice.
    ScopeNotFound {
        /// The normalized prefix that matched nothing.
        prefix: String,
    },
    /// No record in the selected store slice carries this commit.
    UnknownCommit {
        /// The commit selector as supplied by the caller.
        commit: String,
    },
    /// The commit prefix matches more than one commit.
    AmbiguousCommit {
        /// The commit selector as supplied by the caller.
        commit: String,
        /// Number of distinct commits matching the prefix.
        count: usize,
    },
}

impl LintSuppressionScopeError {
    /// Stable machine-readable diagnostic code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MalformedPrefix { .. } => "malformed_prefix",
            Self::ScopeNotFound { .. } => "scope_not_found",
            Self::UnknownCommit { .. } => "unknown_commit",
            Self::AmbiguousCommit { .. } => "ambiguous_commit",
        }
    }
}

/// One `#[allow(...)]` / `#![allow(...)]` suppression returned by
/// [`lint_suppressions`].
#[derive(Debug, Clone)]
pub struct LintSuppressionRow<'a> {
    /// The `LintSuppression` record itself.
    pub record: &'a GraphRecord,
    /// Sorted, deduplicated lint names silenced by the attribute
    /// (`dead_code`, `clippy::too_many_arguments`, …).
    pub lints: Vec<&'a str>,
    /// Closed scope class: `item` / `module` / `crate`.
    pub scope: &'a str,
    /// Adjacent justification-comment signal detected at extraction time.
    pub has_justification: bool,
    /// The symbol the suppression is attached to, resolved deterministically
    /// (issue #227): an inner `#![allow(...)]` applies to the *enclosing*
    /// item, so it resolves to the innermost symbol whose span contains the
    /// attribute; an outer `#[allow(...)]` annotates the *following* item —
    /// Tree-sitter item spans start at the item keyword, excluding preceding
    /// outer attributes — so it resolves to the nearest following symbol in
    /// the same file version. `None` when no symbol qualifies (crate scope,
    /// module scope with no enclosing symbol, or an outer attribute whose
    /// nearest following item is not a symbol, e.g. a `use` or `mod`).
    pub enclosing_symbol: Option<&'a GraphRecord>,
}

/// Deterministic lint-suppression inventory returned by [`lint_suppressions`].
#[derive(Debug, Clone, Default)]
pub struct LintSuppressionInventory<'a> {
    /// Suppressions ordered by `(repo_relative_path, span.start_byte,
    /// git_commit, record_id)` — byte-identical across repeated runs on an
    /// unchanged store.
    pub suppressions: Vec<LintSuppressionRow<'a>>,
    /// Full commit SHA the inventory was pinned to, when `--at` was supplied.
    pub at_commit: Option<String>,
}

/// Inventories `#[allow(...)]` / `#![allow(...)]` lint suppressions
/// (issue #227).
///
/// Results derive solely from deterministic `LintSuppression` extractor
/// facts: the lane never rewrites or re-scores a code fact, never judges
/// whether a suppression is justified, and introduces no agent-authored
/// observation. Strictly read-only.
///
/// * `path_prefix` — optional segment-aware repo-relative prefix (the same
///   matching contract as `eg query subsystem`).
/// * `at` — optional commit SHA or unique prefix pinning the valid-time axis
///   (the same selector contract as `eg query symbol --at`). Without it the
///   current view is returned: non-temporal records that are not tombstoned,
///   plus every history-backed version present in the store.
/// * `repo` — optional resolved repository record ID (issue #67 scoping).
///
/// # Errors
///
/// Returns [`LintSuppressionScopeError`] when the prefix is malformed or
/// matches nothing, or when the commit selector is unknown or ambiguous.
pub fn lint_suppressions<'a>(
    records: &'a [GraphRecord],
    path_prefix: Option<&str>,
    at: Option<&str>,
    index: &RepositoryIndex,
    repo: Option<&str>,
) -> Result<LintSuppressionInventory<'a>, LintSuppressionScopeError> {
    // ── GREEN: real implementation (issue #227) ──────────────────────────
    // 1. Prefix validation (mirrors the subsystem lane).
    let normalized_prefix = match path_prefix {
        Some(prefix) => {
            let normalized = prefix.trim_end_matches('/');
            if normalized.is_empty() {
                return Err(LintSuppressionScopeError::MalformedPrefix {
                    prefix: prefix.to_owned(),
                });
            }
            Some(normalized)
        }
        None => None,
    };

    let record_in_repo_scope = |record: &GraphRecord| -> bool {
        let Some(repo_id) = repo else { return true };
        match record {
            GraphRecord::Node { id, .. } => index.owner_of(id) == Some(repo_id),
            GraphRecord::Edge { source, target, .. } => {
                index.owner_of(source) == Some(repo_id) || index.owner_of(target) == Some(repo_id)
            }
            GraphRecord::Tombstone { .. } => false,
        }
    };

    // 2. Commit resolution (mirrors `eg query symbol --at` prefix handling,
    //    repo-scoped so a prefix colliding only across the repository boundary
    //    stays unambiguous within the selected repository).
    let at_commit: Option<String> = match at {
        None => None,
        Some(selector) => {
            let matching: BTreeSet<&str> = records
                .iter()
                .filter(|r| record_in_repo_scope(r))
                .filter_map(|r| match r {
                    GraphRecord::Node {
                        temporal: Some(t), ..
                    }
                    | GraphRecord::Edge {
                        temporal: Some(t), ..
                    } if t.git_commit.starts_with(selector) => Some(t.git_commit.as_str()),
                    _ => None,
                })
                .collect();
            match matching.len() {
                0 => {
                    return Err(LintSuppressionScopeError::UnknownCommit {
                        commit: selector.to_owned(),
                    });
                }
                1 => matching.iter().next().map(|c| (*c).to_owned()),
                count => {
                    return Err(LintSuppressionScopeError::AmbiguousCommit {
                        commit: selector.to_owned(),
                        count,
                    });
                }
            }
        }
    };

    // Latest-write-wins liveness (issues #421/#432): over an append-only
    // `--graph` a node re-ingested AFTER its own tombstone is live again,
    // matching the embedded `--data-dir` current-state read. The shared gate
    // reports a tombstone active only when it is the id's most recent write and
    // preserves the history/temporal exemption, so the `tombstoned` set below
    // retains a deleted_id only while its tombstone is still the latest write.
    // See `super::liveness`.
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

    // A node participates in the selected valid-time view when it belongs to
    // the pinned commit (`--at`), or — in the current view — when it is either
    // history-backed or a live (non-tombstoned) current-tree record.
    let in_selected_view = |record: &GraphRecord| -> bool {
        let GraphRecord::Node { id, temporal, .. } = record else {
            return false;
        };
        match (&at_commit, temporal) {
            (Some(commit), Some(t)) => t.git_commit == *commit,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => !tombstoned.contains(id.as_str()),
        }
    };

    // 3. Scope-existence honesty check: a prefix that matches no file-backed
    //    record in the selected view is `scope_not_found`, never a silent
    //    empty result.
    if let Some(prefix) = normalized_prefix {
        let scope_exists = records.iter().any(|r| {
            let GraphRecord::Node {
                kind,
                repo_relative_path: Some(path),
                ..
            } = r
            else {
                return false;
            };
            matches!(
                kind,
                NodeKind::File | NodeKind::Symbol | NodeKind::LintSuppression
            ) && record_in_repo_scope(r)
                && in_selected_view(r)
                && path_is_under_prefix(path.as_str(), prefix)
        });
        if !scope_exists {
            return Err(LintSuppressionScopeError::ScopeNotFound {
                prefix: prefix.to_owned(),
            });
        }
    }

    // 4. Symbol spans per path, for enclosing-symbol resolution.
    //
    // Coalesce non-temporal Symbol versions to the latest write per id (issue
    // #432): over an append-only `--graph` a symbol revived AFTER its own
    // tombstone leaves several physical writes whose spans can differ, and the
    // embedded `--data-dir` read exposes only the latest. Without coalescing, a
    // STALE pre-tombstone span could be accepted as the enclosing symbol (the
    // `same_file_version` current-view check reads only liveness, not version
    // recency), diverging from `--data-dir`. History-backed (temporal) versions
    // are kept individually — `same_file_version` already keys them by commit.
    let latest_symbol: BTreeMap<&str, usize> = records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| match r {
            GraphRecord::Node {
                kind: NodeKind::Symbol,
                id,
                temporal: None,
                span: Some(_),
                repo_relative_path: Some(_),
                ..
            } => Some((id.as_str(), i)),
            _ => None,
        })
        .collect();
    let mut symbols_by_path: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
    for (i, record) in records.iter().enumerate() {
        if let GraphRecord::Node {
            kind: NodeKind::Symbol,
            id,
            repo_relative_path: Some(path),
            span: Some(_),
            temporal,
            ..
        } = record
        {
            if temporal.is_none() && latest_symbol.get(id.as_str()) != Some(&i) {
                continue;
            }
            symbols_by_path
                .entry(path.as_str())
                .or_default()
                .push(record);
        }
    }

    // Module/Import nodes per path, for the outer-attribute blocker check:
    // an outer `#[allow]` whose nearest following item is a `use` or `mod`
    // annotates that node, not a symbol, so a Module/Import node starting
    // between the attribute and the nearest following symbol vetoes the
    // attribution (the row honestly reports no enclosing symbol).
    let mut blockers_by_path: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Module | NodeKind::Import,
            repo_relative_path: Some(path),
            span: Some(_),
            ..
        } = record
        {
            blockers_by_path
                .entry(path.as_str())
                .or_default()
                .push(record);
        }
    }

    let same_file_version = |row: &GraphRecord, symbol: &GraphRecord| -> bool {
        let row_commit = match row {
            GraphRecord::Node { temporal, .. } => temporal.as_ref().map(|t| t.git_commit.as_str()),
            _ => None,
        };
        let symbol_commit = match symbol {
            GraphRecord::Node { temporal, .. } => temporal.as_ref().map(|t| t.git_commit.as_str()),
            _ => None,
        };
        match (row_commit, symbol_commit) {
            (Some(row_sha), Some(symbol_sha)) => row_sha == symbol_sha,
            (None, None) => !tombstoned.contains(symbol.id()),
            _ => false,
        }
    };

    // Latest-write-wins coalescing (issue #432): over an append-only `--graph`
    // the same non-temporal `LintSuppression` id can appear as several physical
    // writes — a re-ingest, or a re-add after a tombstone. The embedded
    // `--data-dir` read collapses these to the latest version, so emit only the
    // latest write per id here too; without this the lane over-counts one live
    // suppression as several rows (the mirror of the pre-fix tombstone under-count).
    // Temporal history versions are keyed by commit and kept individually.
    let latest_nontemporal: BTreeMap<&str, usize> = records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| match r {
            GraphRecord::Node {
                kind: NodeKind::LintSuppression,
                id,
                temporal: None,
                ..
            } => Some((id.as_str(), i)),
            _ => None,
        })
        .collect();

    // 5. Collect, resolve enclosing symbols, and order deterministically.
    let mut suppressions: Vec<LintSuppressionRow<'a>> = Vec::new();
    for (record_index, record) in records.iter().enumerate() {
        let GraphRecord::Node {
            kind: NodeKind::LintSuppression,
            id,
            lint_suppression: Some(facts),
            repo_relative_path: Some(path),
            span: Some(suppression_span),
            temporal,
            ..
        } = record
        else {
            continue;
        };
        if temporal.is_none() && latest_nontemporal.get(id.as_str()) != Some(&record_index) {
            continue;
        }
        if !record_in_repo_scope(record) || !in_selected_view(record) {
            continue;
        }
        if let Some(prefix) = normalized_prefix
            && !path_is_under_prefix(path.as_str(), prefix)
        {
            continue;
        }

        // Enclosing-symbol resolution (issue #227): an inner `#![allow(...)]`
        // applies to the *enclosing* item, so it resolves by span containment
        // like the other structural lanes; an outer `#[allow(...)]`
        // annotates the *following* item — Tree-sitter item spans start at
        // the item keyword, excluding preceding outer attributes — so it
        // resolves to the nearest following symbol in the same file version.
        // A Module/Import node starting between the attribute and that
        // candidate means the attribute annotates the nearer node, not a
        // symbol, and the row honestly reports no enclosing symbol.
        let enclosing_symbol = if facts.is_inner || facts.scope != LintSuppressionScope::Item {
            symbols_by_path
                .get(path.as_str())
                .into_iter()
                .flatten()
                .filter(|symbol| {
                    let GraphRecord::Node {
                        span: Some(symbol_span),
                        ..
                    } = symbol
                    else {
                        return false;
                    };
                    symbol_span.start_byte <= suppression_span.start_byte
                        && suppression_span.end_byte <= symbol_span.end_byte
                        && same_file_version(record, symbol)
                        // Repository-boundary honesty: two repositories can share
                        // a repo-relative path (and, for clones of one history,
                        // even a commit SHA), so the enclosing symbol must belong
                        // to the suppression's own repository — never a same-path
                        // symbol from another repository.
                        && index.owner_of(symbol.id()) == index.owner_of(record.id())
                })
                .min_by(|a, b| {
                    let width = |r: &GraphRecord| match r {
                        GraphRecord::Node {
                            span: Some(span), ..
                        } => span.end_byte - span.start_byte,
                        _ => usize::MAX,
                    };
                    width(a).cmp(&width(b)).then_with(|| a.id().cmp(b.id()))
                })
                .copied()
        } else {
            let candidate = symbols_by_path
                .get(path.as_str())
                .into_iter()
                .flatten()
                .filter(|symbol| {
                    let GraphRecord::Node {
                        span: Some(symbol_span),
                        ..
                    } = symbol
                    else {
                        return false;
                    };
                    symbol_span.start_byte >= suppression_span.end_byte
                        && same_file_version(record, symbol)
                        && index.owner_of(symbol.id()) == index.owner_of(record.id())
                })
                .min_by(|a, b| {
                    let start = |r: &GraphRecord| match r {
                        GraphRecord::Node {
                            span: Some(span), ..
                        } => span.start_byte,
                        _ => usize::MAX,
                    };
                    start(a).cmp(&start(b)).then_with(|| a.id().cmp(b.id()))
                })
                .copied();
            candidate.and_then(|symbol| {
                let candidate_start = match symbol {
                    GraphRecord::Node {
                        span: Some(span), ..
                    } => span.start_byte,
                    _ => usize::MAX,
                };
                let blocked = blockers_by_path
                    .get(path.as_str())
                    .into_iter()
                    .flatten()
                    .any(|blocker| {
                        let GraphRecord::Node {
                            span: Some(blocker_span),
                            ..
                        } = blocker
                        else {
                            return false;
                        };
                        blocker_span.start_byte > suppression_span.end_byte
                            && blocker_span.start_byte < candidate_start
                            && same_file_version(record, blocker)
                            && index.owner_of(blocker.id()) == index.owner_of(record.id())
                    });
                (!blocked).then_some(symbol)
            })
        };

        suppressions.push(LintSuppressionRow {
            record,
            lints: facts.lints.iter().map(String::as_str).collect(),
            scope: facts.scope.as_str(),
            has_justification: facts.has_justification,
            enclosing_symbol,
        });
    }

    suppressions.sort_by(|a, b| lint_suppression_sort_key(a).cmp(&lint_suppression_sort_key(b)));

    Ok(LintSuppressionInventory {
        suppressions,
        at_commit,
    })
}

/// Deterministic ordering key for lint-suppression rows:
/// `(repo_relative_path, span.start_byte, git_commit, record_id)`.
fn lint_suppression_sort_key<'k>(
    row: &LintSuppressionRow<'k>,
) -> (&'k str, usize, &'k str, &'k str) {
    match row.record {
        GraphRecord::Node {
            repo_relative_path,
            span,
            temporal,
            id,
            ..
        } => (
            repo_relative_path.as_deref().unwrap_or(""),
            span.map_or(0, |s| s.start_byte),
            temporal.as_ref().map_or("", |t| t.git_commit.as_str()),
            id.as_str(),
        ),
        _ => ("", 0, "", ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{LintSuppressionFacts, LintSuppressionScope, SCHEMA_VERSION, SourceSpan};

    fn span(start: usize, end: usize) -> SourceSpan {
        SourceSpan {
            start_byte: start,
            end_byte: end,
            start_line: 1,
            end_line: 2,
            start_column: None,
            end_column: None,
        }
    }

    fn suppression(
        id: &str,
        lints: &[&str],
        scope: LintSuppressionScope,
        justified: bool,
        is_inner: bool,
        start: usize,
        end: usize,
    ) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::LintSuppression,
            Some("src/lib.rs".to_owned()),
            Some(span(start, end)),
            Some(scope.as_str().to_owned()),
            "lint suppression".to_owned(),
        )
        .with_lint_suppression(LintSuppressionFacts {
            lints: lints.iter().map(|s| (*s).to_owned()).collect(),
            scope,
            has_justification: justified,
            is_inner,
        })
    }

    fn symbol(id: &str, name: &str, start: usize, end: usize) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            Some(span(start, end)),
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
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

    fn inventory_of(records: &[GraphRecord]) -> LintSuppressionInventory<'_> {
        let index = RepositoryIndex::build(records);
        lint_suppressions(records, None, None, &index, None).expect("inventory")
    }

    #[test]
    fn inventory_lists_rows_with_facts_and_enclosing_symbol() {
        let records = vec![
            // Outer attribute: annotates the FOLLOWING item, so the symbol
            // span starts after the attribute span (Tree-sitter item spans
            // exclude preceding outer attributes).
            suppression(
                "codegraph:v6:sup_a",
                &["dead_code"],
                LintSuppressionScope::Item,
                true,
                false,
                10,
                30,
            ),
            symbol("codegraph:v6:sym_helper", "helper", 40, 100),
            suppression(
                "codegraph:v6:sup_b",
                &["clippy::too_many_arguments", "dead_code"],
                LintSuppressionScope::Module,
                false,
                true,
                200,
                230,
            ),
        ];
        let inv = inventory_of(&records);
        assert_eq!(inv.suppressions.len(), 2, "both suppressions are listed");
        let item_row = &inv.suppressions[0];
        assert_eq!(item_row.lints, vec!["dead_code"]);
        assert_eq!(item_row.scope, "item");
        assert!(item_row.has_justification);
        assert_eq!(
            item_row.enclosing_symbol.map(GraphRecord::id),
            Some("codegraph:v6:sym_helper"),
            "an outer item suppression resolves to the following symbol"
        );
        let module_row = &inv.suppressions[1];
        assert_eq!(
            module_row.lints,
            vec!["clippy::too_many_arguments", "dead_code"]
        );
        assert_eq!(module_row.scope, "module");
        assert!(!module_row.has_justification);
        assert!(
            module_row.enclosing_symbol.is_none(),
            "no symbol encloses the module-scope span"
        );
    }

    #[test]
    fn inner_suppression_resolves_to_containing_symbol() {
        // `#![allow]` inside a function body applies to the enclosing item.
        let records = vec![
            symbol("codegraph:v6:sym_outer", "outer", 0, 100),
            suppression(
                "codegraph:v6:sup_inner",
                &["dead_code"],
                LintSuppressionScope::Item,
                false,
                true,
                10,
                30,
            ),
        ];
        let inv = inventory_of(&records);
        assert_eq!(inv.suppressions.len(), 1);
        assert_eq!(
            inv.suppressions[0].enclosing_symbol.map(GraphRecord::id),
            Some("codegraph:v6:sym_outer"),
            "an inner item suppression resolves to the containing symbol"
        );
    }

    #[test]
    fn outer_suppression_before_import_reports_no_symbol() {
        // `#[allow(unused_imports)] use a::b;` annotates the import, not the
        // next function: the nearer Import node vetoes the attribution.
        let records = vec![
            suppression(
                "codegraph:v6:sup_use",
                &["unused_imports"],
                LintSuppressionScope::Item,
                false,
                false,
                10,
                30,
            ),
            GraphRecord::node(
                "codegraph:v6:imp_a".to_owned(),
                NodeKind::Import,
                Some("src/lib.rs".to_owned()),
                Some(span(35, 45)),
                Some("a::b".to_owned()),
                "import a::b".to_owned(),
            ),
            symbol("codegraph:v6:sym_later", "later", 60, 100),
        ];
        let inv = inventory_of(&records);
        assert_eq!(inv.suppressions.len(), 1);
        assert!(
            inv.suppressions[0].enclosing_symbol.is_none(),
            "an outer attribute annotating a nearer import reports no enclosing symbol"
        );
    }

    #[test]
    fn inventory_is_deterministically_ordered() {
        let records = vec![
            suppression(
                "codegraph:v6:sup_b",
                &["dead_code"],
                LintSuppressionScope::Item,
                false,
                false,
                200,
                210,
            ),
            suppression(
                "codegraph:v6:sup_a",
                &["dead_code"],
                LintSuppressionScope::Item,
                false,
                false,
                10,
                20,
            ),
        ];
        let inv = inventory_of(&records);
        assert_eq!(
            inv.suppressions
                .iter()
                .map(|r| r.record.id())
                .collect::<Vec<_>>(),
            vec!["codegraph:v6:sup_a", "codegraph:v6:sup_b"],
            "rows order by (path, start_byte, commit, id)"
        );
    }

    #[test]
    fn malformed_prefix_is_an_error() {
        let records: Vec<GraphRecord> = vec![];
        let index = RepositoryIndex::build(&records);
        let err = lint_suppressions(&records, Some("/"), None, &index, None)
            .expect_err("a slash-only prefix is malformed");
        assert_eq!(err.code(), "malformed_prefix");
    }

    #[test]
    fn unknown_prefix_is_scope_not_found() {
        let records = vec![suppression(
            "codegraph:v6:sup_a",
            &["dead_code"],
            LintSuppressionScope::Item,
            false,
            false,
            10,
            20,
        )];
        let index = RepositoryIndex::build(&records);
        let err = lint_suppressions(&records, Some("no/such/dir"), None, &index, None)
            .expect_err("an unmatched prefix is scope_not_found");
        assert_eq!(err.code(), "scope_not_found");
    }

    #[test]
    fn tombstoned_suppression_is_excluded() {
        let id = "codegraph:v6:sup_a";
        let records = vec![
            suppression(
                id,
                &["dead_code"],
                LintSuppressionScope::Item,
                false,
                false,
                10,
                20,
            ),
            tomb(id),
        ];
        let inv = inventory_of(&records);
        assert!(
            inv.suppressions.is_empty(),
            "a tombstoned suppression stays deleted"
        );
    }

    #[test]
    fn duplicate_writes_coalesce_to_one_row() {
        let id = "codegraph:v6:sup_a";
        let records = vec![
            suppression(
                id,
                &["dead_code"],
                LintSuppressionScope::Item,
                false,
                false,
                10,
                20,
            ),
            suppression(
                id,
                &["dead_code"],
                LintSuppressionScope::Item,
                false,
                false,
                10,
                20,
            ),
        ];
        let inv = inventory_of(&records);
        assert_eq!(
            inv.suppressions.len(),
            1,
            "latest-write-wins coalescing matches the --data-dir read"
        );
    }
}
