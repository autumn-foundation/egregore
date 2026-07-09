use std::collections::BTreeSet;

use super::{
    ContextSection, UnresolvedRef, classify_node, evidence_link_triple_handle, is_bfs_relay_node,
    is_cross_domain_label, is_forward_only_label,
};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

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

    // Source-link records (ExternalLink nodes, EXTERNAL_HANDLE edges) that were
    // tombstoned must not resolve their task on current-state reads: a retracted
    // external handle is stale, not a live handle.
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();

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

            if matches && !tombstoned.contains(id.as_str()) {
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
            id: edge_id,
            label: EdgeLabel::ExternalHandle,
            source,
            target,
            ..
        } = r
            && matched_links.contains(target)
            && !tombstoned.contains(edge_id.as_str())
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

    // Tombstoned (deleted) tasks are not part of the current state: drop them
    // before reporting ambiguity so a re-created/re-imported task sharing a
    // handle with an older deleted one resolves the live task instead of failing
    // `Ambiguous`.
    matched_ids.retain(|id| !tombstoned.contains(id.as_str()));

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
