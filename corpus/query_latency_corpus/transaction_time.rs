use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use crate::ir::{GraphRecord, NodeKind};

/// A machine-readable diagnostic emitted by a transaction-time query.
///
/// Diagnostics never silently change the result set; they explain edge cases
/// (empty views, excluded rows, out-of-range instants) so callers can tell a
/// real "prior view" apart from a missing-metadata or out-of-range condition.
/// See issue #66 AC6.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TxDiagnostic {
    /// Stable machine-readable code (e.g. `before_first_transaction`).
    pub code: String,
    /// Human-readable explanation. Never contains raw record bodies.
    pub message: String,
}

/// Error that aborts a transaction-time query before any rows are produced.
///
/// Distinct from [`TxDiagnostic`]: an error means the query itself was
/// malformed (e.g. an unparseable timestamp), so no result set exists.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TxQueryError {
    /// Stable machine-readable code (e.g. `invalid_timestamp`).
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
}

/// Outcome of a transaction-time symbol query.
#[derive(Debug, Default)]
pub struct TxSymbolQuery<'r> {
    /// Selected records: one per stable record ID, the latest version the store
    /// knew at the requested transaction time (further constrained by valid time
    /// when a valid-time instant is also supplied). Sorted by record ID for
    /// deterministic output (AC7).
    pub records: Vec<&'r GraphRecord>,
    /// Machine-readable diagnostics for this query (AC6). Sorted and deduplicated.
    pub diagnostics: Vec<TxDiagnostic>,
}

/// Best-candidate tuple tracked per stable record ID while resolving a
/// transaction-time query: the record, its parsed transaction time, and its
/// parsed valid time (present only when a valid-time axis is requested).
type TxCandidate<'r> = (
    &'r GraphRecord,
    DateTime<chrono::FixedOffset>,
    Option<DateTime<chrono::FixedOffset>>,
    // Deterministic tie-break rank for equal transaction (and valid) times:
    // commit topological rank for history records, input order otherwise.
    usize,
);

/// Resolves the transaction-time handle of a record from its body fields.
///
/// Priority order:
/// 1. explicit `transaction_time` (project-domain mutations, seeded fixtures),
/// 2. `ingested_at` (agent-memory / verification commit time),
/// 3. `valid_time` when `valid_time_source == "inferred_from_transaction_time"`
///    (current-tree scans set `valid_time` to the scan's wall-clock instant,
///    which *is* the transaction time),
/// 4. `temporal.observed_at` for history-replay records (`scan-history`), whose
///    only available store-observation timeline is the commit timeline. For
///    replayed history the transaction axis collapses onto that timeline, which
///    is the honest "known by Egregore then" handle for deterministic replay.
///
/// Returns `None` when no transaction-time stamp can be resolved. Callers MUST
/// treat `None` as *missing metadata*, never as a current-state record — a
/// transaction-time query must not silently fall back to current state (AC6).
#[must_use]
pub fn record_transaction_time(record: &GraphRecord) -> Option<&str> {
    let GraphRecord::Node {
        transaction_time,
        ingested_at,
        valid_time,
        valid_time_source,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    if let Some(tt) = transaction_time.as_deref() {
        return Some(tt);
    }
    if let Some(ia) = ingested_at.as_deref() {
        return Some(ia);
    }
    if valid_time_source.as_deref() == Some("inferred_from_transaction_time") {
        return valid_time.as_deref();
    }
    if let Some(t) = temporal {
        return Some(t.observed_at.as_str());
    }
    None
}

/// Resolves the valid-time of a node from the history temporal block or the
/// current-tree node-level field.
#[must_use]
fn node_valid_time(record: &GraphRecord) -> Option<&str> {
    let GraphRecord::Node {
        temporal,
        valid_time,
        ..
    } = record
    else {
        return None;
    };
    temporal
        .as_ref()
        .map(|t| t.valid_time.as_str())
        .or(valid_time.as_deref())
}

/// Returns the Git commit SHA of a history-replay (temporal) record.
#[must_use]
const fn node_git_commit(record: &GraphRecord) -> Option<&str> {
    if let GraphRecord::Node {
        temporal: Some(t), ..
    } = record
    {
        Some(t.git_commit.as_str())
    } else {
        None
    }
}

/// Deterministic ordering of history commits derived from the Git commit DAG
/// carried on temporal records (`git_commit` + `git_parent_commits`).
///
/// Git commit timestamps are only second-resolution and batch-created commits
/// frequently collide, so a timestamp comparison alone cannot order two commits
/// made in the same second. This reconstructs the parent topology and assigns
/// each commit a `rank` (its longest ancestor-chain length), so a descendant
/// always outranks its ancestors regardless of identical timestamps.
///
/// It also inverts the parent links into a child adjacency map so removal
/// detection can ask whether a commit has a *strict descendant* in the requested
/// view (see [`Self::strict_descendants`]). Child-ward reachability — rather than
/// connected-component membership — is what distinguishes two forks that share
/// Git ancestry: a later commit on one fork is not a descendant of a live symbol
/// on the other, so it never makes that symbol look removed.
#[derive(Default)]
pub(super) struct CommitOrder<'a> {
    /// Commit SHA → longest ancestor-chain length (topological rank).
    rank: BTreeMap<&'a str, usize>,
    /// Commit SHA → its direct child commits (parent links inverted).
    children: BTreeMap<&'a str, Vec<&'a str>>,
}

