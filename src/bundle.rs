use crate::error::{CodegraphError, Result};
use crate::ir::{GraphRecord, SnapshotHead, NodeKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Manifest metadata for the evidence bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleManifest {
    /// Root selector string used to query the starting records.
    pub root_selector: String,
    /// Source query or audit workflow.
    pub source_query: String,
    /// Snapshot or transaction-time marker when available.
    pub snapshot: Option<SnapshotHead>,
    /// Repository identity stable ID.
    pub repository_identity: String,
    /// Egregore version.
    pub egregore_version: String,
    /// Count of included records grouped by domain/trust class.
    pub included_record_counts: BTreeMap<String, usize>,
    /// Count of omitted records.
    pub omitted_record_counts: usize,
    /// Stable IDs of the root records selected for export.
    pub root_record_ids: Vec<String>,
}

/// A wrapped record with its BLAKE3 hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleRecord {
    /// The scrubbed graph record.
    pub record: GraphRecord,
    /// BLAKE3 hex hash of the serialized record.
    pub hash: String,
}

/// An unresolved outgoing/incoming link found during traversal.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
pub struct UnresolvedLink {
    /// The ID of the record containing the link.
    pub source_id: String,
    /// The target handle or record ID that could not be resolved.
    pub target_handle: String,
    /// The relation label or field name of the link.
    pub relation: String,
}

/// The complete self-contained evidence bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceBundle {
    /// Bundle manifest.
    pub manifest: BundleManifest,
    /// Included canonically ordered records.
    pub records: Vec<BundleRecord>,
    /// Diagnostics for unresolved/missing links.
    pub unresolved_links: Vec<UnresolvedLink>,
}

/// Extracts all outgoing target IDs from a record.
pub fn collect_references(record: &GraphRecord) -> Vec<String> {
    let mut refs = Vec::new();
    match record {
        GraphRecord::Edge { source, target, .. } => {
            refs.push(source.clone());
            refs.push(target.clone());
        }
        GraphRecord::Node {
            superseded_by,
            parent_task_id,
            verification_link_id,
            source_external_link_id,
            linked_patch_id,
            linked_turn_id,
            produced_evidence_id,
            evidence_links,
            source_snapshot,
            user_context,
            ..
        } => {
            if let Some(id) = superseded_by {
                refs.push(id.clone());
            }
            if let Some(id) = parent_task_id {
                refs.push(id.clone());
            }
            if let Some(id) = verification_link_id {
                refs.push(id.clone());
            }
            if let Some(id) = source_external_link_id {
                refs.push(id.clone());
            }
            if let Some(id) = linked_patch_id {
                refs.push(id.clone());
            }
            if let Some(id) = linked_turn_id {
                refs.push(id.clone());
            }
            if let Some(id) = produced_evidence_id {
                refs.push(id.clone());
            }
            if let Some(id) = &user_context.approval_decision_id {
                refs.push(id.clone());
            }
            if let Some(id) = &user_context.materialized_record_id {
                refs.push(id.clone());
            }
            if let Some(links) = evidence_links {
                for link in links {
                    if let Some(id) = &link.target_record_id {
                        refs.push(id.clone());
                    }
                }
            }
            if let Some(snapshot) = source_snapshot {
                refs.push(snapshot.repository_id.clone());
            }
        }
        GraphRecord::Tombstone { deleted_id, .. } => {
            refs.push(deleted_id.clone());
        }
    }
    refs
}

fn find_root_records(records: &[GraphRecord], selector: &str) -> Result<Vec<GraphRecord>> {
    let Some((prefix, value)) = selector.split_once(':') else {
        return Err(CodegraphError::InvalidArgument {
            message: format!("invalid selector format: '{}', expected prefix:value", selector),
        });
    };

    let roots: Vec<GraphRecord> = records
        .iter()
        .filter(|rec| match prefix {
            "id" => rec.id() == value,
            "symbol" => {
                if let GraphRecord::Node { kind: NodeKind::Symbol, name: Some(n), .. } = rec {
                    n == value
                } else {
                    false
                }
            }
            "file" => {
                if let GraphRecord::Node { kind: NodeKind::File, repo_relative_path: Some(p), .. } = rec {
                    p == value
                } else {
                    false
                }
            }
            "task" => {
                if let GraphRecord::Node { kind, entity_id, id, .. } = rec {
                    kind.as_str() == "Task" && (entity_id.as_deref() == Some(value) || id == value)
                } else {
                    false
                }
            }
            "memory" => {
                if let GraphRecord::Node { kind, id, .. } = rec {
                    matches!(kind.as_str(), "Observation" | "Decision" | "Failure" | "Lesson") && id == value
                } else {
                    false
                }
            }
            _ => false,
        })
        .cloned()
        .collect();

    if roots.is_empty() {
        return Err(CodegraphError::InvalidArgument {
            message: format!("no records matched selector: '{}'", selector),
        });
    }

    Ok(roots)
}

