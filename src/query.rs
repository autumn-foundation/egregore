//! Agent-facing graph query helpers.
#![allow(
    clippy::uninlined_format_args,
    clippy::manual_let_else,
    clippy::collapsible_if,
    clippy::match_like_matches_macro,
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::cast_precision_loss
)]

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use crate::ir::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SemanticDriftMetadata, UserContextScope,
};
use crate::redaction::redact_value;
/// Finds a symbol record by name at a specific Git commit.
///
/// `commit` may be a full SHA or a unique prefix from the caller's graph.
#[must_use]
pub fn symbol_at_commit<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    commit: &str,
) -> Option<&'records GraphRecord> {
    let mut matches = records
        .iter()
        .filter(|record| matches_symbol_at_commit(record, symbol_name, commit))
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.id().cmp(right.id()));
    matches.into_iter().next()
}

/// Returns semantic drift nodes ranked by score descending.
#[must_use]
pub fn largest_semantic_drifts(records: &[GraphRecord], limit: usize) -> Vec<&GraphRecord> {
    let mut drifts = records
        .iter()
        .filter_map(|record| semantic_drift(record).map(|drift| (record, drift_score(drift))))
        .collect::<Vec<_>>();
    drifts.sort_by(|(left_record, left_score), (right_record, right_score)| {
        right_score
            .partial_cmp(left_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left_record.id().cmp(right_record.id()))
    });
    drifts
        .into_iter()
        .take(limit)
        .map(|(record, _)| record)
        .collect()
}

fn matches_symbol_at_commit(record: &GraphRecord, symbol_name: &str, commit: &str) -> bool {
    let GraphRecord::Node {
        kind,
        name,
        temporal,
        ..
    } = record
    else {
        return false;
    };
    *kind == NodeKind::Symbol
        && name.as_deref() == Some(symbol_name)
        && temporal
            .as_ref()
            .is_some_and(|temporal| temporal.git_commit.starts_with(commit))
}

// ── symbol_context ────────────────────────────────────────────────────────────

/// An evidence link target that could not be resolved in the current store.
///
/// Surfaced in [`SymbolContext::unresolved`] instead of being silently dropped.
/// Per AC5 from issue #38.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnresolvedRef {
    /// Record ID of the node that carries the unresolved evidence link.
    pub source_record_id: String,
    /// Handle from the evidence link (e.g. `target_record_id` or path triple).
    pub target_handle: String,
    /// Relation type from the evidence link (e.g. `"VALIDATED_BY"`).
    pub relation: String,
    /// Target domain string from the evidence link.
    pub target_domain: String,
}

/// Evidence-backed symbol context returned by [`symbol_context`].
///
/// Sections are kept separate so the caller can present source facts
/// (deterministic) differently from observations (subjective) without
/// mixing trust levels. An [`Observation`] node MUST NOT appear in
/// [`SymbolContext::source_facts`].
///
/// [`Observation`]: crate::ir::NodeKind::Observation
#[derive(Debug, Default, Clone)]
pub struct SymbolContext<'a> {
    /// The queried symbol name.
    pub symbol_name: String,
    /// Code-graph records: `Symbol` and `File` nodes that are the source of truth.
    ///
    /// Every record has a stable record ID and at least one of:
    /// `repo_relative_path`, `temporal.git_commit`, or `valid_time`.
    pub source_facts: Vec<&'a GraphRecord>,
    /// Codegraph topology edges (DEFINES, CALLS, IMPORTS, etc.) whose source
    /// and target are both in `source_facts`. Allows consumers to cite the
    /// file→symbol definition relationship without re-reading the raw graph.
    pub topology_edges: Vec<&'a GraphRecord>,
    /// Agent-authored `Observation` nodes that mention or observe the symbol.
    ///
    /// Every record carries `agent_id`, `observed_at`, and `confidence`.
    /// These are subjective and MUST NOT be treated as source truth.
    pub observations: Vec<&'a GraphRecord>,
    /// `Task` and `AcceptanceCriterion` nodes linked to the symbol.
    pub project_state: Vec<&'a GraphRecord>,
    /// `Artifact` and `PatchArtifact` nodes linked to the symbol.
    pub artifacts: Vec<&'a GraphRecord>,
    /// `Verification`, `TestRun`, `CommandRun`, and `CommandEvidence` nodes
    /// linked to the symbol.
    pub verification_evidence: Vec<&'a GraphRecord>,
    /// Evidence link targets referenced by agent-memory nodes that are absent
    /// from this store slice. Surfaced explicitly per AC5.
    pub unresolved: Vec<UnresolvedRef>,
}

impl SymbolContext<'_> {
    /// Returns `true` when no symbol with the queried name exists in the store.
    ///
    /// Callers MUST check this before inspecting sections — all sections are
    /// empty for a no-match result.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.source_facts.is_empty()
            && self.observations.is_empty()
            && self.project_state.is_empty()
            && self.artifacts.is_empty()
            && self.verification_evidence.is_empty()
            && self.unresolved.is_empty()
    }
}

