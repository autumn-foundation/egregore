use std::collections::{BTreeMap, BTreeSet};

use super::{
    ContextSection, UnresolvedRef, classify_node, evidence_link_triple_handle, is_bfs_relay_node,
    is_cross_domain_label, is_forward_only_label, resolve_drift_target, semantic_drift,
};
use crate::ir::{
    EdgeLabel, ErrorSignaturePayload, FrameResolution, GraphRecord, LogPayload, NodeKind,
    SourceSpan,
};

/// Why a subsystem prefix was rejected.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SubsystemPrefixError {
    /// The prefix is empty or reduces to nothing after stripping trailing slashes.
    Malformed {
        /// The prefix as supplied by the caller.
        prefix: String,
    },
}

impl SubsystemPrefixError {
    /// Stable machine-readable diagnostic code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Malformed { .. } => "malformed_prefix",
        }
    }
}

/// Returns `true` when `path` is the prefix itself or lies strictly under it
/// on a path-segment boundary.
///
/// Both the bare form (`src/alpha`) and the trailing-slash form (`src/alpha/`)
/// resolve identically. Segment awareness prevents sibling-path bleed:
/// `src/alpha` matches `src/alpha/foo.rs` but never `src/alphabet/x.rs`.
#[must_use]
pub fn path_is_under_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    match path.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// One in-prefix resolved backtrace frame carried on a subsystem log-signature
/// row (issue #325).
///
/// A frame is in-prefix iff its `FRAME_RESOLVES_TO` edge (issue #322) carries a
/// `resolved` or `path_only` resolution and its code-graph target's
/// `repo_relative_path` passes the segment-aware [`path_is_under_prefix`]
/// matcher. Frames are non-identity leads: a binding proves the frame *names*
/// the symbol, never that the symbol is at fault.
#[derive(Debug, Clone)]
pub struct SubsystemLogFrame<'a> {
    /// Zero-based backtrace frame index the resolution applies to.
    pub frame_index: u32,
    /// Closed resolution class: `resolved` or `path_only`.
    pub frame_resolution: &'static str,
    /// Repo-relative path of the resolved code-graph target (Symbol or File).
    pub target_repo_relative_path: &'a str,
    /// Span of the resolved target when it is a `Symbol` (absent for a `File`).
    pub target_span: Option<SourceSpan>,
}

/// One runtime `ErrorSignature` whose frames resolve under the prefix
/// (issue #325).
///
/// Rows are runtime observations — a producing program's own claim, deterministically
/// parsed but never verified: **leads, not proof** the subsystem is unhealthy.
/// `occurrence_count` reflects the scanned log sources only, not all runtime
/// reality. Records sharing a stable ID across scan sources are coalesced
/// (earliest `first_seen`, latest `last_seen`, summed `occurrence_count`).
#[derive(Debug, Clone)]
pub struct SubsystemLogSignature<'a> {
    /// Stable `ErrorSignature` record ID.
    pub record_id: &'a str,
    /// Schema version stamped on the signature record.
    pub schema_version: u32,
    /// Closed severity class: `fatal`, `error`, or `warn`.
    pub severity: &'a str,
    /// Total occurrences summed across the coalesced scan sources.
    pub occurrence_count: u64,
    /// Bounded, post-redaction template excerpt (never raw log text).
    pub template_excerpt: &'a str,
    /// Valid time of the earliest occurrence (empty when unrecorded).
    pub first_seen: &'a str,
    /// Valid time of the latest occurrence (empty when unrecorded).
    pub last_seen: &'a str,
    /// In-prefix resolved frames, sorted deterministically.
    pub in_prefix_frames: Vec<SubsystemLogFrame<'a>>,
}