/// Scrubs raw protected payloads and sensitive prose fields from a GraphRecord.
pub fn scrub_record(mut record: GraphRecord) -> GraphRecord {
    if let GraphRecord::Node {
        text,
        validation_summary,
        arguments_summary,
        arguments_handle,
        result_handle,
        stdout_handle,
        stderr_handle,
        patch_handle,
        body_handle,
        diff_hunk_handle,
        user_context,
        ..
    } = &mut record
    {
        // 1. Scrub Node prose/text fields
        *text = None;
        *validation_summary = None;
        *arguments_summary = None;

        // 2. Scrub inline handle content
        if let Some(h) = arguments_handle {
            h.inline = None;
        }
        if let Some(h) = result_handle {
            h.inline = None;
        }
        if let Some(h) = stdout_handle {
            h.inline = None;
        }
        if let Some(h) = stderr_handle {
            h.inline = None;
        }
        if let Some(h) = patch_handle {
            h.inline = None;
        }
        if let Some(h) = body_handle {
            h.inline = None;
        }
        if let Some(h) = diff_hunk_handle {
            h.inline = None;
        }

        // 3. Scrub user context fields
        user_context.proposed_rule_text = None;
        user_context.prompt_text = None;
        user_context.decision_rationale = None;
        user_context.edited_rule_text = None;
        user_context.rule_text = None;
        user_context.action_summary = None;
        user_context.constraint_text = None;
    }
    record
}

