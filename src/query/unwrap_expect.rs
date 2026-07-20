use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::{RepositoryIndex, path_is_under_prefix};
use crate::ir::{GraphRecord, NodeKind};

// ── Unwrap/expect panic-risk call-site inventory (issue #223) ─────────────────

/// Why the unwrap/expect lane rejected its scope selectors.
///
/// Every variant carries a stable machine-readable code so an out-of-store
/// path, unknown commit, or malformed prefix is a documented diagnostic and
/// never a silent empty result (issue #196 honesty contract).
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum UnwrapExpectScopeError {
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

impl UnwrapExpectScopeError {
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

/// One `.unwrap()` / `.expect()` panic-risk call site returned by
/// [`unwrap_expect_sites`].
#[derive(Debug, Clone)]
pub struct UnwrapExpectSite<'a> {
    /// The `PanicRiskSite` record itself.
    pub record: &'a GraphRecord,
    /// Closed machine-readable category: `unwrap` or `expect`.
    pub category: &'a str,
    /// Closed context class: `production` or `test`.
    pub context: &'a str,
    /// Innermost `Symbol` record whose span encloses the site in the same
    /// file version; `None` when the site is top-level (no `DEFINES` owner).
    pub enclosing_symbol: Option<&'a GraphRecord>,
}

/// Deterministic unwrap/expect inventory returned by [`unwrap_expect_sites`].
#[derive(Debug, Clone, Default)]
pub struct UnwrapExpectInventory<'a> {
    /// Sites ordered by `(repo_relative_path, span.start_byte, git_commit,
    /// record_id)` — byte-identical across repeated runs on an unchanged store.
    pub sites: Vec<UnwrapExpectSite<'a>>,
    /// Full commit SHA the inventory was pinned to, when `--at` was supplied.
    pub at_commit: Option<String>,
}

/// Inventories `.unwrap()` / `.expect()` panic-risk call sites (issue #223).
///
/// Results derive solely from deterministic `PanicRiskSite` extractor facts:
/// the lane never rewrites or re-scores a code fact, and each row asserts only
/// that a call exists at a span in a context. Strictly read-only.
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
/// Returns [`UnwrapExpectScopeError`] when the prefix is malformed or matches
/// nothing, or when the commit selector is unknown or ambiguous.
pub fn unwrap_expect_sites<'a>(
    records: &'a [GraphRecord],
    path_prefix: Option<&str>,
    at: Option<&str>,
    index: &RepositoryIndex,
    repo: Option<&str>,
) -> Result<UnwrapExpectInventory<'a>, UnwrapExpectScopeError> {
    // 1. Prefix validation (mirrors the subsystem lane).
    let normalized_prefix = match path_prefix {
        Some(prefix) => {
            let normalized = prefix.trim_end_matches('/');
            if normalized.is_empty() {
                return Err(UnwrapExpectScopeError::MalformedPrefix {
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
                    return Err(UnwrapExpectScopeError::UnknownCommit {
                        commit: selector.to_owned(),
                    });
                }
                1 => matching.iter().next().map(|c| (*c).to_owned()),
                count => {
                    return Err(UnwrapExpectScopeError::AmbiguousCommit {
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
                NodeKind::File | NodeKind::Symbol | NodeKind::PanicRiskSite
            ) && record_in_repo_scope(r)
                && in_selected_view(r)
                && path_is_under_prefix(path.as_str(), prefix)
        });
        if !scope_exists {
            return Err(UnwrapExpectScopeError::ScopeNotFound {
                prefix: prefix.to_owned(),
            });
        }
    }

    // 4. Symbol spans per path, for enclosing-symbol resolution.
    let mut symbols_by_path: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Symbol,
            repo_relative_path: Some(path),
            span: Some(_),
            ..
        } = record
        {
            symbols_by_path
                .entry(path.as_str())
                .or_default()
                .push(record);
        }
    }

    let same_file_version = |site: &GraphRecord, symbol: &GraphRecord| -> bool {
        let site_commit = match site {
            GraphRecord::Node { temporal, .. } => temporal.as_ref().map(|t| t.git_commit.as_str()),
            _ => None,
        };
        let symbol_commit = match symbol {
            GraphRecord::Node { temporal, .. } => temporal.as_ref().map(|t| t.git_commit.as_str()),
            _ => None,
        };
        match (site_commit, symbol_commit) {
            (Some(site_sha), Some(symbol_sha)) => site_sha == symbol_sha,
            (None, None) => !tombstoned.contains(symbol.id()),
            _ => false,
        }
    };

    // Latest-write-wins coalescing (issue #432): over an append-only `--graph`
    // the same non-temporal `PanicRiskSite` id can appear as several physical
    // writes — a re-ingest, or a re-add after a tombstone. The embedded
    // `--data-dir` read collapses these to the latest version, so emit only the
    // latest write per id here too; without this the lane over-counts one live
    // site as several rows (the mirror of the pre-fix tombstone under-count).
    // Temporal history versions are keyed by commit and kept individually.
    let latest_nontemporal: BTreeMap<&str, usize> = records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| match r {
            GraphRecord::Node {
                kind: NodeKind::PanicRiskSite,
                id,
                temporal: None,
                ..
            } => Some((id.as_str(), i)),
            _ => None,
        })
        .collect();

    // 5. Collect, resolve enclosing symbols, and order deterministically.
    let mut sites: Vec<UnwrapExpectSite<'a>> = Vec::new();
    for (record_index, record) in records.iter().enumerate() {
        let GraphRecord::Node {
            kind: NodeKind::PanicRiskSite,
            id,
            name: Some(category),
            call_context: Some(context),
            repo_relative_path: Some(path),
            span: Some(site_span),
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
                symbol_span.start_byte <= site_span.start_byte
                    && site_span.end_byte <= symbol_span.end_byte
                    && same_file_version(record, symbol)
                    // Repository-boundary honesty: two repositories can share
                    // a repo-relative path (and, for clones of one history,
                    // even a commit SHA), so the enclosing symbol must belong
                    // to the site's own repository — never a same-path symbol
                    // from another repository.
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

        sites.push(UnwrapExpectSite {
            record,
            category,
            context,
            enclosing_symbol,
        });
    }

    sites.sort_by(|a, b| unwrap_expect_sort_key(a).cmp(&unwrap_expect_sort_key(b)));

    Ok(UnwrapExpectInventory { sites, at_commit })
}

