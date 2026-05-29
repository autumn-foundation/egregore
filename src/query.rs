//! Agent-facing graph query helpers.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use chrono::DateTime;

use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SemanticDriftMetadata};

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
    /// Code-graph records: `Symbol`, `File`, and topology edges linking them.
    ///
    /// Every record has a stable record ID and at least one of:
    /// `repo_relative_path`, `temporal.git_commit`, or `valid_time`.
    pub source_facts: Vec<&'a GraphRecord>,
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

    // Step 1: collect symbol record IDs, excluding any that have been tombstoned.
    let symbol_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                name,
                ..
            } = r
            else {
                return None;
            };
            if name.as_deref() == Some(symbol_name) && !tombstoned_ids.contains(id.as_str()) {
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

    // Build a lookup map: record_id → record for fast resolution.
    let by_id: std::collections::BTreeMap<&str, &GraphRecord> =
        records.iter().map(|r| (r.id(), r)).collect();

    // Step 2: the symbol nodes themselves are source_facts.
    let mut source_facts: BTreeSet<&str> = symbol_ids.clone();
    // Also include File nodes at the same repo_relative_path as any LIVE symbol
    // (tombstoned symbols are excluded via symbol_ids).
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
            // Tombstoned or non-matching symbol — skip its file co-location.
            continue;
        }
        // Find File nodes at this path.
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
            if file_path == path {
                source_facts.insert(file_id.as_str());
            }
        }
    }

    // Snapshot seed IDs (symbol IDs + co-located file IDs) before the main
    // loop. Used to detect links that target the symbol's context, including
    // file-scoped relations such as CommandRun --TOUCHED_FILE--> File.
    let seed_ids: BTreeSet<&str> = source_facts.iter().copied().collect();

    // Step 3a + 3b: classify linked records.
    let mut observations: BTreeSet<&str> = BTreeSet::new();
    let mut project_state: BTreeSet<&str> = BTreeSet::new();
    let mut artifacts: BTreeSet<&str> = BTreeSet::new();
    let mut verification_evidence: BTreeSet<&str> = BTreeSet::new();

    // Collect IDs of all records present in the slice for unresolved detection.
    let present_ids: BTreeSet<&str> = records.iter().map(GraphRecord::id).collect();
    let mut unresolved: Vec<UnresolvedRef> = Vec::new();

    // Helper: insert a record ID into the appropriate section.
    let classify_and_insert =
        |record_id: &'a str,
         source_facts: &mut BTreeSet<&'a str>,
         observations: &mut BTreeSet<&'a str>,
         project_state: &mut BTreeSet<&'a str>,
         artifacts: &mut BTreeSet<&'a str>,
         verification_evidence: &mut BTreeSet<&'a str>| {
            let Some(rec) = by_id.get(record_id) else {
                return;
            };
            let GraphRecord::Node { kind, .. } = rec else {
                return;
            };
            match classify_node(*kind) {
                Some(ContextSection::SourceFact) => {
                    source_facts.insert(record_id);
                }
                Some(ContextSection::Observation) => {
                    observations.insert(record_id);
                }
                Some(ContextSection::ProjectState) => {
                    project_state.insert(record_id);
                }
                Some(ContextSection::Artifact) => {
                    artifacts.insert(record_id);
                }
                Some(ContextSection::VerificationEvidence) => {
                    verification_evidence.insert(record_id);
                }
                None => {}
            }
        };

    for record in records {
        match record {
            GraphRecord::Edge {
                label,
                source,
                target,
                ..
            } => {
                if !is_cross_domain_label(*label) {
                    // Code-graph topology edges (CONTAINS, DEFINES, …) don't
                    // cross trust boundaries, so skip them.
                } else if seed_ids.contains(source.as_str()) {
                    // Edge outgoing from a seed (symbol or co-located file) → classify target.
                    classify_and_insert(
                        target.as_str(),
                        &mut source_facts,
                        &mut observations,
                        &mut project_state,
                        &mut artifacts,
                        &mut verification_evidence,
                    );
                } else if seed_ids.contains(target.as_str()) {
                    // Edge incoming to a seed → classify the source.
                    classify_and_insert(
                        source.as_str(),
                        &mut source_facts,
                        &mut observations,
                        &mut project_state,
                        &mut artifacts,
                        &mut verification_evidence,
                    );
                }
            }
            GraphRecord::Node {
                id: node_id,
                evidence_links: Some(links),
                ..
            } => {
                // Check whether any evidence link targets a seed ID (symbol or
                // co-located file). This handles file-scoped evidence such as
                // CommandRun --TOUCHED_FILE--> File(src/lib.rs).
                let links_to_symbol = links.iter().any(|link| {
                    link.target_record_id
                        .as_deref()
                        .is_some_and(|tid| seed_ids.contains(tid))
                });

                if links_to_symbol {
                    classify_and_insert(
                        node_id.as_str(),
                        &mut source_facts,
                        &mut observations,
                        &mut project_state,
                        &mut artifacts,
                        &mut verification_evidence,
                    );

                    // Step 4: scan evidence links on nodes that ARE linked to the
                    // symbol. Present non-symbol targets are backing evidence and get
                    // classified into their own section. Missing targets go to
                    // unresolved (AC5).
                    for link in links {
                        if let Some(target_id) = &link.target_record_id {
                            if present_ids.contains(target_id.as_str())
                                && !symbol_ids.contains(target_id.as_str())
                            {
                                // Backing record exists: classify it (e.g. a
                                // Verification that validates an Observation that
                                // mentions the symbol).
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
                                    source_record_id: node_id.clone(),
                                    target_handle: target_id.clone(),
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

    // Post-processing: scan evidence_links of nodes classified via the edge arm.
    // Those nodes are in the response but their evidence_links were never scanned
    // (the evidence_links arm only runs when a node's OWN links target the symbol).
    // This covers cases like Obs --edge--> Symbol where Obs also has a VALIDATED_BY
    // link to a Verification that needs to appear in verification_evidence.
    //
    // Collect IDs as owned Strings to release borrows on the BTreeSets before
    // calling classify_and_insert with mutable references to them.
    let classified_for_backfill: Vec<String> = source_facts
        .iter()
        .chain(observations.iter())
        .chain(project_state.iter())
        .chain(artifacts.iter())
        .chain(verification_evidence.iter())
        .filter(|id| !symbol_ids.contains(**id))
        .map(|id| (*id).to_owned())
        .collect();

    for node_id in &classified_for_backfill {
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
            }
        }
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
    let resolve = |ids: &BTreeSet<&str>| -> Vec<&'a GraphRecord> {
        let mut out: Vec<&'a GraphRecord> =
            ids.iter().filter_map(|id| by_id.get(id).copied()).collect();
        out.sort_by_key(|r| r.id());
        out
    };

    SymbolContext {
        symbol_name: symbol_name.to_owned(),
        source_facts: resolve(&source_facts),
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
            | EdgeLabel::ProducedPatch
            | EdgeLabel::ProducedEvidence
            | EdgeLabel::ClosesAcceptanceCriterion
            | EdgeLabel::RelatesTo
            | EdgeLabel::Contradicts
            | EdgeLabel::Supersedes
    )
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