impl<'a> CommitOrder<'a> {
    pub(super) fn build(records: &'a [GraphRecord]) -> Self {
        // Longest chain over commits that are themselves present in the store
        // (absent shallow-boundary parents anchor at 0).
        fn rank_of<'b>(
            sha: &'b str,
            parents: &BTreeMap<&'b str, Vec<&'b str>>,
            memo: &mut BTreeMap<&'b str, usize>,
            stack: &mut BTreeSet<&'b str>,
        ) -> usize {
            if let Some(&r) = memo.get(sha) {
                return r;
            }
            if !stack.insert(sha) {
                return 0; // cycle guard (not expected in a Git history)
            }
            let mut best = 0;
            if let Some(ps) = parents.get(sha) {
                for &p in ps {
                    if parents.contains_key(p) {
                        best = best.max(rank_of(p, parents, memo, stack) + 1);
                    }
                }
            }
            stack.remove(sha);
            memo.insert(sha, best);
            best
        }

        // commit → deduplicated parent SHAs, for every commit observed as a
        // commit record.
        let mut parents: BTreeMap<&'a str, Vec<&'a str>> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node {
                temporal: Some(t), ..
            } = record
            {
                let entry = parents.entry(t.git_commit.as_str()).or_default();
                for parent in &t.git_parent_commits {
                    if !entry.contains(&parent.as_str()) {
                        entry.push(parent.as_str());
                    }
                }
            }
        }

        // Topological rank per commit.
        let mut rank: BTreeMap<&'a str, usize> = BTreeMap::new();
        let mut stack: BTreeSet<&'a str> = BTreeSet::new();
        for &commit in parents.keys() {
            rank_of(commit, &parents, &mut rank, &mut stack);
        }

        // Invert parent links into a child adjacency map for descendant walks.
        let mut children: BTreeMap<&'a str, Vec<&'a str>> = BTreeMap::new();
        for (&commit, ps) in &parents {
            for &p in ps {
                let entry = children.entry(p).or_default();
                if !entry.contains(&commit) {
                    entry.push(commit);
                }
            }
        }

        Self { rank, children }
    }

    pub(super) fn rank(&self, sha: &str) -> usize {
        self.rank.get(sha).copied().unwrap_or(0)
    }

    /// All transitive descendant commits of `sha` (children, grandchildren, …),
    /// excluding `sha` itself. Reachability follows the commit DAG child-ward, so
    /// commits on a sibling branch (or a fork that merely shares an ancestor) are
    /// *not* descendants even when they sit in the same connected component.
    pub(super) fn strict_descendants(&self, sha: &str) -> BTreeSet<&'a str> {
        let mut out: BTreeSet<&'a str> = BTreeSet::new();
        let mut stack: Vec<&'a str> = self
            .children
            .get(sha)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        while let Some(c) = stack.pop() {
            if out.insert(c) {
                if let Some(kids) = self.children.get(c) {
                    stack.extend(kids.iter().copied());
                }
            }
        }
        out
    }
}

