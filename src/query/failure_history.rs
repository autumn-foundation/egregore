use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::{
    MemoryAuditDiagnostic, MemoryEvidenceItem, RepositoryIndex, TaskResolveError,
    is_codegraph_kind, is_project_kind, is_verification_kind, record_node_kind, resolve_task_ids,
};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

/// Which handle type a failure-history query resolved from.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FailureTargetKind {
    /// A code symbol (record ID or exact name).
    Symbol,
    /// A repo-relative file path.
    File,
    /// A task handle (canonical ID, GitHub handle, or local JSONL handle).
    Task,
    /// A source/provenance handle naming failures directly.
    Source,
}

impl FailureTargetKind {
    /// Stable wire string for the resolved handle type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::File => "file",
            Self::Task => "task",
            Self::Source => "source",
        }
    }
}

/// A resolved failure-history target.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ResolvedFailureTarget {
    /// Original handle as provided by the operator.
    pub handle: String,
    /// Which handle type matched.
    pub kind: FailureTargetKind,
    /// Live code/task record IDs to traverse inbound from. Empty for `Source`.
    pub anchor_ids: BTreeSet<String>,
    /// Failure/verification record IDs matched directly by a source handle.
    pub seed_failures: BTreeSet<String>,
    /// True when the handle named a record that exists only as a tombstone.
    pub stale: bool,
}

impl ResolvedFailureTarget {
    /// Returns true when the handle resolved to nothing live in the store, so
    /// the caller emits a `no_match` (or `stale_handle`) envelope (AC6).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor_ids.is_empty() && self.seed_failures.is_empty()
    }
}

/// Error returned when resolving a failure-history handle (AC2).
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FailureHandleError {
    /// The handle matched targets in more than one repository without `--repo`.
    Ambiguous {
        /// The query handle.
        handle: String,
        /// The candidate record IDs the handle resolved to.
        candidates: Vec<String>,
    },
    /// The handle is malformed (empty or a malformed canonical ID).
    Unsupported {
        /// The query handle.
        handle: String,
        /// Why the handle is unsupported.
        message: String,
    },
}

/// Read-time status of one failed attempt relative to the queried target.
///
/// `SinceResolved` means a later passing verification exists on a shared target
/// handle; `StillFailing` is the conservative default whenever supersession
/// cannot be proven (including missing or unparseable timestamps). The failed
/// attempt is never deleted, hidden, or rewritten — this is a purely additive
/// read-time annotation (AC5).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ResolutionStatus {
    /// No later passing verification supersedes this failure on a shared target.
    StillFailing,
    /// A later passing verification on a shared target supersedes this failure.
    SinceResolved,
}

impl ResolutionStatus {
    /// Stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StillFailing => "still_failing",
            Self::SinceResolved => "since_resolved",
        }
    }
}

/// One prior failed attempt reached by inbound traversal, with its read-time
/// resolution status and the passing verification (if any) that resolved it.
#[derive(Debug, Clone)]
pub struct FailureAttempt<'a> {
    /// The reached failure record plus the relation that connected it.
    pub item: MemoryEvidenceItem<'a>,
    /// Read-time `still_failing` / `since_resolved` status (AC5).
    pub status: ResolutionStatus,
    /// Record ID of the later passing verification that resolved it, if any.
    pub resolved_by: Option<&'a str>,
    /// The target handle (anchor record ID) this attempt linked to.
    pub matched_target: &'a str,
}

/// Structured prior-failed-attempt context returned by [`failure_history_context`].
///
/// Sections keep runtime failure evidence separate from agent-authored failure
/// claims (AC4); every vector is canonically ordered for determinism (AC7).
#[derive(Debug, Default, Clone)]
pub struct FailureHistoryContext<'a> {
    /// Resolved handle type (`symbol`/`file`/`task`/`source`).
    pub target_kind: &'static str,
    /// Resolved code/task anchor record IDs, canonically sorted.
    pub target_ids: Vec<String>,
    /// Runtime command/test/CI failures (verification domain, status
    /// fail/error/timeout) — trust class `verification_evidence`.
    pub runtime_failures: Vec<FailureAttempt<'a>>,
    /// Agent-authored `Failure` claims — trust class `agent_authored`.
    pub agent_failures: Vec<FailureAttempt<'a>>,
    /// Later PASSING verifications on a shared target — a separate contrasting
    /// section that never hides the older failures (AC5).
    pub superseding_successes: Vec<MemoryEvidenceItem<'a>>,
    /// Patch artifacts produced by reached failures (1 hop, `PRODUCED_PATCH`).
    pub patch_artifacts: Vec<MemoryEvidenceItem<'a>>,
    /// `AgentSession` provenance for reached agent failures.
    pub agent_sessions: Vec<&'a GraphRecord>,
    /// `Agent` provenance for reached agent failures.
    pub agents: Vec<&'a GraphRecord>,
    /// Stable diagnostics (unresolved links, stale targets, missing timestamps).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
}

impl FailureHistoryContext<'_> {
    /// Returns true when the resolved target has no recorded failures. This is a
    /// real (exit-0, `ok:true`) empty answer, not a handle no-match.
    #[must_use]
    pub const fn has_no_failures(&self) -> bool {
        self.runtime_failures.is_empty() && self.agent_failures.is_empty()
    }
}

/// Verification statuses that count as a failed runtime attempt.
fn is_failed_status(status: Option<&str>) -> bool {
    matches!(status, Some("fail" | "error" | "timeout"))
}

/// Verification statuses that count as a passing runtime success.
fn is_pass_status(status: Option<&str>) -> bool {
    matches!(status, Some("pass"))
}

