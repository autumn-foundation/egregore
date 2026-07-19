use std::collections::{BTreeMap, BTreeSet};

use super::{CommitOrder, RepositoryIndex};
use crate::ir::{GraphRecord, NodeKind, SourceSpan};

/// The lifecycle event kind.
#[derive(serde::Serialize, serde::Deserialize, Copy, Clone, Debug, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LifelineEventKind {
    /// Symbol was first seen in the commit.
    Introduced,
    /// Symbol's body or drift was changed.
    Modified,
    /// Symbol was absent/removed.
    Removed,
    /// Symbol was reintroduced after being removed.
    Reintroduced,
}

impl std::fmt::Display for LifelineEventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Introduced => "introduced",
            Self::Modified => "modified",
            Self::Removed => "removed",
            Self::Reintroduced => "reintroduced",
        };
        write!(f, "{s}")
    }
}

/// A temporal event in a symbol's lifecycle.
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Debug)]
pub struct LifelineEvent {
    /// The event kind: "introduced", "modified", "removed", or "reintroduced".
    pub event_type: LifelineEventKind,
    /// The stable record ID associated with this event (symbol node ID, or tombstone ID).
    pub record_id: String,
    /// The Git commit SHA.
    pub commit: String,
    /// The valid time (commit time) of the event's commit.
    pub valid_time: String,
    /// The repository-relative path (absent for removal events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// The syntax source span (absent for removal events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// The documented reason the span is absent (e.g. "tombstone" for removal events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent_span_reason: Option<String>,
    /// The SemanticDrift record ID if this is a modifying event with drift.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift_record_id: Option<String>,
    /// The semantic drift score if this is a modifying event with drift.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift_score: Option<f64>,
    /// Corpus this history-analysis lane read (issue #427): `union` over a
    /// scan-history store, `single_snapshot` over a snapshot-less store. This
    /// lane emits bare NDJSON with no summary line, so the disclosure rides
    /// every event row; all rows in one query share the same corpus. Populated
    /// by the CLI via [`super::disclose_corpus`]; the disclosure never changes
    /// traversal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corpus_mode: Option<&'static str>,
    /// How the corpus mode was chosen: always `default` for this lane. Present
    /// only with `corpus_mode`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corpus_mode_source: Option<&'static str>,
    /// One-line human description of the corpus that was read. Present only with
    /// `corpus_mode`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corpus_disclaimer: Option<String>,
}

/// Errors returned by the symbol lifeline query.
#[derive(thiserror::Error, Debug, Clone, Eq, PartialEq)]
pub enum LifelineError {
    /// The query symbol handle/name was not found in the graph.
    #[error("symbol not found in the graph: {query}")]
    UnknownSymbol {
        /// The query string.
        query: String,
    },
    /// The query name matched multiple symbols.
    #[error("ambiguous symbol name '{query}' matches multiple symbols")]
    AmbiguousSymbol {
        /// The query string.
        query: String,
        /// The unique record IDs matching the query.
        candidates: Vec<String>,
    },
}

