//! Working-tree-diff-scoped evidence briefing (issue #214).
//!
//! Entry point for agents resuming work in a dirty checkout: resolves the
//! uncommitted working-tree diff (see [`crate::worktree_diff`]) into `File`
//! and `Symbol` handles, then returns a trust-separated briefing of everything
//! the graph already knows about exactly those handles — prior failures,
//! decisions, in-flight tasks, verification evidence, and drift — reusing the
//! existing linkage surfaces ([`record_context`], [`failure_history_context`],
//! and the redacted row builders from [`super::changes`]).
//!
//! Read-only throughout: the diff comes from read-only Git plumbing and the
//! store is only read, never re-scanned or mutated.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    ContextLinkedItem, ContextObservation, FailureHistoryContext, FailureTargetKind,
    RepositoryIndex, ResolvedFailureTarget, TrustClass, TrustIndex, failure_history_context,
    record_context, redacted_context_linked_item, redacted_context_observation,
};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};
use crate::worktree_diff::{PathStatus, WorkingTreeDiff};

/// One changed file resolved against the store.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BriefChangedFile {
    /// Repo-relative path (worktree side).
    pub path: String,
    /// Stable status string (`added`/`modified`/`deleted`/`renamed`/`untracked`).
    pub status: String,
    /// Old path for renames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
    /// Stable record ID of the resolved `File` record, if the store indexes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    /// Number of changed hunks in worktree terms.
    pub hunk_count: usize,
    /// Total changed lines across hunks in worktree terms.
    pub changed_lines: u64,
}

/// One symbol whose recorded span intersects a changed hunk.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BriefChangedSymbol<'a> {
    /// Stable record ID of the symbol.
    pub record_id: &'a str,
    /// Derived trust class (issue #114).
    pub trust: TrustClass,
    /// Symbol name.
    pub name: &'a str,
    /// Repo-relative path of the symbol definition.
    pub path: &'a str,
    /// Recorded source span, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A changed path the store does not index — reported, never silently dropped.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BriefUnindexedPath {
    /// Repo-relative path (worktree side).
    pub path: String,
    /// Stable status string.
    pub status: String,
    /// Old path for renames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
}

/// A code-graph source fact merged across the working set.
///
/// Bounded by design: identity, kind, path, and span only — never source
/// bodies, mirroring [`super::ChangesFileItem`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct BriefSourceFact<'a> {
    /// Stable record ID.
    pub record_id: &'a str,
    /// Derived trust class (issue #114).
    pub trust: TrustClass,
    /// Node kind wire string.
    pub kind: &'static str,
    /// Symbol/file name, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    /// Repo-relative path, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<&'a str>,
    /// Recorded source span, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A semantic-drift record targeting the working set.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BriefDriftWarning<'a> {
    /// Stable record ID of the drift record.
    pub record_id: &'a str,
    /// Derived trust class (issue #114).
    pub trust: TrustClass,
    /// Stable record ID of the drift target.
    pub target_record_id: &'a str,
    /// Computed drift score.
    pub score: f64,
    /// Target path, when the target record is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_path: Option<&'a str>,
    /// Target name, when the target record is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_name: Option<&'a str>,
}

/// Store-level domain presence: the #196 domain-absent vs entity-absent signal.
///
/// Each value states whether the store contains at least one record in that
/// trust-section domain. An empty briefing section whose domain is present
/// means "no records for this working set" (entity-absent); an empty section
/// whose domain is absent means "domain not in this store" (domain-absent).
/// Presence never asserts code is untested, unverified, or broken.
//
// Five booleans is the honest shape here: the struct serializes directly into
// the briefing's `store_coverage` map, one flag per trust-section domain.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct StoreCoverage {
    /// At least one code-graph record (`Symbol`/`File`/`Module`/`Import`).
    pub code_graph: bool,
    /// At least one agent-memory record (`Observation`/`Decision`/`Failure`/…).
    pub agent_memory: bool,
    /// At least one project record (`Task`/`PR`/`GitHubIssue`/…).
    pub project: bool,
    /// At least one artifact record (`Artifact`/`PatchArtifact`/…).
    pub artifact: bool,
    /// At least one verification record (`Verification`/`TestRun`/…).
    pub verification: bool,
}

impl StoreCoverage {
    /// Computes store-level domain presence over `records` for the
    /// `query context` / `query task` lanes (issue #196).
    ///
    /// Classification follows [`classify_node`](super::context::classify_node)
    /// — the exact contract the context/task trust sections use — so a flag
    /// can never disagree with its section: `agent_memory` is set only by
    /// node kinds that can render in `observations`
    /// (`Observation`/`Decision`/`Failure`), and likewise for the other four
    /// domains. Edges, tombstones, and infrastructure nodes (`Repository`,
    /// `Agent`, `Commit`, `ScanCoverage`, …) never set a flag.
    ///
    /// This differs deliberately from [`compute_store_coverage`] (the
    /// `eg brief` lane), which counts agent-memory-adjacent infrastructure
    /// kinds toward its broader sections. Each lane's flags match the
    /// sections that lane renders.
    ///
    /// Tombstone-naive by design: presence answers "was this domain ever
    /// ingested into this store", not "does the current-state view show a
    /// live record". A domain whose records were all forgotten still reports
    /// present — the absence is per-entity, not per-store. It synthesizes no
    /// records and asserts nothing about the code itself.
    ///
    /// Stops scanning at the first record that completes all five flags.
    #[must_use]
    pub fn from_records(records: &[GraphRecord]) -> Self {
        let mut coverage = Self::default();
        for record in records {
            let GraphRecord::Node { kind, .. } = record else {
                continue;
            };
            coverage.mark(super::context::classify_node(*kind));
            if coverage.is_complete() {
                break;
            }
        }
        coverage
    }