/// A reached verification/failure record with its accumulated anchor set and the
/// relation it was first reached through. Keyed by record ID for dedup + order.
type CandidateMap<'a> = BTreeMap<&'a str, (&'a GraphRecord, BTreeSet<&'a str>, &'a str)>;

/// True when a verification record is a failed runtime attempt. Importers emit
/// `CommandRun` nodes with an `exit_code` and no `status`, so a nonzero exit code
/// is consulted as a fallback when `status` is absent (issue #63 review).
fn is_failed_verification(node: &GraphRecord) -> bool {
    let GraphRecord::Node {
        status, exit_code, ..
    } = node
    else {
        return false;
    };
    is_failed_status(status.as_deref())
        || (status.is_none() && matches!(exit_code, Some(c) if *c != 0))
}

/// True for a verification record whose status is `pass`, or — symmetric with
/// [`is_failed_verification`] — a status-absent `CommandRun` with a zero exit
/// code, so a later successful command can supersede a prior failure.
fn is_pass_status_node(node: &GraphRecord) -> bool {
    let GraphRecord::Node {
        status, exit_code, ..
    } = node
    else {
        return false;
    };
    is_pass_status(status.as_deref()) || (status.is_none() && *exit_code == Some(0))
}

/// Merges a reached candidate into a classification map, unioning anchor sets
/// when the same record is reached through more than one target.
fn merge_candidate<'a>(
    map: &mut CandidateMap<'a>,
    node: &'a GraphRecord,
    anchors: &BTreeSet<&'a str>,
    rel: &'a str,
) {
    let entry = map
        .entry(node.id())
        .or_insert_with(|| (node, BTreeSet::new(), rel));
    entry.1.extend(anchors.iter().copied());
}

/// Routes a reached record into the agent-failure, runtime-failure, or passing-
/// success classification map by kind and status.
fn route_candidate<'a>(
    node: &'a GraphRecord,
    anchors: &BTreeSet<&'a str>,
    rel: &'a str,
    agent: &mut CandidateMap<'a>,
    runtime: &mut CandidateMap<'a>,
    success: &mut CandidateMap<'a>,
) {
    match record_node_kind(node) {
        Some(NodeKind::Failure) => merge_candidate(agent, node, anchors, rel),
        Some(k) if is_verification_kind(k) => {
            if is_failed_verification(node) {
                merge_candidate(runtime, node, anchors, rel);
            } else if is_pass_status_node(node) {
                merge_candidate(success, node, anchors, rel);
            }
        }
        _ => {}
    }
}

/// Cross-domain relations that connect a failure/verification record to a code
/// or task target. A record reaching a target through one of these is a
/// candidate prior attempt on that target.
const FAILURE_TARGET_LINK_RELS: &[&str] = &[
    "FAILED_ON",
    "TOUCHED_FILE",
    "MENTIONS_SYMBOL",
    "OBSERVES",
    "REFERENCES_TASK",
    "PRODUCED_EVIDENCE",
    "HAS_EVIDENCE",
    "VALIDATED_BY",
];

/// Parses a node's wall-clock instant (`executed_at` preferred, else
/// `observed_at`) as an RFC-3339 timestamp. Returns `None` for non-nodes or
/// unparseable/absent timestamps so the caller stays conservative (AC5/AC7).
fn node_instant(record: &GraphRecord) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let GraphRecord::Node {
        executed_at,
        observed_at,
        ..
    } = record
    else {
        return None;
    };
    let raw = executed_at.as_deref().or(observed_at.as_deref())?;
    chrono::DateTime::parse_from_rfc3339(raw).ok()
}