/// Evidence-backed subsystem context returned by [`subsystem_context`].
///
/// Sections mirror [`SymbolContext`] but the entry point is a repo-relative
/// directory / module prefix rather than a symbol name. The `semantic_drift`
/// section is added because path-scoped subsystem triage benefits from knowing
/// which symbols under the prefix have drifted (unlike symbol context, which
/// omits drift as out-of-scope).
///
/// An [`Observation`] node MUST NOT appear in `source_facts`.
///
/// [`Observation`]: crate::ir::NodeKind::Observation
#[derive(Debug, Default, Clone)]
pub struct SubsystemContext<'a> {
    /// The queried path prefix (normalized: trailing slash stripped).
    pub prefix: String,
    /// Code-graph records: `File` and `Symbol` nodes under the prefix.
    pub source_facts: Vec<&'a GraphRecord>,
    /// Codegraph topology edges between nodes in `source_facts`.
    pub topology_edges: Vec<&'a GraphRecord>,
    /// Agent-authored `Observation` nodes linked to source facts.
    pub observations: Vec<&'a GraphRecord>,
    /// `Task` and `AcceptanceCriterion` nodes linked to source facts.
    pub project_state: Vec<&'a GraphRecord>,
    /// `Artifact` and `PatchArtifact` nodes linked to source facts.
    pub artifacts: Vec<&'a GraphRecord>,
    /// `Verification`, `TestRun`, `CommandRun` nodes linked to source facts.
    pub verification_evidence: Vec<&'a GraphRecord>,
    /// `SemanticDrift` nodes whose resolved target path is under the prefix.
    pub semantic_drift: Vec<&'a GraphRecord>,
    /// Runtime `ErrorSignature` records whose frames resolve under the prefix
    /// (issue #325). Always present (empty when no scanned source resolves here);
    /// an empty section means "no scanned source resolved here", not "no errors
    /// exist".
    pub log_signatures: Vec<SubsystemLogSignature<'a>>,
    /// Evidence link targets referenced by agent-memory nodes that are absent
    /// from this store slice.
    pub unresolved: Vec<UnresolvedRef>,
}

impl SubsystemContext<'_> {
    /// Returns `true` when no records exist under the queried prefix.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.source_facts.is_empty()
            && self.observations.is_empty()
            && self.project_state.is_empty()
            && self.artifacts.is_empty()
            && self.verification_evidence.is_empty()
            && self.semantic_drift.is_empty()
            && self.log_signatures.is_empty()
            && self.unresolved.is_empty()
    }
}