    /// Computes store-level domain presence from sidecar-index kind keys
    /// (issue #447 fast path): the index's `by_kind` lane maps each
    /// `PascalCase` [`NodeKind`] spelling to its record offsets, so presence
    /// is answerable without parsing any record bodies.
    ///
    /// Keys resolve through [`NodeKind::ALL`] + [`NodeKind::as_str`] — the
    /// same spellings the index builder wrote — into the same
    /// [`classify_node`](super::context::classify_node) contract as
    /// [`from_records`](Self::from_records), so the two constructors agree on
    /// every store. Unrecognized kind strings (an index written by a newer
    /// binary) are skipped.
    #[must_use]
    pub fn from_index_kind_keys<'a>(kinds: impl Iterator<Item = &'a str>) -> Self {
        let mut coverage = Self::default();
        for key in kinds {
            let Some(kind) = NodeKind::ALL.into_iter().find(|k| k.as_str() == key) else {
                continue;
            };
            coverage.mark(super::context::classify_node(kind));
            if coverage.is_complete() {
                break;
            }
        }
        coverage
    }

    /// Marks the flag for one classified section.
    const fn mark(&mut self, section: Option<super::context::ContextSection>) {
        match section {
            Some(super::context::ContextSection::SourceFact) => self.code_graph = true,
            Some(
                super::context::ContextSection::Observation
                | super::context::ContextSection::Decision,
            ) => self.agent_memory = true,
            Some(super::context::ContextSection::ProjectState) => self.project = true,
            Some(super::context::ContextSection::Artifact) => self.artifact = true,
            Some(super::context::ContextSection::VerificationEvidence) => {
                self.verification = true;
            }
            None => {}
        }
    }

    /// `true` once every domain flag is set; used for the early exit.
    const fn is_complete(self) -> bool {
        self.code_graph && self.agent_memory && self.project && self.artifact && self.verification
    }
}

/// Trust-separated briefing over a working-tree diff.
#[derive(Debug, Default)]
pub struct WorkingSetBrief<'a> {
    /// Changed files with their resolved `File` record IDs.
    pub changed_files: Vec<BriefChangedFile>,
    /// Symbols whose recorded spans intersect changed hunks.
    pub changed_symbols: Vec<BriefChangedSymbol<'a>>,
    /// Changed paths with no `File` record in the store.
    pub unindexed: Vec<BriefUnindexedPath>,
    /// Code-graph source facts merged across the working set.
    pub source_facts: Vec<BriefSourceFact<'a>>,
    /// Prior failed attempts merged across the working set, split into
    /// runtime failures (verification domain) and agent-authored failures.
    pub failure_context: FailureHistoryContext<'a>,
    /// Agent-authored observations (provenance attached; never source truth).
    pub observations: Vec<ContextObservation<'a>>,
    /// Agent-authored decisions, split out from observations.
    pub decisions: Vec<ContextObservation<'a>>,
    /// Project/PR records touching the files, filtered to non-terminal status.
    pub in_flight_tasks: Vec<ContextLinkedItem<'a>>,
    /// Verification runs and test results touching the working set.
    pub verification_evidence: Vec<ContextLinkedItem<'a>>,
    /// Semantic-drift records targeting the working set.
    pub drift_warnings: Vec<BriefDriftWarning<'a>>,
    /// Store-level domain presence (#196).
    pub store_coverage: StoreCoverage,
}

/// Task statuses that count as in-flight (issue #214).
///
/// Mirrors the closed vocabulary in [`crate::local_project`]: `open`,
/// `in_progress`, `blocked`, and `unknown` (conservative: an unknown status is
/// not terminal) are in flight; `closed_completed` and `closed_dropped` are
/// terminal.
fn task_status_in_flight(status: Option<&str>) -> bool {
    !matches!(status, Some("closed_completed" | "closed_dropped"))
}

/// PR/issue statuses that count as in-flight: `open`, or absent (conservative).
fn review_status_in_flight(status: Option<&str>) -> bool {
    !matches!(status, Some("closed" | "merged"))
}

/// True when a project-state row belongs in `in_flight_tasks`.
///
/// Work items (`Task`, `LocalTask`, `GitHubIssue`, `PR`) are filtered to
/// non-terminal status; review/state facts (`Review`, `AcceptanceCriterion`,
/// `ReviewStateTransition`, `ExternalIdentity`) describe review state rather
/// than work items and are kept unfiltered.
fn in_flight_task(kind: &str, status: Option<&str>) -> bool {
    match kind {
        "Task" | "LocalTask" => task_status_in_flight(status),
        "GitHubIssue" | "PR" => review_status_in_flight(status),
        _ => true,
    }
}