/// Resolves a code or task handle to the target record IDs a failure-history
/// query traverses inbound from (AC2).
///
/// Resolution is structural — it never falls back to transcript text search
/// (AC6). The attempt order is: canonical code record ID, then task / task-source
/// handle (reusing [`resolve_task_ids`]), then repo-relative file path, then
/// exact symbol name, then a source/provenance handle naming failures directly.
///
/// `repo_scope`, when set, restricts file/symbol resolution to one repository;
/// without it, a file path or symbol name matching targets in more than one
/// repository is reported as `Ambiguous` rather than resolved implicitly.
///
/// # Errors
///
/// Returns [`FailureHandleError::Unsupported`] for an empty handle or a malformed
/// canonical task ID, and [`FailureHandleError::Ambiguous`] for a cross-repository
/// collision.
#[allow(clippy::too_many_lines)]
pub fn resolve_failure_handle(
    records: &[GraphRecord],
    handle: &str,
    repo_index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> Result<ResolvedFailureTarget, FailureHandleError> {
    if handle.is_empty() {
        return Err(FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: "handle cannot be empty".to_owned(),
        });
    }

    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    // Latest-write-wins tombstone / temporal liveness (issue #421): over an
    // append-only `--graph`, a record re-ingested AFTER its own tombstone is live
    // again, and a record/edge that still has a temporal (history) version is not
    // deleted for history-bearing reads. The shared gate reports a tombstone
    // active only when it is the id's most recent write, matching the embedded
    // current-state read so `--graph` and `--data-dir` agree. See
    // `super::liveness`.
    let liveness = Liveness::new(records);
    let deleted = |id: &str| liveness.deleted(id);
    let in_scope = |id: &str| -> bool {
        repo_scope.is_none_or(|scope| repo_index.owner_of(id) == Some(scope))
    };

    let empty_target = |kind: FailureTargetKind, stale: bool| ResolvedFailureTarget {
        handle: handle.to_owned(),
        kind,
        anchor_ids: BTreeSet::new(),
        seed_failures: BTreeSet::new(),
        stale,
    };

    // 1) Canonical code record ID (codegraph:vN:<hex>). A malformed canonical ID
    //    is unsupported (exit 1); a well-formed but absent or out-of-scope ID
    //    resolves to nothing (caller emits no_match).
    if handle.starts_with("codegraph:") {
        let parts: Vec<&str> = handle.split(':').collect();
        let well_formed = parts.len() == 3
            && parts[0] == "codegraph"
            && parts[1].starts_with('v')
            && parts[1].len() > 1
            && parts[1][1..].chars().all(|c| c.is_ascii_digit())
            && parts[2].len() == 64
            && parts[2].chars().all(|c| c.is_ascii_hexdigit());
        if !well_formed {
            return Err(FailureHandleError::Unsupported {
                handle: handle.to_owned(),
                message: "malformed canonical codegraph ID".to_owned(),
            });
        }
        if deleted(handle) {
            return Ok(empty_target(FailureTargetKind::Symbol, true));
        }
        for r in records {
            if let GraphRecord::Node { id, kind, .. } = r
                && id == handle
                && is_codegraph_kind(*kind)
                && in_scope(handle)
            {
                let kind = if matches!(kind, NodeKind::File) {
                    FailureTargetKind::File
                } else {
                    FailureTargetKind::Symbol
                };
                let mut anchor_ids = BTreeSet::new();
                anchor_ids.insert(handle.to_owned());
                return Ok(ResolvedFailureTarget {
                    handle: handle.to_owned(),
                    kind,
                    anchor_ids,
                    seed_failures: BTreeSet::new(),
                    stale: false,
                });
            }
        }
        return Ok(empty_target(FailureTargetKind::Symbol, false));
    }

    // 2) Task / task-source handle — reuse the task resolver verbatim.
    match resolve_task_ids(records, handle) {
        Ok(ids) => {
            // Drop tombstoned (deleted) task IDs from the current-state read, the
            // same way code/file/symbol handles are filtered. A handle that named
            // only deleted tasks is stale, not a live target.
            let had_match = !ids.is_empty();
            let live: BTreeSet<String> = ids
                .into_iter()
                .filter(|id| !tombstoned.contains(id.as_str()))
                .collect();
            if !live.is_empty() {
                // Expand to the tasks' acceptance criteria so failures/verifications
                // attached to an `AcceptanceCriterion` are included (mirrors the
                // task-evidence query, which expands tasks to their ACs).
                let mut anchor_ids = live.clone();
                for r in records {
                    if let GraphRecord::Node {
                        id,
                        kind: NodeKind::AcceptanceCriterion,
                        parent_task_id: Some(parent),
                        ..
                    } = r
                        && live.contains(parent)
                        && !tombstoned.contains(id.as_str())
                    {
                        anchor_ids.insert(id.clone());
                    }
                }
                // The project graph also represents AC ownership with
                // `AcceptanceCriterion --OWNED_BY_TASK--> Task` edges (as consumed
                // by `task_evidence_context`); include ACs connected only by the
                // edge, without the denormalized `parent_task_id` field.
                for r in records {
                    if let GraphRecord::Edge {
                        id: edge_id,
                        label: EdgeLabel::OwnedByTask,
                        source,
                        target,
                        ..
                    } = r
                        && live.contains(target)
                        && !tombstoned.contains(edge_id.as_str())
                        && !tombstoned.contains(source.as_str())
                    {
                        anchor_ids.insert(source.clone());
                    }
                }
                return Ok(ResolvedFailureTarget {
                    handle: handle.to_owned(),
                    kind: FailureTargetKind::Task,
                    anchor_ids,
                    seed_failures: BTreeSet::new(),
                    stale: false,
                });
            }
            // No live task. Only a canonical `project:` ID definitively names a
            // (now-absent or deleted) task and stops here; a GitHub/JSONL handle
            // may also be a failure's source handle, so fall through to the
            // source/file/symbol steps rather than returning no_match early.
            if handle.starts_with("project:") {
                return Ok(empty_target(
                    FailureTargetKind::Task,
                    had_match || tombstoned.contains(handle),
                ));
            }
        }
        Err(TaskResolveError::Ambiguous {
            handle: h,
            candidates,
        }) => {
            return Err(FailureHandleError::Ambiguous {
                handle: h,
                candidates,
            });
        }
        Err(TaskResolveError::Unsupported { handle: h, message }) => {
            // A malformed canonical task ID is a hard error; an unrecognized
            // format merely means "not a task handle" — fall through.
            if handle.starts_with("project:") {
                return Err(FailureHandleError::Unsupported { handle: h, message });
            }
        }
    }

    // A path/name handle that matches only tombstoned (deleted) records is stale,
    // not a never-seen handle: track that so step 6 reports `stale_handle` rather
    // than `no_match`, the same distinction code/task handles already make.
    let mut saw_tombstoned = false;

    // 3) Repo-relative file path.
    let mut file_matches: BTreeSet<String> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path: Some(path),
            ..
        } = r
            && path == handle
            && in_scope(id)
        {
            if deleted(id.as_str()) {
                saw_tombstoned = true;
            } else {
                file_matches.insert(id.clone());
            }
        }
    }
    if !file_matches.is_empty() {
        if let Some(candidates) = cross_repo_ambiguity(&file_matches, repo_index, repo_scope) {
            return Err(FailureHandleError::Ambiguous {
                handle: handle.to_owned(),
                candidates,
            });
        }
        return Ok(ResolvedFailureTarget {
            handle: handle.to_owned(),
            kind: FailureTargetKind::File,
            anchor_ids: file_matches,
            seed_failures: BTreeSet::new(),
            stale: false,
        });
    }

    // 4) Exact symbol name. Several symbols of the same name in one repository
    //    form a multi-ID target; the same name across repositories is ambiguous.
    let mut symbol_matches: BTreeSet<String> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name: Some(name),
            ..
        } = r
            && name == handle
            && in_scope(id)
        {
            if deleted(id.as_str()) {
                saw_tombstoned = true;
            } else {
                symbol_matches.insert(id.clone());
            }
        }
    }
    if !symbol_matches.is_empty() {
        if let Some(candidates) = cross_repo_ambiguity(&symbol_matches, repo_index, repo_scope) {
            return Err(FailureHandleError::Ambiguous {
                handle: handle.to_owned(),
                candidates,
            });
        }
        return Ok(ResolvedFailureTarget {
            handle: handle.to_owned(),
            kind: FailureTargetKind::Symbol,
            anchor_ids: symbol_matches,
            seed_failures: BTreeSet::new(),
            stale: false,
        });
    }

    // 5) Source / provenance handle naming failures directly. A handle that is
    //    itself a tombstoned record ID (e.g. a retracted AgentSession) is stale —
    //    its live child evidence must not resurrect it as a source target.
    let mut seeds: BTreeSet<String> = BTreeSet::new();
    if deleted(handle) {
        saw_tombstoned = true;
    } else {
        for r in records {
            if let GraphRecord::Node {
                id,
                kind,
                session_id,
                source_handle,
                source_artifact_path,
                source_artifact_hash,
                ..
            } = r
                && (matches!(kind, NodeKind::Failure) || is_verification_kind(*kind))
                && (source_handle.as_deref() == Some(handle)
                    || source_artifact_path.as_deref() == Some(handle)
                    || source_artifact_hash.as_deref() == Some(handle)
                    || session_id.as_deref() == Some(handle))
            {
                if deleted(id.as_str()) {
                    saw_tombstoned = true;
                } else {
                    seeds.insert(id.clone());
                }
            }
        }
        // If the handle is an `AgentSession` record ID, resolve the failures
        // authored in that session even when provenance lives only in
        // `AUTHORED_BY` edges or the session_id value differs from the record ID
        // (the command emits these record IDs as citable provenance).
        let session_key = records.iter().find_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::AgentSession,
                session_id,
                name,
                ..
            } if id == handle => Some(session_id.clone().or_else(|| name.clone())),
            _ => None,
        });
        if let Some(key) = session_key {
            if let Some(k) = key.as_deref() {
                for r in records {
                    if let GraphRecord::Node {
                        id,
                        kind,
                        session_id: Some(sid),
                        ..
                    } = r
                        && (matches!(kind, NodeKind::Failure) || is_verification_kind(*kind))
                        && sid == k
                        && !deleted(id.as_str())
                    {
                        seeds.insert(id.clone());
                    }
                }
            }
            let authored_sources: BTreeSet<&str> = records
                .iter()
                .filter_map(|r| match r {
                    GraphRecord::Edge {
                        id: eid,
                        label: EdgeLabel::AuthoredBy,
                        source,
                        target,
                        ..
                    } if target == handle && !deleted(eid.as_str()) => Some(source.as_str()),
                    _ => None,
                })
                .collect();
            if !authored_sources.is_empty() {
                for r in records {
                    if let GraphRecord::Node { id, kind, .. } = r
                        && authored_sources.contains(id.as_str())
                        && (matches!(kind, NodeKind::Failure) || is_verification_kind(*kind))
                        && !deleted(id.as_str())
                    {
                        seeds.insert(id.clone());
                    }
                }
            }
        }
    }
    if !seeds.is_empty() {
        return Ok(ResolvedFailureTarget {
            handle: handle.to_owned(),
            kind: FailureTargetKind::Source,
            anchor_ids: BTreeSet::new(),
            seed_failures: seeds,
            stale: false,
        });
    }

    // 6) Nothing matched. A handle that named a tombstoned record — or only
    //    tombstoned path/name matches — is stale; otherwise it is a plain
    //    no-match. The resolver never guesses a replacement (AC6).
    Ok(empty_target(
        FailureTargetKind::Symbol,
        saw_tombstoned || deleted(handle),
    ))
}

