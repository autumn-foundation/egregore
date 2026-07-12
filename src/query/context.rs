use std::collections::BTreeSet;

use super::{
    evidence_link_triple_handle, is_bfs_relay_node, is_cross_domain_label, is_forward_only_label,
};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

/// An evidence link target that could not be resolved in the current store.
///
/// Surfaced in [`SymbolContext::unresolved`] instead of being silently dropped.
/// Per AC5 from issue #38.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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
pub(super) const fn classify_node(kind: NodeKind) -> Option<ContextSection> {
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
        | NodeKind::PR
        // Reviewer-identity join (issue #335): a Review row surfaces in the
        // project-state section, and the ExternalIdentity reached via
        // REVIEWED_BY / REQUESTED_REVIEW_FROM carries the participant login.
        | NodeKind::Review
        | NodeKind::ExternalIdentity
        // Review-state history (issue #336): a ReviewStateTransition is a
        // project-state fact — the append-only record of a review's dismissal or
        // request transition — so it surfaces alongside the Review it concerns.
        | NodeKind::ReviewStateTransition => Some(ContextSection::ProjectState),
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
pub(super) enum ContextSection {
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

    context_from_seeds(records, symbol_name, source_facts, &symbol_ids)
}

/// Resolves the trust-separated context sections from a frozen set of
/// source-fact seed IDs.
///
/// Shared core used by [`symbol_context`] (seeds collected by symbol *name*)
/// and [`record_context`] (seeds collected from a specific *record ID*, so a
/// File-typed match is first-class). `source_facts` is the seed set already
/// containing the anchor node(s) plus their co-located/defined neighbors;
/// `symbol_ids` is the set of *primary* query nodes (the thing the caller asked
/// about), used to keep them in the source-facts section and to avoid
/// re-scanning them during backfill. The bounded cross-domain BFS, backfill to
/// convergence, and per-section sort are identical regardless of how the seeds
/// were chosen, so both entry points share one implementation and one set of
/// determinism guarantees.
#[must_use]
#[allow(clippy::too_many_lines)]
fn context_from_seeds<'a>(
    records: &'a [GraphRecord],
    symbol_name: &str,
    source_facts: BTreeSet<&'a str>,
    symbol_ids: &BTreeSet<&'a str>,
) -> SymbolContext<'a> {
    // Recompute the prelim lookups the core needs. These are cheap O(n) scans
    // and are derived deterministically from `records`, so computing them here
    // (rather than threading them through the seeding step) keeps the seam
    // narrow without changing behavior.
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
    let mut source_facts = source_facts;

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
    for sid in symbol_ids {
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

/// Returns evidence-backed context anchored on a specific record ID, so a
/// File-typed semantic match (which has no symbol name) is first-class (#90).
///
/// Mirrors [`symbol_context`] but seeds from the record itself rather than from
/// a name:
/// - a `Symbol` anchor seeds the symbol plus its co-located `File` (via
///   DEFINES, falling back to the shared repo-relative path);
/// - a `File` anchor seeds the file plus the `Symbol`s it DEFINES;
/// - any other anchor seeds just itself.
///
/// Returns an empty [`SymbolContext`] (`is_no_match()` is `true`) when the
/// anchor is absent from the slice or is a tombstoned current-state record.
/// The bounded BFS, backfill, trust separation, and deterministic ordering are
/// shared with [`symbol_context`] via [`context_from_seeds`].
#[must_use]
pub fn record_context<'a>(records: &'a [GraphRecord], anchor_id: &str) -> SymbolContext<'a> {
    let tombstoned_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
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

    // A record id is live unless it is a tombstoned current-state record; a
    // historical (temporal) version survives the current-state tombstone.
    let is_live = |id: &str| has_any_temporal_version.contains(id) || !tombstoned_ids.contains(id);

    // Canonical `&'a str` for a record id, if present in the slice.
    let id_ref = |wanted: &str| -> Option<&'a str> {
        records
            .iter()
            .find_map(|r| if r.id() == wanted { Some(r.id()) } else { None })
    };

    // Node kind for an id (from any version present in the slice).
    let kind_of = |wanted: &str| -> Option<NodeKind> {
        records.iter().find_map(|r| match r {
            GraphRecord::Node { id, kind, .. } if id == wanted => Some(*kind),
            _ => None,
        })
    };

    // No-match for an anchor that is absent, not a node, or tombstoned in the
    // current state without a surviving historical version.
    let (Some(anchor_ref), Some(anchor_kind)) = (id_ref(anchor_id), kind_of(anchor_id)) else {
        return SymbolContext {
            symbol_name: anchor_id.to_owned(),
            ..Default::default()
        };
    };
    if !is_live(anchor_id) {
        return SymbolContext {
            symbol_name: anchor_id.to_owned(),
            ..Default::default()
        };
    }

    let mut source_facts: BTreeSet<&str> = BTreeSet::new();
    let mut primary: BTreeSet<&str> = BTreeSet::new();
    source_facts.insert(anchor_ref);
    primary.insert(anchor_ref);

    if anchor_kind == NodeKind::File {
        // File anchor: BFS over DEFINES, CONTAINS, and IMPORTS edges to seed
        // all code-graph nodes belonging to this file — top-level items
        // (File → DEFINES → Symbol), modules (File → CONTAINS → Module),
        // deeper nesting (Module → DEFINES → Symbol, ImplBlock → DEFINES →
        // Method), and imports (owner → IMPORTS → Import). Pure edge traversal
        // stays within the file's own tree so same-path nodes from other
        // repositories are never mixed in.
        let mut frontier: Vec<&str> = vec![anchor_ref];
        while !frontier.is_empty() {
            let mut next_frontier: Vec<&str> = Vec::new();
            for &container in &frontier {
                for r in records {
                    let GraphRecord::Edge {
                        id: edge_id,
                        label,
                        source,
                        target,
                        temporal,
                        ..
                    } = r
                    else {
                        continue;
                    };
                    if source.as_str() != container {
                        continue;
                    }
                    if !matches!(
                        label,
                        EdgeLabel::Defines | EdgeLabel::Contains | EdgeLabel::Imports
                    ) {
                        continue;
                    }
                    let edge_live =
                        temporal.is_some() || !tombstoned_ids.contains(edge_id.as_str());
                    if !edge_live || !is_live(target.as_str()) {
                        continue;
                    }
                    let target_kind = kind_of(target.as_str());
                    // Seed Symbol, Module, and Import nodes; skip File (already
                    // the anchor) and infrastructure kinds.
                    if !matches!(
                        target_kind,
                        Some(NodeKind::Symbol | NodeKind::Module | NodeKind::Import)
                    ) {
                        continue;
                    }
                    if let Some(t) = id_ref(target.as_str())
                        && !source_facts.contains(t)
                    {
                        source_facts.insert(t);
                        primary.insert(t);
                        // Symbol and Module can contain further items — keep
                        // them in the frontier to continue the traversal.
                        if matches!(target_kind, Some(NodeKind::Symbol | NodeKind::Module)) {
                            next_frontier.push(t);
                        }
                    }
                }
            }
            frontier = next_frontier;
        }
    } else {
        // Symbol (or other) anchor: find the co-located File by traversing
        // upward through DEFINES and CONTAINS edges. Handles both top-level
        // items (File → DEFINES → Symbol) and nested items
        // (File → CONTAINS → Module → DEFINES → Symbol and
        //  File → DEFINES → ImplBlock → DEFINES → Method). No path-only
        // fallback is used, so same-path files from other repositories cannot
        // bleed into this match's source_facts.
        let mut to_search: Vec<&str> = vec![anchor_ref];
        let mut visited_up: BTreeSet<&str> = BTreeSet::new();
        visited_up.insert(anchor_ref);
        while !to_search.is_empty() {
            let mut next: Vec<&str> = Vec::new();
            for &target_id in &to_search {
                for r in records {
                    let GraphRecord::Edge {
                        id: edge_id,
                        label,
                        source,
                        target,
                        temporal,
                        ..
                    } = r
                    else {
                        continue;
                    };
                    if target.as_str() != target_id {
                        continue;
                    }
                    if !matches!(label, EdgeLabel::Defines | EdgeLabel::Contains) {
                        continue;
                    }
                    let edge_live =
                        temporal.is_some() || !tombstoned_ids.contains(edge_id.as_str());
                    if !edge_live || !is_live(source.as_str()) {
                        continue;
                    }
                    if kind_of(source.as_str()) == Some(NodeKind::File) {
                        if let Some(file) = id_ref(source.as_str()) {
                            source_facts.insert(file);
                        }
                    } else if let Some(container) = id_ref(source.as_str())
                        && !visited_up.contains(container)
                    {
                        visited_up.insert(container);
                        next.push(container);
                    }
                }
            }
            to_search = next;
        }
    }

    let label = records
        .iter()
        .find_map(|r| match r {
            GraphRecord::Node {
                id, name: Some(n), ..
            } if id == anchor_id => Some(n.clone()),
            _ => None,
        })
        .unwrap_or_else(|| anchor_id.to_owned());

    context_from_seeds(records, &label, source_facts, &primary)
}