/// Computes [`StoreCoverage`] over the store: one linear pass classifying
/// every node into its trust-section domain.
fn compute_store_coverage(records: &[GraphRecord]) -> StoreCoverage {
    let mut coverage = StoreCoverage::default();
    for record in records {
        let GraphRecord::Node { kind, .. } = record else {
            continue;
        };
        match kind {
            NodeKind::Symbol | NodeKind::File | NodeKind::Module | NodeKind::Import => {
                coverage.code_graph = true;
            }
            NodeKind::Observation
            | NodeKind::Decision
            | NodeKind::Failure
            | NodeKind::Agent
            | NodeKind::AgentSession
            | NodeKind::AgentRun
            | NodeKind::AgentTurn
            | NodeKind::ToolCall
            | NodeKind::CommandRun
            | NodeKind::FileEdit
            | NodeKind::PromoteCandidate
            | NodeKind::PromotionPrompt
            | NodeKind::PromotionDecision
            | NodeKind::Preference
            | NodeKind::WorkflowRule
            | NodeKind::NamingDecision
            | NodeKind::Constraint => {
                coverage.agent_memory = true;
            }
            NodeKind::Task
            | NodeKind::LocalTask
            | NodeKind::GitHubIssue
            | NodeKind::PR
            | NodeKind::Review
            | NodeKind::AcceptanceCriterion
            | NodeKind::ReviewStateTransition
            | NodeKind::ExternalIdentity
            | NodeKind::Project
            | NodeKind::Plan
            | NodeKind::Product => {
                coverage.project = true;
            }
            NodeKind::Artifact
            | NodeKind::PatchArtifact
            | NodeKind::Adr
            | NodeKind::Prd
            | NodeKind::PlanDoc
            | NodeKind::CommandEvidence => {
                coverage.artifact = true;
            }
            NodeKind::Verification
            | NodeKind::TestRun
            | NodeKind::CIStatus
            | NodeKind::BenchmarkRun
            | NodeKind::CoverageReport
            | NodeKind::ProofResult => {
                coverage.verification = true;
            }
            _ => {}
        }
        if coverage.code_graph
            && coverage.agent_memory
            && coverage.project
            && coverage.artifact
            && coverage.verification
        {
            break;
        }
    }
    coverage
}

/// True when `record_id` belongs to the selected repository scope.
///
/// With no scope every record is in scope. With a scope, only records the
/// [`RepositoryIndex`] attributes to that repository are — this is what keeps
/// `--repo` working-tree briefings from leaking handles across repos that
/// share a store.
#[must_use]
fn in_scope(repo_index: &RepositoryIndex, repo_scope: Option<&str>, record_id: &str) -> bool {
    repo_scope.is_none_or(|scope| repo_index.owner_of(record_id) == Some(scope))
}

