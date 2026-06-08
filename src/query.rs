//! Agent-facing graph query helpers.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use crate::ir::{EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SemanticDriftMetadata};

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

// ── Transaction-time queries (Issue #66) ───────────────────────────────────────

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
);

/// Resolves the transaction-time handle of a record from its body fields.
///
/// Priority order:
/// 1. explicit `transaction_time` (project-domain mutations, seeded fixtures),
/// 2. `ingested_at` (agent-memory / verification commit time),
/// 3. `valid_time` when `valid_time_source == "inferred_from_transaction_time"`
///    (current-tree scans set `valid_time` to the scan's wall-clock instant,
///    which *is* the transaction time).
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
pub fn symbol_as_of_transaction_time<'r>(
    records: &'r [GraphRecord],
    symbol_name: &str,
    tx_as_of: &str,
    as_of_valid_time: Option<&str>,
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

    if named.is_empty() {
        diagnostics.push(TxDiagnostic {
            code: "no_named_symbol".to_owned(),
            message: format!("no Symbol named '{symbol_name}' exists in the store"),
        });
        return Ok(TxSymbolQuery {
            records: Vec::new(),
            diagnostics,
        });
    }

    // Track the earliest/latest known transaction time across all named
    // versions so we can report before-first / after-latest conditions.
    let mut min_tx: Option<DateTime<chrono::FixedOffset>> = None;
    let mut max_tx: Option<DateTime<chrono::FixedOffset>> = None;

    // Best candidate per stable record ID.
    // Comparison key: (valid_time, transaction_time) when a valid-time axis is
    // requested; (transaction_time,) otherwise.
    let mut best: BTreeMap<&str, TxCandidate<'r>> = BTreeMap::new();

    for record in &named {
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

        min_tx = Some(min_tx.map_or(tt, |m| m.min(tt)));
        max_tx = Some(max_tx.map_or(tt, |m| m.max(tt)));

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
            Some((_, prev_transaction, prev_valid)) => match (vt, prev_valid) {
                // Valid-time axis requested: prefer most-recent valid_time,
                // tie-break on most-recent transaction_time.
                (Some(cur_vt), Some(prev)) => {
                    cur_vt > *prev || (cur_vt == *prev && tt > *prev_transaction)
                }
                // No valid-time axis: prefer most-recent transaction_time.
                _ => tt > *prev_transaction,
            },
        };
        if replace {
            best.insert(key, (record, tt, vt));
        }
    }

    // Out-of-range diagnostics (do not change the result set, only annotate it).
    if let Some(min) = min_tx
        && tx_instant < min
    {
        diagnostics.push(TxDiagnostic {
            code: "before_first_transaction".to_owned(),
            message: format!(
                "tx-as-of '{tx_as_of}' precedes the earliest known transaction ('{}'); empty view",
                min.to_rfc3339()
            ),
        });
    }
    if let Some(max) = max_tx
        && tx_instant >= max
    {
        diagnostics.push(TxDiagnostic {
            code: "after_latest_transaction".to_owned(),
            message: format!(
                "tx-as-of '{tx_as_of}' is at or after the latest known transaction ('{}'); view reflects all known history",
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
    let mut selected: Vec<&GraphRecord> = best.into_values().map(|(r, _, _)| r).collect();

    // Cross-id supersession (AC2): a Symbol carrying `superseded_by` is dropped
    // once its replacement is *also known by the instant* — i.e. the target has a
    // resolvable transaction time at or before `tx_as_of`. If the superseding
    // record is not yet known (committed after the instant), the superseded row
    // is kept, because the store did not yet know about the supersession then.
    let known_by_instant: BTreeSet<&str> = records
        .iter()
        .filter(|r| {
            record_transaction_time(r)
                .and_then(|tt| DateTime::parse_from_rfc3339(tt).ok())
                .is_some_and(|tt| tt <= tx_instant)
        })
        .map(GraphRecord::id)
        .collect();
    selected.retain(|r| {
        let GraphRecord::Node {
            superseded_by: Some(target),
            ..
        } = r
        else {
            return true;
        };
        if known_by_instant.contains(target.as_str()) {
            diagnostics.push(TxDiagnostic {
                code: "superseded".to_owned(),
                message: format!(
                    "record '{}' is superseded by '{target}', which is known by the instant; excluded",
                    r.id()
                ),
            });
            false
        } else {
            true
        }
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