/// Returns the sorted candidate IDs when `matches` spans more than one
/// repository and no `--repo` scope was given, else `None`.
fn cross_repo_ambiguity(
    matches: &BTreeSet<String>,
    repo_index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> Option<Vec<String>> {
    if repo_scope.is_some() {
        return None;
    }
    // Unattributed (legacy) records form their own ambiguity group, matching the
    // repository-scoped query behavior: a handle matching both a repo-owned record
    // and an unattributed one must fail closed rather than silently merge them.
    let owners: BTreeSet<Option<&str>> = matches.iter().map(|id| repo_index.owner_of(id)).collect();
    if owners.len() > 1 {
        Some(matches.iter().cloned().collect())
    } else {
        None
    }
}

/// Builds the prior-failed-attempt context for a resolved target (AC1, AC3-AC7).
///
/// The traversal reads only existing edges and evidence links and never reads
/// raw transcript bodies or infers a failure cause when supporting evidence is
/// absent (AC8, success metric). It is bounded to two hops: failures /
/// verifications linked directly to a target (hop 1), and the patch artifacts
/// and session provenance attached to those failures (hop 2).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn failure_history_context<'a>(
    records: &'a [GraphRecord],
    target: &ResolvedFailureTarget,
) -> FailureHistoryContext<'a> {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    // Latest-write-wins tombstone / temporal liveness (issue #421): over an
    // append-only `--graph`, a record re-ingested AFTER its own tombstone is live
    // again, and history-bearing reads keep records/edges that carry a temporal
    // version even when a current-state tombstone shares their ID. The shared
    // gate reports a tombstone active only when it is the id's most recent write,
    // matching the embedded current-state read so `--graph` and `--data-dir`
    // agree. See `super::liveness`.
    let liveness = Liveness::new(records);
    let deleted = |id: &str| liveness.deleted(id);
    let present = |id: &str| -> Option<&'a GraphRecord> {
        if deleted(id) {
            None
        } else {
            by_id.get(id).copied()
        }
    };

    // Outgoing edges keyed by source (for PRODUCED_PATCH + provenance walk).
    let mut edges_from: BTreeMap<&str, Vec<(&EdgeLabel, &str)>> = BTreeMap::new();
    // Inbound index: target_id -> sorted (source_id, relation), from both graph
    // edges and denormalized node evidence_links (dual-source robustness).
    let mut inbound: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for r in records {
        match r {
            GraphRecord::Edge {
                id,
                label,
                source,
                target,
                ..
            } => {
                // Skip retracted edges: a tombstoned `FAILED_ON` / `VALIDATED_BY`
                // / `PRODUCED_PATCH` edge must not surface stale relationships on
                // current-state reads, matching `symbol_context`'s convention —
                // unless the edge has a temporal version (history read).
                if deleted(id.as_str()) {
                    continue;
                }
                edges_from
                    .entry(source.as_str())
                    .or_default()
                    .push((label, target.as_str()));
                inbound
                    .entry(target.as_str())
                    .or_default()
                    .push((source.as_str(), label.as_str()));
            }
            GraphRecord::Node {
                id,
                evidence_links: Some(links),
                ..
            } => {
                for link in links {
                    if let Some(t) = link.target_record_id.as_deref() {
                        inbound
                            .entry(t)
                            .or_default()
                            .push((id.as_str(), link.relation.as_str()));
                    }
                }
            }
            _ => {}
        }
    }
    for list in inbound.values_mut() {
        list.sort_unstable();
        list.dedup();
    }

    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    // Anchor universe: the code/task targets to traverse inbound from. For a
    // source handle, derive it from the seed failures' outbound code/task links.
    let mut anchor_universe: BTreeSet<&str> = BTreeSet::new();
    for a in &target.anchor_ids {
        if let Some((id, _)) = by_id.get_key_value(a.as_str()) {
            anchor_universe.insert(id);
        } else {
            let code = if matches!(target.kind, FailureTargetKind::Task) {
                "missing_task_ref"
            } else if deleted(a.as_str()) {
                "stale_code_handle"
            } else {
                "missing_code_handle"
            };
            diagnostics.push(MemoryAuditDiagnostic {
                code: code.to_owned(),
                source_record_id: target.handle.clone(),
                target_handle: a.clone(),
                relation: String::new(),
                target_domain: String::new(),
            });
        }
    }
    if matches!(target.kind, FailureTargetKind::Source) {
        for seed in &target.seed_failures {
            if let Some(node) = present(seed) {
                for anchor in outbound_code_task_targets(node, &edges_from, &present) {
                    anchor_universe.insert(anchor);
                }
            }
        }
    }

    // Candidate prior attempts: records linking to any anchor through a relevant
    // relation, plus the forced seed failures of a source handle. Track each
    // candidate's anchor set and the first relation it linked through.
    let mut candidate_anchors: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut candidate_rel: BTreeMap<&str, &str> = BTreeMap::new();
    for anchor in &anchor_universe {
        let Some(srcs) = inbound.get(*anchor) else {
            continue;
        };
        for (src, rel) in srcs {
            if !FAILURE_TARGET_LINK_RELS.contains(rel) {
                continue;
            }
            if present(src).is_none() {
                continue;
            }
            candidate_anchors.entry(src).or_default().insert(anchor);
            candidate_rel.entry(src).or_insert(rel);
        }
    }
    for seed in &target.seed_failures {
        if let Some((id, _)) = by_id.get_key_value(seed.as_str())
            && present(id).is_some()
        {
            candidate_anchors.entry(id).or_default();
            candidate_rel.entry(id).or_insert("SOURCE_HANDLE");
        }
    }
    // `CLOSES_ACCEPTANCE_CRITERION` runs AcceptanceCriterion -> Verification, so
    // the verification that closes an AC is reached by following the AC anchor's
    // OUTBOUND closure edge (or its denormalized `verification_link_id`) rather
    // than an inbound link. Without this, a task whose AC was closed by a passing
    // run would show no superseding success.
    for anchor in &anchor_universe {
        let Some(ac_node) = present(anchor) else {
            continue;
        };
        if !matches!(
            record_node_kind(ac_node),
            Some(NodeKind::AcceptanceCriterion)
        ) {
            continue;
        }
        if let Some(edges) = edges_from.get(*anchor) {
            for (label, t) in edges {
                if matches!(label, EdgeLabel::ClosesAcceptanceCriterion) && present(t).is_some() {
                    candidate_anchors.entry(t).or_default().insert(anchor);
                    candidate_rel
                        .entry(t)
                        .or_insert("CLOSES_ACCEPTANCE_CRITERION");
                }
            }
        }
        // Denormalized form: an AC may carry `verification_link_id` without a
        // synthesized closure edge (project-imported / daemon-written data).
        if let GraphRecord::Node {
            verification_link_id: Some(vid),
            ..
        } = ac_node
            && present(vid).is_some()
        {
            candidate_anchors
                .entry(vid.as_str())
                .or_default()
                .insert(anchor);
            candidate_rel
                .entry(vid.as_str())
                .or_insert("CLOSES_ACCEPTANCE_CRITERION");
        }
    }

    // PatchArtifact relay: `link_evidence` attaches a patch to a File via
    // `TOUCHED_FILE` while the failing attempt is `Failure --FAILED_ON-->
    // PatchArtifact`. A file/symbol query therefore reaches the patch, not the
    // failure; walk each reached patch's inbound `FAILED_ON` edges so those
    // failures enter the candidate set on the same anchor.
    let patch_relays: Vec<(&str, BTreeSet<&str>)> = candidate_anchors
        .iter()
        .filter(|(cid, _)| {
            present(cid)
                .is_some_and(|n| matches!(record_node_kind(n), Some(NodeKind::PatchArtifact)))
        })
        .map(|(cid, anchors)| (*cid, anchors.clone()))
        .collect();
    for (patch_id, anchors) in patch_relays {
        let Some(srcs) = inbound.get(patch_id) else {
            continue;
        };
        for (src, rel) in srcs {
            if *rel != "FAILED_ON"
                || !present(src)
                    .is_some_and(|n| matches!(record_node_kind(n), Some(NodeKind::Failure)))
            {
                continue;
            }
            for a in &anchors {
                candidate_anchors.entry(src).or_default().insert(a);
            }
            candidate_rel.entry(src).or_insert("FAILED_ON");
        }
    }

    // ── Classify candidates into agent failures, runtime failures, and passing
    //    successes, unioning anchor sets when a record is reached more than once. ──
    let source_kind = matches!(target.kind, FailureTargetKind::Source);
    let mut agent: CandidateMap<'a> = BTreeMap::new();
    let mut runtime: CandidateMap<'a> = BTreeMap::new();
    let mut success: CandidateMap<'a> = BTreeMap::new();

    for (cid, anchors) in &candidate_anchors {
        let Some(node) = present(cid) else { continue };
        let rel = candidate_rel.get(cid).copied().unwrap_or("RELATES_TO");
        // A source/provenance handle names failures directly: only the matched
        // seeds are prior attempts. Anchor-linked records are kept solely as
        // superseding successes, never as unrelated failures from other sessions.
        if source_kind && !target.seed_failures.contains(*cid) {
            if is_pass_status_node(node) {
                merge_candidate(&mut success, node, anchors, rel);
            }
            continue;
        }
        route_candidate(node, anchors, rel, &mut agent, &mut runtime, &mut success);
    }

    // Hop 2: from each reached agent `Failure`, follow PRODUCED_PATCH / FAILED_ON
    // to its patch artifact and runtime command/test evidence — the Codex/traj
    // importers attach the rejected patch and failed `CommandRun` to the `Failure`
    // via FAILED_ON (PRODUCED_PATCH comes from the AgentTurn) — and walk the
    // AUTHORED_BY / SESSION_OF chain for provenance.
    let mut patch: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut sessions: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut agents: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let agent_seeds: Vec<(&'a str, &'a GraphRecord, BTreeSet<&'a str>)> = agent
        .iter()
        .map(|(id, (node, anchors, _))| (*id, *node, anchors.clone()))
        .collect();
    for (fid, fnode, fanchors) in &agent_seeds {
        collect_failure_links(
            fnode,
            fid,
            fanchors,
            &edges_from,
            &present,
            &mut patch,
            &mut runtime,
            &mut success,
            &mut diagnostics,
        );
        collect_provenance(fid, &edges_from, &present, &mut sessions, &mut agents);
    }

    // Reached-failure instants per anchor: used both to compute read-time status
    // and to keep only successes that actually supersede a failure (AC5).
    let mut failure_instant_by_anchor: BTreeMap<&str, Vec<chrono::DateTime<chrono::FixedOffset>>> =
        BTreeMap::new();
    for (node, anchors, _) in agent.values().chain(runtime.values()) {
        if let Some(inst) = node_instant(node) {
            for a in anchors {
                failure_instant_by_anchor.entry(a).or_default().push(inst);
            }
        }
    }

    // A passing verification is surfaced only when it is strictly later than at
    // least one reached failure on a shared target. A pass with no failures, or a
    // pass that predates every failure, superseded nothing and is not shown (AC5).
    let mut success_by_anchor: BTreeMap<&str, Vec<(chrono::DateTime<chrono::FixedOffset>, &str)>> =
        BTreeMap::new();
    let mut superseding: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    for (sid, (node, anchors, rel)) in &success {
        let Some(inst) = node_instant(node) else {
            diagnostics.push(MemoryAuditDiagnostic {
                code: "missing_timestamp".to_owned(),
                source_record_id: (*sid).to_owned(),
                target_handle: (*sid).to_owned(),
                relation: "executed_at".to_owned(),
                target_domain: "verification".to_owned(),
            });
            continue;
        };
        let supersedes = anchors.iter().any(|a| {
            failure_instant_by_anchor
                .get(a)
                .is_some_and(|fs| fs.iter().any(|fi| *fi < inst))
        });
        if !supersedes {
            continue;
        }
        for a in anchors {
            success_by_anchor
                .entry(a)
                .or_default()
                .push((inst, node.id()));
        }
        superseding
            .entry(node.id())
            .or_insert_with(|| MemoryEvidenceItem {
                record: node,
                relation: (*rel).to_owned(),
            });
    }
    for list in success_by_anchor.values_mut() {
        list.sort_unstable();
    }

    // Build the failed-attempt items, computing each one's read-time status.
    let mut agent_failures: Vec<FailureAttempt<'a>> = Vec::new();
    let mut runtime_failures: Vec<FailureAttempt<'a>> = Vec::new();
    for (is_agent, source) in [(true, &agent), (false, &runtime)] {
        for (node, anchors, rel) in source.values() {
            // AC6: surface this attempt's own unresolved / stale / triple-only
            // evidence links rather than silently dropping them.
            push_attempt_link_diagnostics(node, &tombstoned, &by_id, &mut diagnostics);
            // AC5/AC6: an undated failure cannot be proven resolved; record why
            // its status stays `still_failing` so callers can tell "no later pass"
            // apart from "timestamp unusable".
            if node_instant(node).is_none() {
                diagnostics.push(MemoryAuditDiagnostic {
                    code: "missing_timestamp".to_owned(),
                    source_record_id: node.id().to_owned(),
                    target_handle: node.id().to_owned(),
                    relation: if is_agent {
                        "observed_at"
                    } else {
                        "executed_at"
                    }
                    .to_owned(),
                    target_domain: if is_agent {
                        "agent_memory"
                    } else {
                        "verification"
                    }
                    .to_owned(),
                });
            }
            let (status, resolved_by) =
                compute_resolution_status(node, anchors, &success_by_anchor);
            let matched_target = anchors.iter().min().copied().unwrap_or("");
            let attempt = FailureAttempt {
                item: MemoryEvidenceItem {
                    record: node,
                    relation: (*rel).to_owned(),
                },
                status,
                resolved_by,
                matched_target,
            };
            if is_agent {
                agent_failures.push(attempt);
            } else {
                runtime_failures.push(attempt);
            }
        }
    }

    // Canonical ordering: oldest-first by parsed instant (None last), then ID.
    sort_attempts(&mut agent_failures);
    sort_attempts(&mut runtime_failures);

    let mut superseding_successes: Vec<MemoryEvidenceItem<'a>> =
        superseding.into_values().collect();
    superseding_successes.sort_by(|a, b| {
        node_instant(a.record)
            .cmp(&node_instant(b.record))
            .then_with(|| a.record.id().cmp(b.record.id()))
    });

    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
            .then_with(|| a.target_domain.cmp(&b.target_domain))
    });
    diagnostics.dedup();

    let target_kind = target.kind.as_str();
    let mut target_ids: Vec<String> = anchor_universe.iter().map(|s| (*s).to_owned()).collect();
    target_ids.sort();

    FailureHistoryContext {
        target_kind,
        target_ids,
        runtime_failures,
        agent_failures,
        superseding_successes,
        patch_artifacts: patch.into_values().collect(),
        agent_sessions: sessions.into_values().collect(),
        agents: agents.into_values().collect(),
        diagnostics,
    }
}