/// Resolves a [`WorkingTreeDiff`] into file/symbol handles and returns the
/// trust-separated briefing of everything the graph knows about them.
///
/// Resolution is deterministic: every section is ordered by stable record ID
/// (paths for the working-set rows), so repeated runs on an unchanged tree
/// and store produce identical output.
#[must_use]
pub fn brief_working_set<'a>(
    records: &'a [GraphRecord],
    diff: &WorkingTreeDiff,
    repo_index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> WorkingSetBrief<'a> {
    let trust = TrustIndex::build(records);
    let by_id: BTreeMap<&str, &'a GraphRecord> =
        records.iter().map(|record| (record.id(), record)).collect();

    let mut brief = WorkingSetBrief {
        store_coverage: compute_store_coverage(records),
        ..Default::default()
    };

    // Live, in-scope `File` records by repo-relative path.
    let mut files_by_path: BTreeMap<&str, &str> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path: Some(path),
            ..
        } = record
        else {
            continue;
        };
        if !in_scope(repo_index, repo_scope, id) {
            continue;
        }
        files_by_path.entry(path.as_str()).or_insert(id.as_str());
    }

    // The failure/linkage anchors: resolved file IDs plus intersected symbol IDs.
    let mut anchor_ids: BTreeSet<&str> = BTreeSet::new();

    for changed in &diff.paths {
        let renamed_from = match &changed.status {
            PathStatus::Renamed { from } => Some(from.as_str()),
            _ => None,
        };
        // Resolve the worktree path first; for renames fall back to the old
        // path because the store indexes the `HEAD` version.
        let record_id = files_by_path
            .get(changed.path.as_str())
            .copied()
            .or_else(|| renamed_from.and_then(|from| files_by_path.get(from).copied()));
        let Some(record_id) = record_id else {
            brief.unindexed.push(BriefUnindexedPath {
                path: changed.path.clone(),
                status: changed.status.as_str().to_owned(),
                renamed_from: renamed_from.map(str::to_owned),
            });
            continue;
        };
        brief.changed_files.push(BriefChangedFile {
            path: changed.path.clone(),
            status: changed.status.as_str().to_owned(),
            renamed_from: renamed_from.map(str::to_owned),
            record_id: Some(record_id.to_owned()),
            hunk_count: changed.hunk_count,
            changed_lines: changed.changed_lines,
        });
        anchor_ids.insert(record_id);

        // Only symbols whose recorded `HEAD` spans intersect the old-side
        // changed ranges are in the working set.
        for record in records {
            let GraphRecord::Node {
                id: symbol_id,
                kind: NodeKind::Symbol,
                name,
                repo_relative_path: Some(symbol_path),
                span,
                ..
            } = record
            else {
                continue;
            };
            if !in_scope(repo_index, repo_scope, symbol_id) {
                continue;
            }
            let path_matches =
                symbol_path == &changed.path || renamed_from == Some(symbol_path.as_str());
            if !path_matches {
                continue;
            }
            if !span_intersects_ranges(span.as_ref(), &changed.old_ranges) {
                continue;
            }
            anchor_ids.insert(symbol_id.as_str());
            brief.changed_symbols.push(BriefChangedSymbol {
                record_id: symbol_id.as_str(),
                trust: trust.classify(record),
                name: name.as_deref().unwrap_or(""),
                path: symbol_path.as_str(),
                span: *span,
            });
        }
    }
    // Deterministic symbol order (anchors accumulate in diff order).
    brief
        .changed_symbols
        .sort_by(|a, b| a.record_id.cmp(b.record_id));

    // Merge evidence across anchors with the existing linkage surfaces.
    let mut source_fact_ids: BTreeSet<&str> = BTreeSet::new();
    let mut observation_ids: BTreeSet<&str> = BTreeSet::new();
    let mut decision_ids: BTreeSet<&str> = BTreeSet::new();
    let mut task_records: BTreeMap<&str, &'a GraphRecord> = BTreeMap::new();
    let mut verification_records: BTreeMap<&str, &'a GraphRecord> = BTreeMap::new();
    for anchor in &anchor_ids {
        let context = record_context(records, anchor);
        for fact in context.source_facts {
            source_fact_ids.insert(fact.id());
        }
        for observation in context.observations {
            observation_ids.insert(observation.id());
        }
        for decision in context.decisions {
            decision_ids.insert(decision.id());
        }
        for task in context.project_state {
            task_records.entry(task.id()).or_insert(task);
        }
        for item in context.verification_evidence {
            verification_records.entry(item.id()).or_insert(item);
        }
    }

    for id in source_fact_ids {
        let Some(record) = by_id.get(id).copied() else {
            continue;
        };
        let GraphRecord::Node {
            name,
            repo_relative_path,
            span,
            ..
        } = record
        else {
            continue;
        };
        brief.source_facts.push(BriefSourceFact {
            record_id: id,
            trust: trust.classify(record),
            kind: record.node_kind_name().unwrap_or("unknown"),
            name: name.as_deref(),
            path: repo_relative_path.as_deref(),
            span: *span,
        });
    }

    // `record_context` separates decisions into their own section (issue
    // #191), so no split-out is needed: observations and decisions arrive
    // pre-separated. Failures are reported under `prior_failures`, never
    // duplicated here.
    for id in observation_ids {
        let Some(record) = by_id.get(id).copied() else {
            continue;
        };
        let GraphRecord::Node { kind, .. } = record else {
            continue;
        };
        let row = redacted_context_observation(record, &trust);
        if let (NodeKind::Observation, Some(row)) = (kind, row) {
            brief.observations.push(row);
        }
    }
    for id in decision_ids {
        let Some(record) = by_id.get(id).copied() else {
            continue;
        };
        let GraphRecord::Node { kind, .. } = record else {
            continue;
        };
        let row = redacted_context_observation(record, &trust);
        if let (NodeKind::Decision, Some(row)) = (kind, row) {
            brief.decisions.push(row);
        }
    }

    for record in task_records.into_values() {
        let GraphRecord::Node { status, .. } = record else {
            continue;
        };
        if !in_flight_task(record.node_kind_name().unwrap_or(""), status.as_deref()) {
            continue;
        }
        if let Some(row) = redacted_context_linked_item(record, &trust) {
            brief.in_flight_tasks.push(row);
        }
    }

    for record in verification_records.into_values() {
        if let Some(row) = redacted_context_linked_item(record, &trust) {
            brief.verification_evidence.push(row);
        }
    }

    // Prior failures, aggregated across every anchor with one
    // `ResolvedFailureTarget` so runtime vs agent-authored splits and
    // read-time resolution statuses come from the same traversal as
    // `eg failure-history`.
    let failure_target = ResolvedFailureTarget {
        handle: "working-set".to_owned(),
        kind: FailureTargetKind::WorkingSet,
        anchor_ids: anchor_ids.iter().map(|id| (*id).to_owned()).collect(),
        seed_failures: BTreeSet::new(),
        stale: false,
    };
    brief.failure_context = failure_history_context(records, &failure_target);

    // Semantic-drift records targeting the working set. Mirrors
    // `record_context`'s drift resolution (DriftsFrom edge, falling back to
    // `target_record_id`; first edge wins) and `largest_semantic_drifts`
    // ordering (score descending, then record ID).
    let mut drifts_from_target: BTreeMap<&str, &str> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Edge {
            label: EdgeLabel::DriftsFrom,
            source,
            target,
            ..
        } = record
        {
            drifts_from_target
                .entry(source.as_str())
                .or_insert(target.as_str());
        }
    }
    let mut warnings: Vec<BriefDriftWarning<'a>> = Vec::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::SemanticDrift,
            semantic_drift: Some(drift),
            ..
        } = record
        else {
            continue;
        };
        let target_id = drifts_from_target
            .get(id.as_str())
            .copied()
            .unwrap_or(drift.target_record_id.as_str());
        if !anchor_ids.contains(target_id) {
            continue;
        }
        let (target_path, target_name) = match by_id.get(target_id).copied() {
            Some(GraphRecord::Node {
                repo_relative_path,
                name,
                ..
            }) => (repo_relative_path.as_deref(), name.as_deref()),
            _ => (None, None),
        };
        warnings.push(BriefDriftWarning {
            record_id: id.as_str(),
            trust: trust.classify(record),
            target_record_id: target_id,
            score: drift.score,
            target_path,
            target_name,
        });
    }
    warnings.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.record_id.cmp(right.record_id))
    });
    brief.drift_warnings = warnings;

    brief
}