/// Classifies a [`NodeKind`] into one of the five context sections.
///
/// Returns `None` for kinds that do not belong to any section (e.g. edges,
/// tombstones, infrastructure nodes like `Repository`, `Agent`, `AgentSession`).
const fn classify_node(kind: NodeKind) -> Option<ContextSection> {
    match kind {
        // code-graph source facts
        NodeKind::Symbol | NodeKind::File | NodeKind::Module | NodeKind::Import => {
            Some(ContextSection::SourceFact)
        }
        // agent-authored observations and failure records
        NodeKind::Observation | NodeKind::Decision | NodeKind::Failure => {
            Some(ContextSection::Observation)
        }
        // project / task domain
        NodeKind::Task
        | NodeKind::AcceptanceCriterion
        | NodeKind::LocalTask
        | NodeKind::GitHubIssue
        | NodeKind::PR => Some(ContextSection::ProjectState),
        // artifact domain
        NodeKind::Artifact | NodeKind::PatchArtifact | NodeKind::FileEdit => {
            Some(ContextSection::Artifact)
        }
        // verification domain
        NodeKind::Verification
        | NodeKind::CommandEvidence
        | NodeKind::TestRun
        | NodeKind::CommandRun
        | NodeKind::CIStatus
        | NodeKind::BenchmarkRun
        | NodeKind::CoverageReport
        | NodeKind::ProofResult => Some(ContextSection::VerificationEvidence),
        // everything else (infrastructure, semantic, user-context, etc.) is excluded
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ContextSection {
    SourceFact,
    Observation,
    ProjectState,
    Artifact,
    VerificationEvidence,
}

/// Returns all known context for a named symbol, separated by trust domain.
///
/// # Algorithm
///
/// 1. Collect all `Symbol` node IDs for `symbol_name`.
/// 2. Add those symbol nodes to `source_facts`.
/// 3. Scan every other record:
///    a. Edges: if source or target is a known symbol ID, follow the other
///    end and classify the referenced node.
///    b. Nodes with `evidence_links`: for each link whose `target_record_id`
///    is a known symbol ID, classify the linking node.
/// 4. Collect any evidence link targets that are not present in the store
///    slice into `unresolved`.
///
/// Output ordering within each section is sorted by record ID for determinism
/// (AC7).
///
/// An empty [`SymbolContext`] where [`SymbolContext::is_no_match`] returns
/// `true` is returned when the symbol is not found. The caller MUST use
/// `is_no_match()` — there is no panic or error path.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn symbol_context<'a>(records: &'a [GraphRecord], symbol_name: &str) -> SymbolContext<'a> {
    // Step 0: collect tombstoned IDs so deleted symbols yield no-match, not
    // stale context. This mirrors the current-state filter used by the other
    // query paths (query symbol, query file).
    let tombstoned_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect();

    // Step 1: collect symbol record IDs, excluding tombstoned CURRENT-STATE records.
    //
    // Temporal records (from scan-history, carrying `temporal` metadata) are
    // historical snapshots — they must NOT be suppressed by a tombstone that
    // reflects deletion only in the current state.
    let symbol_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                name,
                temporal,
                ..
            } = r
            else {
                return None;
            };
            let is_historical = temporal.is_some();
            if name.as_deref() == Some(symbol_name)
                && (is_historical || !tombstoned_ids.contains(id.as_str()))
            {
                Some(id.as_str())
            } else {
                None
            }
        })
        .collect();

    if symbol_ids.is_empty() {
        return SymbolContext {
            symbol_name: symbol_name.to_owned(),
            ..Default::default()
        };
    }

    // Build a lookup map: record_id → record for fast classification checks.
    // For records with the same stable ID (temporal versions), last-write-wins
    // is acceptable here because we only use by_id for kind inspection.
    // Actual resolution of output records uses records.iter() to capture all
    // versions (see the `resolve` closure below).
    let by_id: std::collections::BTreeMap<&str, &GraphRecord> =
        records.iter().map(|r| (r.id(), r)).collect();

    // IDs that have at least one temporal version in the slice.  Used to
    // exempt historical records from current-state tombstone suppression.
    let has_any_temporal_version: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                temporal: Some(_),
                ..
            }
            | GraphRecord::Edge {
                id,
                temporal: Some(_),
                ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();

    // Step 2: the symbol nodes themselves are source_facts.
    let mut source_facts: BTreeSet<&str> = symbol_ids.clone();

    // Add co-located File nodes via DEFINES topology edges (primary mechanism).
    // DEFINES edges are created within a single repository scan, so they
    // unambiguously identify the correct file even in multi-repo stores.
    // Guard: only seed a file if its node is present AND not tombstoned.
    // Absent or tombstoned file sources from stale DEFINES edges would pollute
    // seed_ids and could pull in unrelated cross-domain context.
    // Track which symbol IDs were resolved via DEFINES (per-symbol, not a global flag).
    // A global flag would suppress the path fallback for ALL matched symbols when even
    // one has a DEFINES edge, silently omitting files for same-named symbols that lack
    // a DEFINES edge in a partial slice.
    let mut symbols_resolved_by_defines: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        if let GraphRecord::Edge {
            id: edge_id,
            label: EdgeLabel::Defines,
            source,
            target,
            temporal: edge_temporal,
            ..
        } = record
            && symbol_ids.contains(target.as_str())
            // Guard: skip tombstoned DEFINES edges — but only for non-temporal (current-state)
            // records. Temporal (historical) edges carry scan-history provenance and must
            // not be suppressed by a tombstone reflecting only the current state.
            && (edge_temporal.is_some() || !tombstoned_ids.contains(edge_id.as_str()))
            // Same temporal guard for the file source: a historical file node must not be
            // excluded by a current-state tombstone on its stable ID.
            && (has_any_temporal_version.contains(source.as_str())
                || !tombstoned_ids.contains(source.as_str()))
            && by_id.get(source.as_str()).is_some_and(|r| {
                matches!(
                    *r,
                    GraphRecord::Node {
                        kind: NodeKind::File,
                        ..
                    }
                )
            })
        {
            source_facts.insert(source.as_str());
            symbols_resolved_by_defines.insert(target.as_str());
        }
    }

    // Path-based fallback: only used per-symbol when no DEFINES edge resolved that
    // symbol's file. Path-matching may produce false positives in multi-repo stores
    // (different repos sharing identical relative paths), so it is skipped per-symbol
    // whenever a DEFINES edge already identified the correct file.
    // Tombstoned file nodes are excluded: a deleted file record must not seed
    // source_facts or its tombstoned ID would pollute seed_ids and could draw in
    // deleted file context via TOUCHED_FILE/TOUCHES_FILE edges.
    for record in records {
        let GraphRecord::Node {
            id: sym_id,
            kind: NodeKind::Symbol,
            name,
            repo_relative_path: Some(path),
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        if !symbol_ids.contains(sym_id.as_str()) {
            continue;
        }
        if symbols_resolved_by_defines.contains(sym_id.as_str()) {
            continue;
        }
        for candidate in records {
            let GraphRecord::Node {
                id: file_id,
                kind: NodeKind::File,
                repo_relative_path: Some(file_path),
                ..
            } = candidate
            else {
                continue;
            };
            if file_path == path
                && (has_any_temporal_version.contains(file_id.as_str())
                    || !tombstoned_ids.contains(file_id.as_str()))
            {
                source_facts.insert(file_id.as_str());
            }
        }
    }

    // Snapshot seed IDs (symbol IDs + co-located file IDs) before the main
    // loop. Used to detect links that target the symbol's context, including
    // file-scoped relations such as CommandRun --TOUCHED_FILE--> File.
    let seed_ids: BTreeSet<&str> = source_facts.iter().copied().collect();

    // Paths from seed nodes (Symbol + File) for gating triple-form evidence links.
    // Only triple-only citations whose target_repo_relative_path matches a seed path
    // are surfaced in `unresolved`; unrelated citations to other files must not
    // pollute the context for the queried symbol.
    let seed_paths: BTreeSet<&str> = seed_ids
        .iter()
        .filter_map(|id| {
            by_id.get(id).and_then(|r| match r {
                GraphRecord::Node {
                    repo_relative_path: Some(p),
                    ..
                } => Some(p.as_str()),
                _ => None,
            })
        })
        .collect();

    // Step 2b: collect topology edges where BOTH endpoints are in seed_ids.
    // These provide citable provenance for the file→symbol definition
    // relationship (and other structural topology) without BFS traversal.
    let mut topology_edge_ids: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = record
            && label.is_codegraph_topology_label()
            // Temporal guard: historical topology edges must not be suppressed by a
            // current-state tombstone. Only non-temporal edges are excluded by tombstones.
            && (has_any_temporal_version.contains(id.as_str())
                || !tombstoned_ids.contains(id.as_str()))
            && seed_ids.contains(source.as_str())
            && seed_ids.contains(target.as_str())
        {
            topology_edge_ids.insert(id.as_str());
        }
    }

    // Step 3a + 3b: classify linked records.
    let mut observations: BTreeSet<&str> = BTreeSet::new();
    let mut project_state: BTreeSet<&str> = BTreeSet::new();
    let mut artifacts: BTreeSet<&str> = BTreeSet::new();
    let mut verification_evidence: BTreeSet<&str> = BTreeSet::new();

    // Collect IDs of all records present in the slice for unresolved detection.
    let present_ids: BTreeSet<&str> = records.iter().map(GraphRecord::id).collect();
    let mut unresolved: Vec<UnresolvedRef> = Vec::new();

    // Helper: insert a record ID into the appropriate section.
    //
    // Returns `true` when the node was actually inserted into a section.
    // Returns `false` for: tombstoned IDs, missing IDs (by_id miss), non-Node
    // records, non-seed SourceFact candidates (sibling symbols/files), and
    // NodeKind variants with no section mapping.
    //
    // Callers MUST gate `next_frontier.push(id)` on this return value — only
    // classified nodes should expand the BFS; pushing skipped IDs would allow
    // missing ghost endpoints and sibling codegraph nodes to traverse further.
    let classify_and_insert = |record_id: &'a str,
                               source_facts: &mut BTreeSet<&'a str>,
                               observations: &mut BTreeSet<&'a str>,
                               project_state: &mut BTreeSet<&'a str>,
                               artifacts: &mut BTreeSet<&'a str>,
                               verification_evidence: &mut BTreeSet<&'a str>|
     -> bool {
        // Suppress tombstoned records, but allow temporal (historical) records
        // through even when their stable ID is tombstoned: the tombstone reflects
        // current-state deletion and must not erase historical context.
        if tombstoned_ids.contains(record_id) && !has_any_temporal_version.contains(record_id) {
            return false;
        }
        let Some(rec) = by_id.get(record_id) else {
            return false;
        };
        let GraphRecord::Node { kind, .. } = rec else {
            return false;
        };
        match classify_node(*kind) {
            Some(ContextSection::SourceFact) if seed_ids.contains(record_id) => {
                source_facts.insert(record_id);
                true
            }
            Some(ContextSection::Observation) => {
                observations.insert(record_id);
                true
            }
            Some(ContextSection::ProjectState) => {
                project_state.insert(record_id);
                true
            }
            Some(ContextSection::Artifact) => {
                artifacts.insert(record_id);
                true
            }
            Some(ContextSection::VerificationEvidence) => {
                verification_evidence.insert(record_id);
                true
            }
            Some(ContextSection::SourceFact) | None => false,
        }
    };

    // Step 3: bounded BFS traversal — 3 hops beyond seeds.
    //
    // Hop 1 discovers nodes directly linked to the symbol/file seeds.
    // Hop 2 discovers nodes linked to hop-1 results, e.g. AcceptanceCriterion
    // nodes owned by a Task that was found in hop 1 via OWNED_BY_TASK edges.
    // Hop 3 discovers nodes linked to hop-2 results, e.g. a Verification that
    // closes an AC discovered in hop 2 via CLOSES_ACCEPTANCE_CRITERION.
    //
    // `visited` prevents re-classifying the same node in a later hop.
    // `frontier` is the set of IDs whose outgoing/incoming edges are scanned
    // in the current hop.
    let mut visited: BTreeSet<&str> = seed_ids.clone();
    let mut frontier: BTreeSet<&str> = seed_ids.clone();
    // Per-version scan tracking for temporal nodes: keyed by "id@git_commit" so
    // each temporal version of a node has its evidence_links scanned independently.
    let mut temporal_evidence_scanned: BTreeSet<String> = BTreeSet::new();

    for _hop in 0..3_usize {
        let mut next_frontier: Vec<&'a str> = Vec::new();

        for record in records {
            match record {
                GraphRecord::Edge {
                    id: edge_id,
                    label,
                    source,
                    target,
                    ..
                } => {
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    // Skip tombstoned edges — a retracted relationship must not
                    // carry the BFS to its formerly-linked node.
                    // Temporal guard: a historical edge is exempt from current-state
                    // tombstone suppression. The tombstone reflects a current deletion;
                    // a temporal edge carries valid historical provenance.
                    if tombstoned_ids.contains(edge_id.as_str())
                        && !has_any_temporal_version.contains(edge_id.as_str())
                    {
                        continue;
                    }
                    let candidate = if frontier.contains(source.as_str()) {
                        Some(target.as_str())
                    } else if frontier.contains(target.as_str()) && !is_forward_only_label(*label) {
                        Some(source.as_str())
                    } else {
                        None
                    };
                    if let Some(id) = candidate
                        && visited.insert(id)
                    {
                        let was_classified = classify_and_insert(
                            id,
                            &mut source_facts,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        // Expand nodes that were classified OR that are valid relay
                        // kinds (e.g. ToolCall) which bridge classifiable sections but
                        // have no output section of their own. Tombstoned and missing
                        // (by_id miss) nodes still must not enter the frontier.
                        if was_classified
                            || is_bfs_relay_node(
                                id,
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(id);
                        }
                    }
                }
                GraphRecord::Node {
                    id: node_id,
                    evidence_links: Some(links),
                    temporal,
                    ..
                } => {
                    // For temporal nodes, track per-version by "id@git_commit" so
                    // each historical version has its evidence_links scanned independently.
                    // For current-state nodes, fall back to the visited set.
                    let already_scanned = temporal.as_ref().map_or_else(
                        || visited.contains(node_id.as_str()),
                        |t| {
                            let key = format!("{}@{}", node_id, t.git_commit);
                            !temporal_evidence_scanned.insert(key)
                        },
                    );
                    if already_scanned {
                        continue;
                    }
                    // Skip tombstoned non-temporal nodes before scanning evidence_links.
                    if tombstoned_ids.contains(node_id.as_str())
                        && !has_any_temporal_version.contains(node_id.as_str())
                    {
                        visited.insert(node_id.as_str());
                        continue;
                    }
                    // Classify if any evidence link directly targets a symbol/file seed.
                    // Using seed_ids (not frontier) prevents shared verification sinks
                    // that entered the frontier via edge traversal from causing sibling
                    // observations to be pulled in via their evidence_links. The edge arm
                    // handles multi-hop traversal; this arm is for direct symbol/file
                    // citations.
                    let links_to_frontier = links.iter().any(|link| {
                        link.target_record_id
                            .as_deref()
                            .is_some_and(|tid| seed_ids.contains(tid))
                    });
                    if links_to_frontier {
                        let was_classified = classify_and_insert(
                            node_id.as_str(),
                            &mut source_facts,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        visited.insert(node_id.as_str());
                        if was_classified
                            || is_bfs_relay_node(
                                node_id.as_str(),
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(node_id.as_str());
                        }

                        // Scan backing evidence_links: present unvisited targets are
                        // backing evidence; missing targets go to unresolved (AC5).
                        // Triple-based links (no target_record_id) are also surfaced
                        // as unresolved so consumers can diagnose absent targets.
                        for link in links {
                            if let Some(target_id) = &link.target_record_id {
                                if present_ids.contains(target_id.as_str())
                                    && !visited.contains(target_id.as_str())
                                {
                                    let target_classified = classify_and_insert(
                                        target_id.as_str(),
                                        &mut source_facts,
                                        &mut observations,
                                        &mut project_state,
                                        &mut artifacts,
                                        &mut verification_evidence,
                                    );
                                    visited.insert(target_id.as_str());
                                    if target_classified {
                                        next_frontier.push(target_id.as_str());
                                    }
                                } else if !present_ids.contains(target_id.as_str()) {
                                    unresolved.push(UnresolvedRef {
                                        source_record_id: node_id.clone(),
                                        target_handle: target_id.clone(),
                                        relation: link.relation.clone(),
                                        target_domain: link.target_domain.clone(),
                                    });
                                }
                            } else if let Some(handle) = evidence_link_triple_handle(link) {
                                unresolved.push(UnresolvedRef {
                                    source_record_id: node_id.clone(),
                                    target_handle: handle,
                                    relation: link.relation.clone(),
                                    target_domain: link.target_domain.clone(),
                                });
                            }
                        }
                    } else {
                        // Node has no resolved evidence link to seed_ids but may have
                        // triple-form links (target_repo_relative_path/span/commit with
                        // no target_record_id). Pre-resolution graph slices produced
                        // before daemon resolution can contain only these triples.
                        // Surface them in `unresolved` so consumers can diagnose the
                        // citation without requiring a raw graph reload.
                        let has_triple = links.iter().any(|link| {
                            link.target_record_id.is_none()
                                && evidence_link_triple_handle(link).is_some()
                        });
                        if has_triple {
                            // Do NOT mark visited here — a node that only has
                            // triple-form links may also be reachable via a graph
                            // edge, and marking it visited would prevent the edge arm
                            // from classifying it on a subsequent hop.
                            for link in links {
                                if link.target_record_id.is_none() {
                                    let Some(handle) = evidence_link_triple_handle(link) else {
                                        continue;
                                    };
                                    // Only surface triples that target one of the seed file
                                    // paths; unrelated citations to other files must not
                                    // pollute the context for the queried symbol.
                                    if !link
                                        .target_repo_relative_path
                                        .as_deref()
                                        .is_some_and(|p| seed_paths.contains(p))
                                    {
                                        continue;
                                    }
                                    unresolved.push(UnresolvedRef {
                                        source_record_id: node_id.clone(),
                                        target_handle: handle,
                                        relation: link.relation.clone(),
                                        target_domain: link.target_domain.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
                GraphRecord::Node { .. } | GraphRecord::Tombstone { .. } => {}
            }
        }

        if next_frontier.is_empty() {
            break; // convergence: no new nodes in this hop
        }
        frontier = next_frontier.into_iter().collect();
    }

    // Post-processing: scan evidence_links of nodes classified via the edge arm.
    // Those nodes are in the response but their evidence_links were never scanned
    // (the evidence_links arm only runs when a node's OWN links target the symbol).
    // This covers cases like Obs --edge--> Symbol where Obs also has a VALIDATED_BY
    // link to a Verification that needs to appear in verification_evidence.
    //
    // Iterative until convergence: a newly classified node (e.g. ObsB found via
    // ObsA's evidence_links) may itself have evidence_links (e.g. VALIDATED_BY →
    // CommandRun) that need scanning in a subsequent pass. Loop until no new nodes
    // are classified.
    //
    // `backfill_scanned` tracks which IDs have already been scanned so that the
    // convergence loop does not re-process nodes from earlier passes.
    let mut backfill_scanned: BTreeSet<String> =
        symbol_ids.iter().map(ToString::to_string).collect();

    loop {
        // Collect IDs that have been classified but not yet scanned in backfill.
        // Owned Strings release the borrows on the section BTreeSets so that
        // classify_and_insert can take mutable references below.
        let to_scan: Vec<String> = source_facts
            .iter()
            .chain(observations.iter())
            .chain(project_state.iter())
            .chain(artifacts.iter())
            .chain(verification_evidence.iter())
            .filter(|id| !backfill_scanned.contains(**id))
            .map(|id| (*id).to_owned())
            .collect();

        if to_scan.is_empty() {
            break;
        }

        for node_id in &to_scan {
            backfill_scanned.insert(node_id.clone());
        }

        for node_id in &to_scan {
            // .copied() converts Option<&&'a GraphRecord> → Option<&'a GraphRecord>
            // so that sub-borrows (nid, links) carry lifetime 'a and satisfy
            // the classify_and_insert closure's &'a str constraint.
            let Some(GraphRecord::Node {
                id: nid,
                evidence_links: Some(links),
                ..
            }) = by_id.get(node_id.as_str()).copied()
            else {
                continue;
            };
            for link in links {
                if let Some(target_id) = &link.target_record_id {
                    if present_ids.contains(target_id.as_str())
                        && !symbol_ids.contains(target_id.as_str())
                    {
                        classify_and_insert(
                            target_id.as_str(),
                            &mut source_facts,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                    } else if !present_ids.contains(target_id.as_str()) {
                        unresolved.push(UnresolvedRef {
                            source_record_id: nid.clone(),
                            target_handle: target_id.clone(),
                            relation: link.relation.clone(),
                            target_domain: link.target_domain.clone(),
                        });
                    }
                } else if let Some(handle) = evidence_link_triple_handle(link) {
                    unresolved.push(UnresolvedRef {
                        source_record_id: nid.clone(),
                        target_handle: handle,
                        relation: link.relation.clone(),
                        target_domain: link.target_domain.clone(),
                    });
                }
            }
        }
    }

    // Frontier expansion for backfill discoveries: nodes newly classified by the
    // backfill (e.g. a Task found via an Observation's evidence_links) were never
    // in the BFS frontier, so their outgoing cross-domain edges were never scanned.
    // Loop until convergence so that multi-hop chains discovered through backfill
    // are fully traversed — e.g. backfill → Task → AC → CommandRun all appear.
    let mut extra_frontier: BTreeSet<&str> = source_facts
        .iter()
        .chain(observations.iter())
        .chain(project_state.iter())
        .chain(artifacts.iter())
        .chain(verification_evidence.iter())
        .copied()
        .filter(|id| !visited.contains(*id))
        .collect();

    while !extra_frontier.is_empty() {
        let mut next_extra: Vec<&'a str> = Vec::new();
        for record in records {
            if let GraphRecord::Edge {
                id: edge_id,
                label,
                source,
                target,
                ..
            } = record
            {
                if !is_cross_domain_label(*label) {
                    continue;
                }
                // Same temporal guard as the main BFS edge arm.
                if tombstoned_ids.contains(edge_id.as_str())
                    && !has_any_temporal_version.contains(edge_id.as_str())
                {
                    continue;
                }
                let candidate = if extra_frontier.contains(source.as_str()) {
                    Some(target.as_str())
                } else if extra_frontier.contains(target.as_str()) && !is_forward_only_label(*label)
                {
                    Some(source.as_str())
                } else {
                    None
                };
                if let Some(id) = candidate
                    && visited.insert(id)
                {
                    let was_classified = classify_and_insert(
                        id,
                        &mut source_facts,
                        &mut observations,
                        &mut project_state,
                        &mut artifacts,
                        &mut verification_evidence,
                    );
                    if was_classified
                        || is_bfs_relay_node(id, &by_id, &tombstoned_ids, &has_any_temporal_version)
                    {
                        next_extra.push(id);
                    }
                }
            }
        }
        extra_frontier = next_extra.into_iter().collect();
    }

    // Remove symbol records from non-source-fact sections to avoid overlap
    // (a Symbol node classified via edge could end up in the wrong section).
    for sid in &symbol_ids {
        observations.remove(sid);
        project_state.remove(sid);
        artifacts.remove(sid);
        verification_evidence.remove(sid);
    }

    // Resolve ID sets → sorted record slices.
    //
    // Using records.iter() (not by_id) captures ALL records matching each ID,
    // including multiple temporal versions of the same symbol that share a
    // stable ID. by_id last-write-wins would silently drop all but one version.
    let resolve = |ids: &BTreeSet<&str>| -> Vec<&'a GraphRecord> {
        let mut out: Vec<&'a GraphRecord> = records
            .iter()
            .filter(|r| {
                ids.contains(r.id())
                    // Exclude non-temporal records whose IDs are tombstoned.
                    // Temporal (historical) versions of the same ID must be kept.
                    && match r {
                        GraphRecord::Node {
                            temporal: Some(_), ..
                        }
                        | GraphRecord::Edge {
                            temporal: Some(_), ..
                        } => true,
                        _ => !tombstoned_ids.contains(r.id()),
                    }
            })
            .collect();
        out.sort_by(|a, b| {
            a.id().cmp(b.id()).then_with(|| {
                let a_commit = if let GraphRecord::Node {
                    temporal: Some(t), ..
                } = a
                {
                    t.git_commit.as_str()
                } else {
                    ""
                };
                let b_commit = if let GraphRecord::Node {
                    temporal: Some(t), ..
                } = b
                {
                    t.git_commit.as_str()
                } else {
                    ""
                };
                a_commit.cmp(b_commit)
            })
        });
        out
    };

    SymbolContext {
        symbol_name: symbol_name.to_owned(),
        source_facts: resolve(&source_facts),
        topology_edges: {
            let mut out: Vec<&'a GraphRecord> = records
                .iter()
                .filter(|r| {
                    topology_edge_ids.contains(r.id())
                        // Exclude non-temporal records whose ID is tombstoned.
                        // Temporal (historical) versions of the same stable ID must be kept.
                        && match r {
                            GraphRecord::Edge {
                                temporal: Some(_), ..
                            } => true,
                            _ => !tombstoned_ids.contains(r.id()),
                        }
                })
                .collect();
            out.sort_by_key(|r| r.id());
            out
        },
        observations: resolve(&observations),
        project_state: resolve(&project_state),
        artifacts: resolve(&artifacts),
        verification_evidence: resolve(&verification_evidence),
        unresolved: {
            let mut u = unresolved;
            u.sort_by(|a, b| {
                a.source_record_id
                    .cmp(&b.source_record_id)
                    .then_with(|| a.target_handle.cmp(&b.target_handle))
                    .then_with(|| a.relation.cmp(&b.relation))
                    .then_with(|| a.target_domain.cmp(&b.target_domain))
            });
            u.dedup_by(|a, b| {
                a.source_record_id == b.source_record_id
                    && a.target_handle == b.target_handle
                    && a.relation == b.relation
                    && a.target_domain == b.target_domain
            });
            u
        },
    }
}

/// Returns `true` for edge labels that cross domain boundaries and therefore
/// signal a meaningful link to a symbol for the context query.
///
/// This is a superset of the previous list: `RelatesTo`, `Contradicts`, and
/// `Supersedes` are now included because they are permitted cross-domain
/// evidence-link labels and can legally appear on graph edges between an
/// agent-memory/project node and a code-graph symbol.
const fn is_cross_domain_label(label: EdgeLabel) -> bool {
    matches!(
        label,
        EdgeLabel::Observes
            | EdgeLabel::MentionsSymbol
            | EdgeLabel::ValidatedBy
            | EdgeLabel::HasEvidence
            | EdgeLabel::ReferencesTask
            | EdgeLabel::FailedOn
            | EdgeLabel::ExplainsChange
            | EdgeLabel::TouchedFile
            | EdgeLabel::TouchesFile
            | EdgeLabel::ProducedPatch
            | EdgeLabel::ProducedEvidence
            | EdgeLabel::ClosesAcceptanceCriterion
            | EdgeLabel::RelatesTo
            | EdgeLabel::Contradicts
            | EdgeLabel::Supersedes
            | EdgeLabel::OwnedByTask
    )
}

/// Returns `true` for edge labels that should only be traversed in the
/// forward direction (source → target) during BFS.
///
/// These labels all point FROM agent-memory/project nodes TOWARD shared sinks
/// (verification runs, artifacts, tasks). Traversing backward from the sink
/// would pull in unrelated sibling nodes that happen to reference the same
/// sink but have no connection to the queried symbol. For example, if two
/// observations are both validated by the same `CommandRun`, following
/// `VALIDATED_BY` backward from the run would classify the unrelated
/// observation as context.
///
/// `ClosesAcceptanceCriterion` has schema direction AC → Verification. Making
/// it forward-only prevents backward traversal from a Verification sink to
/// unrelated `AcceptanceCriteria` that happen to share the same run.
///
/// `ExplainsChange` is intentionally NOT forward-only: its schema direction
/// is Observation → Symbol/File (the same as `MentionsSymbol`). Backward
/// traversal from the Symbol/File seed is required to discover the explaining
/// Observation.
const fn is_forward_only_label(label: EdgeLabel) -> bool {
    matches!(
        label,
        EdgeLabel::ValidatedBy
            | EdgeLabel::HasEvidence
            | EdgeLabel::ProducedEvidence
            | EdgeLabel::ProducedPatch
            | EdgeLabel::ReferencesTask
            | EdgeLabel::ClosesAcceptanceCriterion
    )
}

/// Returns `true` when `record_id` identifies a node that should expand the BFS
/// frontier even though it has no output context section.
///
/// "Relay" nodes are infrastructure connectors that bridge classifiable sections:
/// - [`NodeKind::ToolCall`]: `TOUCHED_FILE → File` backward traversal discovers the
///   `ToolCall`; its `PRODUCED_EVIDENCE` forward edges then reach `CommandRun`/`TestRun`.
///
/// Relay expansion is only allowed for nodes that are present in `by_id` and
/// not tombstoned (current-state). Temporal relay nodes with the same stable ID
/// as a current-state tombstone are exempt — the tombstone reflects only the
/// current state; the historical relay must still bridge its edges.
fn is_bfs_relay_node(
    record_id: &str,
    by_id: &std::collections::BTreeMap<&str, &GraphRecord>,
    tombstoned_ids: &BTreeSet<&str>,
    has_any_temporal_version: &BTreeSet<&str>,
) -> bool {
    if tombstoned_ids.contains(record_id) && !has_any_temporal_version.contains(record_id) {
        return false;
    }
    let Some(rec) = by_id.get(record_id) else {
        return false;
    };
    matches!(
        rec,
        GraphRecord::Node {
            kind: NodeKind::ToolCall | NodeKind::AgentTurn | NodeKind::AgentRun,
            ..
        }
    )
}

/// Constructs an unresolved-ref handle string from the triple fields of an
/// `EvidenceLink` that has no `target_record_id`.
///
/// Returns `None` when none of the triple fields are present (link is unusable).
/// Format: `{path}:{start}..{end}@{commit}` when all fields present; subsets
/// when only some are available.
fn evidence_link_triple_handle(link: &EvidenceLink) -> Option<String> {
    let path = link.target_repo_relative_path.as_deref()?;
    Some(match (&link.target_span, &link.target_git_commit) {
        (Some(span), Some(commit)) => {
            format!("{path}:{}..{}@{commit}", span.start_line, span.end_line)
        }
        (Some(span), None) => format!("{path}:{}..{}", span.start_line, span.end_line),
        (None, Some(commit)) => format!("{path}@{commit}"),
        (None, None) => path.to_owned(),
    })
}

fn semantic_drift(record: &GraphRecord) -> Option<&SemanticDriftMetadata> {
    let GraphRecord::Node {
        kind,
        semantic_drift,
        ..
    } = record
    else {
        return None;
    };
    if *kind == NodeKind::SemanticDrift {
        semantic_drift.as_deref()
    } else {
        None
    }
}

const fn drift_score(drift: &SemanticDriftMetadata) -> f64 {
    drift.score
}

/// Finds a symbol record by name at the most recent commit at or before `as_of`.
///
/// `as_of` must be an RFC 3339 timestamp string. Returns an error string if the
/// timestamp cannot be parsed. Returns `None` when no record exists at or before
/// the given instant.
///
/// # Errors
///
/// Returns an error string when `as_of` is not a valid RFC 3339 timestamp.
pub fn symbol_as_of_valid_time<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    as_of: &str,
) -> Result<Option<&'records GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;

    let mut best: Option<(&GraphRecord, DateTime<chrono::FixedOffset>)> = None;

    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Symbol,
            name,
            temporal,
            valid_time,
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        // Resolve valid_time from history temporal block (history records) or
        // node-level field (current-tree records stamped by with_valid_time_inferred).
        let vt_str = temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref());
        let Some(vt_str) = vt_str else {
            continue;
        };
        let Ok(vt) = DateTime::parse_from_rfc3339(vt_str) else {
            continue;
        };
        if vt > as_of_dt {
            continue;
        }
        let is_better = best.as_ref().is_none_or(|(prev_r, prev_vt)| {
            vt > *prev_vt || (vt == *prev_vt && record.id() < prev_r.id())
        });
        if is_better {
            best = Some((record, vt));
        }
    }

    Ok(best.map(|(r, _)| r))
}

// ── Task Evidence Queries (Issue #48) ──────────────────────────────────────────

/// Error returned when resolving a task ID or handle.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TaskResolveError {
    /// The handle matches more than one task node.
    Ambiguous {
        /// The query handle.
        handle: String,
        /// The list of matched task IDs.
        candidates: Vec<String>,
    },
    /// The handle format is malformed or unsupported.
    Unsupported {
        /// The query handle.
        handle: String,
        /// Description of why the handle is unsupported.
        message: String,
    },
}

/// Structured task evidence context returned by [`task_evidence_context`].
#[derive(Debug, Default, Clone)]
pub struct TaskEvidenceContext<'a> {
    /// Stable Task record ID.
    pub task_id: String,
    /// The queried Task node(s), including history versions.
    pub tasks: Vec<&'a GraphRecord>,
    /// `AcceptanceCriterion` nodes owned by the Task.
    pub acceptance_criteria: Vec<&'a GraphRecord>,
    /// Code-graph files or symbols linked to the task.
    pub source_facts: Vec<&'a GraphRecord>,
    /// Agent-authored observations and failures referencing the task.
    pub observations: Vec<&'a GraphRecord>,
    /// Artifact handles (`Artifact`, `PatchArtifact`, etc.) linked to the task.
    pub artifacts: Vec<&'a GraphRecord>,
    /// Verification evidence (`Verification`, `CommandRun`, etc.) linked to the task.
    pub verification_evidence: Vec<&'a GraphRecord>,
    /// `Review` nodes (issue comments, PR reviews, etc.) referencing the task.
    pub reviews: Vec<&'a GraphRecord>,
    /// `ExternalLink` nodes referencing source links.
    pub external_links: Vec<&'a GraphRecord>,
    /// Evidence link targets referenced by agent-memory nodes that are absent.
    pub unresolved: Vec<UnresolvedRef>,
}

impl TaskEvidenceContext<'_> {
    /// Returns `true` when no task matching the queried handle exists in the store.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.tasks.is_empty()
            && self.acceptance_criteria.is_empty()
            && self.source_facts.is_empty()
            && self.observations.is_empty()
            && self.artifacts.is_empty()
            && self.verification_evidence.is_empty()
            && self.reviews.is_empty()
            && self.external_links.is_empty()
            && self.unresolved.is_empty()
    }
}