/// Traces a single symbol's lifecycle across Git history.
///
/// # Errors
///
/// Returns `LifelineError::UnknownSymbol` if the query matches 0 symbols,
/// or `LifelineError::AmbiguousSymbol` if it matches multiple symbols.
pub fn symbol_lifeline(
    records: &[GraphRecord],
    query: &str,
    repo_id: Option<&str>,
) -> Result<Vec<LifelineEvent>, LifelineError> {
    let index = RepositoryIndex::build(records);

    let is_owned =
        |id: &str| -> bool { repo_id.is_none_or(|r_id| index.owner_of(id) == Some(r_id)) };

    // Pre-build a map from deleted_id to tombstone ID for O(log T) lookup
    let mut tombstone_map: BTreeMap<&str, &str> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Tombstone { id, deleted_id, .. } = r {
            tombstone_map.insert(deleted_id.as_str(), id.as_str());
        }
    }

    // Gather all matching Symbol nodes
    let mut matching_symbol_ids = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name,
            ..
        } = r
        {
            let tombstoned = tombstone_map.contains_key(id.as_str());
            if tombstoned {
                continue;
            }
            if !is_owned(id.as_str()) {
                continue;
            }
            if id == query || name.as_deref() == Some(query) {
                matching_symbol_ids.insert(id.as_str());
            }
        }
    }

    if matching_symbol_ids.is_empty() {
        return Err(LifelineError::UnknownSymbol {
            query: query.to_owned(),
        });
    }
    if matching_symbol_ids.len() > 1 {
        let candidates: Vec<String> = matching_symbol_ids
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        return Err(LifelineError::AmbiguousSymbol {
            query: query.to_owned(),
            candidates,
        });
    }

    let target_symbol_id =
        matching_symbol_ids
            .into_iter()
            .next()
            .ok_or_else(|| LifelineError::UnknownSymbol {
                query: query.to_owned(),
            })?;

    // Find the repository of the target symbol
    let target_repo_id = index.owner_of(target_symbol_id);

    // Build the CommitOrder for repository commits
    let commit_order = CommitOrder::build(records);

    // Filter commits, symbol snapshots, and drift records in a single consolidated loop
    let mut repo_commits = Vec::new();
    let mut commit_valid_times: BTreeMap<&str, &str> = BTreeMap::new();
    let mut parent_map: BTreeMap<&str, &[String]> = BTreeMap::new();
    let mut symbol_snapshots: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut symbol_snapshot_bodies: BTreeMap<&str, &str> = BTreeMap::new();
    let mut drift_map: BTreeMap<&str, &GraphRecord> = BTreeMap::new();

    for r in records {
        match r {
            GraphRecord::Node {
                kind: NodeKind::Commit,
                name: Some(sha),
                temporal: Some(t),
                ..
            } => {
                let in_repo = target_repo_id.map_or_else(
                    || index.owner_of(r.id()).is_none(),
                    |r_id| index.owner_of(r.id()) == Some(r_id),
                );
                if in_repo {
                    repo_commits.push((sha.as_str(), r));
                    commit_valid_times.insert(sha.as_str(), t.valid_time.as_str());
                    parent_map.insert(sha.as_str(), &t.git_parent_commits);
                }
            }
            GraphRecord::Node {
                kind: NodeKind::Symbol,
                id,
                temporal: Some(t),
                summary,
                ..
            } if id == target_symbol_id => {
                symbol_snapshots.insert(t.git_commit.as_str(), r);
                symbol_snapshot_bodies.insert(t.git_commit.as_str(), summary.as_str());
            }
            GraphRecord::Node {
                kind: NodeKind::SemanticDrift,
                semantic_drift: Some(drift),
                ..
            } if drift.target_record_id == target_symbol_id => {
                drift_map.insert(drift.after_git_commit.as_str(), r);
            }
            _ => {}
        }
    }

    // Sort repository commits by topological rank
    repo_commits.sort_by(|a, b| {
        let rank_a = commit_order.rank(a.0);
        let rank_b = commit_order.rank(b.0);
        rank_a.cmp(&rank_b).then_with(|| a.0.cmp(b.0))
    });

    // Helper: did the symbol body change in a commit?
    let symbol_body_changed = |commit: &str, summary: &str| -> bool {
        let Some(parents) = parent_map.get(commit) else {
            return true;
        };
        if parents.is_empty() {
            return true;
        }
        let mut saw_parent_snapshot = false;
        for parent in *parents {
            if let Some(parent_body) = symbol_snapshot_bodies.get(parent.as_str()) {
                saw_parent_snapshot = true;
                if *parent_body != summary {
                    return true;
                }
            }
        }
        !saw_parent_snapshot
    };

    // Helper: the valid time of a repository commit. Every commit gathered
    // into `repo_commits` carries temporal metadata, so this is always
    // present for event commits.
    let valid_time_of = |sha: &str| -> String {
        commit_valid_times
            .get(sha)
            .map_or_else(String::new, |vt| (*vt).to_owned())
    };

    let mut events = Vec::new();
    let mut commit_live: BTreeMap<&str, bool> = BTreeMap::new();
    let mut introduced_commits: BTreeSet<&str> = BTreeSet::new();

    for (commit_sha, _) in &repo_commits {
        let snapshot = symbol_snapshots.get(commit_sha);
        if let Some(node) = snapshot {
            let GraphRecord::Node {
                repo_relative_path,
                span,
                summary,
                ..
            } = node
            else {
                continue;
            };

            let parents = parent_map.get(commit_sha);
            let was_live_at_any_parent = parents.is_some_and(|ps| {
                ps.iter()
                    .any(|p| commit_live.get(p.as_str()).copied().unwrap_or(false))
            });

            if was_live_at_any_parent {
                // If it was already live, check if modified
                let changed = symbol_body_changed(commit_sha, summary);
                let drift_node = drift_map.get(commit_sha);
                if changed || drift_node.is_some() {
                    let drift_record_id = drift_node.map(|r| r.id().to_owned());
                    let drift_score = drift_node.and_then(|r| {
                        if let GraphRecord::Node {
                            semantic_drift: Some(d),
                            ..
                        } = r
                        {
                            Some(d.score)
                        } else {
                            None
                        }
                    });

                    let absent_span_reason = if span.is_none() {
                        Some("no_span_module_level".to_owned())
                    } else {
                        None
                    };

                    events.push(LifelineEvent {
                        event_type: LifelineEventKind::Modified,
                        record_id: target_symbol_id.to_string(),
                        commit: (*commit_sha).to_owned(),
                        valid_time: valid_time_of(commit_sha),
                        repo_relative_path: repo_relative_path.clone(),
                        span: *span,
                        absent_span_reason,
                        drift_record_id,
                        drift_score,
                        corpus_mode: None,
                        corpus_mode_source: None,
                        corpus_disclaimer: None,
                    });
                }
            } else {
                // Determine introduced vs reintroduced using strict_descendants ancestry check
                let is_reintroduction = introduced_commits.iter().any(|&intro_commit| {
                    commit_order
                        .strict_descendants(intro_commit)
                        .contains(commit_sha)
                });

                let event_type = if is_reintroduction {
                    LifelineEventKind::Reintroduced
                } else {
                    introduced_commits.insert(commit_sha);
                    LifelineEventKind::Introduced
                };

                let absent_span_reason = if span.is_none() {
                    Some("no_span_module_level".to_owned())
                } else {
                    None
                };

                events.push(LifelineEvent {
                    event_type,
                    record_id: target_symbol_id.to_string(),
                    commit: (*commit_sha).to_owned(),
                    valid_time: valid_time_of(commit_sha),
                    repo_relative_path: repo_relative_path.clone(),
                    span: *span,
                    absent_span_reason,
                    drift_record_id: None,
                    drift_score: None,
                    corpus_mode: None,
                    corpus_mode_source: None,
                    corpus_disclaimer: None,
                });
            }
            commit_live.insert(commit_sha, true);
        } else {
            // Symbol is absent at this commit
            let parents = parent_map.get(commit_sha);
            let was_live_at_any_parent = parents.is_some_and(|ps| {
                ps.iter()
                    .any(|p| commit_live.get(p.as_str()).copied().unwrap_or(false))
            });

            if was_live_at_any_parent {
                // Symbol was live but is now absent -> removal event!
                let tombstone_record_id = tombstone_map.get(target_symbol_id).map_or_else(
                    || crate::ir::stable_id(&["tombstone", target_symbol_id]),
                    |&id| id.to_owned(),
                );

                events.push(LifelineEvent {
                    event_type: LifelineEventKind::Removed,
                    record_id: tombstone_record_id,
                    commit: (*commit_sha).to_owned(),
                    valid_time: valid_time_of(commit_sha),
                    repo_relative_path: None,
                    span: None,
                    absent_span_reason: Some("tombstone".to_owned()),
                    drift_record_id: None,
                    drift_score: None,
                    corpus_mode: None,
                    corpus_mode_source: None,
                    corpus_disclaimer: None,
                });
            }
            commit_live.insert(commit_sha, false);
        }
    }

    Ok(events)
}

// ---------------------------------------------------------------------------
// Commit-range symbol/file deltas (issue #118)
// ---------------------------------------------------------------------------
