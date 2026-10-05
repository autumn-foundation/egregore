// ---------------------------------------------------------------------------
// Stub and panic-risk macro marker triage inventory (issue #210)
// ---------------------------------------------------------------------------

use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::{RepositoryIndex, path_is_under_prefix};
use crate::ir::{GraphRecord, NodeKind};

/// Closed machine-readable risk category for a classified macro marker.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RiskCategory {
    /// `todo!` / `unimplemented!` / `unreachable!` — unfinished-stub markers.
    Stub,
    /// `panic!` — explicit panic points.
    Panic,
}

impl RiskCategory {
    /// Stable machine-readable category string (`stub` / `panic`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stub => "stub",
            Self::Panic => "panic",
        }
    }
}

/// Known-risk stub macros, the closed set for this slice (issue #210).
const STUB_MACROS: [&str; 3] = ["todo!", "unimplemented!", "unreachable!"];

/// Known-risk panic macros, the closed set for this slice (issue #210).
const PANIC_MACROS: [&str; 1] = ["panic!"];

/// The documented closed known-risk macro set for this slice, in canonical
/// (sorted) order. Carried on the JSON response envelope so agents can see
/// exactly which macros this lane classifies.
pub const RISK_MACRO_SET: [&str; 4] = ["panic!", "todo!", "unimplemented!", "unreachable!"];

/// Summary prefix the extractor stamps on macro-invocation diagnostics.
const MACRO_DIAGNOSTIC_SUMMARY_PREFIX: &str = "unsupported macro invocation ";

/// Why the risk-marker lane rejected its scope selectors.
///
/// Every variant carries a stable machine-readable code so an out-of-store
/// path, unknown commit, or malformed prefix is a documented diagnostic and
/// never a silent empty result (issue #196 honesty contract).
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RiskMarkerScopeError {
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

impl RiskMarkerScopeError {
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

/// One risk-classified macro marker returned by [`risk_markers`].
#[derive(Debug, Clone)]
pub struct RiskMarkerRow<'a> {
    /// The `Diagnostic` record itself.
    pub record: &'a GraphRecord,
    /// Closed machine-readable category: `stub` / `panic`.
    pub category: RiskCategory,
    /// The macro name as recorded by the extractor (e.g. `todo!`).
    pub macro_name: &'a str,
    /// Innermost `Symbol` record whose span encloses the marker in the same
    /// file version; `None` when the marker sits at module top level (no
    /// `DEFINES` owner).
    pub enclosing_symbol: Option<&'a GraphRecord>,
    /// Repository attribution for the marker. The scanner wires no
    /// containment edges for `Diagnostic` nodes, so this falls back to the
    /// owning `File`'s repository (unambiguous paths only); always `Some`
    /// when the query is repository-scoped, since the scope filter has
    /// already established membership.
    pub repository_id: Option<&'a str>,
}

/// Deterministic risk-marker inventory returned by [`risk_markers`].
#[derive(Debug, Clone, Default)]
pub struct RiskMarkerInventory<'a> {
    /// Markers ordered by `(repo_relative_path, span.start_byte, git_commit,
    /// record_id)` — byte-identical across repeated runs on an unchanged store.
    pub markers: Vec<RiskMarkerRow<'a>>,
    /// Full commit SHA the inventory was pinned to, when `--at` was supplied.
    pub at_commit: Option<String>,
}

/// Classifies a macro invocation into the closed risk set (issue #210).
///
/// Exact and conservative:
/// - only `Diagnostic` records whose summary names *exactly this macro*
///   (`unsupported macro invocation todo!` for name `todo!`) can classify, so
///   a non-macro diagnostic whose name happens to end in `!`, or a record
///   whose name and summary disagree, never risks a false positive;
/// - path-qualified names reduce to their final `::` segment (`std::panic!`
///   is still a `panic!`), but matching is exact — `mytodo!` and
///   `panic_info!` never match `todo!` / `panic!` by substring;
/// - `assert!` / `assert_eq!` (conditional panics) and `.unwrap()` /
///   `.expect()` (method calls, not macros) are deliberately outside the
///   closed set: benign macros always return `None` (zero false-risk).
#[must_use]
pub fn risk_category(macro_name: &str, summary: &str) -> Option<RiskCategory> {
    // Exact summary/name consistency: the summary must stamp exactly this
    // macro's invocation. A record whose name and summary disagree — a
    // mismatched or hand-built diagnostic — classifies as nothing.
    if summary.strip_prefix(MACRO_DIAGNOSTIC_SUMMARY_PREFIX) != Some(macro_name) {
        return None;
    }
    let base = macro_name.rsplit("::").next()?;
    if STUB_MACROS.contains(&base) {
        Some(RiskCategory::Stub)
    } else if PANIC_MACROS.contains(&base) {
        Some(RiskCategory::Panic)
    } else {
        None
    }
}

