use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::{RepositoryIndex, path_is_under_prefix};
use crate::ir::{GraphRecord, NodeKind};

// ── Debt-comment marker inventory (issue #218) ────────────────────────────────

/// Why the debt-marker lane rejected its scope selectors.
///
/// Every variant carries a stable machine-readable code so an out-of-store
/// path, unknown commit, or malformed prefix is a documented diagnostic and
/// never a silent empty result (issue #196 honesty contract).
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DebtMarkerScopeError {
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

impl DebtMarkerScopeError {
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

/// One debt-comment marker returned by [`debt_markers`].
#[derive(Debug, Clone)]
pub struct DebtMarkerRow<'a> {
    /// The `DebtMarker` record itself.
    pub record: &'a GraphRecord,
    /// Closed machine-readable category: `todo` / `fixme` / `hack` / `xxx`.
    pub category: &'a str,
    /// Trimmed single-line note text following the marker token.
    pub note: &'a str,
    /// Innermost `Symbol` record whose span encloses the marker in the same
    /// file version; `None` when the marker sits at module top level (no
    /// `DEFINES`/`CONTAINS` owner).
    pub enclosing_symbol: Option<&'a GraphRecord>,
}

/// Deterministic debt-marker inventory returned by [`debt_markers`].
#[derive(Debug, Clone, Default)]
pub struct DebtMarkerInventory<'a> {
    /// Markers ordered by `(repo_relative_path, span.start_byte, git_commit,
    /// record_id)` — byte-identical across repeated runs on an unchanged store.
    pub markers: Vec<DebtMarkerRow<'a>>,
    /// Full commit SHA the inventory was pinned to, when `--at` was supplied.
    pub at_commit: Option<String>,
}