/// Computes the store-wide transaction-time range across every record carrying
/// a parseable transaction handle, as `(earliest, latest)`.
///
/// Daemon callers derive this from the *unfiltered* store and pass it into
/// [`symbol_as_of_transaction_time`] so the `before_first_transaction`
/// diagnostic reflects the whole store rather than a domain-filtered slice
/// (otherwise a mixed store with an earlier non-codegraph transaction and a
/// later codegraph symbol would spuriously report a between-the-two instant as
/// out of range). The CLI `--graph` path already holds the full graph, so it
/// passes `None` and lets the resolver derive the bounds from `records`.
#[must_use]
pub fn store_transaction_bounds(
    records: &[GraphRecord],
) -> Option<(DateTime<chrono::FixedOffset>, DateTime<chrono::FixedOffset>)> {
    let mut min_tx: Option<DateTime<chrono::FixedOffset>> = None;
    let mut max_tx: Option<DateTime<chrono::FixedOffset>> = None;
    for r in records {
        if let Some(tt) =
            record_transaction_time(r).and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        {
            min_tx = Some(min_tx.map_or(tt, |m| m.min(tt)));
            max_tx = Some(max_tx.map_or(tt, |m| m.max(tt)));
        }
    }
    match (min_tx, max_tx) {
        (Some(min), Some(max)) => Some((min, max)),
        _ => None,
    }
}