/// Resolves a task ID or handle to a set of canonical Task record IDs.
///
/// # Errors
///
/// Returns `TaskResolveError` when the handle format is unsupported or ambiguous.
#[allow(clippy::too_many_lines)]
pub fn resolve_task_ids(
    records: &[GraphRecord],
    id_or_handle: &str,
) -> Result<BTreeSet<String>, TaskResolveError> {
    if id_or_handle.is_empty() {
        return Err(TaskResolveError::Unsupported {
            handle: id_or_handle.to_owned(),
            message: "handle cannot be empty".to_owned(),
        });
    }

    let mut matched_ids = BTreeSet::new();

    // Case 1: Canonical Task record ID
    if id_or_handle.starts_with("project:") {
        let parts: Vec<&str> = id_or_handle.split(':').collect();
        let is_valid = parts.len() == 3
            && parts[0] == "project"
            && parts[1].starts_with('v')
            && parts[1][1..].chars().all(|c| c.is_ascii_digit())
            && parts[2].len() == 64
            && parts[2].chars().all(|c| c.is_ascii_hexdigit());

        if !is_valid {
            return Err(TaskResolveError::Unsupported {
                handle: id_or_handle.to_owned(),
                message: "malformed canonical task ID".to_owned(),
            });
        }

        // Search for a Task node with this ID
        for r in records {
            if let GraphRecord::Node {
                kind: NodeKind::Task,
                id,
                ..
            } = r
                && id == id_or_handle
            {
                matched_ids.insert(id.clone());
            }
        }
        return Ok(matched_ids);
    }

    // Determine supported handle formats
    let mut is_supported = false;

    // A: GitHub URL
    if id_or_handle.starts_with("https://github.com/")
        || id_or_handle.starts_with("http://github.com/")
    {
        is_supported = true;
    }
    // B: GitHub short handle (owner/repo#num or #num)
    else if id_or_handle.contains('#') {
        if let Some(pos) = id_or_handle.find('#') {
            let num_part = &id_or_handle[pos + 1..];
            if !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit()) {
                is_supported = true;
            }
        }
    }
    // C: Local JSONL task handle (path ending in .jsonl followed by :local_id)
    else if let Some(pos) = id_or_handle.rfind(':') {
        let (file_path, _) = id_or_handle.split_at(pos);
        if std::path::Path::new(file_path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            is_supported = true;
        }
    }

    if !is_supported {
        return Err(TaskResolveError::Unsupported {
            handle: id_or_handle.to_owned(),
            message: "handle format is not recognized. Supported formats: canonical ID, GitHub URL, GitHub short handle (owner/repo#num), local JSONL handle (path.jsonl:local_id)".to_owned(),
        });
    }

    // Resolve via ExternalLink nodes
    let mut matched_links = BTreeSet::new();

    // Check if handle is a GitHub short handle (owner/repo#num or #num)
    let mut github_short_handle_matches = None;
    if let Some(pos) = id_or_handle.find('#') {
        let repo_part = &id_or_handle[..pos];
        let num_part = &id_or_handle[pos + 1..];
        if !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit()) {
            github_short_handle_matches = Some((repo_part, num_part));
        }
    }

    // Local JSONL handle: convert to native ID representation
    let mut local_native_id = None;
    if let Some(pos) = id_or_handle.rfind(':') {
        let (file_path, local_id) = id_or_handle.split_at(pos);
        let local_id = &local_id[1..];
        if std::path::Path::new(file_path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            let encoded_file = crate::local_project::percent_encode(file_path);
            let encoded_id = crate::local_project::percent_encode(local_id);
            local_native_id = Some(format!("{encoded_file}:{encoded_id}"));
        }
    }

    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::ExternalLink,
            id,
            url,
            system_native_id,
            repository_remote,
            ..
        } = r
        {
            let mut matches = url.as_deref().is_some_and(|u| u == id_or_handle)
                || system_native_id
                    .as_deref()
                    .is_some_and(|n| n == id_or_handle)
                || local_native_id.as_ref().is_some_and(|native_id| {
                    system_native_id.as_deref().is_some_and(|n| n == native_id)
                });

            if let (false, Some((repo_part, num_part))) = (matches, github_short_handle_matches) {
                let native_id_matches = system_native_id.as_deref().is_some_and(|n| {
                    n == format!("issue:{num_part}") || n == format!("pr:{num_part}")
                });
                if native_id_matches {
                    if repo_part.is_empty() {
                        matches = true;
                    } else {
                        let expected_remote =
                            format!("https://github.com/{repo_part}").to_lowercase();
                        matches = repository_remote.as_deref().is_some_and(|r| {
                            r.to_lowercase().trim_end_matches(".git")
                                == expected_remote.trim_end_matches(".git")
                        });
                    }
                }
            }

            if matches {
                matched_links.insert(id.clone());
            }
        }
    }

    // Find Task nodes linked to matched ExternalLinks
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Task,
            id,
            source_external_link_id: Some(sel_id),
            ..
        } = r
            && matched_links.contains(sel_id)
        {
            matched_ids.insert(id.clone());
        }
    }

    // Also check EXTERNAL_HANDLE edges from Task to ExternalLink
    for r in records {
        if let GraphRecord::Edge {
            label: EdgeLabel::ExternalHandle,
            source,
            target,
            ..
        } = r
            && matched_links.contains(target)
        {
            for task_record in records {
                if let GraphRecord::Node {
                    kind: NodeKind::Task,
                    id,
                    ..
                } = task_record
                    && id == source
                {
                    matched_ids.insert(id.clone());
                }
            }
        }
    }

    if matched_ids.len() > 1 {
        let candidates: Vec<String> = matched_ids.iter().cloned().collect();
        return Err(TaskResolveError::Ambiguous {
            handle: id_or_handle.to_owned(),
            candidates,
        });
    }

    Ok(matched_ids)
}

