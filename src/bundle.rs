use crate::error::{CodegraphError, Result};
use crate::ir::{GraphRecord, NodeKind, SnapshotHead};
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

/// Verdict of one bundle verification check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationVerdict {
    /// True when the check passes.
    pub passed: bool,
    /// Detailed diagnostic message.
    pub detail: String,
}

/// Verification report returned by `verify_bundle`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    /// True if all three verdicts pass.
    pub ok: bool,
    /// Integrity check verdict.
    pub integrity: VerificationVerdict,
    /// Citation coverage check verdict.
    pub coverage: VerificationVerdict,
    /// Safety check verdict.
    pub safety: VerificationVerdict,
}

/// Extracts all outgoing target IDs from a record.
#[must_use]
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
            message: format!("invalid selector format: '{selector}', expected prefix:value"),
        });
    };

    let roots: Vec<GraphRecord> = records
        .iter()
        .filter(|rec| match prefix {
            "id" => rec.id() == value,
            "symbol" => {
                if let GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name: Some(n),
                    ..
                } = rec
                {
                    n == value
                } else {
                    false
                }
            }
            "file" => {
                if let GraphRecord::Node {
                    kind: NodeKind::File,
                    repo_relative_path: Some(p),
                    ..
                } = rec
                {
                    p == value
                } else {
                    false
                }
            }
            "task" => {
                if let GraphRecord::Node {
                    kind,
                    entity_id,
                    id,
                    ..
                } = rec
                {
                    kind.as_str() == "Task" && (entity_id.as_deref() == Some(value) || id == value)
                } else {
                    false
                }
            }
            "memory" => {
                if let GraphRecord::Node { kind, id, .. } = rec {
                    matches!(
                        kind.as_str(),
                        "Observation" | "Decision" | "Failure" | "Lesson"
                    ) && id == value
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
            message: format!("no records matched selector: '{selector}'"),
        });
    }

    Ok(roots)
}

/// Scrubs raw protected payloads and sensitive prose fields from a `GraphRecord`.
#[must_use]
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
        author_email,
        author_name,
        title,
        summary,
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

        // 4. Redact author email
        if let Some(email) = author_email {
            let hash = blake3::hash(email.as_bytes());
            let hex = hash.to_hex();
            let prefix = &hex.as_str()[..12];
            *author_email = Some(format!("<REDACTED:email:{prefix}>"));
        }

        // 5. Redact other preserved text/metadata fields
        if let Some(t) = title {
            *t = crate::redaction::redact_value(t);
        }
        if let Some(name) = author_name {
            *name = crate::redaction::redact_value(name);
        }
        *summary = crate::redaction::redact_value(summary);
    }

    // Also scrub Edge summaries
    if let GraphRecord::Edge { summary, .. } = &mut record {
        *summary = crate::redaction::redact_value(summary);
    }

    record
}