/// True when a symbol's recorded span intersects any changed line range.
///
/// A symbol with no recorded span never intersects (spans are the only
/// evidence of where the symbol lives). Boundary-touching counts as
/// intersecting: a hunk starting exactly where a symbol ends still edits its
/// last line's neighborhood.
fn span_intersects_ranges(span: Option<&SourceSpan>, ranges: &[(u32, u32)]) -> bool {
    let Some(span) = span else {
        return false;
    };
    let (start, end) = (
        u32::try_from(span.start_line).unwrap_or(u32::MAX),
        u32::try_from(span.end_line).unwrap_or(u32::MAX),
    );
    ranges
        .iter()
        .any(|(range_start, range_end)| start <= *range_end && *range_start <= end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::EdgeLabel;

    fn file_node(id: &str, path: &str) -> GraphRecord {
        let mut node = GraphRecord::node(
            id.to_owned(),
            NodeKind::File,
            None,
            None,
            None,
            format!("file {path}"),
        );
        if let GraphRecord::Node {
            repo_relative_path, ..
        } = &mut node
        {
            *repo_relative_path = Some(path.to_owned());
        }
        node
    }

    fn symbol_node(id: &str, name: &str, path: &str, start: usize, end: usize) -> GraphRecord {
        let mut node = GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(name.to_owned()),
            None,
            None,
            format!("symbol {name}"),
        );
        if let GraphRecord::Node {
            repo_relative_path,
            span,
            ..
        } = &mut node
        {
            *repo_relative_path = Some(path.to_owned());
            *span = Some(SourceSpan {
                start_byte: 0,
                end_byte: 0,
                start_line: start,
                end_line: end,
                start_column: None,
                end_column: None,
            });
        }
        node
    }

    fn evidence_link(target: &str, relation: &str) -> crate::ir::EvidenceLink {
        crate::ir::EvidenceLink {
            target_record_id: Some(target.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: relation.to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }
    }

    fn observation_node(id: &str, target_file_id: &str) -> GraphRecord {
        let mut node = GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "observation text".to_owned(),
        );
        if let GraphRecord::Node {
            agent_id,
            session_id,
            observed_at,
            evidence_links,
            ..
        } = &mut node
        {
            *agent_id = Some("agent-1".to_owned());
            *session_id = Some("sess-1".to_owned());
            *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
            *evidence_links = Some(vec![evidence_link(target_file_id, "OBSERVES")]);
        }
        node
    }

    fn decision_node(id: &str, target_file_id: &str) -> GraphRecord {
        let mut node = observation_node(id, target_file_id);
        if let GraphRecord::Node { kind, .. } = &mut node {
            *kind = NodeKind::Decision;
        }
        node
    }

    fn task_node(id: &str, status: &str) -> GraphRecord {
        let mut node = GraphRecord::node(
            id.to_owned(),
            NodeKind::Task,
            Some("task one".to_owned()),
            None,
            None,
            "task text".to_owned(),
        );
        if let GraphRecord::Node { status: s, .. } = &mut node {
            *s = Some(status.to_owned());
        }
        node
    }

    fn failure_node(id: &str, target_file_id: &str) -> GraphRecord {
        let mut node = GraphRecord::node(
            id.to_owned(),
            NodeKind::Failure,
            None,
            None,
            None,
            "failure text".to_owned(),
        );
        if let GraphRecord::Node {
            failure_kind,
            agent_id,
            session_id,
            observed_at,
            evidence_links,
            ..
        } = &mut node
        {
            *failure_kind = Some("command_failure".to_owned());
            *agent_id = Some("agent-1".to_owned());
            *session_id = Some("sess-1".to_owned());
            *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
            *evidence_links = Some(vec![evidence_link(target_file_id, "FAILED_ON")]);
        }
        node
    }

    fn sample_diff() -> WorkingTreeDiff {
        WorkingTreeDiff {
            paths: vec![crate::worktree_diff::ChangedPath {
                path: "src/a.rs".to_owned(),
                status: PathStatus::Modified,
                // The graph records HEAD spans, so intersection uses old_ranges.
                old_ranges: vec![(10, 12)],
                new_ranges: vec![(10, 12)],
                changed_lines: 3,
                hunk_count: 1,
            }],
        }
    }

    fn sample_records() -> Vec<GraphRecord> {
        vec![
            file_node("file-1", "src/a.rs"),
            symbol_node("sym-1", "touched_fn", "src/a.rs", 9, 15),
            symbol_node("sym-2", "untouched_fn", "src/a.rs", 30, 40),
            observation_node("obs-1", "file-1"),
            decision_node("dec-1", "file-1"),
            task_node("task-1", "open"),
            task_node("task-2", "closed_completed"),
            failure_node("fail-1", "file-1"),
        ]
    }

    #[test]
    fn resolves_file_and_intersecting_symbol_only() {
        let records = sample_records();
        let index = RepositoryIndex::build(&records);
        let brief = brief_working_set(&records, &sample_diff(), &index, None);
        assert_eq!(brief.changed_files.len(), 1);
        assert_eq!(brief.changed_files[0].record_id.as_deref(), Some("file-1"));
        // Only sym-1 (lines 9-15) intersects the (10,12) hunk; sym-2 does not.
        assert_eq!(brief.changed_symbols.len(), 1);
        assert_eq!(brief.changed_symbols[0].record_id, "sym-1");
        assert!(brief.unindexed.is_empty());
    }

    #[test]
    fn unindexed_paths_are_reported_not_dropped() {
        let records = sample_records();
        let index = RepositoryIndex::build(&records);
        let diff = WorkingTreeDiff {
            paths: vec![crate::worktree_diff::ChangedPath {
                path: "src/missing.rs".to_owned(),
                status: PathStatus::Added,
                old_ranges: vec![(0, 1)],
                new_ranges: vec![(1, 5)],
                changed_lines: 5,
                hunk_count: 1,
            }],
        };
        let brief = brief_working_set(&records, &diff, &index, None);
        assert!(brief.changed_files.is_empty());
        assert_eq!(brief.unindexed.len(), 1);
        assert_eq!(brief.unindexed[0].path, "src/missing.rs");
    }

    #[test]
    fn empty_diff_yields_empty_but_well_formed_briefing() {
        let records = sample_records();
        let index = RepositoryIndex::build(&records);
        let brief = brief_working_set(&records, &WorkingTreeDiff::default(), &index, None);
        assert!(brief.changed_files.is_empty());
        assert!(brief.changed_symbols.is_empty());
        assert!(brief.unindexed.is_empty());
        assert!(brief.source_facts.is_empty());
        assert!(brief.observations.is_empty());
        assert!(brief.decisions.is_empty());
        assert!(brief.in_flight_tasks.is_empty());
        assert!(brief.verification_evidence.is_empty());
        assert!(brief.drift_warnings.is_empty());
        assert!(brief.failure_context.has_no_failures());
        // The store itself still reports its domains.
        assert!(brief.store_coverage.code_graph);
        assert!(brief.store_coverage.agent_memory);
        assert!(brief.store_coverage.project);
    }

    #[test]
    fn observations_and_decisions_are_split_with_provenance() {
        let records = sample_records();
        let index = RepositoryIndex::build(&records);
        let brief = brief_working_set(&records, &sample_diff(), &index, None);
        assert_eq!(brief.observations.len(), 1);
        assert_eq!(brief.observations[0].record_id, "obs-1");
        assert_eq!(brief.decisions.len(), 1);
        assert_eq!(brief.decisions[0].record_id, "dec-1");
        // Agent-authored content carries provenance, never presented as source truth.
        assert_eq!(
            brief.observations[0].provenance_handle.as_deref(),
            Some("agent-1:sess-1")
        );
    }

    #[test]
    fn in_flight_tasks_excludes_terminal_statuses() {
        let records = sample_records();
        // Link the tasks to the file so the merge reaches them.
        let mut records = records;
        for task_id in ["task-1", "task-2"] {
            records.push(GraphRecord::edge(
                EdgeLabel::ReferencesTask,
                "file-1".to_owned(),
                task_id.to_owned(),
                None,
                "task touches file".to_owned(),
            ));
        }
        let index = RepositoryIndex::build(&records);
        let brief = brief_working_set(&records, &sample_diff(), &index, None);
        let ids: Vec<&str> = brief.in_flight_tasks.iter().map(|t| t.record_id).collect();
        assert!(ids.contains(&"task-1"), "open task is in flight");
        assert!(
            !ids.contains(&"task-2"),
            "closed_completed task is terminal"
        );
    }

    #[test]
    fn prior_failures_split_runtime_vs_agent() {
        let records = sample_records();
        let index = RepositoryIndex::build(&records);
        let brief = brief_working_set(&records, &sample_diff(), &index, None);
        // fail-1 is an agent-authored Failure claim (not a verification-domain run).
        assert_eq!(brief.failure_context.agent_failures.len(), 1);
        assert!(brief.failure_context.runtime_failures.is_empty());
    }

    #[test]
    fn store_coverage_marks_absent_domains() {
        // Structural-only store: code graph, nothing else.
        let records = vec![
            file_node("file-1", "src/a.rs"),
            symbol_node("sym-1", "f", "src/a.rs", 1, 5),
        ];
        let index = RepositoryIndex::build(&records);
        let brief = brief_working_set(&records, &sample_diff(), &index, None);
        assert!(brief.store_coverage.code_graph);
        assert!(!brief.store_coverage.agent_memory);
        assert!(!brief.store_coverage.project);
        assert!(!brief.store_coverage.artifact);
        assert!(!brief.store_coverage.verification);
        // Empty observations with agent_memory absent = domain-absent, derivable
        // from the coverage map without consulting any other command.
        assert!(brief.observations.is_empty());
    }

    #[test]
    fn briefing_is_deterministic_across_runs() {
        // Only the serializable sections are compared; the failure context is
        // covered by the CLI-level byte-identity test.
        let records = sample_records();
        let index = RepositoryIndex::build(&records);
        let snapshot = |brief: &WorkingSetBrief<'_>| {
            serde_json::to_string(&(
                &brief.changed_files,
                &brief.changed_symbols,
                &brief.unindexed,
                &brief.source_facts,
                &brief.observations,
                &brief.decisions,
                &brief.in_flight_tasks,
                &brief.verification_evidence,
                &brief.drift_warnings,
                &brief.store_coverage,
            ))
            .expect("serialize")
        };
        let first = snapshot(&brief_working_set(&records, &sample_diff(), &index, None));
        let second = snapshot(&brief_working_set(&records, &sample_diff(), &index, None));
        assert_eq!(first, second);
    }

    #[test]
    fn span_intersection_boundary_cases() {
        use crate::ir::SourceSpan;
        let span = |s: usize, e: usize| {
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 0,
                start_line: s,
                end_line: e,
                start_column: None,
                end_column: None,
            })
        };
        assert!(span_intersects_ranges(span(9, 15).as_ref(), &[(10, 12)]));
        // Touching at the boundary counts as intersecting.
        assert!(span_intersects_ranges(span(5, 10).as_ref(), &[(10, 12)]));
        assert!(!span_intersects_ranges(span(13, 20).as_ref(), &[(10, 12)]));
        assert!(!span_intersects_ranges(None, &[(10, 12)]));
        assert!(!span_intersects_ranges(span(9, 15).as_ref(), &[]));
    }

    #[test]
    fn rename_falls_back_to_old_path_for_symbol_resolution() {
        // The store records HEAD spans; a renamed file's symbols still live
        // under the old path, so resolution falls back to the rename source.
        let records = vec![
            file_node("file-1", "src/old.rs"),
            symbol_node("sym-1", "moved_fn", "src/old.rs", 9, 15),
        ];
        let index = RepositoryIndex::build(&records);
        let diff = WorkingTreeDiff {
            paths: vec![crate::worktree_diff::ChangedPath {
                path: "src/new.rs".to_owned(),
                status: PathStatus::Renamed {
                    from: "src/old.rs".to_owned(),
                },
                old_ranges: vec![(10, 12)],
                new_ranges: vec![(10, 12)],
                changed_lines: 3,
                hunk_count: 1,
            }],
        };
        let brief = brief_working_set(&records, &diff, &index, None);
        assert_eq!(brief.changed_files.len(), 1);
        assert_eq!(brief.changed_symbols.len(), 1);
        assert_eq!(brief.changed_symbols[0].record_id, "sym-1");
        assert!(brief.unindexed.is_empty());
    }

    #[test]
    fn deletion_hunk_intersects_on_old_side() {
        // A pure deletion has no new-side ranges; the symbol is still found
        // via the HEAD-side (old) ranges.
        let records = vec![
            file_node("file-1", "src/a.rs"),
            symbol_node("sym-1", "deleted_fn", "src/a.rs", 4, 8),
            symbol_node("sym-2", "kept_fn", "src/a.rs", 30, 40),
        ];
        let index = RepositoryIndex::build(&records);
        let diff = WorkingTreeDiff {
            paths: vec![crate::worktree_diff::ChangedPath {
                path: "src/a.rs".to_owned(),
                status: PathStatus::Modified,
                old_ranges: vec![(5, 7)],
                new_ranges: vec![],
                changed_lines: 3,
                hunk_count: 1,
            }],
        };
        let brief = brief_working_set(&records, &diff, &index, None);
        assert_eq!(brief.changed_symbols.len(), 1);
        assert_eq!(brief.changed_symbols[0].record_id, "sym-1");
    }

    #[test]
    fn insertion_only_hunk_anchors_on_old_side() {
        // An insertion-only hunk carries an old-side anchor line, so a symbol
        // surrounding the insertion point still resolves.
        let records = vec![
            file_node("file-1", "src/a.rs"),
            symbol_node("sym-1", "grown_fn", "src/a.rs", 8, 14),
        ];
        let index = RepositoryIndex::build(&records);
        let diff = WorkingTreeDiff {
            paths: vec![crate::worktree_diff::ChangedPath {
                path: "src/a.rs".to_owned(),
                status: PathStatus::Modified,
                old_ranges: vec![(10, 10)],
                new_ranges: vec![(10, 14)],
                changed_lines: 5,
                hunk_count: 1,
            }],
        };
        let brief = brief_working_set(&records, &diff, &index, None);
        assert_eq!(brief.changed_symbols.len(), 1);
        assert_eq!(brief.changed_symbols[0].record_id, "sym-1");
    }

    /// Minimal node fixture for the `StoreCoverage::from_records` lane tests.
    fn coverage_node(id: &str, kind: NodeKind) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            "coverage test".to_owned(),
        )
    }

    #[test]
    fn coverage_from_records_empty_store_reports_all_domains_absent() {
        let coverage = StoreCoverage::from_records(&[]);
        assert!(!coverage.code_graph);
        assert!(!coverage.agent_memory);
        assert!(!coverage.project);
        assert!(!coverage.artifact);
        assert!(!coverage.verification);
    }

    #[test]
    fn coverage_from_records_codegraph_only_store() {
        let records = vec![
            coverage_node("c:1", NodeKind::Symbol),
            coverage_node("c:2", NodeKind::File),
            coverage_node("c:3", NodeKind::Module),
            coverage_node("c:4", NodeKind::Import),
        ];
        let coverage = StoreCoverage::from_records(&records);
        assert!(coverage.code_graph);
        assert!(!coverage.agent_memory);
        assert!(!coverage.project);
        assert!(!coverage.artifact);
        assert!(!coverage.verification);
    }

    #[test]
    fn coverage_from_records_each_domain_kind_sets_its_flag() {
        // Every kind the context/task sections classify must flip exactly
        // its domain's flag — and no other.
        let records = vec![
            coverage_node("m:1", NodeKind::Observation),
            coverage_node("m:2", NodeKind::Decision),
            coverage_node("m:3", NodeKind::Failure),
            coverage_node("p:1", NodeKind::Task),
            coverage_node("p:2", NodeKind::AcceptanceCriterion),
            coverage_node("p:3", NodeKind::LocalTask),
            coverage_node("p:4", NodeKind::GitHubIssue),
            coverage_node("p:5", NodeKind::PR),
            coverage_node("p:6", NodeKind::Review),
            coverage_node("p:7", NodeKind::ExternalIdentity),
            coverage_node("p:8", NodeKind::ReviewStateTransition),
            coverage_node("a:1", NodeKind::Artifact),
            coverage_node("a:2", NodeKind::PatchArtifact),
            coverage_node("a:3", NodeKind::FileEdit),
            coverage_node("v:1", NodeKind::Verification),
            coverage_node("v:2", NodeKind::CommandEvidence),
            coverage_node("v:3", NodeKind::TestRun),
            coverage_node("v:4", NodeKind::CommandRun),
            coverage_node("v:5", NodeKind::CIStatus),
            coverage_node("v:6", NodeKind::BenchmarkRun),
            coverage_node("v:7", NodeKind::CoverageReport),
            coverage_node("v:8", NodeKind::ProofResult),
        ];
        let coverage = StoreCoverage::from_records(&records);
        assert!(coverage.agent_memory, "memory kinds must set agent_memory");
        assert!(coverage.project, "project kinds must set project");
        assert!(coverage.artifact, "artifact kinds must set artifact");
        assert!(
            coverage.verification,
            "verification kinds must set verification"
        );
        assert!(
            !coverage.code_graph,
            "no codegraph kind present: flag must stay false"
        );
    }

    #[test]
    fn coverage_from_records_infrastructure_sets_no_flags() {
        // Kinds no context/task section renders — including the
        // agent-memory-adjacent infrastructure the `eg brief` lane counts —
        // must not flip any flag in this lane.
        let records = vec![
            coverage_node("i:1", NodeKind::Repository),
            coverage_node("i:2", NodeKind::Agent),
            coverage_node("i:3", NodeKind::AgentSession),
            coverage_node("i:4", NodeKind::Commit),
            coverage_node("i:5", NodeKind::ScanCoverage),
            coverage_node("i:6", NodeKind::SemanticDrift),
            coverage_node("i:7", NodeKind::Diagnostic),
            coverage_node("i:8", NodeKind::ExternalLink),
            GraphRecord::Tombstone {
                id: "tomb:1".to_owned(),
                schema_version: crate::ir::SCHEMA_VERSION,
                deleted_id: "m:1".to_owned(),
                summary: "forget".to_owned(),
                producer: None,
            },
            GraphRecord::Edge {
                id: "e:1".to_owned(),
                schema_version: crate::ir::SCHEMA_VERSION,
                label: EdgeLabel::Defines,
                source: "i:1".to_owned(),
                target: "i:2".to_owned(),
                confidence: None,
                resolution: None,
                frame_resolution: None,
                frame_index: None,
                basis: None,
                call_site_spans: None,
                is_exhaustive: None,
                temporal: None,
                summary: "edge".to_owned(),
                producer: None,
            },
        ];
        let coverage = StoreCoverage::from_records(&records);
        assert!(
            !coverage.code_graph
                && !coverage.agent_memory
                && !coverage.project
                && !coverage.artifact
                && !coverage.verification,
            "infrastructure records must not flip any domain flag"
        );
    }

    #[test]
    fn coverage_from_records_tombstoned_domain_still_counts_as_present() {
        // Tombstone-naive by design: the domain WAS ingested into this store,
        // so an empty section reads as entity-absent, not domain-absent.
        let records = vec![
            coverage_node("m:1", NodeKind::Observation),
            GraphRecord::Tombstone {
                id: "tomb:1".to_owned(),
                schema_version: crate::ir::SCHEMA_VERSION,
                deleted_id: "m:1".to_owned(),
                summary: "forget".to_owned(),
                producer: None,
            },
        ];
        let coverage = StoreCoverage::from_records(&records);
        assert!(
            coverage.agent_memory,
            "a tombstoned observation still proves the domain was ingested"
        );
    }

    #[test]
    fn coverage_from_index_kind_keys_agrees_with_from_records() {
        let records = vec![
            coverage_node("c:1", NodeKind::Symbol),
            coverage_node("m:1", NodeKind::Failure),
            coverage_node("p:1", NodeKind::Task),
            coverage_node("a:1", NodeKind::Artifact),
            coverage_node("v:1", NodeKind::TestRun),
            coverage_node("i:1", NodeKind::Repository),
        ];
        let from_records = StoreCoverage::from_records(&records);
        let from_keys =
            StoreCoverage::from_index_kind_keys(records.iter().filter_map(|r| match r {
                GraphRecord::Node { kind, .. } => Some(kind.as_str()),
                _ => None,
            }));
        assert_eq!(
            from_records, from_keys,
            "index-key constructor must agree with the record constructor"
        );
        assert!(from_keys.code_graph);
        assert!(from_keys.agent_memory);
        assert!(from_keys.project);
        assert!(from_keys.artifact);
        assert!(from_keys.verification);
    }

    #[test]
    fn coverage_from_index_kind_keys_ignores_unknown_kinds() {
        let coverage = StoreCoverage::from_index_kind_keys(["NotAKind", "Symbol"].into_iter());
        assert!(coverage.code_graph);
        assert!(!coverage.agent_memory);
    }

    #[test]
    fn coverage_serialization_is_canonical_declaration_order() {
        let coverage = StoreCoverage {
            code_graph: true,
            ..StoreCoverage::default()
        };
        let json = serde_json::to_string(&coverage).expect("coverage serializes");
        assert_eq!(
            json,
            r#"{"code_graph":true,"agent_memory":false,"project":false,"artifact":false,"verification":false}"#,
            "store_coverage must serialize as a canonical key-ordered map"
        );
    }

    #[test]
    fn coverage_repeated_calls_render_byte_identical() {
        let records = vec![
            coverage_node("c:1", NodeKind::Symbol),
            coverage_node("m:1", NodeKind::Observation),
        ];
        let first = serde_json::to_string(&StoreCoverage::from_records(&records))
            .expect("coverage serializes");
        for _ in 0..5 {
            let again = serde_json::to_string(&StoreCoverage::from_records(&records))
                .expect("coverage serializes");
            assert_eq!(first, again, "store_coverage must be deterministic");
        }
    }
}