/// Retrieves the evidence-backed task context starting from a resolved task ID.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn task_evidence_context<'a>(
    records: &'a [GraphRecord],
    task_id: &str,
) -> TaskEvidenceContext<'a> {
    let tombstoned_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect();

    let present_ids: BTreeSet<&str> = records.iter().map(GraphRecord::id).collect();

    let by_id: std::collections::BTreeMap<&str, &GraphRecord> =
        records.iter().map(|r| (r.id(), r)).collect();

    let has_any_temporal_version: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                temporal: Some(_),
                ..
            }
            | GraphRecord::Edge {
                id,
                temporal: Some(_),
                ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();

    let mut tasks = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Task,
            temporal,
            ..
        } = r
            && id == task_id
        {
            let is_historical = temporal.is_some();
            if is_historical || !tombstoned_ids.contains(id.as_str()) {
                tasks.insert(r.id());
            }
        }
    }

    if tasks.is_empty() {
        return TaskEvidenceContext::default();
    }

    let mut acceptance_criteria = BTreeSet::new();
    let mut source_facts = BTreeSet::new();
    let mut observations = BTreeSet::new();
    let mut artifacts = BTreeSet::new();
    let mut verification_evidence = BTreeSet::new();
    let mut reviews = BTreeSet::new();
    let mut external_links = BTreeSet::new();

    // Local ExternalLink from Task field
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Task,
            source_external_link_id: Some(sel_id),
            ..
        } = r
            && id == task_id
        {
            external_links.insert(sel_id.as_str());
        }
    }

    // Direct ACs by field
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::AcceptanceCriterion,
            parent_task_id: Some(parent_task_id),
            ..
        } = r
            && parent_task_id == task_id
        {
            acceptance_criteria.insert(id.as_str());
        }
    }

    // Scan all edges to collect direct links
    for r in records {
        if let GraphRecord::Edge {
            id: edge_id,
            label,
            source,
            target,
            ..
        } = r
        {
            if tombstoned_ids.contains(edge_id.as_str())
                && !has_any_temporal_version.contains(edge_id.as_str())
            {
                continue;
            }

            match label {
                EdgeLabel::OwnedByTask if target == task_id => {
                    acceptance_criteria.insert(source.as_str());
                }
                EdgeLabel::ExternalHandle if source == task_id => {
                    external_links.insert(target.as_str());
                }
                EdgeLabel::TouchesFile | EdgeLabel::MentionsSymbol if source == task_id => {
                    source_facts.insert(target.as_str());
                }
                EdgeLabel::ReferencesTask if target == task_id => {
                    if let Some(GraphRecord::Node { kind, .. }) = by_id.get(source.as_str()) {
                        match kind {
                            NodeKind::Observation | NodeKind::Decision | NodeKind::Failure => {
                                observations.insert(source.as_str());
                            }
                            NodeKind::Artifact | NodeKind::PatchArtifact | NodeKind::FileEdit => {
                                artifacts.insert(source.as_str());
                            }
                            NodeKind::Verification | NodeKind::CommandRun | NodeKind::TestRun => {
                                verification_evidence.insert(source.as_str());
                            }
                            NodeKind::Review => {
                                reviews.insert(source.as_str());
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Scan all nodes for evidence_links targeting the Task ID
    for r in records {
        if let GraphRecord::Node {
            id,
            kind,
            evidence_links: Some(links),
            ..
        } = r
        {
            let links_to_task = links
                .iter()
                .any(|link| link.target_record_id.as_deref() == Some(task_id));
            if links_to_task {
                match kind {
                    NodeKind::Observation | NodeKind::Decision | NodeKind::Failure => {
                        observations.insert(id.as_str());
                    }
                    NodeKind::Artifact | NodeKind::PatchArtifact | NodeKind::FileEdit => {
                        artifacts.insert(id.as_str());
                    }
                    NodeKind::Verification | NodeKind::CommandRun | NodeKind::TestRun => {
                        verification_evidence.insert(id.as_str());
                    }
                    NodeKind::Review => {
                        reviews.insert(id.as_str());
                    }
                    _ => {}
                }
            }
        }
    }

    // Seed BFS visited and frontier with all collected IDs
    let mut visited = BTreeSet::new();
    visited.insert(task_id);
    for id in &acceptance_criteria {
        visited.insert(*id);
    }
    for id in &source_facts {
        visited.insert(*id);
    }
    for id in &observations {
        visited.insert(*id);
    }
    for id in &artifacts {
        visited.insert(*id);
    }
    for id in &verification_evidence {
        visited.insert(*id);
    }
    for id in &reviews {
        visited.insert(*id);
    }
    for id in &external_links {
        visited.insert(*id);
    }

    let mut frontier: BTreeSet<&str> = visited.clone();
    let mut temporal_evidence_scanned: BTreeSet<String> = BTreeSet::new();
    let mut evidence_links_scanned: BTreeSet<&str> = BTreeSet::new();
    let mut unresolved = Vec::new();

    let classify_and_insert_task = |record_id: &'a str,
                                    source_facts: &mut BTreeSet<&'a str>,
                                    observations: &mut BTreeSet<&'a str>,
                                    artifacts: &mut BTreeSet<&'a str>,
                                    verification_evidence: &mut BTreeSet<&'a str>,
                                    reviews: &mut BTreeSet<&'a str>|
     -> bool {
        if tombstoned_ids.contains(record_id) && !has_any_temporal_version.contains(record_id) {
            return false;
        }
        let Some(rec) = by_id.get(record_id) else {
            return false;
        };
        let GraphRecord::Node { kind, .. } = rec else {
            return false;
        };
        if *kind == NodeKind::Review {
            reviews.insert(record_id);
            return true;
        }
        match classify_node(*kind) {
            Some(ContextSection::SourceFact) => {
                source_facts.insert(record_id);
                true
            }
            Some(ContextSection::Observation) => {
                observations.insert(record_id);
                true
            }
            Some(ContextSection::Artifact) => {
                artifacts.insert(record_id);
                true
            }
            Some(ContextSection::VerificationEvidence) => {
                verification_evidence.insert(record_id);
                true
            }
            _ => false,
        }
    };

    // BFS loop - run 2 more hops
    for _hop in 0..2_usize {
        let mut next_frontier: Vec<&'a str> = Vec::new();

        for record in records {
            match record {
                GraphRecord::Edge {
                    id: edge_id,
                    label,
                    source,
                    target,
                    ..
                } => {
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    if tombstoned_ids.contains(edge_id.as_str())
                        && !has_any_temporal_version.contains(edge_id.as_str())
                    {
                        continue;
                    }
                    let candidate = if frontier.contains(source.as_str()) {
                        Some(target.as_str())
                    } else if frontier.contains(target.as_str()) && !is_forward_only_label(*label) {
                        Some(source.as_str())
                    } else {
                        None
                    };
                    if let Some(id) = candidate
                        && visited.insert(id)
                    {
                        let was_classified = classify_and_insert_task(
                            id,
                            &mut source_facts,
                            &mut observations,
                            &mut artifacts,
                            &mut verification_evidence,
                            &mut reviews,
                        );
                        if was_classified
                            || is_bfs_relay_node(
                                id,
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(id);
                        }
                    }
                }
                GraphRecord::Node {
                    id: node_id,
                    evidence_links: Some(links),
                    temporal,
                    ..
                } => {
                    let already_scanned = temporal.as_ref().map_or_else(
                        || !evidence_links_scanned.insert(node_id.as_str()),
                        |t| {
                            let key = format!("{}@{}", node_id, t.git_commit);
                            !temporal_evidence_scanned.insert(key)
                        },
                    );
                    if already_scanned {
                        continue;
                    }
                    if tombstoned_ids.contains(node_id.as_str())
                        && !has_any_temporal_version.contains(node_id.as_str())
                    {
                        visited.insert(node_id.as_str());
                        continue;
                    }

                    if frontier.contains(node_id.as_str()) {
                        for link in links {
                            if let Some(target_id) = &link.target_record_id {
                                if present_ids.contains(target_id.as_str())
                                    && !visited.contains(target_id.as_str())
                                {
                                    let target_classified = classify_and_insert_task(
                                        target_id.as_str(),
                                        &mut source_facts,
                                        &mut observations,
                                        &mut artifacts,
                                        &mut verification_evidence,
                                        &mut reviews,
                                    );
                                    visited.insert(target_id.as_str());
                                    if target_classified {
                                        next_frontier.push(target_id.as_str());
                                    }
                                } else if !present_ids.contains(target_id.as_str()) {
                                    unresolved.push(UnresolvedRef {
                                        source_record_id: node_id.clone(),
                                        target_handle: target_id.clone(),
                                        relation: link.relation.clone(),
                                        target_domain: link.target_domain.clone(),
                                    });
                                }
                            } else if let Some(handle) = evidence_link_triple_handle(link) {
                                unresolved.push(UnresolvedRef {
                                    source_record_id: node_id.clone(),
                                    target_handle: handle,
                                    relation: link.relation.clone(),
                                    target_domain: link.target_domain.clone(),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier.into_iter().collect();
    }

    let resolve = |ids: &BTreeSet<&str>| -> Vec<&'a GraphRecord> {
        let mut out: Vec<&'a GraphRecord> = records
            .iter()
            .filter(|r| {
                ids.contains(r.id())
                    && match r {
                        GraphRecord::Node {
                            temporal: Some(_), ..
                        }
                        | GraphRecord::Edge {
                            temporal: Some(_), ..
                        } => true,
                        _ => !tombstoned_ids.contains(r.id()),
                    }
            })
            .collect();
        out.sort_by(|a, b| {
            a.id().cmp(b.id()).then_with(|| {
                let a_commit = if let GraphRecord::Node {
                    temporal: Some(t), ..
                } = a
                {
                    t.git_commit.as_str()
                } else {
                    ""
                };
                let b_commit = if let GraphRecord::Node {
                    temporal: Some(t), ..
                } = b
                {
                    t.git_commit.as_str()
                } else {
                    ""
                };
                a_commit.cmp(b_commit)
            })
        });
        out
    };

    TaskEvidenceContext {
        task_id: task_id.to_owned(),
        tasks: resolve(&tasks),
        acceptance_criteria: resolve(&acceptance_criteria),
        source_facts: resolve(&source_facts),
        observations: resolve(&observations),
        artifacts: resolve(&artifacts),
        verification_evidence: resolve(&verification_evidence),
        reviews: resolve(&reviews),
        external_links: resolve(&external_links),
        unresolved: {
            let mut u = unresolved;
            u.sort_by(|a, b| {
                a.source_record_id
                    .cmp(&b.source_record_id)
                    .then_with(|| a.target_handle.cmp(&b.target_handle))
                    .then_with(|| a.relation.cmp(&b.relation))
                    .then_with(|| a.target_domain.cmp(&b.target_domain))
            });
            u.dedup();
            u
        },
    }
}

// ── user_context query helpers ────────────────────────────────────────────────

fn scope_subset(a: &UserContextScope, b: &UserContextScope) -> bool {
    if let Some(r) = &a.repo {
        if Some(r) != b.repo.as_ref() {
            return false;
        }
    }
    if let Some(p) = &a.path_glob {
        if Some(p) != b.path_glob.as_ref() {
            return false;
        }
    }
    if let Some(l) = &a.language {
        if Some(l) != b.language.as_ref() {
            return false;
        }
    }
    if let Some(ph) = &a.lifecycle_phase {
        if Some(ph) != b.lifecycle_phase.as_ref() {
            return false;
        }
    }
    true
}

/// Checks if two UserContextScopes are compatible (one is subset of other or equal)
#[must_use]
pub fn scopes_compatible(a: &UserContextScope, b: &UserContextScope) -> bool {
    scope_subset(a, b) || scope_subset(b, a)
}

/// Checks if a record scope matches a query scope
#[must_use]
pub fn scope_matches(record_scope: &UserContextScope, query_scope: &UserContextScope) -> bool {
    if let (Some(r_repo), Some(q_repo)) = (&record_scope.repo, &query_scope.repo) {
        if r_repo != q_repo {
            return false;
        }
    }
    if let (Some(r_path), Some(q_path)) = (&record_scope.path_glob, &query_scope.path_glob) {
        if r_path != q_path {
            return false;
        }
    }
    if let (Some(r_lang), Some(q_lang)) = (&record_scope.language, &query_scope.language) {
        if r_lang != q_lang {
            return false;
        }
    }
    if let (Some(r_phase), Some(q_phase)) =
        (&record_scope.lifecycle_phase, &query_scope.lifecycle_phase)
    {
        if r_phase != q_phase {
            return false;
        }
    }
    true
}

/// Calculates Jaccard token similarity between two proposed rule texts
#[must_use]
pub fn jaccard_similarity(s1: &str, s2: &str) -> f64 {
    let normalize = |s: &str| -> Vec<String> {
        s.to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect()
    };
    let w1 = normalize(s1);
    let w2 = normalize(s2);
    if w1.is_empty() && w2.is_empty() {
        return 1.0;
    }
    let set1: std::collections::BTreeSet<String> = w1.into_iter().collect();
    let set2: std::collections::BTreeSet<String> = w2.into_iter().collect();

    let intersection = set1.intersection(&set2).count() as f64;
    let union = set1.union(&set2).count() as f64;
    intersection / union
}

/// Returns the latest decision node for the given candidate ID.
#[must_use]
pub fn latest_decision_for_candidate<'a>(
    records: &'a [GraphRecord],
    candidate_id: &str,
) -> Option<&'a GraphRecord> {
    records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node {
                kind: NodeKind::PromotionDecision,
                user_context,
                ..
            } = r
            {
                user_context.candidate_id.as_deref() == Some(candidate_id)
            } else {
                false
            }
        })
        .max_by(|a, b| {
            let a_time = match a {
                GraphRecord::Node { user_context, .. } => {
                    user_context.decided_at.as_deref().unwrap_or("")
                }
                _ => "",
            };
            let b_time = match b {
                GraphRecord::Node { user_context, .. } => {
                    user_context.decided_at.as_deref().unwrap_or("")
                }
                _ => "",
            };
            if let (Ok(a_t), Ok(b_t)) = (
                chrono::DateTime::parse_from_rfc3339(a_time),
                chrono::DateTime::parse_from_rfc3339(b_time),
            ) {
                a_t.cmp(&b_t)
            } else {
                a_time.cmp(b_time)
            }
        })
}

/// Checks if a candidate is suppressed under the rejection debounce window
#[must_use]
pub fn is_candidate_suppressed(records: &[GraphRecord], cand_id: &str) -> Option<String> {
    let cand = records.iter().rfind(|r| r.id() == cand_id)?;
    let (cand_text, cand_scope, superseded_by_id) = match cand {
        GraphRecord::Node {
            user_context,
            superseded_by,
            ..
        } => {
            let text = user_context.proposed_rule_text.as_deref()?;
            let scope = user_context.scope.as_ref()?;
            (text, scope, superseded_by.as_deref())
        }
        _ => return None,
    };

    if let Some(old_id) = superseded_by_id {
        if let Some(old_rec) = records.iter().rfind(|r| r.id() == old_id) {
            let decision = records.iter().rfind(|r| {
                if let GraphRecord::Node {
                    kind: NodeKind::PromotionDecision,
                    user_context,
                    ..
                } = r
                {
                    user_context.candidate_id.as_deref() == Some(old_id)
                        && user_context.outcome.as_deref() == Some("rejected")
                } else {
                    false
                }
            });
            if let Some(GraphRecord::Node {
                user_context: decision_fields,
                ..
            }) = decision
            {
                if decision_fields.outcome.as_deref() == Some("rejected") {
                    let decided_at_str = decision_fields.decided_at.as_deref()?;
                    let current_time_str = match cand {
                        GraphRecord::Node { valid_time, .. } => valid_time.as_deref(),
                        _ => None,
                    }
                    .unwrap_or("");

                    let elapsed_ok = if let Ok(decided_at) =
                        chrono::DateTime::parse_from_rfc3339(decided_at_str)
                        && let Ok(current_time) =
                            chrono::DateTime::parse_from_rfc3339(current_time_str)
                    {
                        current_time.signed_duration_since(decided_at) >= chrono::Duration::days(30)
                    } else {
                        false
                    };

                    let old_obs: std::collections::BTreeSet<&str> = match old_rec {
                        GraphRecord::Node { user_context, .. } => user_context
                            .supporting_evidence
                            .as_deref()
                            .map(|v| {
                                v.iter()
                                    .filter_map(|l| l.target_record_id.as_deref())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        _ => std::collections::BTreeSet::new(),
                    };

                    let new_obs: std::collections::BTreeSet<&str> = match cand {
                        GraphRecord::Node { user_context, .. } => user_context
                            .supporting_evidence
                            .as_deref()
                            .map(|v| {
                                v.iter()
                                    .filter_map(|l| l.target_record_id.as_deref())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        _ => std::collections::BTreeSet::new(),
                    };

                    let additional_count = new_obs.difference(&old_obs).count();

                    if additional_count < 5 || !elapsed_ok {
                        return Some("rejection_debounce".to_string());
                    }
                }
            }
        }
    }

    for old_rec in records {
        let (old_id, old_text, old_scope) = match old_rec {
            GraphRecord::Node {
                id,
                kind: NodeKind::PromoteCandidate,
                user_context,
                ..
            } => {
                if let Some(text) = &user_context.proposed_rule_text
                    && let Some(scope) = &user_context.scope
                {
                    (id.as_str(), text.as_str(), scope)
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        if old_id == cand_id {
            continue;
        }
        if jaccard_similarity(cand_text, old_text) >= 0.6
            && scopes_compatible(cand_scope, old_scope)
        {
            let decision = records.iter().rfind(|r| {
                if let GraphRecord::Node {
                    kind: NodeKind::PromotionDecision,
                    user_context,
                    ..
                } = r
                {
                    user_context.candidate_id.as_deref() == Some(old_id)
                        && user_context.outcome.as_deref() == Some("rejected")
                } else {
                    false
                }
            });
            if let Some(GraphRecord::Node {
                user_context: decision_fields,
                ..
            }) = decision
            {
                if decision_fields.outcome.as_deref() == Some("rejected") {
                    let decided_at_str = decision_fields.decided_at.as_deref()?;
                    let current_time_str = match cand {
                        GraphRecord::Node { valid_time, .. } => valid_time.as_deref(),
                        _ => None,
                    }
                    .unwrap_or("");

                    let elapsed_ok = if let Ok(decided_at) =
                        chrono::DateTime::parse_from_rfc3339(decided_at_str)
                        && let Ok(current_time) =
                            chrono::DateTime::parse_from_rfc3339(current_time_str)
                    {
                        current_time.signed_duration_since(decided_at) >= chrono::Duration::days(30)
                    } else {
                        false
                    };

                    let old_obs: std::collections::BTreeSet<&str> = {
                        match old_rec {
                            GraphRecord::Node { user_context, .. } => user_context
                                .supporting_evidence
                                .as_deref()
                                .map(|v| {
                                    v.iter()
                                        .filter_map(|l| l.target_record_id.as_deref())
                                        .collect()
                                })
                                .unwrap_or_default(),
                            _ => std::collections::BTreeSet::new(),
                        }
                    };

                    let new_obs: std::collections::BTreeSet<&str> = {
                        match cand {
                            GraphRecord::Node { user_context, .. } => user_context
                                .supporting_evidence
                                .as_deref()
                                .map(|v| {
                                    v.iter()
                                        .filter_map(|l| l.target_record_id.as_deref())
                                        .collect()
                                })
                                .unwrap_or_default(),
                            _ => std::collections::BTreeSet::new(),
                        }
                    };

                    let additional_count = new_obs.difference(&old_obs).count();

                    if additional_count < 5 || !elapsed_ok {
                        return Some("rejection_debounce".to_string());
                    }
                }
            }
        }
    }

    None
}

/// Returns a list of pending candidates (those without a decision, or whose decision is deferred)
#[must_use]
pub fn pending_candidates<'a>(
    records: &'a [GraphRecord],
    query_scope: Option<&UserContextScope>,
) -> Vec<&'a GraphRecord> {
    let mut candidates = Vec::new();

    let mut terminal_candidates = std::collections::BTreeSet::new();
    let terminal_outcomes = ["approved", "edited_then_approved", "rejected", "expired"];

    for rec in records {
        if let GraphRecord::Node {
            kind: NodeKind::PromotionDecision,
            user_context,
            ..
        } = rec
            && let (Some(cand_id), Some(outcome)) =
                (&user_context.candidate_id, &user_context.outcome)
            && terminal_outcomes.contains(&outcome.as_str())
        {
            terminal_candidates.insert(cand_id.clone());
        }
    }

    for rec in records {
        if let GraphRecord::Node {
            kind: NodeKind::PromoteCandidate,
            user_context,
            ..
        } = rec
        {
            if terminal_candidates.contains(rec.id()) {
                continue;
            }
            if is_candidate_suppressed(records, rec.id()).is_some() {
                continue;
            }
            if let Some(q_scope) = query_scope {
                if let Some(r_scope) = &user_context.scope {
                    if !scope_matches(r_scope, q_scope) {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            candidates.push(rec);
        }
    }

    candidates.sort_by_key(|c| c.id());
    candidates
}

/// Returns active policy records matching the query scope
#[must_use]
pub fn active_policy<'a>(
    records: &'a [GraphRecord],
    query_scope: Option<&UserContextScope>,
) -> Vec<&'a GraphRecord> {
    let revoked_ids: std::collections::BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Node {
                kind, user_context, ..
            } = r
            {
                if matches!(
                    kind,
                    NodeKind::Preference
                        | NodeKind::WorkflowRule
                        | NodeKind::NamingDecision
                        | NodeKind::Constraint
                ) && user_context.active_to.is_some()
                {
                    return Some(r.id());
                }
            }
            None
        })
        .collect();

    let mut policy_map = std::collections::BTreeMap::new();
    for rec in records {
        if let GraphRecord::Node {
            kind, user_context, ..
        } = rec
        {
            if matches!(
                kind,
                NodeKind::Preference
                    | NodeKind::WorkflowRule
                    | NodeKind::NamingDecision
                    | NodeKind::Constraint
            ) {
                if revoked_ids.contains(rec.id()) {
                    continue;
                }
                if user_context.active_to.is_some() {
                    continue;
                }
                if audit_trail(records, rec).is_err() {
                    continue;
                }
                if let Some(q_scope) = query_scope {
                    if let Some(r_scope) = &user_context.scope {
                        if !scope_matches(r_scope, q_scope) {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                policy_map.insert(rec.id(), rec);
            }
        }
    }
    let mut policy: Vec<&'a GraphRecord> = policy_map.into_values().collect();
    policy.sort_by_key(|p| p.id());
    policy
}

fn validate_contradicting_evidence_links(
    candidate_id: &str,
    links: &[EvidenceLink],
    records: &[GraphRecord],
) -> std::result::Result<(), String> {
    for link in links {
        if link.target_domain != "user_context" || link.relation != "CONTRADICTS" {
            return Err(format!(
                "PromoteCandidate.contradicting_evidence for '{}' must use target_domain 'user_context' and relation CONTRADICTS",
                candidate_id
            ));
        }
        let conf_val: f64 = link.confidence.parse().map_err(|_| {
            format!(
                "PromoteCandidate.contradicting_evidence[].confidence '{}' must be a numeric float string",
                link.confidence
            )
        })?;
        if !(0.0..=1.0).contains(&conf_val) {
            return Err(format!(
                "PromoteCandidate.contradicting_evidence[].confidence '{}' must be in the range [0.0, 1.0]",
                link.confidence
            ));
        }
        let target_id = link.target_record_id.as_deref().ok_or_else(|| {
            "PromoteCandidate.contradicting_evidence[].target_record_id is missing".to_owned()
        })?;
        let target_node = records
            .iter()
            .rfind(|r| r.id() == target_id)
            .ok_or_else(|| format!("evidence target '{}' not found", target_id))?;
        let kind = match target_node {
            GraphRecord::Node { kind, .. } => *kind,
            _ => return Err(format!("evidence target '{}' is not a node", target_id)),
        };
        if !matches!(
            kind,
            NodeKind::Preference
                | NodeKind::WorkflowRule
                | NodeKind::NamingDecision
                | NodeKind::Constraint
        ) {
            return Err(format!(
                "PromoteCandidate.contradicting_evidence target '{}' must be a Preference, WorkflowRule, NamingDecision, or Constraint, got {}",
                target_id,
                kind.as_str()
            ));
        }
    }
    Ok(())
}

fn validate_scope(
    scope: Option<&UserContextScope>,
    field_name: &str,
) -> std::result::Result<(), String> {
    let scope = scope.ok_or_else(|| format!("{} lacks scope", field_name))?;
    if let Some(lifecycle_phase) = scope.lifecycle_phase.as_deref() {
        if !["pre_commit", "pre_pr", "pre_merge", "runtime", "any"].contains(&lifecycle_phase) {
            return Err(format!(
                "{} has invalid lifecycle_phase '{}'",
                field_name, lifecycle_phase
            ));
        }
    }
    Ok(())
}

fn validate_durable_fields(
    kind: NodeKind,
    user_context: &crate::ir::UserContextFields,
    durable_id: &str,
) -> std::result::Result<(), String> {
    match kind {
        NodeKind::Preference => {
            if user_context.proposed_rule_kind.as_deref() != Some("preference") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'preference'",
                    durable_id
                ));
            }
            let text = user_context.rule_text.as_deref().unwrap_or("");
            if text.is_empty() {
                return Err(format!("Durable record '{}' lacks rule_text", durable_id));
            }
        }
        NodeKind::WorkflowRule => {
            if user_context.proposed_rule_kind.as_deref() != Some("workflow_rule") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'workflow_rule'",
                    durable_id
                ));
            }
            let text = user_context.rule_text.as_deref().unwrap_or("");
            if text.is_empty() {
                return Err(format!("Durable record '{}' lacks rule_text", durable_id));
            }
            let triggers = user_context
                .triggers
                .as_ref()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| format!("Durable record '{}' lacks triggers", durable_id))?;
            for trigger in triggers {
                if trigger.is_empty() {
                    return Err(format!(
                        "Durable record '{}' has empty trigger entry",
                        durable_id
                    ));
                }
                if ![
                    "pre_commit",
                    "pre_pr",
                    "pre_merge",
                    "pre_command",
                    "post_command",
                ]
                .contains(&trigger.as_str())
                {
                    return Err(format!(
                        "Durable record '{}' trigger '{}' is invalid",
                        durable_id, trigger
                    ));
                }
            }
            let action_summary = user_context.action_summary.as_deref().unwrap_or("");
            if action_summary.is_empty() {
                return Err(format!(
                    "Durable record '{}' lacks action_summary",
                    durable_id
                ));
            }
        }
        NodeKind::NamingDecision => {
            if user_context.proposed_rule_kind.as_deref() != Some("naming_decision") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'naming_decision'",
                    durable_id
                ));
            }
            let entity_kind = user_context.entity_kind.as_deref().unwrap_or("");
            if ![
                "crate", "module", "type", "function", "field", "feature", "other",
            ]
            .contains(&entity_kind)
            {
                return Err(format!(
                    "Durable record '{}' entity_kind '{}' is invalid",
                    durable_id, entity_kind
                ));
            }
            let name = user_context.canonical_name.as_deref().unwrap_or("");
            if name.is_empty() {
                return Err(format!(
                    "Durable record '{}' lacks canonical_name",
                    durable_id
                ));
            }
            if user_context.alternatives_rejected.is_none() {
                return Err(format!(
                    "Durable record '{}' lacks alternatives_rejected",
                    durable_id
                ));
            }
        }
        NodeKind::Constraint => {
            if user_context.proposed_rule_kind.as_deref() != Some("constraint") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'constraint'",
                    durable_id
                ));
            }
            let text = user_context.constraint_text.as_deref().unwrap_or("");
            if text.is_empty() {
                return Err(format!(
                    "Durable record '{}' lacks constraint_text",
                    durable_id
                ));
            }
            let level = user_context.enforcement_level.as_deref().unwrap_or("");
            if !["advisory", "blocking"].contains(&level) {
                return Err(format!(
                    "Durable record '{}' enforcement_level '{}' is invalid",
                    durable_id, level
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Traces the approval chain back to supporting observations
///
/// # Errors
///
/// Returns an error if any hop in the chain is missing, stale, or ambiguous.
pub fn audit_trail<'a>(
    records: &'a [GraphRecord],
    durable: &'a GraphRecord,
) -> std::result::Result<Vec<&'a GraphRecord>, String> {
    let mut chain = Vec::new();

    let durable_id = durable.id();
    let kind = match durable {
        GraphRecord::Node { kind, .. } => *kind,
        _ => return Err(format!("Durable record '{}' is not a node", durable_id)),
    };
    if !matches!(
        kind,
        NodeKind::Preference
            | NodeKind::WorkflowRule
            | NodeKind::NamingDecision
            | NodeKind::Constraint
    ) {
        return Err(format!(
            "Record '{}' is not a durable user-context policy node",
            durable_id
        ));
    }
    chain.push(durable);

    let user_context = match durable {
        GraphRecord::Node { user_context, .. } => user_context,
        _ => unreachable!(),
    };

    validate_scope(
        user_context.scope.as_ref(),
        &format!("Durable record '{}'", durable_id),
    )?;
    validate_durable_fields(kind, user_context, durable_id)?;

    let decision_id = user_context
        .approval_decision_id
        .as_deref()
        .ok_or_else(|| format!("Durable record '{}' lacks approval_decision_id", durable_id))?;

    let decision = records
        .iter()
        .rfind(|r| r.id() == decision_id)
        .ok_or_else(|| {
            format!(
                "Approval decision '{}' not found for durable record '{}'",
                decision_id, durable_id
            )
        })?;
    chain.push(decision);

    let decision_fields = match decision {
        GraphRecord::Node {
            kind: NodeKind::PromotionDecision,
            user_context,
            ..
        } => user_context,
        _ => {
            return Err(format!(
                "Decision '{}' is not a PromotionDecision node",
                decision_id
            ));
        }
    };
    let decided_by = decision_fields
        .decided_by
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks decided_by", decision_id))?;
    if decided_by.is_empty() {
        return Err(format!("Decision '{}' has empty decided_by", decision_id));
    }
    let outcome = decision_fields
        .outcome
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks outcome", decision_id))?;
    if outcome != "approved" && outcome != "edited_then_approved" {
        return Err(format!(
            "Decision '{}' has non-approving outcome '{}'",
            decision_id, outcome
        ));
    }
    let mat_id = decision_fields
        .materialized_record_id
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks materialized_record_id", decision_id))?;
    if mat_id != durable_id {
        return Err(format!(
            "Decision '{}' targets materialized record '{}', expected '{}'",
            decision_id, mat_id, durable_id
        ));
    }
    let decided_at = decision_fields.decided_at.as_deref();
    let active_from = user_context.active_from.as_deref();

    let decided_at_str =
        decided_at.ok_or_else(|| format!("Decision '{}' lacks decided_at", decision_id))?;
    let active_from_str =
        active_from.ok_or_else(|| format!("Durable record '{}' lacks active_from", durable_id))?;

    let decided_at_parsed = chrono::DateTime::parse_from_rfc3339(decided_at_str)
        .map_err(|e| format!("Decision '{}' has invalid decided_at: {}", decision_id, e))?;
    let active_from_parsed =
        chrono::DateTime::parse_from_rfc3339(active_from_str).map_err(|e| {
            format!(
                "Durable record '{}' has invalid active_from: {}",
                durable_id, e
            )
        })?;

    if decided_at_parsed != active_from_parsed {
        return Err(format!(
            "Durable record '{}' active_from '{}' does not match decision '{}' decided_at '{}'",
            durable_id, active_from_str, decision_id, decided_at_str
        ));
    }
    let prompt_id = decision_fields
        .prompt_id
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks prompt_id", decision_id))?;
    let prompt = records
        .iter()
        .rfind(|r| r.id() == prompt_id)
        .ok_or_else(|| {
            format!(
                "PromotionPrompt '{}' not found for decision '{}'",
                prompt_id, decision_id
            )
        })?;
    chain.push(prompt);

    let prompt_fields = match prompt {
        GraphRecord::Node {
            kind: NodeKind::PromotionPrompt,
            user_context,
            ..
        } => user_context,
        _ => {
            return Err(format!(
                "Prompt '{}' is not a PromotionPrompt node",
                prompt_id
            ));
        }
    };
    let candidate_id = prompt_fields
        .candidate_id
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks candidate_id", prompt_id))?;
    let surface = prompt_fields
        .prompt_surface
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompt_surface", prompt_id))?;
    if !["cli", "mcp", "web", "other"].contains(&surface) {
        return Err(format!(
            "Prompt '{}' prompt_surface '{}' is invalid",
            prompt_id, surface
        ));
    }
    let prompt_text = prompt_fields
        .prompt_text
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompt_text", prompt_id))?;
    if prompt_text.is_empty() {
        return Err(format!("Prompt '{}' has empty prompt_text", prompt_id));
    }
    let prompted_at = prompt_fields
        .prompted_at
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompted_at", prompt_id))?;
    chrono::DateTime::parse_from_rfc3339(prompted_at)
        .map_err(|e| format!("Prompt '{}' has invalid prompted_at: {}", prompt_id, e))?;
    let prompted_to = prompt_fields
        .prompted_to
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompted_to", prompt_id))?;
    if prompted_to.is_empty() {
        return Err(format!("Prompt '{}' has empty prompted_to", prompt_id));
    }
    let decision_candidate_id = decision_fields
        .candidate_id
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks candidate_id", decision_id))?;
    if decision_candidate_id != candidate_id {
        return Err(format!(
            "Decision '{}' targets candidate '{}', but prompt '{}' targets candidate '{}'",
            decision_id, decision_candidate_id, prompt_id, candidate_id
        ));
    }
    let candidate = records
        .iter()
        .rfind(|r| r.id() == candidate_id)
        .ok_or_else(|| {
            format!(
                "PromoteCandidate '{}' not found for prompt '{}'",
                candidate_id, prompt_id
            )
        })?;
    chain.push(candidate);

    let (candidate_fields, cand_confidence, cand_evidence_quality) = match candidate {
        GraphRecord::Node {
            kind: NodeKind::PromoteCandidate,
            user_context,
            confidence,
            evidence_quality,
            ..
        } => (
            user_context,
            confidence.as_deref(),
            evidence_quality.as_deref(),
        ),
        _ => {
            return Err(format!(
                "Candidate '{}' is not a PromoteCandidate node",
                candidate_id
            ));
        }
    };

    let conf_str = cand_confidence
        .ok_or_else(|| format!("PromoteCandidate '{}' lacks confidence", candidate_id))?;
    let conf_val: f64 = conf_str.parse().map_err(|_| {
        format!(
            "PromoteCandidate '{}' confidence '{}' must be a numeric float string",
            candidate_id, conf_str
        )
    })?;
    if !(0.0..=1.0).contains(&conf_val) {
        return Err(format!(
            "PromoteCandidate '{}' confidence '{}' must be in the range [0.0, 1.0]",
            candidate_id, conf_str
        ));
    }

    let eq_str = cand_evidence_quality
        .ok_or_else(|| format!("PromoteCandidate '{}' lacks evidence_quality", candidate_id))?;
    if !["verbatim", "summarized", "referenced_only"].contains(&eq_str) {
        return Err(format!(
            "PromoteCandidate '{}' evidence_quality '{}' is invalid (must be verbatim, summarized, or referenced_only)",
            candidate_id, eq_str
        ));
    }

    validate_scope(
        candidate_fields.scope.as_ref(),
        &format!("PromoteCandidate '{}'", candidate_id),
    )?;

    let contradicting = candidate_fields
        .contradicting_evidence
        .as_ref()
        .ok_or_else(|| {
            format!(
                "PromoteCandidate '{}' lacks contradicting_evidence",
                candidate_id
            )
        })?;
    validate_contradicting_evidence_links(candidate_id, contradicting, records)?;

    // Perform candidate-kind/body consistency checks
    let expected_kind = match kind {
        NodeKind::Preference => "preference",
        NodeKind::WorkflowRule => "workflow_rule",
        NodeKind::NamingDecision => "naming_decision",
        NodeKind::Constraint => "constraint",
        _ => unreachable!(),
    };
    if candidate_fields.proposed_rule_kind.as_deref() != Some(expected_kind) {
        return Err(format!(
            "Candidate '{}' proposed rule kind '{:?}' does not match durable policy kind '{:?}'",
            candidate_id, candidate_fields.proposed_rule_kind, kind
        ));
    }

    let is_edited = outcome == "edited_then_approved";
    match kind {
        NodeKind::Preference | NodeKind::WorkflowRule => {
            let durable_text = user_context.rule_text.as_deref().unwrap_or("");
            if is_edited {
                let edited_text = decision_fields.edited_rule_text.as_deref().unwrap_or("");
                if durable_text != edited_text {
                    return Err(format!(
                        "Durable record rule_text '{}' does not match decision edited_rule_text '{}'",
                        durable_text, edited_text
                    ));
                }
            } else {
                let cand_text = candidate_fields.proposed_rule_text.as_deref().unwrap_or("");
                let redacted_cand_text = redact_value(cand_text);
                if durable_text != redacted_cand_text {
                    return Err(format!(
                        "Durable record rule_text '{}' does not match candidate proposed_rule_text '{}' (redacted: '{}')",
                        durable_text, cand_text, redacted_cand_text
                    ));
                }
            }
        }
        NodeKind::Constraint => {
            let durable_text = user_context.constraint_text.as_deref().unwrap_or("");
            if is_edited {
                let edited_text = decision_fields.edited_rule_text.as_deref().unwrap_or("");
                if durable_text != edited_text {
                    return Err(format!(
                        "Durable record constraint_text '{}' does not match decision edited_rule_text '{}'",
                        durable_text, edited_text
                    ));
                }
            } else {
                let cand_text = candidate_fields.proposed_rule_text.as_deref().unwrap_or("");
                let redacted_cand_text = redact_value(cand_text);
                if durable_text != redacted_cand_text {
                    return Err(format!(
                        "Durable record constraint_text '{}' does not match candidate proposed_rule_text '{}' (redacted: '{}')",
                        durable_text, cand_text, redacted_cand_text
                    ));
                }
            }
        }
        NodeKind::NamingDecision => {
            let durable_name = user_context.canonical_name.as_deref().unwrap_or("");
            if is_edited {
                let edited_name = decision_fields.edited_rule_text.as_deref().unwrap_or("");
                if durable_name != edited_name {
                    return Err(format!(
                        "Durable record canonical_name '{}' does not match decision edited_rule_text '{}'",
                        durable_name, edited_name
                    ));
                }
            } else {
                let cand_name = candidate_fields.proposed_rule_text.as_deref().unwrap_or("");
                let redacted_cand_name = redact_value(cand_name);
                if durable_name != redacted_cand_name {
                    return Err(format!(
                        "Durable record canonical_name '{}' does not match candidate proposed_rule_text '{}' (redacted: '{}')",
                        durable_name, cand_name, redacted_cand_name
                    ));
                }
            }
        }
        _ => unreachable!(),
    }
    let supporting = candidate_fields
        .supporting_evidence
        .as_deref()
        .ok_or_else(|| format!("Candidate '{}' lacks supporting_evidence", candidate_id))?;

    let mut unique_supporting_targets = std::collections::BTreeSet::new();
    let mut sessions = std::collections::BTreeSet::new();
    let mut obs_nodes = Vec::new();
    for link in supporting {
        if link.target_domain != "agent_memory" {
            return Err(format!(
                "Candidate '{}' supporting evidence target domain '{}' is invalid (must be 'agent_memory')",
                candidate_id, link.target_domain
            ));
        }
        if link.relation != "PROPOSED_BY" {
            return Err(format!(
                "Candidate '{}' supporting evidence relation '{}' is invalid (must be 'PROPOSED_BY')",
                candidate_id, link.relation
            ));
        }
        let conf_val: f64 = link.confidence.parse().map_err(|_| {
            format!(
                "Candidate '{}' supporting evidence link confidence '{}' must be a numeric float string",
                candidate_id, link.confidence
            )
        })?;
        if !(0.0..=1.0).contains(&conf_val) {
            return Err(format!(
                "Candidate '{}' supporting evidence link confidence '{}' must be in the range [0.0, 1.0]",
                candidate_id, link.confidence
            ));
        }
        let obs_id = link.target_record_id.as_deref().ok_or_else(|| {
            format!(
                "Candidate '{}' supporting evidence link lacks target_record_id",
                candidate_id
            )
        })?;
        let obs = records.iter().rfind(|r| r.id() == obs_id).ok_or_else(|| {
            format!(
                "Supporting observation '{}' not found for candidate '{}'",
                obs_id, candidate_id
            )
        })?;
        let session_id = match obs {
            GraphRecord::Node {
                kind, session_id, ..
            } => {
                if !matches!(
                    kind,
                    NodeKind::Observation | NodeKind::AgentTurn | NodeKind::Decision
                ) {
                    return Err(format!(
                        "Supporting evidence '{}' has invalid node kind '{:?}' (must be Observation, AgentTurn, or Decision)",
                        obs_id, kind
                    ));
                }
                session_id.as_deref()
            }
            _ => {
                return Err(format!("Supporting evidence '{}' is not a node", obs_id));
            }
        };
        let sess = session_id.ok_or_else(|| {
            format!(
                "supporting_evidence.session_id is required for evidence target '{}' in candidate '{}'",
                obs_id, candidate_id
            )
        })?;
        if sess.is_empty() {
            return Err(format!(
                "supporting_evidence.session_id is required for evidence target '{}' in candidate '{}'",
                obs_id, candidate_id
            ));
        }
        if unique_supporting_targets.insert(obs_id.to_owned()) {
            sessions.insert(sess.to_owned());
            obs_nodes.push(obs);
        }
    }

    if unique_supporting_targets.len() < 3 {
        return Err(format!(
            "Candidate '{}' has {} unique supporting observations; at least 3 are required",
            candidate_id,
            unique_supporting_targets.len()
        ));
    }
    if sessions.len() < 2 {
        return Err(format!(
            "Candidate '{}' has evidence from {} distinct sessions; at least 2 are required",
            candidate_id,
            sessions.len()
        ));
    }

    obs_nodes.sort_by_key(|o| o.id());
    chain.extend(obs_nodes);

    Ok(chain)
}

// ── Memory Evidence Audit Queries (Issue #64) ──────────────────────────────────

/// Error returned when resolving a memory record ID or source/session handle.
///
/// Mirrors [`TaskResolveError`]: `Ambiguous` and `Unsupported` are the two
/// machine-readable, non-network failure modes. "Missing" (no match) and
/// "stale" (tombstoned) handles are surfaced by the CLI layer as `no_match` /
/// `stale_handle` envelopes, since both are about store state rather than the
/// handle's syntax.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MemoryResolveError {
    /// The handle resolves to more than one distinct memory claim.
    Ambiguous {
        /// The query handle.
        handle: String,
        /// The list of matched memory record IDs (canonical-sorted).
        candidates: Vec<String>,
    },
    /// The handle is empty or a malformed canonical agent-memory ID.
    Unsupported {
        /// The query handle.
        handle: String,
        /// Why the handle is unsupported.
        message: String,
    },
}

/// One stable, machine-readable diagnostic emitted by a memory audit.
///
/// Every diagnostic carries the original source handle so an operator can
/// follow it without the audit inferring a replacement (AC6).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MemoryAuditDiagnostic {
    /// Stable diagnostic code (e.g. `unresolved_evidence_link`).
    pub code: String,
    /// Record ID of the node that carries the issue.
    pub source_record_id: String,
    /// Original handle (record ID, path, or hash) — never an inferred value.
    pub target_handle: String,
    /// Relation that produced the handle, when applicable.
    pub relation: String,
    /// Target domain string, when applicable.
    pub target_domain: String,
}

/// One evidence record reached from the audited memory claim, with the
/// relation (edge label / evidence-link relation) that connected it.
#[derive(Debug, Clone)]
pub struct MemoryEvidenceItem<'a> {
    /// The reached graph record.
    pub record: &'a GraphRecord,
    /// The relation that connected it to the claim (e.g. `CONTRADICTS`).
    pub relation: String,
}

/// Structured memory evidence audit returned by [`memory_audit_context`].
///
/// Sections keep trust classes separate so an agent-authored claim is never
/// presented as source truth or proof by itself (AC3). Every section is
/// canonically ordered by record ID for determinism (AC8).
#[derive(Debug, Default, Clone)]
pub struct MemoryAuditContext<'a> {
    /// The queried memory record ID.
    pub memory_id: String,
    /// The agent-authored claim node(s) under audit.
    pub memory_claim: Vec<&'a GraphRecord>,
    /// `AgentSession` provenance node(s) linked via `AUTHORED_BY`.
    pub agent_sessions: Vec<&'a GraphRecord>,
    /// `Agent` provenance node(s) linked via `SESSION_OF`.
    pub agents: Vec<&'a GraphRecord>,
    /// Supporting evidence that is neither code, project, nor verification
    /// (artifacts, command evidence, other supporting memory).
    pub supporting_evidence: Vec<MemoryEvidenceItem<'a>>,
    /// Records connected to the claim via `CONTRADICTS` (either direction).
    pub contradicting_evidence: Vec<MemoryEvidenceItem<'a>>,
    /// Records that supersede the claim (`SUPERSEDES` / `superseded_by`).
    pub superseding_records: Vec<MemoryEvidenceItem<'a>>,
    /// Code-graph `File` / `Symbol` handles cited by the claim.
    pub related_code_handles: Vec<MemoryEvidenceItem<'a>>,
    /// Project-domain `Task` / `AcceptanceCriterion` handles cited by the claim.
    pub related_project_handles: Vec<MemoryEvidenceItem<'a>>,
    /// Verification-domain evidence cited by the claim.
    pub verification_evidence: Vec<MemoryEvidenceItem<'a>>,
    /// Stable diagnostics (unresolved links, triple-only targets, etc.).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
    /// Records excluded by `--verified-only`, reported rather than dropped (AC5).
    pub excluded: Vec<MemoryEvidenceItem<'a>>,
}

impl MemoryAuditContext<'_> {
    /// Returns `true` when no claim matching the queried ID exists in the store.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.memory_claim.is_empty()
    }
}

const fn is_verification_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Verification
            | NodeKind::CommandEvidence
            | NodeKind::TestRun
            | NodeKind::CommandRun
            | NodeKind::CIStatus
            | NodeKind::BenchmarkRun
            | NodeKind::CoverageReport
            | NodeKind::ProofResult
    )
}