/// Returns the name of the first field that [`scrub_record`] clears to `None`
/// but which is still populated in `record`, or `None` when every such field is
/// cleared.
///
/// This is the shared Safety predicate for offline scrub verification. It MUST
/// stay in lockstep with [`scrub_record`]: every field that function nulls —
/// the top-level prose (`text`, `validation_summary`, `arguments_summary`), the
/// inline handle payloads, and the nested `user_context` prose fields — is
/// asserted here. Any new field added to [`scrub_record`]'s clearing set must be
/// added here too. Redaction transforms (`author_email`, `title`,
/// `author_name`, `summary`) are not cleared to `None`, so they are not checked
/// here; unredacted secrets are covered separately by the secret scan.
///
/// The returned name is redaction-safe: it is a static field label, never a
/// record value.
#[must_use]
pub fn first_unscrubbed_field(record: &GraphRecord) -> Option<&'static str> {
    let GraphRecord::Node {
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
    } = record
    else {
        return None;
    };

    // 1. Top-level prose fields cleared by scrub_record.
    if text.is_some() {
        return Some("text");
    }
    if validation_summary.is_some() {
        return Some("validation_summary");
    }
    if arguments_summary.is_some() {
        return Some("arguments_summary");
    }

    // 2. Inline handle payloads nulled by scrub_record.
    if arguments_handle
        .as_ref()
        .is_some_and(|h| h.inline.is_some())
    {
        return Some("arguments_handle.inline");
    }
    if result_handle.as_ref().is_some_and(|h| h.inline.is_some()) {
        return Some("result_handle.inline");
    }
    if stdout_handle.as_ref().is_some_and(|h| h.inline.is_some()) {
        return Some("stdout_handle.inline");
    }
    if stderr_handle.as_ref().is_some_and(|h| h.inline.is_some()) {
        return Some("stderr_handle.inline");
    }
    if patch_handle.as_ref().is_some_and(|h| h.inline.is_some()) {
        return Some("patch_handle.inline");
    }
    if body_handle.as_ref().is_some_and(|h| h.inline.is_some()) {
        return Some("body_handle.inline");
    }
    if diff_hunk_handle
        .as_ref()
        .is_some_and(|h| h.inline.is_some())
    {
        return Some("diff_hunk_handle.inline");
    }

    // 3. Nested user_context prose fields cleared by scrub_record.
    if user_context.proposed_rule_text.is_some() {
        return Some("user_context.proposed_rule_text");
    }
    if user_context.prompt_text.is_some() {
        return Some("user_context.prompt_text");
    }
    if user_context.decision_rationale.is_some() {
        return Some("user_context.decision_rationale");
    }
    if user_context.edited_rule_text.is_some() {
        return Some("user_context.edited_rule_text");
    }
    if user_context.rule_text.is_some() {
        return Some("user_context.rule_text");
    }
    if user_context.action_summary.is_some() {
        return Some("user_context.action_summary");
    }
    if user_context.constraint_text.is_some() {
        return Some("user_context.constraint_text");
    }

    None
}