/// Sorts failed attempts oldest-first by parsed instant (absent last), then ID.
fn sort_attempts(attempts: &mut [FailureAttempt<'_>]) {
    attempts.sort_by(|a, b| {
        let ai = node_instant(a.item.record);
        let bi = node_instant(b.item.record);
        // `None` (absent timestamp) sorts last: present-and-ordered first.
        match (ai, bi) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.item.record.id().cmp(b.item.record.id()))
    });
}

/// Returns the read-time resolution status for one failed attempt: `SinceResolved`
/// iff a passing verification on a shared anchor has a parsed instant strictly
/// after the attempt's, else `StillFailing` (AC5).
fn compute_resolution_status<'a>(
    node: &'a GraphRecord,
    anchors: &BTreeSet<&str>,
    success_by_anchor: &BTreeMap<&str, Vec<(chrono::DateTime<chrono::FixedOffset>, &'a str)>>,
) -> (ResolutionStatus, Option<&'a str>) {
    let Some(fail_time) = node_instant(node) else {
        return (ResolutionStatus::StillFailing, None);
    };
    let mut best: Option<(chrono::DateTime<chrono::FixedOffset>, &str)> = None;
    for anchor in anchors {
        let Some(list) = success_by_anchor.get(anchor) else {
            continue;
        };
        for (instant, sid) in list {
            if *instant <= fail_time {
                continue;
            }
            // `resolved_by` is the pass that *first* resolved the failure — the
            // earliest later success — not the most recent run. Tie-break by ID.
            let better = match best {
                None => true,
                Some((bt, bid)) => (*instant, *sid) < (bt, bid),
            };
            if better {
                best = Some((*instant, sid));
            }
        }
    }
    best.map_or((ResolutionStatus::StillFailing, None), |(_, sid)| {
        (ResolutionStatus::SinceResolved, Some(sid))
    })
}