/// Inventories human-authored debt-comment markers (issue #218).
///
/// Results derive solely from deterministic `DebtMarker` extractor facts: the
/// lane never rewrites or re-scores a code fact, never asserts the
/// surrounding code is correct or incorrect, and introduces no agent-authored
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
/// Returns [`DebtMarkerScopeError`] when the prefix is malformed or matches
/// nothing, or when the commit selector is unknown or ambiguous.
pub fn debt_markers<'a>(
    records: &'a [GraphRecord],
    path_prefix: Option<&str>,
    at: Option<&str>,
    index: &RepositoryIndex,
    repo: Option<&str>,
) -> Result<DebtMarkerInventory<'a>, DebtMarkerScopeError> {
    // 1. Prefix validation (mirrors the subsystem lane).
    let normalized_prefix = match path_prefix {
        Some(prefix) => {
            let normalized = prefix.trim_end_matches('/');
            if normalized.is_empty() {
                return Err(DebtMarkerScopeError::MalformedPrefix {
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
                    return Err(DebtMarkerScopeError::UnknownCommit {
                        commit: selector.to_owned(),
                    });
                }
                1 => matching.iter().next().map(|c| (*c).to_owned()),
                count => {
                    return Err(DebtMarkerScopeError::AmbiguousCommit {
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
                NodeKind::File | NodeKind::Symbol | NodeKind::DebtMarker
            ) && record_in_repo_scope(r)
                && in_selected_view(r)
                && path_is_under_prefix(path.as_str(), prefix)
        });
        if !scope_exists {
            return Err(DebtMarkerScopeError::ScopeNotFound {
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
    // the same non-temporal `DebtMarker` id can appear as several physical writes
    // — a re-ingest, or a re-add after a tombstone. The embedded `--data-dir`
    // read collapses these to the latest version, so emit only the latest write
    // per id here too; without this the lane over-counts one live marker as
    // several rows (the mirror of the pre-fix tombstone under-count). Temporal
    // history versions are keyed by commit and kept individually.
    let latest_nontemporal: BTreeMap<&str, usize> = records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| match r {
            GraphRecord::Node {
                kind: NodeKind::DebtMarker,
                id,
                temporal: None,
                ..
            } => Some((id.as_str(), i)),
            _ => None,
        })
        .collect();

    // 5. Collect, resolve enclosing symbols, and order deterministically.
    let mut markers: Vec<DebtMarkerRow<'a>> = Vec::new();
    for (record_index, record) in records.iter().enumerate() {
        let GraphRecord::Node {
            kind: NodeKind::DebtMarker,
            id,
            name: Some(category),
            note: Some(note),
            repo_relative_path: Some(path),
            span: Some(marker_span),
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
                    // In a shared multi-repository store two repositories can
                    // define the same repo-relative path; a candidate symbol
                    // must belong to the marker's own repository or the lane
                    // could cite an unrelated repo's symbol whenever its span
                    // is narrower.
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
            .copied();

        markers.push(DebtMarkerRow {
            record,
            category,
            note,
            enclosing_symbol,
        });
    }

    markers.sort_by(|a, b| debt_marker_sort_key(a).cmp(&debt_marker_sort_key(b)));

    Ok(DebtMarkerInventory { markers, at_commit })
}

/// Deterministic ordering key for debt-marker rows:
/// `(repo_relative_path, span.start_byte, git_commit, record_id)`.
fn debt_marker_sort_key<'k>(marker: &DebtMarkerRow<'k>) -> (&'k str, usize, &'k str, &'k str) {
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
mod liveness_parity_tests {
    //! Transport-parity regression (issues #421/#432): over an append-only
    //! `--graph`, a `DebtMarker` re-ingested AFTER its own tombstone is live
    //! again — matching the embedded `--data-dir` current-state read — while a
    //! tombstone with no later re-add still deletes its id.
    use super::*;
    use crate::ir::{SCHEMA_VERSION, SourceSpan};

    fn debt_marker(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::DebtMarker,
            Some("src/lib.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
            }),
            Some("todo".to_owned()),
            "todo marker".to_owned(),
        )
        .with_note("clean this up")
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
    fn marker_reingested_after_tombstone_is_live() {
        let id = "codegraph:v6:marker_a";
        let records = vec![debt_marker(id), tomb(id), debt_marker(id)];
        let index = RepositoryIndex::build(&records);
        let inv = debt_markers(&records, None, None, &index, None).expect("inventory");
        assert!(
            inv.markers.iter().any(|m| m.record.id() == id),
            "a marker re-ingested after its tombstone must be reported live"
        );
    }

    #[test]
    fn marker_tombstone_without_reingest_stays_deleted() {
        let id = "codegraph:v6:marker_a";
        let records = vec![debt_marker(id), tomb(id)];
        let index = RepositoryIndex::build(&records);
        let inv = debt_markers(&records, None, None, &index, None).expect("inventory");
        assert!(
            inv.markers.is_empty(),
            "a tombstone with no later re-ingest still deletes the marker"
        );
    }

    #[test]
    fn marker_reingested_after_tombstone_emits_one_row() {
        // Latest-write-wins coalescing (issue #432): a revived marker must emit
        // EXACTLY ONE row, matching the coalesced `--data-dir` read — not one row
        // per physical write.
        let id = "codegraph:v6:marker_a";
        let records = vec![debt_marker(id), tomb(id), debt_marker(id)];
        let index = RepositoryIndex::build(&records);
        let inv = debt_markers(&records, None, None, &index, None).expect("inventory");
        assert_eq!(
            inv.markers.len(),
            1,
            "a revived marker coalesces to a single row"
        );
    }

    fn symbol_with_span(id: &str, start: usize, end: usize) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            Some(SourceSpan {
                start_byte: start,
                end_byte: end,
                start_line: 1,
                end_line: 2,
            }),
            Some("outer".to_owned()),
            "symbol outer".to_owned(),
        )
    }

    #[test]
    fn revived_symbol_with_changed_span_is_not_stale_enclosing() {
        // FINDING 2 (issue #432, round 2): a Symbol revived AFTER its tombstone
        // with a DIFFERENT span must not leave its STALE pre-tombstone span as an
        // enclosing-symbol candidate. Over an append-only `--graph` both physical
        // symbol writes are present; the embedded `--data-dir` read exposes only
        // the latest span. The enclosing-symbol index must coalesce to the latest
        // write per id so the two transports agree.
        let marker_id = "codegraph:v6:marker_a";
        let sym_id = "codegraph:v6:sym_a";
        // Marker sits at bytes [10, 20].
        let marker = GraphRecord::node(
            marker_id.to_owned(),
            NodeKind::DebtMarker,
            Some("src/lib.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 10,
                end_byte: 20,
                start_line: 1,
                end_line: 2,
            }),
            Some("todo".to_owned()),
            "todo marker".to_owned(),
        )
        .with_note("clean this up");
        let records = vec![
            // Stale symbol span [0, 100] ENCLOSES the marker.
            symbol_with_span(sym_id, 0, 100),
            tomb(sym_id),
            // Revived symbol span [0, 5] does NOT enclose the marker.
            symbol_with_span(sym_id, 0, 5),
            marker,
        ];
        let index = RepositoryIndex::build(&records);
        let inv = debt_markers(&records, None, None, &index, None).expect("inventory");
        assert_eq!(inv.markers.len(), 1, "one live marker row");
        assert!(
            inv.markers[0].enclosing_symbol.is_none(),
            "the stale pre-tombstone span must not be reported as the enclosing symbol"
        );
    }

    #[test]
    fn duplicate_marker_versions_emit_one_row() {
        // A double non-temporal write with NO tombstone must also coalesce to one
        // row (the broader multi-version case, not tombstone-entangled).
        let id = "codegraph:v6:marker_a";
        let records = vec![debt_marker(id), debt_marker(id)];
        let index = RepositoryIndex::build(&records);
        let inv = debt_markers(&records, None, None, &index, None).expect("inventory");
        assert_eq!(
            inv.markers.len(),
            1,
            "duplicate non-temporal versions coalesce to one row"
        );
    }
}