/// Finds symbol records as the store knew them at a transaction-time instant.
///
/// Returns, per stable record ID, the version whose transaction time is the
/// latest at or before `tx_as_of`. Records committed after `tx_as_of` (later
/// corrections, supersessions, re-imports) are excluded, so the result is the
/// *prior graph view* — what Egregore knew then, not the current state.
///
/// When `as_of_valid_time` is also supplied, both axes are applied
/// independently (issue #66 AC4): first restrict to versions known by
/// `tx_as_of` (transaction axis), then, within those, return the version whose
/// `valid_time` is the most recent at or before `as_of_valid_time` (valid
/// axis). This answers "what was true at valid time V, as known by transaction
/// time T."
///
/// Tombstones are intentionally ignored on the transaction-time path: a
/// current-state tombstone marks a *later* deletion whose transaction time is
/// not recorded on the tombstone itself, so it must not erase a historical
/// view that predates the deletion (AC2).
///
/// # Errors
///
/// Returns [`TxQueryError`] with code `invalid_timestamp` when `tx_as_of` or
/// `as_of_valid_time` is not a valid RFC 3339 instant.
#[allow(clippy::too_many_lines)]
///
/// `store_tx_bounds` supplies the store-wide transaction range explicitly (see
/// [`store_transaction_bounds`]); pass `None` to derive it from `records`. Use
/// it when `records` is a domain-filtered subset of a larger store so the
/// out-of-range diagnostics stay store-wide.
pub fn symbol_as_of_transaction_time<'r>(
    records: &'r [GraphRecord],
    symbol_name: &str,
    tx_as_of: &str,
    as_of_valid_time: Option<&str>,
    store_tx_bounds: Option<(DateTime<chrono::FixedOffset>, DateTime<chrono::FixedOffset>)>,
) -> Result<TxSymbolQuery<'r>, TxQueryError> {
    let tx_instant = DateTime::parse_from_rfc3339(tx_as_of).map_err(|e| TxQueryError {
        code: "invalid_timestamp".to_owned(),
        message: format!("invalid --tx-as-of timestamp '{tx_as_of}': {e}"),
    })?;
    let vt_requested = match as_of_valid_time {
        Some(v) => Some(DateTime::parse_from_rfc3339(v).map_err(|e| TxQueryError {
            code: "invalid_timestamp".to_owned(),
            message: format!("invalid --as-of timestamp '{v}': {e}"),
        })?),
        None => None,
    };

    let mut diagnostics: Vec<TxDiagnostic> = Vec::new();

    // Deterministic commit ordering (topological rank + per-repository
    // component) used to break same-second/equal-transaction ties and to scope
    // history-removal detection to the queried symbol's own repository.
    let commit_order = CommitOrder::build(records);

    // All Symbol nodes carrying the queried name.
    let named: Vec<&GraphRecord> = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name,
                    ..
                } if name.as_deref() == Some(symbol_name)
            )
        })
        .collect();

    // Store-wide transaction lower bound, used to tell a truly out-of-range
    // instant apart from an in-range instant where the queried symbol simply did
    // not exist yet. Caller-supplied bounds win (domain-filtered daemon reads);
    // otherwise derive from `records` (the CLI holds the full graph).
    let store_min_tx = match store_tx_bounds {
        Some((min, _max)) => Some(min),
        None => store_transaction_bounds(records).map(|(min, _max)| min),
    };

    if named.is_empty() {
        diagnostics.push(TxDiagnostic {
            code: "no_named_symbol".to_owned(),
            message: format!("no Symbol named '{symbol_name}' exists in the store"),
        });
        // Even with no matching symbol, report whether the instant predates the
        // whole store so clients can distinguish an out-of-range temporal query
        // from a genuine in-range absence (a misspelled or removed name).
        if let Some(min) = store_min_tx
            && tx_instant < min
        {
            diagnostics.push(TxDiagnostic {
                code: "before_first_transaction".to_owned(),
                message: format!(
                    "tx-as-of '{tx_as_of}' precedes the earliest known store transaction ('{}'); empty view",
                    min.to_rfc3339()
                ),
            });
        }
        diagnostics.sort_by(|a, b| a.code.cmp(&b.code).then_with(|| a.message.cmp(&b.message)));
        diagnostics.dedup();
        return Ok(TxSymbolQuery {
            records: Vec::new(),
            diagnostics,
        });
    }

    // Track the earliest/latest known transaction time across all named
    // versions so we can report symbol-scoped not-yet-known / after-latest
    // conditions, distinct from the store-wide range computed below.
    let mut name_min_tx: Option<DateTime<chrono::FixedOffset>> = None;
    let mut name_max_tx: Option<DateTime<chrono::FixedOffset>> = None;

    // Best candidate per stable record ID.
    // Comparison key: (valid_time, transaction_time) when a valid-time axis is
    // requested; (transaction_time,) otherwise.
    let mut best: BTreeMap<&str, TxCandidate<'r>> = BTreeMap::new();

    for (named_idx, record) in named.iter().enumerate() {
        // Deterministic tie-break for equal transaction (and valid) times:
        // history records order by commit topological rank (a descendant
        // outranks its ancestor even within the same second); non-history
        // records fall back to input order (later-written wins on a tie).
        let tie = node_git_commit(record).map_or(named_idx, |c| commit_order.rank(c));
        let Some(tt_str) = record_transaction_time(record) else {
            diagnostics.push(TxDiagnostic {
                code: "missing_transaction_metadata".to_owned(),
                message: format!(
                    "record '{}' has no transaction-time metadata; excluded (no current-state fallback)",
                    record.id()
                ),
            });
            continue;
        };
        let Ok(tt) = DateTime::parse_from_rfc3339(tt_str) else {
            diagnostics.push(TxDiagnostic {
                code: "invalid_record_transaction_time".to_owned(),
                message: format!(
                    "record '{}' has an unparseable transaction_time '{tt_str}'; excluded",
                    record.id()
                ),
            });
            continue;
        };

        name_min_tx = Some(name_min_tx.map_or(tt, |m| m.min(tt)));
        name_max_tx = Some(name_max_tx.map_or(tt, |m| m.max(tt)));

        // Transaction axis: exclude anything committed after the instant.
        if tt > tx_instant {
            continue;
        }

        // Valid axis (when requested): exclude versions not yet true at V.
        let vt = if let Some(vt_req) = vt_requested {
            let Some(vt_str) = node_valid_time(record) else {
                diagnostics.push(TxDiagnostic {
                    code: "missing_valid_time".to_owned(),
                    message: format!(
                        "record '{}' has no valid_time but --as-of was supplied; excluded",
                        record.id()
                    ),
                });
                continue;
            };
            let Ok(vt) = DateTime::parse_from_rfc3339(vt_str) else {
                diagnostics.push(TxDiagnostic {
                    code: "invalid_record_valid_time".to_owned(),
                    message: format!(
                        "record '{}' has an unparseable valid_time '{vt_str}'; excluded",
                        record.id()
                    ),
                });
                continue;
            };
            if vt > vt_req {
                continue;
            }
            Some(vt)
        } else {
            None
        };

        let key = record.id();
        let replace = match best.get(key) {
            None => true,
            Some((_, prev_transaction, prev_valid, prev_tie)) => match (vt, prev_valid) {
                // Valid-time axis requested: prefer most-recent valid_time, then
                // most-recent transaction_time, then the commit/input tie-break.
                (Some(cur_vt), Some(prev)) => {
                    (cur_vt, tt, tie) > (*prev, *prev_transaction, *prev_tie)
                }
                // No valid-time axis: prefer most-recent transaction_time, then
                // the commit/input tie-break so equal stamps resolve to the
                // later version rather than the first one seen.
                _ => (tt, tie) > (*prev_transaction, *prev_tie),
            },
        };
        if replace {
            best.insert(key, (record, tt, vt, tie));
        }
    }

    // Out-of-range / not-yet-known diagnostics (annotate, never change the set).
    // `store_min_tx` was resolved above (caller-supplied or derived).
    if let Some(min) = store_min_tx
        && tx_instant < min
    {
        // Truly before any store activity.
        diagnostics.push(TxDiagnostic {
            code: "before_first_transaction".to_owned(),
            message: format!(
                "tx-as-of '{tx_as_of}' precedes the earliest known store transaction ('{}'); empty view",
                min.to_rfc3339()
            ),
        });
    } else if let Some(name_min) = name_min_tx
        && tx_instant < name_min
    {
        // In store range, but the queried symbol was introduced later: a real
        // in-range absence, not an out-of-range query.
        diagnostics.push(TxDiagnostic {
            code: "symbol_not_yet_known".to_owned(),
            message: format!(
                "symbol '{symbol_name}' has no transaction at or before '{tx_as_of}' (first known at '{}'); empty view",
                name_min.to_rfc3339()
            ),
        });
    }
    if let Some(max) = name_max_tx
        && tx_instant >= max
    {
        diagnostics.push(TxDiagnostic {
            code: "after_latest_transaction".to_owned(),
            message: format!(
                "tx-as-of '{tx_as_of}' is at or after the latest known transaction for symbol '{symbol_name}' ('{}'); view reflects all known history",
                max.to_rfc3339()
            ),
        });
    }

    // Sort by (span.start_line, record_id) to match the daemon `symbol_by_name`
    // contract and the non-tx handler, so a `max_results` truncation on the
    // daemon side keeps the documented prefix. Deterministic (AC7).
    let span_start = |r: &GraphRecord| -> Option<usize> {
        if let GraphRecord::Node { span, .. } = r {
            span.map(|s| s.start_line)
        } else {
            None
        }
    };
    // Cross-id supersession (AC2): a Symbol carrying `superseded_by` is dropped
    // once its replacement is *effective in the requested view* — the target
    // record (under any name, so renames count) has a version satisfying every
    // requested axis: transaction time ≤ `tx_as_of`, and valid time ≤ the
    // requested `--as-of` when supplied. Two-axis correctness: if the
    // replacement's `valid_time` is after the requested `--as-of`, it is not yet
    // effective and the older row that was true at that valid time is retained.
    let replacement_effective = |target: &str| -> bool {
        records.iter().any(|r| {
            if r.id() != target {
                return false;
            }
            let Some(tt) =
                record_transaction_time(r).and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            else {
                return false;
            };
            if tt > tx_instant {
                return false;
            }
            if let Some(vt_req) = vt_requested {
                let Some(vt) =
                    node_valid_time(r).and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                else {
                    return false;
                };
                if vt > vt_req {
                    return false;
                }
            }
            true
        })
    };
    let mut selected: Vec<&GraphRecord> = best.values().map(|(r, _, _, _)| *r).collect();
    selected.retain(|r| {
        let GraphRecord::Node {
            superseded_by: Some(target),
            ..
        } = r
        else {
            return true;
        };
        if replacement_effective(target.as_str()) {
            diagnostics.push(TxDiagnostic {
                code: "superseded".to_owned(),
                message: format!(
                    "record '{}' is superseded by '{target}', which is effective in the view; excluded",
                    r.id()
                ),
            });
            false
        } else {
            true
        }
    });

    // History-replay removal detection (AC2): `scan-history` emits a full symbol
    // snapshot per commit, so a symbol present at a commit always has a version
    // stamped there. A symbol is absent in the requested view when its latest
    // snapshot commit has a descendant commit, visible in the view, that no longer
    // carries that symbol.
    //
    // Axis handling: the "visible in the view" test bounds commits by the
    // requested axes — known by `tx_as_of` (transaction axis) and, with `--as-of
    // V`, valid at V (valid axis). So a removal that happens *after* V leaves the
    // row intact (the symbol was genuinely true at V), while a removal at or before
    // V correctly drops it — removal is evaluated against the requested valid-time
    // view rather than disabled for two-axis queries.
    //
    // Same-second commits, forks, and multi-repository stores: removal is keyed on
    // commit-DAG reachability, not a component-wide maximum. A symbol present at
    // its latest snapshot commit `L` was removed iff `L` has a *strict descendant*
    // commit, visible in the requested view, that carries no snapshot of that
    // stable ID. Sibling-branch commits — and forks that merely share an ancestor,
    // which land in one connected component — are not descendants of `L`, so a
    // later commit on another branch never makes a still-live symbol look removed.
    // Pruning is per stable record ID, so a surviving `foo` never masks a
    // different stable `foo` that was actually deleted.
    let is_temporal = |r: &GraphRecord| -> bool {
        matches!(
            r,
            GraphRecord::Node {
                temporal: Some(_),
                ..
            } | GraphRecord::Edge {
                temporal: Some(_),
                ..
            }
        )
    };
    // A record is visible in the requested view when it carries a commit known by
    // `tx_as_of` (transaction axis) and, with `--as-of V`, valid at V (valid axis).
    let axes_visible = |r: &GraphRecord| -> bool {
        if node_git_commit(r).is_none() {
            return false;
        }
        let Some(observed) =
            record_transaction_time(r).and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        else {
            return false;
        };
        if observed > tx_instant {
            return false;
        }
        if let Some(vt_req) = vt_requested {
            let Some(vt) = node_valid_time(r).and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            else {
                return false;
            };
            if vt > vt_req {
                return false;
            }
        }
        true
    };
    // Commits visible in the view, and — per stable ID — the commits at which that
    // symbol has a snapshot in the view.
    let in_view_commits: BTreeSet<&str> = records
        .iter()
        .filter(|r| axes_visible(r))
        .filter_map(node_git_commit)
        .collect();
    let mut id_commits: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for r in &named {
        if axes_visible(r)
            && let Some(commit) = node_git_commit(r)
        {
            id_commits.entry(r.id()).or_default().insert(commit);
        }
    }
    selected.retain(|r| {
        if !is_temporal(r) {
            return true;
        }
        let Some(last_commit) = node_git_commit(r) else {
            return true;
        };
        let snapshots = id_commits.get(r.id());
        // Removed iff a strict descendant of this symbol's latest snapshot is
        // visible in the view but carries no snapshot of this stable ID.
        let removed = commit_order
            .strict_descendants(last_commit)
            .into_iter()
            .any(|c| in_view_commits.contains(c) && snapshots.is_none_or(|s| !s.contains(c)));
        if removed {
            diagnostics.push(TxDiagnostic {
                code: "absent_at_transaction".to_owned(),
                message: format!(
                    "symbol '{symbol_name}' (record '{}') was absent at a commit descending from its last snapshot '{last_commit}' in the requested view; excluded",
                    r.id()
                ),
            });
        }
        !removed
    });

    selected.sort_by(|a, b| {
        span_start(a)
            .cmp(&span_start(b))
            .then_with(|| a.id().cmp(b.id()))
    });

    diagnostics.sort_by(|a, b| a.code.cmp(&b.code).then_with(|| a.message.cmp(&b.message)));
    diagnostics.dedup();

    Ok(TxSymbolQuery {
        records: selected,
        diagnostics,
    })
}

// ── Task Evidence Queries (Issue #48) ──────────────────────────────────────────