/// Emits an attempt's own unresolved / stale / triple-only evidence-link
/// diagnostics, carrying the original handles (AC6).
fn push_attempt_link_diagnostics(
    node: &GraphRecord,
    tombstoned: &BTreeSet<&str>,
    by_id: &BTreeMap<&str, &GraphRecord>,
    diagnostics: &mut Vec<MemoryAuditDiagnostic>,
) {
    let GraphRecord::Node {
        id,
        evidence_links: Some(links),
        ..
    } = node
    else {
        return;
    };
    for link in links {
        let Some(target_id) = link.target_record_id.as_deref() else {
            let handle = link
                .target_repo_relative_path
                .clone()
                .unwrap_or_else(|| "<triple>".to_owned());
            diagnostics.push(MemoryAuditDiagnostic {
                code: "evidence_target_unresolved".to_owned(),
                source_record_id: id.clone(),
                target_handle: handle,
                relation: link.relation.clone(),
                target_domain: link.target_domain.clone(),
            });
            continue;
        };
        let code = if tombstoned.contains(target_id) {
            "stale_evidence_target"
        } else if by_id.contains_key(target_id) {
            continue;
        } else {
            "unresolved_evidence_link"
        };
        diagnostics.push(MemoryAuditDiagnostic {
            code: code.to_owned(),
            source_record_id: id.clone(),
            target_handle: target_id.to_owned(),
            relation: link.relation.clone(),
            target_domain: link.target_domain.clone(),
        });
    }
}