// ── semantic → context bridge (issue #90) ──────────────────────────────────

#[cfg(test)]
mod reviewer_identity_tests {
    use super::*;
    use crate::ir::{EvidenceLink, PROJECT_SCHEMA_VERSION};

    fn sym(name: &str) -> GraphRecord {
        GraphRecord::node(
            format!("codegraph:v5:{name}"),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn project_node(id: &str, kind: NodeKind, login: Option<&str>) -> GraphRecord {
        let mut rec = GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            format!("{} node", kind.as_str()),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            author,
            identity_system,
            ..
        } = &mut rec
        {
            *schema_version = PROJECT_SCHEMA_VERSION;
            *domain = Some("project".to_owned());
            if kind == NodeKind::ExternalIdentity {
                *author = login.map(str::to_owned);
                *identity_system = Some("github".to_owned());
            }
        }
        rec
    }

    fn id_for(login: &str) -> String {
        crate::github::records::external_identity_id("github", login)
    }

    fn ev_link_to(target: &str) -> EvidenceLink {
        EvidenceLink {
            target_record_id: Some(target.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }
    }

    #[test]
    fn symbol_context_surfaces_reviewer_identity_via_reviewed_by_and_requested_review_from() {
        // Issue #335: a symbol's project-state context includes the reviewer
        // identities reachable via REVIEWED_BY (from a Review that cites the
        // symbol) and REQUESTED_REVIEW_FROM (from a Task that mentions it).
        let foo = sym("foo");
        let foo_id = foo.id().to_owned();

        // Task that mentions the symbol; requests review from "alice".
        let task = project_node("project:v1:task", NodeKind::Task, None);
        let id_alice = project_node(&id_for("alice"), NodeKind::ExternalIdentity, Some("alice"));
        let mentions = GraphRecord::project_edge(
            EdgeLabel::MentionsSymbol,
            task.id().to_owned(),
            foo_id.clone(),
            Some("1.0".to_owned()),
            "task mentions foo".to_owned(),
        );
        let requested = GraphRecord::project_edge(
            EdgeLabel::RequestedReviewFrom,
            task.id().to_owned(),
            id_alice.id().to_owned(),
            None,
            "requested review".to_owned(),
        );

        // Review that cites the symbol via evidence_link; authored by "bob".
        let mut review = project_node("project:v1:review", NodeKind::Review, None);
        review = review.with_evidence_links(vec![ev_link_to(&foo_id)]);
        let id_bob = project_node(&id_for("bob"), NodeKind::ExternalIdentity, Some("bob"));
        let reviewed = GraphRecord::project_edge(
            EdgeLabel::ReviewedBy,
            review.id().to_owned(),
            id_bob.id().to_owned(),
            None,
            "review by bob".to_owned(),
        );

        let records = vec![
            foo, task, id_alice, mentions, requested, review, id_bob, reviewed,
        ];
        let ctx = symbol_context(&records, "foo");
        let project_ids: Vec<&str> = ctx.project_state.iter().map(|r| r.id()).collect();

        assert!(
            project_ids.contains(&id_for("alice").as_str()),
            "requested-reviewer identity must surface: {project_ids:?}"
        );
        assert!(
            project_ids.contains(&id_for("bob").as_str()),
            "review-author identity must surface: {project_ids:?}"
        );

        // Byte-stable across repeated runs.
        for _ in 0..5 {
            let again = symbol_context(&records, "foo");
            let ids: Vec<&str> = again.project_state.iter().map(|r| r.id()).collect();
            assert_eq!(ids, project_ids, "project_state must be byte-stable");
        }
    }
}