const fn is_codegraph_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::File
            | NodeKind::Symbol
            | NodeKind::Module
            | NodeKind::Import
            | NodeKind::Commit
            | NodeKind::Change
            | NodeKind::Repository
    )
}

const fn is_project_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Task
            | NodeKind::AcceptanceCriterion
            | NodeKind::LocalTask
            | NodeKind::GitHubIssue
            | NodeKind::PR
            | NodeKind::Review
            | NodeKind::ExternalLink
            | NodeKind::Product
            | NodeKind::Project
            | NodeKind::Plan
    )
}

/// An agent-authored claim shape eligible for the memory audit subject and for
/// the unverified-observation exclusion filter.
const fn is_agent_claim_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Observation | NodeKind::Decision | NodeKind::Failure
    )
}

const fn record_node_kind(record: &GraphRecord) -> Option<NodeKind> {
    match record {
        GraphRecord::Node { kind, .. } => Some(*kind),
        _ => None,
    }
}

/// A claim is **verified** when it cites at least one present verification-domain
/// record through an evidence link (`VALIDATED_BY`, `HAS_EVIDENCE`,
/// `PRODUCED_EVIDENCE`) or an equivalent outgoing edge. This is a structural,
/// non-inferential rule over existing contracts — not a truth judgement.
fn is_verified_claim(
    record: &GraphRecord,
    by_id: &BTreeMap<&str, &GraphRecord>,
    edges_from: &BTreeMap<&str, Vec<(&EdgeLabel, &str)>>,
    tombstoned: &BTreeSet<&str>,
) -> bool {
    if let GraphRecord::Node {
        evidence_links: Some(links),
        ..
    } = record
    {
        for link in links {
            // A backing relation is required; a generic link (e.g. RELATES_TO)
            // that merely happens to point at a verification record does not make
            // the claim verified. Tombstoned targets are treated as absent.
            let backed = matches!(
                link.relation.as_str(),
                "VALIDATED_BY" | "HAS_EVIDENCE" | "PRODUCED_EVIDENCE"
            );
            if backed
                && let Some(target_id) = link.target_record_id.as_deref()
                && !tombstoned.contains(target_id)
                && let Some(target) = by_id.get(target_id)
                && record_node_kind(target).is_some_and(is_verification_kind)
            {
                return true;
            }
        }
    }
    if let Some(out) = edges_from.get(record.id()) {
        for (label, target) in out {
            let backed = matches!(
                label,
                EdgeLabel::ValidatedBy | EdgeLabel::HasEvidence | EdgeLabel::ProducedEvidence
            );
            if backed
                && !tombstoned.contains(*target)
                && let Some(target) = by_id.get(*target)
                && record_node_kind(target).is_some_and(is_verification_kind)
            {
                return true;
            }
        }
    }
    false
}