/// Returns the live code/task record IDs a node links to outbound, via graph
/// edges or denormalized evidence links with a target-linking relation.
fn outbound_code_task_targets<'a>(
    node: &'a GraphRecord,
    edges_from: &BTreeMap<&'a str, Vec<(&'a EdgeLabel, &'a str)>>,
    present: &impl Fn(&str) -> Option<&'a GraphRecord>,
) -> BTreeSet<&'a str> {
    let mut out: BTreeSet<&str> = BTreeSet::new();
    let mut consider = |id: &'a str| {
        let Some(t) = present(id) else {
            return;
        };
        match record_node_kind(t) {
            Some(k) if is_codegraph_kind(k) || is_project_kind(k) => {
                out.insert(id);
            }
            // Relay through a patch artifact to the file(s) it touched, the same
            // `Failure --FAILED_ON--> PatchArtifact --TOUCHED_FILE--> File` shape a
            // file query relays in reverse, so a source query on a patch-invalid
            // failure still anchors on the touched file.
            Some(NodeKind::PatchArtifact) => {
                if let Some(patch_edges) = edges_from.get(id) {
                    for (plabel, pt) in patch_edges {
                        if matches!(plabel, EdgeLabel::TouchedFile)
                            && present(pt).is_some_and(|n| {
                                matches!(record_node_kind(n), Some(NodeKind::File))
                            })
                        {
                            out.insert(pt);
                        }
                    }
                }
            }
            _ => {}
        }
    };
    if let Some(edges) = edges_from.get(node.id()) {
        for (label, t) in edges {
            if FAILURE_TARGET_LINK_RELS.contains(&label.as_str()) {
                consider(t);
            }
        }
    }
    if let GraphRecord::Node {
        evidence_links: Some(links),
        ..
    } = node
    {
        for link in links {
            if FAILURE_TARGET_LINK_RELS.contains(&link.relation.as_str())
                && let Some(t) = link.target_record_id.as_deref()
            {
                consider(t);
            }
        }
    }
    out
}

/// From a reached agent `Failure`, follows `PRODUCED_PATCH` / `FAILED_ON` edges
/// and denormalized links to its patch artifact and runtime command/test
/// evidence, inheriting the failure's anchor set for the reached runtime records.
///
/// The Codex/trajectory importers link a patch-invalid failure to its rejected
/// `PatchArtifact` and a failed command to its `CommandRun` via `FAILED_ON`
/// (`PRODUCED_PATCH` is emitted from the AgentTurn), so following only
/// `PRODUCED_PATCH` from the failure would lose those citable artifacts.
#[expect(clippy::too_many_arguments)]
fn collect_failure_links<'a>(
    failure: &'a GraphRecord,
    failure_id: &str,
    anchors: &BTreeSet<&'a str>,
    edges_from: &BTreeMap<&'a str, Vec<(&'a EdgeLabel, &'a str)>>,
    present: &impl Fn(&str) -> Option<&'a GraphRecord>,
    patch: &mut BTreeMap<&'a str, MemoryEvidenceItem<'a>>,
    runtime: &mut CandidateMap<'a>,
    success: &mut CandidateMap<'a>,
    diagnostics: &mut Vec<MemoryAuditDiagnostic>,
) {
    // (relation, target_id) from both graph edges and denormalized links. A
    // failure cites its patch/runtime evidence via PRODUCED_PATCH or FAILED_ON,
    // or via the verification-evidence relations PRODUCED_EVIDENCE / HAS_EVIDENCE
    // / VALIDATED_BY, so all are followed.
    let mut links: Vec<(&'a str, &'a str)> = Vec::new();
    if let Some(edges) = edges_from.get(failure_id) {
        for (label, target) in edges {
            if matches!(
                label,
                EdgeLabel::ProducedPatch
                    | EdgeLabel::FailedOn
                    | EdgeLabel::ProducedEvidence
                    | EdgeLabel::HasEvidence
                    | EdgeLabel::ValidatedBy
            ) {
                links.push((label.as_str(), *target));
            }
        }
    }
    if let GraphRecord::Node {
        evidence_links: Some(el),
        ..
    } = failure
    {
        for link in el {
            if matches!(
                link.relation.as_str(),
                "PRODUCED_PATCH"
                    | "FAILED_ON"
                    | "PRODUCED_EVIDENCE"
                    | "HAS_EVIDENCE"
                    | "VALIDATED_BY"
            ) && let Some(t) = link.target_record_id.as_deref()
            {
                links.push((link.relation.as_str(), t));
            }
        }
    }
    links.sort_unstable();
    links.dedup();

    for (rel, target) in links {
        let Some(node) = present(target) else {
            diagnostics.push(MemoryAuditDiagnostic {
                code: "unresolved_evidence_link".to_owned(),
                source_record_id: failure_id.to_owned(),
                target_handle: target.to_owned(),
                relation: rel.to_owned(),
                target_domain: String::new(),
            });
            continue;
        };
        match record_node_kind(node) {
            Some(NodeKind::PatchArtifact) => {
                patch
                    .entry(node.id())
                    .or_insert_with(|| MemoryEvidenceItem {
                        record: node,
                        relation: rel.to_owned(),
                    });
            }
            Some(k) if is_verification_kind(k) => {
                if is_failed_verification(node) {
                    merge_candidate(runtime, node, anchors, rel);
                } else if is_pass_status_node(node) {
                    merge_candidate(success, node, anchors, rel);
                }
            }
            _ => {}
        }
    }
}