/// Deterministic ordering key for unwrap/expect site rows:
/// `(repo_relative_path, span.start_byte, git_commit, record_id)`.
fn unwrap_expect_sort_key<'k>(site: &UnwrapExpectSite<'k>) -> (&'k str, usize, &'k str, &'k str) {
    match site.record {
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
    //! `--graph`, a `PanicRiskSite` re-ingested AFTER its own tombstone is live
    //! again — matching the embedded `--data-dir` current-state read — while a
    //! tombstone with no later re-add still deletes its id.
    use super::*;
    use crate::ir::{SCHEMA_VERSION, SourceSpan};

    fn panic_site(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::PanicRiskSite,
            Some("src/lib.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
            }),
            Some("unwrap".to_owned()),
            "unwrap site".to_owned(),
        )
        .with_call_context("production")
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
    fn site_reingested_after_tombstone_is_live() {
        let id = "codegraph:v6:site_a";
        let records = vec![panic_site(id), tomb(id), panic_site(id)];
        let index = RepositoryIndex::build(&records);
        let inv = unwrap_expect_sites(&records, None, None, &index, None).expect("inventory");
        assert!(
            inv.sites.iter().any(|s| s.record.id() == id),
            "a site re-ingested after its tombstone must be reported live"
        );
    }

    #[test]
    fn site_tombstone_without_reingest_stays_deleted() {
        let id = "codegraph:v6:site_a";
        let records = vec![panic_site(id), tomb(id)];
        let index = RepositoryIndex::build(&records);
        let inv = unwrap_expect_sites(&records, None, None, &index, None).expect("inventory");
        assert!(
            inv.sites.is_empty(),
            "a tombstone with no later re-ingest still deletes the site"
        );
    }

    #[test]
    fn site_reingested_after_tombstone_emits_one_row() {
        // Latest-write-wins coalescing (issue #432): a revived site must emit
        // EXACTLY ONE row, matching the coalesced `--data-dir` read.
        let id = "codegraph:v6:site_a";
        let records = vec![panic_site(id), tomb(id), panic_site(id)];
        let index = RepositoryIndex::build(&records);
        let inv = unwrap_expect_sites(&records, None, None, &index, None).expect("inventory");
        assert_eq!(
            inv.sites.len(),
            1,
            "a revived site coalesces to a single row"
        );
    }

    #[test]
    fn duplicate_site_versions_emit_one_row() {
        // A double non-temporal write with NO tombstone must also coalesce to one
        // row (the broader multi-version case, not tombstone-entangled).
        let id = "codegraph:v6:site_a";
        let records = vec![panic_site(id), panic_site(id)];
        let index = RepositoryIndex::build(&records);
        let inv = unwrap_expect_sites(&records, None, None, &index, None).expect("inventory");
        assert_eq!(
            inv.sites.len(),
            1,
            "duplicate non-temporal versions coalesce to one row"
        );
    }
}