/// Inventories stub and panic-risk macro markers (issue #210).
///
/// Results derive solely from deterministic extractor `Diagnostic` facts: the
/// lane never rewrites or re-scores a code fact, never asserts the
/// surrounding code is correct or incorrect, and introduces no agent-authored
/// observation. Each row asserts only that a marker of the given category
/// exists at the given span. Strictly read-only.
///
/// * `path_prefix` — optional segment-aware repo-relative prefix (the same
///   matching contract as `eg query subsystem`).
/// * `at` — optional commit SHA or unique prefix pinning the valid-time axis
///   (the same selector contract as `eg query symbol --at`). Without it the
///   current view is returned: non-temporal records that are not tombstoned,
///   plus every history-backed version present in the store.
/// * `repo` — optional resolved repository record ID (issue #67 scoping).
///   The scanner wires no containment edges for `Diagnostic` nodes, so
///   markers are attributed through their owning `File`: a marker is in
///   scope when its file is. Rows carry the attribution in
///   [`RiskMarkerRow::repository_id`].
///
/// # Errors
///
/// Returns [`RiskMarkerScopeError`] when the prefix is malformed or matches
/// nothing, or when the commit selector is unknown or ambiguous.
pub fn risk_markers<'a>(
    records: &'a [GraphRecord],
    path_prefix: Option<&str>,
    at: Option<&str>,
    index: &'a RepositoryIndex,
    repo: Option<&'a str>,
) -> Result<RiskMarkerInventory<'a>, RiskMarkerScopeError> {
    // 1. Prefix validation (mirrors the subsystem lane).
    let normalized_prefix = match path_prefix {
        Some(prefix) => {
            let normalized = prefix.trim_end_matches('/');
            if normalized.is_empty() {
                return Err(RiskMarkerScopeError::MalformedPrefix {
                    prefix: prefix.to_owned(),
                });
            }
            Some(normalized)
        }
        None => None,
    };

    // Strict owner-based repository filter, used for commit resolution: a
    // commit that touched the repository always leaves temporal file/symbol
    // records carrying repository topology, so the strict filter finds it.
    let repo_filter_strict = |record: &GraphRecord| -> bool {
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
                .filter(|r| repo_filter_strict(r))
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
                    return Err(RiskMarkerScopeError::UnknownCommit {
                        commit: selector.to_owned(),
                    });
                }
                1 => matching.iter().next().map(|c| (*c).to_owned()),
                count => {
                    return Err(RiskMarkerScopeError::AmbiguousCommit {
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

    // File-mediated repository membership. The scanner wires no containment
    // edges for `Diagnostic` nodes (`owner_of` is `None` for every marker),
    // so a marker is attributed to the selected repository through its file:
    // the marker textually lives in that file, and the `File` node carries
    // the repository topology. Map each repo-relative path in the selected
    // view to the set of owning repository IDs (a set, because two
    // repositories in one store can share a path).
    let file_owners: BTreeMap<&str, BTreeSet<&str>> = {
        let mut map: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for record in records.iter().filter(|r| in_selected_view(r)) {
            if let GraphRecord::Node {
                kind: NodeKind::File,
                id,
                repo_relative_path: Some(path),
                ..
            } = record
            {
                if let Some(owner) = index.owner_of(id) {
                    map.entry(path.as_str()).or_default().insert(owner);
                }
            }
        }
        map
    };
    // The single unambiguous owning repository for a path, if exactly one
    // repository owns every live `File` node at that path.
    let single_file_owner = |path: &str| -> Option<&str> {
        let owners = file_owners.get(path)?;
        if owners.len() == 1 {
            owners.iter().next().copied()
        } else {
            None
        }
    };

    let record_in_repo_scope = |record: &GraphRecord| -> bool {
        let Some(repo_id) = repo else { return true };
        match record {
            GraphRecord::Node {
                id,
                repo_relative_path: Some(path),
                ..
            } => {
                index.owner_of(id) == Some(repo_id)
                    || file_owners
                        .get(path.as_str())
                        .is_some_and(|owners| owners.contains(repo_id))
            }
            GraphRecord::Node { id, .. } => index.owner_of(id) == Some(repo_id),
            GraphRecord::Edge { source, target, .. } => {
                index.owner_of(source) == Some(repo_id) || index.owner_of(target) == Some(repo_id)
            }
            GraphRecord::Tombstone { .. } => false,
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
                NodeKind::File | NodeKind::Symbol | NodeKind::Diagnostic
            ) && record_in_repo_scope(r)
                && in_selected_view(r)
                && path_is_under_prefix(path.as_str(), prefix)
        });
        if !scope_exists {
            return Err(RiskMarkerScopeError::ScopeNotFound {
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
    // STALE pre-tombstone span could be accepted as the enclosing symbol,
    // diverging from `--data-dir`. History-backed (temporal) versions are kept
    // individually — `same_file_version` already keys them by commit.
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

    let same_file_version = |marker: &GraphRecord, symbol: &GraphRecord| -> bool {
        let marker_commit = match marker {
            GraphRecord::Node { temporal, .. } => temporal.as_ref().map(|t| t.git_commit.as_str()),
            _ => None,
        };
        let symbol_commit = match symbol {
            GraphRecord::Node { temporal, .. } => temporal.as_ref().map(|t| t.git_commit.as_str()),
            _ => None,
        };
        match (marker_commit, symbol_commit) {
            (Some(marker_sha), Some(symbol_sha)) => marker_sha == symbol_sha,
            (None, None) => !tombstoned.contains(symbol.id()),
            _ => false,
        }
    };

    // Latest-write-wins coalescing (issue #432): over an append-only `--graph`
    // the same non-temporal `Diagnostic` id can appear as several physical
    // writes — a re-ingest, or a re-add after a tombstone. The embedded
    // `--data-dir` read collapses these to the latest version, so emit only
    // the latest write per id here too; without this the lane over-counts one
    // live marker as several rows. Temporal history versions are keyed by
    // commit and kept individually.
    let latest_nontemporal: BTreeMap<&str, usize> = records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| match r {
            GraphRecord::Node {
                kind: NodeKind::Diagnostic,
                id,
                temporal: None,
                ..
            } => Some((id.as_str(), i)),
            _ => None,
        })
        .collect();

    // 5. Collect, classify, resolve enclosing symbols, and order
    //    deterministically.
    let mut markers: Vec<RiskMarkerRow<'a>> = Vec::new();
    for (record_index, record) in records.iter().enumerate() {
        let GraphRecord::Node {
            kind: NodeKind::Diagnostic,
            id,
            name: Some(macro_name),
            summary,
            repo_relative_path: Some(path),
            span: Some(marker_span),
            temporal,
            ..
        } = record
        else {
            continue;
        };
        let Some(category) = risk_category(macro_name, summary) else {
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

        let enclosing_symbol = symbols_by_path
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
                symbol_span.start_byte <= marker_span.start_byte
                    && marker_span.end_byte <= symbol_span.end_byte
                    && same_file_version(record, symbol)
                    // Repository guard: the scanner provides no
                    // diagnostic↔repository topology, so same-path span
                    // containment is the attribution signal (as in
                    // debt_markers). When the query is repository-scoped, a
                    // candidate symbol positively owned by a DIFFERENT
                    // repository is still excluded — it cannot enclose this
                    // repository's marker.
                    && repo.is_none_or(|selected| {
                        index.owner_of(symbol.id()).is_none_or(|owner| owner == selected)
                    })
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
            .copied();

        // Repository attribution for the row: the direct owner when the
        // scanner provides one, else the owning file's repository for
        // unambiguous paths. Under `--repo` every surviving marker is in the
        // selected repository by construction of the scope filter.
        let repository_id: Option<&str> = repo
            .or_else(|| index.owner_of(id))
            .or_else(|| single_file_owner(path.as_str()));

        markers.push(RiskMarkerRow {
            record,
            category,
            macro_name,
            enclosing_symbol,
            repository_id,
        });
    }

    markers.sort_by(|a, b| risk_marker_sort_key(a).cmp(&risk_marker_sort_key(b)));

    Ok(RiskMarkerInventory { markers, at_commit })
}

/// Deterministic ordering key for risk-marker rows:
/// `(repo_relative_path, span.start_byte, git_commit, record_id)`.
fn risk_marker_sort_key<'k>(marker: &RiskMarkerRow<'k>) -> (&'k str, usize, &'k str, &'k str) {
    match marker.record {
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
    use crate::ir::{EdgeLabel, SourceSpan, TemporalMetadata};

    const MACRO_SUMMARY_PANIC: &str = "unsupported macro invocation panic!";

    fn span(start_byte: usize, end_byte: usize, start_line: usize) -> SourceSpan {
        SourceSpan {
            start_byte,
            end_byte,
            start_line,
            end_line: start_line,
            start_column: None,
            end_column: None,
        }
    }

    fn macro_diagnostic(id: &str, path: &str, macro_name: &str, span: SourceSpan) -> GraphRecord {
        GraphRecord::syntax_node(
            id.to_owned(),
            NodeKind::Diagnostic,
            path.to_owned(),
            span,
            macro_name.to_owned(),
            "rust",
            format!("unsupported macro invocation {macro_name}"),
        )
    }

    fn temporal(commit: &str) -> TemporalMetadata {
        TemporalMetadata {
            git_commit: commit.to_owned(),
            git_parent_commits: Vec::new(),
            valid_time: "2026-01-01T00:00:00Z".to_owned(),
            author_time: Some("2026-01-01T00:00:00Z".to_owned()),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_time_source: Some("git_commit_committer_date".to_owned()),
        }
    }

    fn symbol(id: &str, path: &str, name: &str, span: SourceSpan) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(path.to_owned()),
            Some(span),
            Some(name.to_owned()),
            format!("fn {name}"),
        )
    }

    #[test]
    fn classify_stub_macros() {
        for macro_name in ["todo!", "unimplemented!", "unreachable!"] {
            let summary = format!("unsupported macro invocation {macro_name}");
            assert_eq!(
                risk_category(macro_name, &summary),
                Some(RiskCategory::Stub),
                "{macro_name} must classify as stub"
            );
        }
    }

    #[test]
    fn classify_panic_macro() {
        assert_eq!(
            risk_category("panic!", MACRO_SUMMARY_PANIC),
            Some(RiskCategory::Panic)
        );
    }

    #[test]
    fn classify_path_qualified_macro_names() {
        // Path qualification never changes the verdict: `std::panic!` is
        // still an explicit panic, `core::todo!` is still a stub.
        assert_eq!(
            risk_category("std::panic!", "unsupported macro invocation std::panic!"),
            Some(RiskCategory::Panic)
        );
        assert_eq!(
            risk_category("core::todo!", "unsupported macro invocation core::todo!"),
            Some(RiskCategory::Stub)
        );
    }

    #[test]
    fn classify_never_false_risks_benign_macros() {
        // Benign macros — including lookalikes and conditional panics — must
        // never classify. Zero false-risk by construction.
        for macro_name in [
            "println!",
            "eprintln!",
            "vec!",
            "format!",
            "write!",
            "assert!",
            "assert_eq!",
            "debug_assert!",
            "mytodo!",
            "panic_info!",
            "todo",  // missing the macro `!`
            "panic", // missing the macro `!`
            "",      // empty name
        ] {
            let summary = format!("unsupported macro invocation {macro_name}");
            assert_eq!(
                risk_category(macro_name, &summary),
                None,
                "{macro_name} must never classify as a risk marker"
            );
        }
    }

    #[test]
    fn classify_requires_macro_diagnostic_summary() {
        // A non-macro diagnostic whose name ends in `!` can never classify:
        // the summary gate keeps the lane to real macro invocations.
        assert_eq!(
            risk_category("panic!", "unresolved call some_crate::panic!"),
            None
        );
        assert_eq!(risk_category("todo!", "something else entirely"), None);
    }

    #[test]
    fn classify_requires_exact_summary_name_consistency() {
        // A record whose name and summary disagree classifies as nothing —
        // a mismatched or hand-built diagnostic is not a risk marker.
        assert_eq!(
            risk_category("todo!", "unsupported macro invocation panic!"),
            None
        );
        assert_eq!(
            risk_category("panic!", "unsupported macro invocation todo!"),
            None
        );
        // Extra suffix on the summary is not an exact match.
        assert_eq!(
            risk_category("todo!", "unsupported macro invocation todo! extra"),
            None
        );
    }

    #[test]
    fn inventory_returns_only_risk_classified_markers() {
        let records = vec![
            macro_diagnostic("m-todo", "src/a.rs", "todo!", span(10, 20, 2)),
            macro_diagnostic("m-print", "src/a.rs", "println!", span(30, 45, 3)),
            macro_diagnostic("m-vec", "src/a.rs", "vec!", span(50, 60, 4)),
            macro_diagnostic("m-fmt", "src/b.rs", "format!", span(0, 12, 1)),
            macro_diagnostic("m-panic", "src/b.rs", "panic!", span(20, 32, 2)),
        ];
        let index = RepositoryIndex::build(&records);
        let inventory = risk_markers(&records, None, None, &index, None).expect("inventory");
        let rows: Vec<(&str, RiskCategory)> = inventory
            .markers
            .iter()
            .map(|m| (m.record.id(), m.category))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("m-todo", RiskCategory::Stub),
                ("m-panic", RiskCategory::Panic)
            ]
        );
    }

    #[test]
    fn inventory_never_mixes_categories() {
        // A `panic!` is never categorized as `stub` and vice versa.
        let records = vec![
            macro_diagnostic("m-todo", "src/a.rs", "todo!", span(10, 20, 2)),
            macro_diagnostic("m-panic", "src/a.rs", "panic!", span(30, 42, 3)),
            macro_diagnostic("m-unreachable", "src/a.rs", "unreachable!", span(50, 68, 4)),
        ];
        let index = RepositoryIndex::build(&records);
        let inventory = risk_markers(&records, None, None, &index, None).expect("inventory");
        let by_id: std::collections::BTreeMap<&str, RiskCategory> = inventory
            .markers
            .iter()
            .map(|m| (m.record.id(), m.category))
            .collect();
        assert_eq!(by_id["m-todo"], RiskCategory::Stub);
        assert_eq!(by_id["m-unreachable"], RiskCategory::Stub);
        assert_eq!(by_id["m-panic"], RiskCategory::Panic);
    }

    #[test]
    fn inventory_resolves_enclosing_symbol_or_none() {
        let records = vec![
            symbol("sym-f", "src/a.rs", "finish", span(0, 200, 1)),
            macro_diagnostic("m-inside", "src/a.rs", "todo!", span(20, 30, 2)),
            macro_diagnostic("m-top", "src/a.rs", "panic!", span(210, 222, 8)),
        ];
        let index = RepositoryIndex::build(&records);
        let inventory = risk_markers(&records, None, None, &index, None).expect("inventory");
        let by_id: std::collections::BTreeMap<&str, Option<&str>> = inventory
            .markers
            .iter()
            .map(|m| (m.record.id(), m.enclosing_symbol.map(GraphRecord::id)))
            .collect();
        assert_eq!(by_id["m-inside"], Some("sym-f"));
        // Module-top-level markers carry an explicit none.
        assert_eq!(by_id["m-top"], None);
    }

    #[test]
    fn inventory_sorts_canonically() {
        let records = vec![
            macro_diagnostic("m-z", "src/z.rs", "todo!", span(0, 10, 1)),
            macro_diagnostic("m-b", "src/a.rs", "panic!", span(90, 100, 9)),
            macro_diagnostic("m-a", "src/a.rs", "todo!", span(10, 20, 2)),
        ];
        let index = RepositoryIndex::build(&records);
        let inventory = risk_markers(&records, None, None, &index, None).expect("inventory");
        let ids: Vec<&str> = inventory.markers.iter().map(|m| m.record.id()).collect();
        assert_eq!(ids, vec!["m-a", "m-b", "m-z"]);
    }

    #[test]
    fn inventory_rejects_malformed_prefix() {
        let records: Vec<GraphRecord> = vec![];
        let index = RepositoryIndex::build(&records);
        let err = risk_markers(&records, Some("/"), None, &index, None).expect_err("malformed");
        assert_eq!(
            err,
            RiskMarkerScopeError::MalformedPrefix {
                prefix: "/".to_owned()
            }
        );
        assert_eq!(err.code(), "malformed_prefix");
    }

    #[test]
    fn inventory_rejects_out_of_store_prefix() {
        let records = vec![macro_diagnostic(
            "m-todo",
            "src/a.rs",
            "todo!",
            span(10, 20, 2),
        )];
        let index = RepositoryIndex::build(&records);
        let err = risk_markers(&records, Some("src/nope"), None, &index, None)
            .expect_err("scope not found");
        assert_eq!(err.code(), "scope_not_found");
    }

    #[test]
    fn inventory_rejects_unknown_commit() {
        let records = vec![macro_diagnostic(
            "m-todo",
            "src/a.rs",
            "todo!",
            span(10, 20, 2),
        )];
        let index = RepositoryIndex::build(&records);
        let err = risk_markers(&records, None, Some("deadbeef"), &index, None)
            .expect_err("unknown commit");
        assert_eq!(err.code(), "unknown_commit");
    }

    #[test]
    fn inventory_temporal_pin_hides_later_markers() {
        // A marker introduced in a later commit must not appear in a query
        // pinned before its introduction.
        let early = macro_diagnostic("m-early", "src/a.rs", "todo!", span(10, 20, 2))
            .with_temporal(temporal("aaa111"));
        let late = macro_diagnostic("m-late", "src/a.rs", "panic!", span(30, 42, 3))
            .with_temporal(temporal("bbb222"));
        let records = vec![early, late];
        let index = RepositoryIndex::build(&records);
        let pinned = risk_markers(&records, None, Some("aaa111"), &index, None).expect("pinned");
        let ids: Vec<&str> = pinned.markers.iter().map(|m| m.record.id()).collect();
        assert_eq!(ids, vec!["m-early"]);
        assert_eq!(pinned.at_commit.as_deref(), Some("aaa111"));
    }

    #[test]
    fn inventory_empty_store_yields_empty() {
        let records: Vec<GraphRecord> = vec![];
        let index = RepositoryIndex::build(&records);
        let inventory = risk_markers(&records, None, None, &index, None).expect("inventory");
        assert!(inventory.markers.is_empty());
        assert_eq!(inventory.at_commit, None);
    }

    /// Builds a repository owning one file via a `CONTAINS` edge, plus an
    /// ownerless risk marker in that file. Mirrors real scans, where the
    /// scanner wires no containment edges for `Diagnostic` nodes.
    fn repo_with_file(repo_id: &str, file_id: &str, path: &str) -> Vec<GraphRecord> {
        let repository = GraphRecord::node(
            repo_id.to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some("repo".to_owned()),
            "repository".to_owned(),
        );
        let file = GraphRecord::node(
            file_id.to_owned(),
            NodeKind::File,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            "file".to_owned(),
        );
        let contains = GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.to_owned(),
            file_id.to_owned(),
            None,
            "contains".to_owned(),
        );
        vec![repository, file, contains]
    }

    #[test]
    fn repo_scope_attributes_ownerless_markers_through_their_file() {
        let mut records = repo_with_file("repo-a", "file-a", "src/a.rs");
        records.push(macro_diagnostic(
            "m-a",
            "src/a.rs",
            "todo!",
            span(10, 20, 2),
        ));
        // A marker in a file no repository owns: outside every repo scope.
        records.push(macro_diagnostic(
            "m-orphan",
            "src/orphan.rs",
            "panic!",
            span(10, 20, 2),
        ));
        let index = RepositoryIndex::build(&records);

        let scoped = risk_markers(&records, None, None, &index, Some("repo-a")).expect("scoped");
        let ids: Vec<&str> = scoped.markers.iter().map(|m| m.record.id()).collect();
        assert_eq!(ids, vec!["m-a"]);
        assert_eq!(scoped.markers[0].repository_id, Some("repo-a"));

        // Unscoped, the orphan marker is still returned (repository_id None:
        // no owner, no unambiguous file).
        let all = risk_markers(&records, None, None, &index, None).expect("all");
        let ids: Vec<&str> = all.markers.iter().map(|m| m.record.id()).collect();
        assert_eq!(ids, vec!["m-a", "m-orphan"]);
        assert_eq!(all.markers[0].repository_id, Some("repo-a"));
        assert_eq!(all.markers[1].repository_id, None);
    }

    #[test]
    fn repo_scope_keeps_only_the_selected_repository() {
        let mut records = repo_with_file("repo-a", "file-a", "src/a.rs");
        records.extend(repo_with_file("repo-b", "file-b", "src/b.rs"));
        records.push(macro_diagnostic(
            "m-a",
            "src/a.rs",
            "todo!",
            span(10, 20, 2),
        ));
        records.push(macro_diagnostic(
            "m-b",
            "src/b.rs",
            "panic!",
            span(10, 20, 2),
        ));
        let index = RepositoryIndex::build(&records);

        let scoped = risk_markers(&records, None, None, &index, Some("repo-b")).expect("scoped");
        let ids: Vec<&str> = scoped.markers.iter().map(|m| m.record.id()).collect();
        assert_eq!(ids, vec!["m-b"]);
    }
}