/// Returns all known cross-domain context for a repo-relative path prefix.
///
/// # Errors
///
/// Returns [`SubsystemPrefixError::Malformed`] when the prefix is empty or
/// reduces to nothing after stripping trailing slashes.
///
/// # Algorithm
///
/// 1. Validate and normalize the prefix.
/// 2. Collect seed `File` and `Symbol` nodes under the prefix.
/// 3. Run the same bounded-BFS + backfill traversal as `symbol_context` to
///    discover cross-domain linked records, classified by trust section.
/// 4. Separately collect `SemanticDrift` nodes whose resolved target path lies
///    under the prefix via `resolve_drift_target`.
///
/// Output ordering is deterministic: sorted by record ID within each section.
#[allow(clippy::too_many_lines)]
pub fn subsystem_context<'a>(
    records: &'a [GraphRecord],
    prefix: &str,
) -> Result<SubsystemContext<'a>, SubsystemPrefixError> {
    let normalized = prefix.trim_end_matches('/');
    if normalized.is_empty() {
        return Err(SubsystemPrefixError::Malformed {
            prefix: prefix.to_owned(),
        });
    }

    // Step 0: tombstone set (mirrors symbol_context).
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

    // Build fast lookup map and temporal-version index.
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

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

    // Step 1: collect File and Symbol node IDs under the prefix.
    let seed_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                kind,
                repo_relative_path: Some(path),
                ..
            } = r
            else {
                return None;
            };
            if !matches!(kind, NodeKind::File | NodeKind::Symbol) {
                return None;
            }
            let is_historical = matches!(
                r,
                GraphRecord::Node {
                    temporal: Some(_),
                    ..
                }
            );
            if !is_historical && tombstoned_ids.contains(id.as_str()) {
                return None;
            }
            if path_is_under_prefix(path.as_str(), normalized) {
                Some(id.as_str())
            } else {
                None
            }
        })
        .collect();

    if seed_ids.is_empty() {
        return Ok(SubsystemContext {
            prefix: normalized.to_owned(),
            ..Default::default()
        });
    }

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

    // Step 1b: collect topology edges between seed nodes.
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
            && (has_any_temporal_version.contains(id.as_str())
                || !tombstoned_ids.contains(id.as_str()))
            && seed_ids.contains(source.as_str())
            && seed_ids.contains(target.as_str())
        {
            topology_edge_ids.insert(id.as_str());
        }
    }

    // Steps 2+: bounded BFS + backfill — same logic as symbol_context.
    let mut source_facts: BTreeSet<&str> = seed_ids.clone();
    let mut observations: BTreeSet<&str> = BTreeSet::new();
    let mut project_state: BTreeSet<&str> = BTreeSet::new();
    let mut artifacts: BTreeSet<&str> = BTreeSet::new();
    let mut verification_evidence: BTreeSet<&str> = BTreeSet::new();

    let present_ids: BTreeSet<&str> = records.iter().map(GraphRecord::id).collect();
    let mut unresolved: Vec<UnresolvedRef> = Vec::new();

    let classify_and_insert = |record_id: &'a str,
                               source_facts: &mut BTreeSet<&'a str>,
                               observations: &mut BTreeSet<&'a str>,
                               project_state: &mut BTreeSet<&'a str>,
                               artifacts: &mut BTreeSet<&'a str>,
                               verification_evidence: &mut BTreeSet<&'a str>|
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

    let mut visited: BTreeSet<&str> = seed_ids.clone();
    let mut frontier: BTreeSet<&str> = seed_ids.clone();
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
                        || visited.contains(node_id.as_str()),
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
                        let has_triple = links.iter().any(|link| {
                            link.target_record_id.is_none()
                                && evidence_link_triple_handle(link).is_some()
                        });
                        if has_triple {
                            for link in links {
                                if link.target_record_id.is_none() {
                                    let Some(handle) = evidence_link_triple_handle(link) else {
                                        continue;
                                    };
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
            break;
        }
        frontier = next_frontier.into_iter().collect();
    }

    // Backfill: scan evidence_links of classified nodes.
    let mut backfill_scanned: BTreeSet<String> = seed_ids.iter().map(ToString::to_string).collect();

    loop {
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
                        && !seed_ids.contains(target_id.as_str())
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

    // Frontier expansion for backfill discoveries.
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

    // Remove seed node IDs from non-source-fact sections.
    for sid in &seed_ids {
        observations.remove(sid);
        project_state.remove(sid);
        artifacts.remove(sid);
        verification_evidence.remove(sid);
    }

    // Resolve ID sets → sorted record slices.
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

    // Semantic drift: every SemanticDrift node whose resolved target path is under the prefix.
    let semantic_drift: Vec<&'a GraphRecord> = {
        let mut drift_records: Vec<&'a GraphRecord> = records
            .iter()
            .filter_map(|r| {
                let drift_meta = semantic_drift(r)?;
                // Skip tombstoned non-temporal drift nodes.
                let is_temporal = matches!(
                    r,
                    GraphRecord::Node {
                        temporal: Some(_),
                        ..
                    }
                );
                if !is_temporal && tombstoned_ids.contains(r.id()) {
                    return None;
                }
                let (resolved_path, _, _) =
                    resolve_drift_target(records, r.id(), drift_meta, None, None);
                let path = resolved_path?;
                if path_is_under_prefix(path, normalized) {
                    Some(r)
                } else {
                    None
                }
            })
            .collect();
        drift_records.sort_by_key(|r| r.id());
        drift_records
    };

    // Runtime log signatures (issue #325): ErrorSignatures whose FRAME_RESOLVES_TO
    // frames land under the prefix. Unresolved/ambiguous-only signatures never
    // enter this section; their in-prefix dangling targets are routed into the
    // existing `unresolved` collection so they are never silently dropped.
    let (log_signatures, extra_unresolved) =
        collect_log_signatures(records, &by_id, &tombstoned_ids, normalized);
    unresolved.extend(extra_unresolved);

    Ok(SubsystemContext {
        prefix: normalized.to_owned(),
        source_facts: resolve(&source_facts),
        topology_edges: {
            let mut out: Vec<&'a GraphRecord> = records
                .iter()
                .filter(|r| {
                    topology_edge_ids.contains(r.id())
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
        semantic_drift,
        log_signatures,
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
    })
}

/// One `FRAME_RESOLVES_TO` edge (issue #322), read from the EDGE record so the
/// resolution class comes from `frame_resolution`/`frame_index`, never the
/// node-side `EvidenceLink` mirror (which omits the resolution enum).
struct FrameEdge<'a> {
    frame_index: u32,
    resolution: FrameResolution,
    target_id: &'a str,
}

/// The `domain:` prefix of a stable record ID (`log:v1:x` -> `log`).
fn domain_prefix(id: &str) -> &str {
    id.split_once(':').map_or(id, |(d, _)| d)
}

/// Collects the `log_signatures` section (issue #325) and the in-prefix dangling
/// frame targets that must surface through the `unresolved` section (AC4).
///
/// A signature enters `log_signatures` iff at least one of its
/// `FRAME_RESOLVES_TO` edges carries a `resolved` or `path_only` resolution whose
/// code-graph target's `repo_relative_path` passes [`path_is_under_prefix`].
/// Signatures whose only in-prefix frames are `ambiguous`/`unresolved` never
/// enter the section; each such dangling target is returned as an
/// [`UnresolvedRef`] so it is never silently dropped. Records sharing a stable ID
/// across scan sources are coalesced (earliest `first_seen`, latest `last_seen`,
/// summed `occurrence_count`) before selection, so exactly one row per signature
/// ID is emitted. Log records are non-temporal, so a tombstoned signature is
/// dropped by the simple `tombstoned_ids` membership test.
fn collect_log_signatures<'a>(
    records: &'a [GraphRecord],
    by_id: &BTreeMap<&'a str, &'a GraphRecord>,
    tombstoned_ids: &BTreeSet<&str>,
    prefix: &str,
) -> (Vec<SubsystemLogSignature<'a>>, Vec<UnresolvedRef>) {
    // Step 1: read FRAME_RESOLVES_TO edges, grouped by signature id. The
    // resolution and frame index come from the edge fields; edges missing either
    // (a base edge without the #322 builders) are skipped.
    let mut frames_by_sig: BTreeMap<&str, Vec<FrameEdge<'a>>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            label: EdgeLabel::FrameResolvesTo,
            source,
            target,
            frame_resolution: Some(resolution),
            frame_index: Some(frame_index),
            ..
        } = r
        {
            frames_by_sig
                .entry(source.as_str())
                .or_default()
                .push(FrameEdge {
                    frame_index: *frame_index,
                    resolution: *resolution,
                    target_id: target.as_str(),
                });
        }
    }

    // Step 2: coalesce ErrorSignature node payloads by stable ID. `LogSource` and
    // `frames` are non-identity, so a graph combining multiple scan-logs outputs
    // carries the same signature ID more than once; group and merge before
    // selecting so exactly one row per ID is emitted.
    let mut sig_groups: BTreeMap<&str, Vec<(u32, &'a ErrorSignaturePayload)>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::ErrorSignature,
            schema_version,
            log: Some(payload),
            ..
        } = r
            && let LogPayload::ErrorSignature(sig) = payload.as_ref()
        {
            sig_groups
                .entry(id.as_str())
                .or_default()
                .push((*schema_version, sig));
        }
    }

    let mut log_signatures: Vec<SubsystemLogSignature<'a>> = Vec::new();
    let mut extra_unresolved: Vec<UnresolvedRef> = Vec::new();

    // BTreeMap iteration is sorted by record ID; every merge below is
    // order-independent, so output is byte-stable.
    for (sig_id, group) in &sig_groups {
        // Log records are non-temporal: a tombstoned signature is simply dropped.
        if tombstoned_ids.contains(*sig_id) {
            continue;
        }

        let mut in_prefix_frames: Vec<SubsystemLogFrame<'a>> = Vec::new();
        // Dangling (target_id, target_domain) for in-prefix ambiguous/unresolved
        // frames, surfaced only when the signature is NOT otherwise selected.
        let mut dangling: Vec<(&'a str, &'a str)> = Vec::new();

        if let Some(frames) = frames_by_sig.get(*sig_id) {
            for fr in frames {
                let target = by_id.get(fr.target_id).copied();
                let target_path = target.and_then(|r| match r {
                    GraphRecord::Node {
                        repo_relative_path: Some(p),
                        ..
                    } => Some(p.as_str()),
                    _ => None,
                });
                let Some(path) = target_path else { continue };
                if !path_is_under_prefix(path, prefix) {
                    continue;
                }
                match fr.resolution {
                    FrameResolution::Resolved | FrameResolution::PathOnly => {
                        let span = target.and_then(|r| match r {
                            GraphRecord::Node { span, .. } => *span,
                            _ => None,
                        });
                        in_prefix_frames.push(SubsystemLogFrame {
                            frame_index: fr.frame_index,
                            frame_resolution: fr.resolution.as_str(),
                            target_repo_relative_path: path,
                            target_span: span,
                        });
                    }
                    FrameResolution::Ambiguous | FrameResolution::Unresolved => {
                        dangling.push((fr.target_id, domain_prefix(fr.target_id)));
                    }
                }
            }
        }

        if in_prefix_frames.is_empty() {
            // AC4: an unresolved/ambiguous-only signature never enters the
            // section; its in-prefix dangling targets surface through `unresolved`.
            for (target_id, domain) in dangling {
                extra_unresolved.push(UnresolvedRef {
                    source_record_id: (*sig_id).to_owned(),
                    target_handle: target_id.to_owned(),
                    relation: EdgeLabel::FrameResolvesTo.as_str().to_owned(),
                    target_domain: domain.to_owned(),
                });
            }
            continue;
        }

        in_prefix_frames.sort_by(|a, b| {
            a.frame_index
                .cmp(&b.frame_index)
                .then_with(|| a.frame_resolution.cmp(b.frame_resolution))
                .then_with(|| a.target_repo_relative_path.cmp(b.target_repo_relative_path))
        });
        in_prefix_frames.dedup_by(|a, b| {
            a.frame_index == b.frame_index
                && a.frame_resolution == b.frame_resolution
                && a.target_repo_relative_path == b.target_repo_relative_path
        });

        // Coalesced payload fields. `severity`, `schema_version`, and the
        // template excerpt are identity-derived (identical within the group);
        // `occurrence_count` sums across scanned sources. `first_seen`/`last_seen`
        // are Z-normalized RFC 3339 from scan-logs, so lexical min/max equals
        // instant order.
        let occurrence_count: u64 = group.iter().map(|(_, s)| s.occurrence_count).sum();
        let first_seen = group
            .iter()
            .map(|(_, s)| s.first_seen.as_str())
            .min()
            .unwrap_or_default();
        let last_seen = group
            .iter()
            .map(|(_, s)| s.last_seen.as_str())
            .max()
            .unwrap_or_default();
        let (schema_version, sig) = (group[0].0, group[0].1);

        log_signatures.push(SubsystemLogSignature {
            record_id: sig_id,
            schema_version,
            severity: sig.severity.as_str(),
            occurrence_count,
            template_excerpt: sig.template_excerpt.as_str(),
            first_seen,
            last_seen,
            in_prefix_frames,
        });
    }

    log_signatures.sort_by(|a, b| a.record_id.cmp(b.record_id));
    (log_signatures, extra_unresolved)
}