/// Exports an evidence bundle for a selected query result, task, memory record, etc.
pub fn export_bundle(
    records: &[GraphRecord],
    root_selector: &str,
    egregore_version: &str,
) -> Result<EvidenceBundle> {
    let roots = find_root_records(records, root_selector)?;
    let root_ids: Vec<String> = roots.iter().map(|r| r.id().to_owned()).collect();

    let source_query = match root_selector.split_once(':') {
        Some((prefix, _)) => prefix.to_owned(),
        None => "custom".to_owned(),
    };

    // Index all records by ID
    let mut records_by_id: HashMap<String, &GraphRecord> = HashMap::new();
    for rec in records {
        records_by_id.insert(rec.id().to_owned(), rec);
    }

    // Build adjacency list for undirected BFS
    let mut adj: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for rec in records {
        let id = rec.id();
        match rec {
            GraphRecord::Edge { source, target, label, .. } => {
                let label_str = label.as_str().to_owned();
                adj.entry(id.to_owned()).or_default().push((source.clone(), label_str.clone()));
                adj.entry(id.to_owned()).or_default().push((target.clone(), label_str.clone()));
                adj.entry(source.clone()).or_default().push((id.to_owned(), label_str.clone()));
                adj.entry(target.clone()).or_default().push((id.to_owned(), label_str.clone()));
            }
            GraphRecord::Node {
                superseded_by,
                parent_task_id,
                verification_link_id,
                source_external_link_id,
                linked_patch_id,
                linked_turn_id,
                produced_evidence_id,
                evidence_links,
                source_snapshot,
                user_context,
                ..
            } => {
                let mut add_ref = |target_id: &str, relation: &str| {
                    adj.entry(id.to_owned()).or_default().push((target_id.to_owned(), relation.to_owned()));
                    adj.entry(target_id.to_owned()).or_default().push((id.to_owned(), relation.to_owned()));
                };

                if let Some(target) = superseded_by {
                    add_ref(target, "superseded_by");
                }
                if let Some(target) = parent_task_id {
                    add_ref(target, "parent_task_id");
                }
                if let Some(target) = verification_link_id {
                    add_ref(target, "verification_link_id");
                }
                if let Some(target) = source_external_link_id {
                    add_ref(target, "source_external_link_id");
                }
                if let Some(target) = linked_patch_id {
                    add_ref(target, "linked_patch_id");
                }
                if let Some(target) = linked_turn_id {
                    add_ref(target, "linked_turn_id");
                }
                if let Some(target) = produced_evidence_id {
                    add_ref(target, "produced_evidence_id");
                }
                if let Some(target) = &user_context.approval_decision_id {
                    add_ref(target, "approval_decision_id");
                }
                if let Some(target) = &user_context.materialized_record_id {
                    add_ref(target, "materialized_record_id");
                }
                if let Some(links) = evidence_links {
                    for link in links {
                        if let Some(target) = &link.target_record_id {
                            add_ref(target, &link.relation);
                        }
                    }
                }
                if let Some(snapshot) = source_snapshot {
                    add_ref(&snapshot.repository_id, "repository_snapshot");
                }
            }
            GraphRecord::Tombstone { deleted_id, .. } => {
                adj.entry(id.to_owned()).or_default().push((deleted_id.clone(), "deleted_id".to_owned()));
                adj.entry(deleted_id.clone()).or_default().push((id.to_owned(), "deleted_id".to_owned()));
            }
        }
    }

    // BFS Traversal
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    for root_id in &root_ids {
        if records_by_id.contains_key(root_id) {
            visited.insert(root_id.clone());
            queue.push_back(root_id.clone());
        }
    }

    let mut included_records = Vec::new();
    let mut unresolved_links = Vec::new();

    while let Some(u) = queue.pop_front() {
        if let Some(rec) = records_by_id.get(&u) {
            included_records.push((*rec).clone());

            if let Some(neighbors) = adj.get(&u) {
                for (v, relation) in neighbors {
                    if records_by_id.contains_key(v) {
                        if visited.insert(v.clone()) {
                            queue.push_back(v.clone());
                        }
                    } else {
                        // Check if it's an outgoing reference from u
                        let is_outgoing = match rec {
                            GraphRecord::Edge { source, target, .. } => source == v || target == v,
                            GraphRecord::Node { .. } => collect_references(rec).contains(v),
                            GraphRecord::Tombstone { deleted_id, .. } => deleted_id == v,
                        };
                        if is_outgoing {
                            unresolved_links.push(UnresolvedLink {
                                source_id: u.clone(),
                                target_handle: v.clone(),
                                relation: relation.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    // Sort unresolved links canonically
    unresolved_links.sort();

    // Canonical ordering of included records by ID (and then by JSON serialization if IDs match)
    included_records.sort_by(|a, b| {
        let id_cmp = a.id().cmp(b.id());
        if id_cmp == std::cmp::Ordering::Equal {
            let a_json = serde_json::to_string(a).unwrap_or_default();
            let b_json = serde_json::to_string(b).unwrap_or_default();
            a_json.cmp(&b_json)
        } else {
            id_cmp
        }
    });

    // Find repository identity and snapshot from all records
    let mut repo_id = "unknown".to_owned();
    let mut snapshot = None;
    for rec in records {
        if let GraphRecord::Node { kind: NodeKind::Repository, id, source_snapshot, .. } = rec {
            repo_id = id.clone();
            if let Some(s) = source_snapshot {
                snapshot = Some(s.head.clone());
            }
            break;
        }
    }

    // Wrap and hash records
    let bundle_records: Vec<BundleRecord> = included_records
        .into_iter()
        .map(|r| {
            let scrubbed = scrub_record(r);
            let serialized = serde_json::to_string(&scrubbed).unwrap();
            let hash = blake3::hash(serialized.as_bytes()).to_hex().to_string();
            BundleRecord { record: scrubbed, hash }
        })
        .collect();

    // Validate coverage threshold
    let mut total_valid = 0;
    let mut non_code_valid = true;
    let total_records_count = bundle_records.len();

    for br in &bundle_records {
        let classified = crate::citation_audit::classify_record_external(&br.record);
        let trust_class = classified.trust_class;
        
        let is_valid = classified.status != crate::citation_audit::CitationStatus::MissingRequiredHandle;
        if is_valid {
            total_valid += 1;
        }

        if trust_class != "source_fact" {
            if !is_valid {
                non_code_valid = false;
            }
        }
    }

    let overall_coverage = if total_records_count > 0 {
        total_valid as f64 / total_records_count as f64
    } else {
        1.0
    };

    if overall_coverage < 0.95 {
        return Err(CodegraphError::BundleVerificationFailed {
            message: format!("below_coverage_threshold: overall coverage is {:.2}%, required 95%", overall_coverage * 100.0),
        });
    }

    if !non_code_valid {
        return Err(CodegraphError::BundleVerificationFailed {
            message: "below_coverage_threshold: non-code trust-class records must have 100% coverage".to_owned(),
        });
    }

    // Counts by domain/trust class
    let mut included_record_counts = BTreeMap::new();
    for br in &bundle_records {
        let tc = crate::citation_audit::citation_trust_class(&br.record).to_owned();
        *included_record_counts.entry(tc).or_insert(0) += 1;
    }

    let manifest = BundleManifest {
        root_selector: root_selector.to_owned(),
        source_query,
        snapshot,
        repository_identity: repo_id,
        egregore_version: egregore_version.to_owned(),
        included_record_counts,
        omitted_record_counts: 0,
        root_record_ids: root_ids,
    };

    Ok(EvidenceBundle {
        manifest,
        records: bundle_records,
        unresolved_links,
    })
}