/// Exports an evidence bundle for a selected query result, task, memory record, etc.
///
/// # Errors
///
/// Returns an error if the root records cannot be resolved, if citation threshold requirements are not met,
/// or if serialization fails.
#[allow(clippy::too_many_lines, clippy::missing_panics_doc)]
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
            GraphRecord::Edge {
                source,
                target,
                label,
                ..
            } => {
                let label_str = label.as_str().to_owned();
                adj.entry(id.to_owned())
                    .or_default()
                    .push((source.clone(), label_str.clone()));
                adj.entry(id.to_owned())
                    .or_default()
                    .push((target.clone(), label_str.clone()));
                adj.entry(source.clone())
                    .or_default()
                    .push((id.to_owned(), label_str.clone()));
                adj.entry(target.clone())
                    .or_default()
                    .push((id.to_owned(), label_str.clone()));
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
                    adj.entry(id.to_owned())
                        .or_default()
                        .push((target_id.to_owned(), relation.to_owned()));
                    adj.entry(target_id.to_owned())
                        .or_default()
                        .push((id.to_owned(), relation.to_owned()));
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
                adj.entry(id.to_owned())
                    .or_default()
                    .push((deleted_id.clone(), "deleted_id".to_owned()));
                adj.entry(deleted_id.clone())
                    .or_default()
                    .push((id.to_owned(), "deleted_id".to_owned()));
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

    // Wrap and hash records
    let mut bundle_records: Vec<BundleRecord> = included_records
        .into_iter()
        .map(|r| {
            let scrubbed = scrub_record(r);
            let json = serde_json::to_string(&scrubbed).unwrap_or_default();
            let hash = blake3::hash(json.as_bytes()).to_string();
            BundleRecord {
                record: scrubbed,
                hash,
            }
        })
        .collect();

    // Canonical ordering of bundle records by ID (and then by JSON serialization of the scrubbed record if IDs match)
    bundle_records.sort_by(|a, b| {
        let id_cmp = a.record.id().cmp(b.record.id());
        if id_cmp == std::cmp::Ordering::Equal {
            let a_json = serde_json::to_string(&a.record).unwrap_or_default();
            let b_json = serde_json::to_string(&b.record).unwrap_or_default();
            a_json.cmp(&b_json)
        } else {
            id_cmp
        }
    });

    // Find repository identity and snapshot from all records
    let mut repo_id = "unknown".to_owned();
    let mut snapshot = None;
    for rec in records {
        if let GraphRecord::Node {
            kind: NodeKind::Repository,
            id,
            source_snapshot,
            ..
        } = rec
        {
            repo_id.clone_from(id);
            if let Some(s) = source_snapshot {
                snapshot = Some(s.head.clone());
            }
            break;
        }
    }

    // Validate coverage threshold. A bundle carries its whole BFS closure —
    // including reachable `LogSource` nodes and `CAPTURED_FROM`/`AGGREGATES` edges
    // — so the class-wide `runtime_observation` provenance requirement (#328) is
    // resolvable here, and building the index from the SAME scrubbed closure that
    // `verify_bundle` sees keeps export and verify in agreement (issue #372).
    let closure: Vec<GraphRecord> = bundle_records.iter().map(|br| br.record.clone()).collect();
    let prov = crate::citation_audit::CitationProvenance::build(&closure);
    let mut total_valid = 0usize;
    let mut non_code_valid = true;
    let total_records_count = bundle_records.len();

    for br in &bundle_records {
        let classified =
            crate::citation_audit::classify_record_external_with_provenance(&br.record, &prov);
        let trust_class = classified.trust_class;

        let is_valid =
            classified.status != crate::citation_audit::CitationStatus::MissingRequiredHandle;
        if is_valid {
            total_valid += 1;
        }

        if trust_class != "source_fact" && !is_valid {
            non_code_valid = false;
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let overall_coverage = if total_records_count > 0 {
        total_valid as f64 / total_records_count as f64
    } else {
        1.0
    };

    if overall_coverage < 0.95 {
        let coverage_pct = overall_coverage * 100.0;
        return Err(CodegraphError::BundleVerificationFailed {
            message: format!(
                "below_coverage_threshold: overall coverage is {coverage_pct:.2}%, required 95%"
            ),
        });
    }

    if !non_code_valid {
        return Err(CodegraphError::BundleVerificationFailed {
            message:
                "below_coverage_threshold: non-code trust-class records must have 100% coverage"
                    .to_string(),
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

/// Verifies the bundle's integrity, coverage, and safety offline and read-only.
#[must_use]
#[allow(clippy::too_many_lines, clippy::if_not_else)]
pub fn verify_bundle(bundle: &EvidenceBundle) -> VerificationReport {
    // 1. Integrity check
    let mut integrity_passed = true;
    let mut integrity_msg =
        "Manifest parses, record hashes match, canonical ordering is stable".to_owned();

    let manifest_included_sum: usize = bundle.manifest.included_record_counts.values().sum();
    if manifest_included_sum != bundle.records.len() {
        integrity_passed = false;
        let records_len = bundle.records.len();
        integrity_msg = format!(
            "Integrity failure: manifest included_record_counts sum ({manifest_included_sum}) does not match records length ({records_len})"
        );
    } else {
        // Check hashes match
        for (i, br) in bundle.records.iter().enumerate() {
            let serialized = match serde_json::to_string(&br.record) {
                Ok(s) => s,
                Err(e) => {
                    integrity_passed = false;
                    integrity_msg =
                        format!("Integrity failure: failed to serialize record at index {i}: {e}");
                    break;
                }
            };
            let computed_hash = blake3::hash(serialized.as_bytes()).to_hex().to_string();
            if computed_hash != br.hash {
                integrity_passed = false;
                let record_id = br.record.id();
                let expected_hash = &br.hash;
                integrity_msg = format!(
                    "Integrity failure: record hash mismatch at index {i}. Record ID: {record_id}. Expected: {expected_hash}, computed: {computed_hash}"
                );
                break;
            }
        }

        // Check canonical ordering
        if integrity_passed {
            for i in 0..bundle.records.len().saturating_sub(1) {
                let a = &bundle.records[i].record;
                let b = &bundle.records[i + 1].record;
                let id_cmp = a.id().cmp(b.id());
                let is_ordered = if id_cmp == std::cmp::Ordering::Equal {
                    let a_json = serde_json::to_string(a).unwrap_or_default();
                    let b_json = serde_json::to_string(b).unwrap_or_default();
                    a_json <= b_json
                } else {
                    id_cmp == std::cmp::Ordering::Less
                };
                if !is_ordered {
                    integrity_passed = false;
                    let a_id = a.id();
                    let b_id = b.id();
                    let next_i = i + 1;
                    integrity_msg = format!(
                        "Integrity failure: canonical ordering is unstable. Record at index {i} ({a_id}) is after record at index {next_i} ({b_id})"
                    );
                    break;
                }
            }
        }
    }

    let integrity = VerificationVerdict {
        passed: integrity_passed,
        detail: integrity_msg,
    };

    // 2. Coverage check
    let mut coverage_passed = true;
    let mut coverage_msg =
        "All selected roots and directly linked evidence meet the citable-handle threshold"
            .to_owned();

    let mut total_valid = 0usize;
    let mut non_code_valid = true;
    let total_records_count = bundle.records.len();

    // Check that all root record IDs are included in the bundle
    let included_ids: HashSet<&str> = bundle.records.iter().map(|br| br.record.id()).collect();
    for root_id in &bundle.manifest.root_record_ids {
        if !included_ids.contains(root_id.as_str()) {
            coverage_passed = false;
            coverage_msg =
                format!("Coverage failure: root record ID {root_id} is not included in the bundle");
            break;
        }
    }

    // Build the provenance index from the bundle's own closure so verify applies
    // the same class-wide `runtime_observation` provenance requirement (#328) that
    // `export_bundle` applied over the identical scrubbed closure (issue #372).
    let closure: Vec<GraphRecord> = bundle.records.iter().map(|br| br.record.clone()).collect();
    let prov = crate::citation_audit::CitationProvenance::build(&closure);
    if coverage_passed {
        for br in &bundle.records {
            let classified =
                crate::citation_audit::classify_record_external_with_provenance(&br.record, &prov);
            let trust_class = classified.trust_class;

            let is_valid =
                classified.status != crate::citation_audit::CitationStatus::MissingRequiredHandle;
            if is_valid {
                total_valid += 1;
            }

            if trust_class != "source_fact" && !is_valid {
                non_code_valid = false;
            }
        }

        #[allow(clippy::cast_precision_loss)]
        let overall_coverage = if total_records_count > 0 {
            total_valid as f64 / total_records_count as f64
        } else {
            1.0
        };

        if overall_coverage < 0.95 {
            let coverage_pct = overall_coverage * 100.0;
            coverage_passed = false;
            coverage_msg = format!(
                "Coverage failure: overall citation coverage is {coverage_pct:.2}% (below 95% threshold)"
            );
        } else if !non_code_valid {
            coverage_passed = false;
            coverage_msg = "Coverage failure: non-code trust-class records must have 100% coverage"
                .to_string();
        }
    }

    let coverage = VerificationVerdict {
        passed: coverage_passed,
        detail: coverage_msg,
    };

    // 3. Safety check
    let mut safety_passed = true;
    let mut safety_msg = "No raw protected payload classes or unredacted secrets found".to_owned();

    for br in &bundle.records {
        let record_id = br.record.id();

        // Check for unredacted secrets in any part of the record (Node, Edge, Tombstone, etc.)
        let serialized = serde_json::to_string(&br.record).unwrap_or_default();
        if let Some((class, _)) = crate::redaction::detect_secret(&serialized) {
            safety_passed = false;
            let class_str = class.as_str();
            safety_msg = format!(
                "Safety failure: record {record_id} contains unredacted secret class: {class_str}"
            );
            break;
        }

        // Assert every field `scrub_record` clears is actually None — shared
        // with the evidence-pack (#338) verify Safety check so the two can
        // never drift (top-level prose, inline handle payloads, and nested
        // user_context prose).
        if let Some(field) = first_unscrubbed_field(&br.record) {
            safety_passed = false;
            safety_msg =
                format!("Safety failure: record {record_id} retains scrubbed field '{field}'");
            break;
        }
    }

    let safety = VerificationVerdict {
        passed: safety_passed,
        detail: safety_msg,
    };

    let ok = integrity.passed && coverage.passed && safety.passed;

    VerificationReport {
        ok,
        integrity,
        coverage,
        safety,
    }
}

#[cfg(test)]
mod tests {
    //! #372: the bundle coverage gate applies the class-wide
    //! `runtime_observation` provenance requirement over the SAME BFS closure
    //! both `export_bundle` and `verify_bundle` see, so a provenance-less
    //! `ErrorSignature` pulled into a bundle is uncited on both — never counted
    //! Cited by its own ID via the context-free catch-all.
    use super::{export_bundle, verify_bundle};
    use crate::ir::{
        EdgeLabel, ErrorSignaturePayload, FrameResolution, GraphRecord, LOG_SCHEMA_VERSION,
        LogPayload, LogSourcePayload, NodeKind, SourceSpan,
    };

    fn symbol(id: &str, path: &str, name: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(path.to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 10,
                end_line: 20,
            }),
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn error_signature(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::ErrorSignature,
            None,
            None,
            Some("error signature".to_owned()),
            "error signature".to_owned(),
        )
        .with_domain("log", LOG_SCHEMA_VERSION)
        .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
            fingerprint_algorithm: "template-v1".to_owned(),
            template_excerpt: "connection refused to HOST".to_owned(),
            severity: "error".to_owned(),
            occurrence_count: 3,
            first_seen: "2026-03-02T09:00:00Z".to_owned(),
            last_seen: "2026-03-02T10:00:00Z".to_owned(),
            frames: None,
            repository_id: String::new(),
        }))
    }

    fn log_source(id: &str, path: &str, hash: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::LogSource,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            "log source".to_owned(),
        )
        .with_domain("log", LOG_SCHEMA_VERSION)
        .with_log(LogPayload::LogSource(LogSourcePayload {
            source_relative_path: path.to_owned(),
            source_format_version: "plain-v1".to_owned(),
            source_artifact_hash: hash.to_owned(),
            line_count: 10,
            repository_id: String::new(),
        }))
    }

    fn frame_resolves_to(signature_id: &str, symbol_id: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::FrameResolvesTo,
            signature_id.to_owned(),
            symbol_id.to_owned(),
            None,
            "frame resolves to symbol".to_owned(),
        )
        .with_frame_resolution(FrameResolution::Resolved)
        .with_frame_index(0)
    }

    fn captured_from(signature_id: &str, source_id: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::CapturedFrom,
            signature_id.to_owned(),
            source_id.to_owned(),
            None,
            "captured from".to_owned(),
        )
    }

    // #372 RED-neg: a well-cited Symbol root reaches a provenance-less
    // ErrorSignature via FRAME_RESOLVES_TO (no LogSource in the graph). The
    // signature is a runtime_observation row with no resolvable provenance, so the
    // bundle coverage gate must reject the export.
    #[test]
    fn export_rejects_provenance_less_log_row_in_closure() {
        let sig_id = crate::ir::log_stable_id(&["error_signature", "repo372b", "tpl", "error"]);
        let sym_id = "codegraph:v5:sym-db";
        let records = vec![
            symbol(sym_id, "src/db.rs", "connect"),
            error_signature(&sig_id),
            frame_resolves_to(&sig_id, sym_id),
        ];
        let result = export_bundle(&records, "symbol:connect", "0.1.0");
        assert!(
            result.is_err(),
            "a provenance-less runtime observation pulled into the closure must fail \
             the bundle coverage gate (#372)"
        );
    }

    // #372 RED-pos: adding a resolvable LogSource + CAPTURED_FROM (reachable in the
    // SAME closure) cites the signature, so export succeeds and verify_bundle —
    // which rebuilds the provenance index from the bundle's own closure — passes.
    #[test]
    fn export_and_verify_accept_provenance_complete_log_row() {
        let sig_id = crate::ir::log_stable_id(&["error_signature", "repo372b", "tpl", "error"]);
        let src_id = crate::ir::log_stable_id(&["log_source", "repo372b", "app.log", "h1"]);
        let sym_id = "codegraph:v5:sym-db";
        let records = vec![
            symbol(sym_id, "src/db.rs", "connect"),
            error_signature(&sig_id),
            frame_resolves_to(&sig_id, sym_id),
            log_source(&src_id, "app.log", "abc123"),
            captured_from(&sig_id, &src_id),
        ];
        let bundle = export_bundle(&records, "symbol:connect", "0.1.0")
            .expect("provenance-complete closure exports");
        assert!(
            verify_bundle(&bundle).ok,
            "verify rebuilds the provenance index from the bundle closure and passes (#372)"
        );
    }
}