/// Pushes an `unresolved_evidence_link` (absent) or `stale_evidence_target`
/// (tombstoned) diagnostic for an edge whose target is not live.
fn push_missing_target(
    diagnostics: &mut Vec<MemoryAuditDiagnostic>,
    memory_id: &str,
    target: &str,
    tombstoned: &BTreeSet<&str>,
    relation: &str,
) {
    let code = if tombstoned.contains(target) {
        "stale_evidence_target"
    } else {
        "unresolved_evidence_link"
    };
    diagnostics.push(MemoryAuditDiagnostic {
        code: code.to_owned(),
        source_record_id: memory_id.to_owned(),
        target_handle: target.to_owned(),
        relation: relation.to_owned(),
        target_domain: String::new(),
    });
}

/// The outcome of resolving a memory handle to live claim IDs.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct MemoryResolution {
    /// Live (non-tombstoned) claim IDs the handle resolved to.
    pub matched: BTreeSet<String>,
    /// True when the handle matched at least one claim but every match was
    /// tombstoned (deleted), so the caller should report `stale_handle` rather
    /// than `no_match`.
    pub tombstoned_only: bool,
}

/// Resolves a memory record ID or source/session handle to live claim IDs.
///
/// Supported handle types (AC2):
/// 1. A canonical memory record ID (`agent_memory:v1:<64-hex>`). When it names
///    an `AgentSession` or `Agent`, it is a scope handle resolving to the claims
///    authored in that scope; a scope with more than one claim returns
///    `Ambiguous` with the candidate IDs (one audit covers one claim).
/// 2. A source artifact / session handle: a string matching a claim node's
///    `source_handle`, `source_artifact_path`, `source_artifact_hash`, or
///    `session_id`.
///
/// Tombstoned (deleted) claims are excluded from `matched` so they neither make
/// a live handle ambiguous nor get audited as current state. When every match
/// was tombstoned, `tombstoned_only` is set so the caller can report
/// `stale_handle` even for a source/session handle (whose text is not the
/// deleted record ID).
///
/// # Errors
///
/// Returns [`MemoryResolveError::Unsupported`] for an empty or malformed
/// canonical ID, and [`MemoryResolveError::Ambiguous`] when the handle resolves
/// to more than one distinct live claim.
#[allow(clippy::too_many_lines)]
pub fn resolve_memory_ids(
    records: &[GraphRecord],
    handle: &str,
) -> Result<MemoryResolution, MemoryResolveError> {
    if handle.is_empty() {
        return Err(MemoryResolveError::Unsupported {
            handle: handle.to_owned(),
            message: "handle cannot be empty".to_owned(),
        });
    }

    let mut matched: BTreeSet<String> = BTreeSet::new();

    if let Some(rest) = handle.strip_prefix("agent_memory:v1:") {
        let is_valid = rest.len() == 64 && rest.chars().all(|c| c.is_ascii_hexdigit());
        if !is_valid {
            return Err(MemoryResolveError::Unsupported {
                handle: handle.to_owned(),
                message: "malformed canonical agent-memory ID".to_owned(),
            });
        }
        // Find the node carrying this ID.
        let mut subject_kind = None;
        let mut subject_name = None;
        let mut subject_session_id = None;
        let mut subject_agent_id = None;
        for r in records {
            if let GraphRecord::Node {
                id,
                kind,
                name,
                session_id,
                agent_id,
                ..
            } = r
                && id == handle
            {
                subject_kind = Some(*kind);
                subject_name.clone_from(name);
                subject_session_id.clone_from(session_id);
                subject_agent_id.clone_from(agent_id);
            }
        }
        match subject_kind {
            Some(kind) if is_agent_claim_kind(kind) => {
                matched.insert(handle.to_owned());
            }
            Some(NodeKind::AgentSession) => {
                // Prefer the session node's `session_id` field; imported session
                // nodes (e.g. Codex) keep a human summary in `name` while claims
                // store the real key in `session_id`. Fall back to `name`.
                let session_key = subject_session_id.as_deref().or(subject_name.as_deref());
                for r in records {
                    if let GraphRecord::Node {
                        id,
                        kind,
                        session_id: Some(sid),
                        ..
                    } = r
                        && is_agent_claim_kind(*kind)
                        && Some(sid.as_str()) == session_key
                    {
                        matched.insert(id.clone());
                    }
                }
            }
            Some(NodeKind::Agent) => {
                // Prefer the agent node's `agent_id` field; fall back to `name`.
                let agent_key = subject_agent_id.as_deref().or(subject_name.as_deref());
                for r in records {
                    if let GraphRecord::Node {
                        id,
                        kind,
                        agent_id: Some(aid),
                        ..
                    } = r
                        && is_agent_claim_kind(*kind)
                        && Some(aid.as_str()) == agent_key
                    {
                        matched.insert(id.clone());
                    }
                }
            }
            // Present but not an auditable claim, or absent entirely: leave the
            // set empty so the caller emits a `no_match` envelope.
            _ => {}
        }
    } else {
        // Source artifact / session handle.
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
                && is_agent_claim_kind(*kind)
                && (source_handle.as_deref() == Some(handle)
                    || source_artifact_path.as_deref() == Some(handle)
                    || source_artifact_hash.as_deref() == Some(handle)
                    || session_id.as_deref() == Some(handle))
            {
                matched.insert(id.clone());
            }
        }
    }

    // Tombstoned (deleted) claims are not part of the current state, so they must
    // not make a live handle ambiguous. Drop them before counting, but remember
    // whether the handle matched anything at all so a handle that pointed only at
    // deleted claims is reported as `stale_handle`, not `no_match`.
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();

    // If the queried canonical ID is itself tombstoned — a deleted claim, or a
    // deleted Agent/AgentSession scope node — the handle names a deleted record
    // and is stale, regardless of whether live claims share its scope key.
    if tombstoned.contains(handle) {
        return Ok(MemoryResolution {
            matched: BTreeSet::new(),
            tombstoned_only: true,
        });
    }

    let had_any_match = !matched.is_empty();
    matched.retain(|id| !tombstoned.contains(id.as_str()));
    let tombstoned_only = had_any_match && matched.is_empty();

    // A single audit covers one claim. A scope handle (Agent / AgentSession ID,
    // or a session_id shared by several claims) that resolves to more than one
    // claim is reported as a stable `Ambiguous` diagnostic listing the candidate
    // claim IDs, so the operator can re-query a specific one. This keeps the
    // single-claim audit contract honest rather than silently picking one.
    if matched.len() > 1 {
        return Err(MemoryResolveError::Ambiguous {
            handle: handle.to_owned(),
            candidates: matched.into_iter().collect(),
        });
    }

    Ok(MemoryResolution {
        matched,
        tombstoned_only,
    })
}