/// Walks `AUTHORED_BY` / `SESSION_OF` from a failure to its session and agent.
fn collect_provenance<'a>(
    failure_id: &'a str,
    edges_from: &BTreeMap<&'a str, Vec<(&'a EdgeLabel, &'a str)>>,
    present: &impl Fn(&str) -> Option<&'a GraphRecord>,
    sessions: &mut BTreeMap<&'a str, &'a GraphRecord>,
    agents: &mut BTreeMap<&'a str, &'a GraphRecord>,
) {
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    let mut frontier: Vec<&str> = vec![failure_id];
    while let Some(id) = frontier.pop() {
        if !visited.insert(id) {
            continue;
        }
        let Some(out) = edges_from.get(id) else {
            continue;
        };
        for (label, target) in out {
            if !matches!(label, EdgeLabel::AuthoredBy | EdgeLabel::SessionOf) {
                continue;
            }
            // A tombstoned (deleted) intermediate provenance node must not relay
            // through to a live session/agent: stop the walk at it rather than
            // enqueueing and continuing along its outgoing edges.
            let Some(node) = present(target) else {
                continue;
            };
            if !visited.contains(*target) {
                frontier.push(target);
            }
            match record_node_kind(node) {
                Some(NodeKind::AgentSession) => {
                    sessions.entry(node.id()).or_insert(node);
                }
                Some(NodeKind::Agent) => {
                    agents.entry(node.id()).or_insert(node);
                }
                _ => {}
            }
        }
    }
}

// ============================================================================
// Change-impact query (issue #76)
// ============================================================================

#[cfg(test)]
mod liveness_parity_tests {
    //! Transport-parity regression (issue #421): over an append-only `--graph`, a
    //! record re-ingested AFTER its own tombstone is live again — matching the
    //! embedded current-state read — in BOTH `resolve_failure_handle` and
    //! `failure_history_context`; a tombstone with no later re-add still deletes
    //! its id.
    use super::*;
    use crate::ir::{SourceSpan, stable_id};

    fn sym(id: &str) -> GraphRecord {
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
            Some("foo".to_owned()),
            "symbol foo".to_owned(),
        )
    }

    fn failed_verification(id: &str) -> GraphRecord {
        let mut rec = GraphRecord::node(
            id.to_owned(),
            NodeKind::Verification,
            None,
            None,
            None,
            "verification".to_owned(),
        )
        .with_domain("verification", 1);
        if let GraphRecord::Node { status, .. } = &mut rec {
            *status = Some("fail".to_owned());
        }
        rec
    }

    fn failed_on(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::FailedOn,
            source.to_owned(),
            target.to_owned(),
            None,
            "failed on".to_owned(),
        )
    }

    fn tomb(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v5:tomb_{deleted_id}"),
            schema_version: 5,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    fn symbol_target(anchor: &str) -> ResolvedFailureTarget {
        let mut anchor_ids = BTreeSet::new();
        anchor_ids.insert(anchor.to_owned());
        ResolvedFailureTarget {
            handle: anchor.to_owned(),
            kind: FailureTargetKind::Symbol,
            anchor_ids,
            seed_failures: BTreeSet::new(),
            stale: false,
        }
    }

    #[test]
    fn resolve_handle_reingested_after_tombstone_is_not_stale() {
        let id = stable_id(&["node", "Symbol", "src/lib.rs", "foo"]);
        let records = vec![sym(&id), tomb(&id), sym(&id)];
        let repo_index = RepositoryIndex::build(&records);
        let target = resolve_failure_handle(&records, &id, &repo_index, None).expect("resolves");
        assert!(
            !target.stale,
            "a code record revived after its tombstone must resolve non-stale"
        );
        assert!(target.anchor_ids.contains(&id));
    }

    #[test]
    fn resolve_handle_tombstone_without_reingest_is_stale() {
        let id = stable_id(&["node", "Symbol", "src/lib.rs", "foo"]);
        let records = vec![sym(&id), tomb(&id)];
        let repo_index = RepositoryIndex::build(&records);
        let target = resolve_failure_handle(&records, &id, &repo_index, None).expect("resolves");
        assert!(
            target.stale && target.is_empty(),
            "a tombstone with no later re-ingest keeps the handle stale"
        );
    }

    #[test]
    fn context_surfaces_failure_reingested_after_tombstone() {
        // Runtime failure V --FAILED_ON--> anchor A; V is re-ingested after its own
        // tombstone, so it must surface as a runtime failure (latest write wins).
        let anchor = "codegraph:v5:anchor";
        let v = "verification:v1:run";
        let records = vec![
            sym(anchor),
            failed_verification(v),
            tomb(v),
            failed_verification(v),
            failed_on(v, anchor),
        ];
        let ctx = failure_history_context(&records, &symbol_target(anchor));
        assert!(
            ctx.runtime_failures.iter().any(|a| a.item.record.id() == v),
            "a runtime failure revived after its tombstone must surface in failure history"
        );
    }

    #[test]
    fn context_omits_failure_tombstoned_without_reingest() {
        let anchor = "codegraph:v5:anchor";
        let v = "verification:v1:run";
        let records = vec![
            sym(anchor),
            failed_verification(v),
            tomb(v),
            failed_on(v, anchor),
        ];
        let ctx = failure_history_context(&records, &symbol_target(anchor));
        assert!(
            ctx.runtime_failures.is_empty(),
            "a failure tombstoned with no re-ingest stays deleted and is not surfaced"
        );
    }
}