/// Section a reached evidence record belongs to.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum AuditSection {
    Supporting,
    Code,
    Project,
    Verification,
}

const fn classify_audit_target(kind: NodeKind) -> AuditSection {
    if is_verification_kind(kind) {
        AuditSection::Verification
    } else if is_codegraph_kind(kind) {
        AuditSection::Code
    } else if is_project_kind(kind) {
        AuditSection::Project
    } else {
        AuditSection::Supporting
    }
}

/// Builds the evidence audit for a resolved memory claim ID.
///
/// The traversal reads only existing contracts (evidence links + cross-domain
/// edges) and never reads raw transcript bodies or infers replacements for
/// missing handles (AC6, AC11). When `verified_only` is set, unverified
/// agent-authored records are moved out of their sections into `excluded`
/// rather than silently dropped (AC5).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn memory_audit_context<'a>(
    records: &'a [GraphRecord],
    memory_id: &str,
    verified_only: bool,
) -> MemoryAuditContext<'a> {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

    // Outgoing edges keyed by source ID, for verification detection.
    let mut edges_from: BTreeMap<&str, Vec<(&EdgeLabel, &str)>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = r
        {
            edges_from
                .entry(source.as_str())
                .or_default()
                .push((label, target.as_str()));
        }
    }

    // Tombstoned IDs are deleted for current-state reads: treat their nodes as
    // absent everywhere in the traversal so a deleted evidence/provenance record
    // is never surfaced as live support (AC6).
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let present = |id: &str| -> Option<&'a GraphRecord> {
        if tombstoned.contains(id) {
            None
        } else {
            by_id.get(id).copied()
        }
    };

    let claim_nodes: Vec<&GraphRecord> = records
        .iter()
        .filter(|r| matches!(r, GraphRecord::Node { id, .. } if id == memory_id))
        .collect();

    let mut ctx = MemoryAuditContext {
        memory_id: memory_id.to_owned(),
        memory_claim: claim_nodes.clone(),
        ..Default::default()
    };

    if claim_nodes.is_empty() {
        return ctx;
    }

    // Dedupe maps keyed by record ID; relation is the first one observed.
    let mut supporting: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut contradicting: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut superseding: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut code: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut project: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut verification: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut sessions: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut agents: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    let mut place = |target: &'a GraphRecord, relation: &str| {
        let Some(kind) = record_node_kind(target) else {
            return;
        };
        let item = MemoryEvidenceItem {
            record: target,
            relation: relation.to_owned(),
        };
        match classify_audit_target(kind) {
            AuditSection::Verification => {
                verification.entry(target.id()).or_insert(item);
            }
            AuditSection::Code => {
                code.entry(target.id()).or_insert(item);
            }
            AuditSection::Project => {
                project.entry(target.id()).or_insert(item);
            }
            AuditSection::Supporting => {
                supporting.entry(target.id()).or_insert(item);
            }
        }
    };

    for claim in &claim_nodes {
        // 1) Denormalized evidence links on the claim node.
        if let GraphRecord::Node {
            evidence_links: Some(links),
            ..
        } = claim
        {
            for link in links {
                let Some(target_id) = link.target_record_id.as_deref() else {
                    // Triple-only target: do not resolve heuristically (AC6).
                    let handle = link
                        .target_repo_relative_path
                        .clone()
                        .unwrap_or_else(|| "<triple>".to_owned());
                    diagnostics.push(MemoryAuditDiagnostic {
                        code: "evidence_target_unresolved".to_owned(),
                        source_record_id: memory_id.to_owned(),
                        target_handle: handle,
                        relation: link.relation.clone(),
                        target_domain: link.target_domain.clone(),
                    });
                    continue;
                };
                // A tombstoned target is stale for current-state reads; an absent
                // target is unresolved. Either way it is surfaced, not placed.
                let code = if tombstoned.contains(target_id) {
                    "stale_evidence_target"
                } else if by_id.contains_key(target_id) {
                    ""
                } else {
                    "unresolved_evidence_link"
                };
                if !code.is_empty() {
                    diagnostics.push(MemoryAuditDiagnostic {
                        code: code.to_owned(),
                        source_record_id: memory_id.to_owned(),
                        target_handle: target_id.to_owned(),
                        relation: link.relation.clone(),
                        target_domain: link.target_domain.clone(),
                    });
                    continue;
                }
                let Some(target) = present(target_id) else {
                    continue;
                };
                // A denormalized `CONTRADICTS` evidence link must reach the
                // contradicting section, same as a `CONTRADICTS` graph edge,
                // so a record that retained only the denormalized link does not
                // mis-report a contradiction as generic support.
                if link.relation == "CONTRADICTS" {
                    contradicting
                        .entry(target.id())
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: target,
                            relation: "CONTRADICTS".to_owned(),
                        });
                } else {
                    place(target, &link.relation);
                }
            }
        }

        // 2) Supersession declared inline on the claim — read even when the
        // claim carries no `evidence_links` (a common stale-claim shape).
        if let GraphRecord::Node {
            superseded_by: Some(sup_id),
            ..
        } = claim
        {
            if let Some(target) = present(sup_id) {
                superseding
                    .entry(target.id())
                    .or_insert_with(|| MemoryEvidenceItem {
                        record: target,
                        relation: "SUPERSEDED_BY".to_owned(),
                    });
            } else {
                let code = if tombstoned.contains(sup_id.as_str()) {
                    "stale_evidence_target"
                } else {
                    "unresolved_evidence_link"
                };
                diagnostics.push(MemoryAuditDiagnostic {
                    code: code.to_owned(),
                    source_record_id: memory_id.to_owned(),
                    target_handle: sup_id.clone(),
                    relation: "SUPERSEDED_BY".to_owned(),
                    target_domain: "agent_memory".to_owned(),
                });
            }
        }

        // 3) Edges touching the claim node.
        for r in records {
            let GraphRecord::Edge {
                label,
                source,
                target,
                ..
            } = r
            else {
                continue;
            };
            let claim_id = claim.id();
            if source == claim_id {
                // AUTHORED_BY provenance is resolved by the chain walk below;
                // code-internal and other labels are not claim evidence.
                let is_evidence = matches!(
                    label,
                    EdgeLabel::Contradicts
                        | EdgeLabel::HasEvidence
                        | EdgeLabel::ValidatedBy
                        | EdgeLabel::Observes
                        | EdgeLabel::MentionsSymbol
                        | EdgeLabel::TouchedFile
                        | EdgeLabel::FailedOn
                        | EdgeLabel::ReferencesTask
                        | EdgeLabel::ExplainsChange
                        | EdgeLabel::ProducedPatch
                        | EdgeLabel::ProducedEvidence
                        | EdgeLabel::RelatesTo
                );
                if !is_evidence {
                    continue;
                }
                let Some(other) = present(target) else {
                    // Edge-only evidence whose target is absent or tombstoned is
                    // surfaced as a diagnostic, like a denormalized link, instead
                    // of silently disappearing.
                    push_missing_target(
                        &mut diagnostics,
                        memory_id,
                        target,
                        &tombstoned,
                        label.as_str(),
                    );
                    continue;
                };
                if matches!(label, EdgeLabel::Contradicts) {
                    contradicting
                        .entry(other.id())
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: other,
                            relation: "CONTRADICTS".to_owned(),
                        });
                } else {
                    place(other, label.as_str());
                }
            } else if target == claim_id {
                if !matches!(label, EdgeLabel::Contradicts | EdgeLabel::Supersedes) {
                    continue;
                }
                let Some(other) = present(source) else {
                    push_missing_target(
                        &mut diagnostics,
                        memory_id,
                        source,
                        &tombstoned,
                        label.as_str(),
                    );
                    continue;
                };
                match label {
                    EdgeLabel::Contradicts => {
                        contradicting
                            .entry(other.id())
                            .or_insert_with(|| MemoryEvidenceItem {
                                record: other,
                                relation: "CONTRADICTS".to_owned(),
                            });
                    }
                    EdgeLabel::Supersedes => {
                        superseding
                            .entry(other.id())
                            .or_insert_with(|| MemoryEvidenceItem {
                                record: other,
                                relation: "SUPERSEDES".to_owned(),
                            });
                    }
                    _ => {}
                }
            }
        }
    }

    // Reverse denormalized links: a newer record may declare the relationship on
    // itself (`SUPERSEDES`/`CONTRADICTS` -> audited claim) without a retained
    // edge. Scan live records' evidence_links for entries targeting the claim so
    // the superseding/contradicting record is not omitted in edge-stripped stores.
    for r in records {
        let GraphRecord::Node {
            id: src_id,
            evidence_links: Some(links),
            ..
        } = r
        else {
            continue;
        };
        if src_id == memory_id || tombstoned.contains(src_id.as_str()) {
            continue;
        }
        for link in links {
            if link.target_record_id.as_deref() != Some(memory_id) {
                continue;
            }
            match link.relation.as_str() {
                "SUPERSEDES" => {
                    superseding
                        .entry(src_id)
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: r,
                            relation: "SUPERSEDES".to_owned(),
                        });
                }
                "CONTRADICTS" => {
                    contradicting
                        .entry(src_id)
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: r,
                            relation: "CONTRADICTS".to_owned(),
                        });
                }
                _ => {}
            }
        }
    }

    // Resolve session/agent provenance by walking the AUTHORED_BY / SESSION_OF
    // chain from each claim. Imported records commonly route a claim through an
    // AgentTurn and AgentRun before reaching the AgentSession, with SESSION_OF on
    // the session→agent edge, so the immediate AUTHORED_BY target is not always
    // the session. The walk classifies every reached node by kind; intermediate
    // turns/runs are traversed but never labelled as a session.
    {
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        let mut frontier: Vec<&str> = claim_nodes.iter().map(|c| c.id()).collect();
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
                if !visited.contains(*target) {
                    frontier.push(*target);
                }
                if let Some(node) = present(target) {
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
    }

    // verified-only filter: move unverified agent-authored records to `excluded`.
    let mut excluded: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    if verified_only {
        for map in [&mut contradicting, &mut superseding, &mut supporting] {
            let drop_ids: Vec<&str> = map
                .iter()
                .filter(|(_, item)| {
                    record_node_kind(item.record).is_some_and(is_agent_claim_kind)
                        && !is_verified_claim(item.record, &by_id, &edges_from, &tombstoned)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in drop_ids {
                if let Some(item) = map.remove(id) {
                    excluded.entry(id).or_insert(item);
                }
            }
        }
    }

    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
    });
    diagnostics.dedup();

    ctx.agent_sessions = sessions.into_values().collect();
    ctx.agents = agents.into_values().collect();
    ctx.supporting_evidence = supporting.into_values().collect();
    ctx.contradicting_evidence = contradicting.into_values().collect();
    ctx.superseding_records = superseding.into_values().collect();
    ctx.related_code_handles = code.into_values().collect();
    ctx.related_project_handles = project.into_values().collect();
    ctx.verification_evidence = verification.into_values().collect();
    ctx.excluded = excluded.into_values().collect();
    ctx.diagnostics = diagnostics;
    ctx
}
