//! Ingest capacity preflight (issue #439).
//!
//! `AletheiaDB` 0.1.1 caps its process-global string interner at a
//! non-overridable `MAX_STRING_COUNT` of `100_000` entries
//! (`src/storage/index_persistence/mod.rs`). At WRITE time only node/edge
//! labels and property KEYS are interned (a small bounded set), so tens of
//! thousands of records write without complaint. At index-PERSIST time the
//! serializer interns every per-record property VALUE string (record id, path,
//! name, summary, signature, doc, boxed-payload JSON, ...). A large graph mints
//! far more than `100_000` distinct value strings and overflows the cap, and the
//! store's background persistence thread then hot-loops on the resulting
//! `CapacityExceeded` error forever — the observed "ingest hangs" symptom.
//!
//! The published crate cannot be patched (project rule: no fork, no path dep),
//! so the primary defense is to REFUSE before opening the store: estimate the
//! distinct interned value strings a graph would produce and, when the estimate
//! reaches the cap, fail fast with a machine-readable diagnostic instead of
//! spawning a doomed writer.
//!
//! This module is feature-INDEPENDENT: it operates purely on [`GraphRecord`]s
//! and pulls in no `AletheiaDB` types, so it compiles and is unit-tested in
//! every feature configuration.

use std::collections::HashSet;

use crate::ir::GraphRecord;

/// The `AletheiaDB` 0.1.1 string-interner capacity, mirrored here so the
/// preflight can refuse before the store's own persist-time check fires.
///
/// Upstream this is `MAX_STRING_COUNT` in
/// `src/storage/index_persistence/mod.rs`: `pub const MAX_STRING_COUNT: u64 =
/// 100_000;`. It is a DoS-protection limit on a process-global,
/// monotonic/append-only interner and is NOT overridable through any public
/// `AletheiaDB` API in 0.1.1, so a workload that would cross it must be split
/// or routed away from the embedded store rather than tuned up.
pub const MAX_INTERNED_STRINGS: u64 = 100_000;

/// The estimate of distinct value strings a graph would intern at persist time.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct InternEstimate {
    /// Estimated number of distinct strings the graph-index persistence would
    /// intern for this graph. See [`estimate_interned_strings`] for exactly
    /// which fields are counted.
    pub distinct_string_count: u64,
    /// Total records considered.
    pub record_count: usize,
    /// Node records considered.
    pub node_count: usize,
    /// Edge records considered.
    pub edge_count: usize,
}

/// A refusal produced when a graph's interned-string estimate reaches the cap.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PreflightRefusal {
    /// The estimated distinct interned-string count.
    pub estimate: u64,
    /// The capacity limit the estimate met or exceeded ([`MAX_INTERNED_STRINGS`]).
    pub limit: u64,
    /// Records in the refused graph.
    pub record_count: usize,
}

/// Estimates the number of distinct value strings the embedded adapter would
/// intern when persisting this graph's index.
///
/// The count is the size of a set built from the string PROPERTY VALUES the
/// embedded `AletheiaDB` adapter inserts per record (see `write_node`,
/// `write_edge`, `write_tombstone`, `base_properties`, and `insert_optional` in
/// `src/adapters/aletheiadb.rs`), PLUS one guaranteed-distinct string per
/// physical record write for the store-side `egregore_seq` monotonic sequence
/// value (a value the record itself does not carry but every write mints).
///
/// Determinism: the count is a `HashSet` cardinality, so it is independent of
/// record order and duplicate values collapse.
///
// NOTE — counted fields and conservatism rationale (issue #439):
//   * Counted per Node: `id` (codegraph_id), `summary`, and every populated
//     string-bearing field the adapter inserts as a STRING property value:
//     repo_relative_path, name, author_name, author_email, language,
//     symbol_kind, visibility, signature, doc, call_context, note,
//     content_signature, valid_time, valid_time_source, entity_id, title,
//     source_kind, source_external_link_id, priority, parent_task_id,
//     verification_link_id, head_sha, head_ref, base_ref, merge_commit_sha,
//     merged_at, system, url, system_native_id, repository_remote,
//     discovered_at, transaction_time, text, superseded_by, agent_id,
//     agent_kind, session_id, observed_at, ingested_at, confidence,
//     source_handle, redaction_policy_version, domain, importer_id,
//     importer_version, source_artifact_path, source_artifact_hash,
//     patch_status, base_commit, unknown_base_reason, patch_bytes_hash,
//     validation_summary, producer_session_id, edit_kind, before_hash,
//     after_hash, rename_to, linked_patch_id, linked_turn_id, tool_name,
//     tool_kind, arguments_summary, produced_evidence_id, started_at,
//     finished_at, failure_kind, evidence_quality, executed_at,
//     verification_kind, status, review_kind, review_state, in_reply_to_id,
//     author, review_side, review_commit_sha, identity_system,
//     transition_kind. Numeric-as-string fields (disambiguator, ordinal,
//     hunk_count, exit_code, turn_index, patch_bytes_size) are included too —
//     the adapter interns them via `.to_string()`.
//   * Counted per Node, one interned string per populated boxed/collection
//     payload (the adapter serializes each to a single JSON property value):
//     temporal, semantic_drift, evidence_links, repository_identity,
//     source_snapshot, dependency, log, scan_coverage, body_handle, assignees,
//     labels, target_files, patch_handle, arguments_handle, result_handle,
//     stdout_handle, stderr_handle, diff_hunk_handle, user_context, producer.
//   * Counted per Edge: `id`, `summary`, `source`, `target`, and populated
//     `confidence` plus the temporal / producer JSON payloads.
//   * Counted per Tombstone: `id`, `summary`, `deleted_id`, producer JSON.
//   * Plus `record_count` for the per-write `egregore_seq` sequence strings.
//   * DELIBERATELY NOT counted: `span` (the adapter stores start/end byte and
//     line as INTEGER property values via `insert_span`, never interned
//     strings) and `schema_version` (also an integer property). Counting a
//     per-record span string would inflate the estimate by ~record_count with
//     no basis in what actually interns.
//   Conservatism: when a field's interning was uncertain it was INCLUDED.
//   Over-counting can only cause a false refusal, which `--force` overrides;
//   under-counting risks missing a real overflow and the resulting hang, so the
//   bias is toward inclusion. One acknowledged minor under-count: `temporal`
//   and `semantic_drift` are each folded into a single JSON string here even
//   though the adapter expands them into several individual interned values;
//   these records are rare relative to the 100_000 cap and their commit-shared
//   substrings dedup heavily, so the effect is negligible.
#[must_use]
#[allow(clippy::too_many_lines)] // Exhaustive per-field enumeration by design.
pub fn estimate_interned_strings(records: &[GraphRecord]) -> InternEstimate {
    let mut values: HashSet<String> = HashSet::new();
    let mut node_count = 0usize;
    let mut edge_count = 0usize;

    for record in records {
        match record {
            GraphRecord::Node {
                id,
                summary,
                repo_relative_path,
                name,
                language,
                symbol_kind,
                disambiguator,
                visibility,
                signature,
                doc,
                call_context,
                note,
                content_signature,
                temporal,
                semantic_drift,
                evidence_links,
                repository_identity,
                source_snapshot,
                dependency,
                log,
                scan_coverage,
                text,
                superseded_by,
                agent_id,
                agent_kind,
                session_id,
                observed_at,
                ingested_at,
                confidence,
                source_handle,
                redaction_policy_version,
                author_name,
                author_email,
                valid_time,
                valid_time_source,
                entity_id,
                title,
                body_handle,
                source_kind,
                source_external_link_id,
                assignees,
                labels,
                priority,
                parent_task_id,
                ordinal,
                verification_link_id,
                head_sha,
                head_ref,
                base_ref,
                merge_commit_sha,
                merged_at,
                system,
                url,
                system_native_id,
                repository_remote,
                discovered_at,
                transaction_time,
                domain,
                importer_id,
                importer_version,
                source_artifact_path,
                source_artifact_hash,
                patch_status,
                base_commit,
                unknown_base_reason,
                target_files,
                patch_bytes_hash,
                patch_bytes_size,
                patch_handle,
                validation_summary,
                producer_session_id,
                edit_kind,
                before_hash,
                after_hash,
                rename_to,
                hunk_count,
                linked_patch_id,
                linked_turn_id,
                tool_name,
                tool_kind,
                arguments_summary,
                arguments_handle,
                result_handle,
                produced_evidence_id,
                started_at,
                finished_at,
                failure_kind,
                exit_code,
                turn_index,
                stdout_handle,
                stderr_handle,
                evidence_quality,
                executed_at,
                verification_kind,
                status,
                review_kind,
                review_state,
                in_reply_to_id,
                author,
                diff_hunk_handle,
                review_side,
                review_commit_sha,
                identity_system,
                transition_kind,
                user_context,
                producer,
                // Non-interned (integer/bool) or identity-only fields excluded
                // on purpose; see the NOTE above. `draft` interns only the two
                // bounded strings "true"/"false".
                kind: _,
                schema_version: _,
                span: _,
                draft: _,
            } => {
                node_count += 1;
                values.insert(id.clone());
                values.insert(summary.clone());
                for field in [
                    repo_relative_path,
                    name,
                    language,
                    symbol_kind,
                    visibility,
                    signature,
                    doc,
                    call_context,
                    note,
                    content_signature,
                    text,
                    superseded_by,
                    agent_id,
                    agent_kind,
                    session_id,
                    observed_at,
                    ingested_at,
                    confidence,
                    source_handle,
                    redaction_policy_version,
                    author_name,
                    author_email,
                    valid_time,
                    valid_time_source,
                    entity_id,
                    title,
                    source_kind,
                    source_external_link_id,
                    priority,
                    parent_task_id,
                    verification_link_id,
                    head_sha,
                    head_ref,
                    base_ref,
                    merge_commit_sha,
                    merged_at,
                    system,
                    url,
                    system_native_id,
                    repository_remote,
                    discovered_at,
                    transaction_time,
                    domain,
                    importer_id,
                    importer_version,
                    source_artifact_path,
                    source_artifact_hash,
                    patch_status,
                    base_commit,
                    unknown_base_reason,
                    patch_bytes_hash,
                    validation_summary,
                    producer_session_id,
                    edit_kind,
                    before_hash,
                    after_hash,
                    rename_to,
                    linked_patch_id,
                    linked_turn_id,
                    tool_name,
                    tool_kind,
                    arguments_summary,
                    produced_evidence_id,
                    started_at,
                    finished_at,
                    failure_kind,
                    evidence_quality,
                    executed_at,
                    verification_kind,
                    status,
                    review_kind,
                    review_state,
                    in_reply_to_id,
                    author,
                    review_side,
                    review_commit_sha,
                    identity_system,
                    transition_kind,
                ]
                .into_iter()
                .flatten()
                {
                    values.insert(field.clone());
                }
                // Numeric fields the adapter interns via `.to_string()`.
                insert_num(&mut values, disambiguator.as_ref());
                insert_num(&mut values, ordinal.as_ref());
                insert_num(&mut values, hunk_count.as_ref());
                insert_num(&mut values, exit_code.as_ref());
                insert_num(&mut values, turn_index.as_ref());
                insert_num(&mut values, patch_bytes_size.as_ref());
                // Boxed / collection payloads: one interned JSON string each.
                insert_json(&mut values, temporal.as_ref());
                insert_json(&mut values, semantic_drift.as_deref());
                insert_json(&mut values, evidence_links.as_ref());
                insert_json(&mut values, repository_identity.as_deref());
                insert_json(&mut values, source_snapshot.as_deref());
                insert_json(&mut values, dependency.as_deref());
                insert_json(&mut values, log.as_deref());
                insert_json(&mut values, scan_coverage.as_deref());
                insert_json(&mut values, body_handle.as_deref());
                insert_json(&mut values, assignees.as_ref());
                insert_json(&mut values, labels.as_ref());
                insert_json(&mut values, target_files.as_ref());
                insert_json(&mut values, patch_handle.as_deref());
                insert_json(&mut values, arguments_handle.as_deref());
                insert_json(&mut values, result_handle.as_deref());
                insert_json(&mut values, stdout_handle.as_deref());
                insert_json(&mut values, stderr_handle.as_deref());
                insert_json(&mut values, diff_hunk_handle.as_deref());
                if !user_context.is_empty() {
                    insert_json(&mut values, Some(user_context));
                }
                insert_json(&mut values, producer.as_ref());
            }
            GraphRecord::Edge {
                id,
                summary,
                source,
                target,
                confidence,
                temporal,
                producer,
                ..
            } => {
                edge_count += 1;
                values.insert(id.clone());
                values.insert(summary.clone());
                values.insert(source.clone());
                values.insert(target.clone());
                if let Some(value) = confidence {
                    values.insert(value.clone());
                }
                insert_json(&mut values, temporal.as_ref());
                insert_json(&mut values, producer.as_ref());
            }
            GraphRecord::Tombstone {
                id,
                deleted_id,
                summary,
                producer,
                ..
            } => {
                values.insert(id.clone());
                values.insert(deleted_id.clone());
                values.insert(summary.clone());
                insert_json(&mut values, producer.as_ref());
            }
        }
    }

    let record_count = records.len();
    // One guaranteed-distinct interned string per physical write for the
    // store-side `egregore_seq` monotonic sequence value.
    let distinct_string_count = values.len() as u64 + record_count as u64;

    InternEstimate {
        distinct_string_count,
        record_count,
        node_count,
        edge_count,
    }
}

/// Refuses an ingest whose interned-string estimate would meet or exceed the
/// `AletheiaDB` cap, unless `force` bypasses the estimate.
///
/// Returns `Ok(estimate)` when `force` is true OR the estimate is strictly
/// below [`MAX_INTERNED_STRINGS`]; otherwise `Err(PreflightRefusal)`.
///
/// This is a GRAPH-ONLY estimate. It does NOT account for strings already
/// interned in a pre-existing store the graph is being ingested into: a store
/// that is already partway to the cap can still overflow on a graph this
/// function passes. That gap is deliberately on the safe side — the preflight
/// refuses eagerly on the graph it can see, and `--force` is the escape hatch
/// for the (rare) false refusal. The synchronous `CapacityExceeded` returned by
/// the adapter's `write`/`persist` path remains the fatal backstop for the
/// pre-existing-store case even under `--force`.
///
/// # Errors
///
/// Returns [`PreflightRefusal`] when the graph alone is estimated to reach the
/// cap and `force` is not set.
pub fn check_ingest_capacity(
    records: &[GraphRecord],
    force: bool,
) -> Result<InternEstimate, PreflightRefusal> {
    let estimate = estimate_interned_strings(records);
    if force || estimate.distinct_string_count < MAX_INTERNED_STRINGS {
        Ok(estimate)
    } else {
        Err(PreflightRefusal {
            estimate: estimate.distinct_string_count,
            limit: MAX_INTERNED_STRINGS,
            record_count: estimate.record_count,
        })
    }
}

fn insert_num<T: ToString>(values: &mut HashSet<String>, field: Option<&T>) {
    if let Some(value) = field {
        values.insert(value.to_string());
    }
}

fn insert_json<T: serde::Serialize>(values: &mut HashSet<String>, payload: Option<&T>) {
    if let Some(payload) = payload
        && let Ok(json) = serde_json::to_string(payload)
    {
        values.insert(json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{GraphRecord, SourceSpan};

    fn node(id: &str, name: &str, path: &str) -> GraphRecord {
        GraphRecord::symbol(
            id.to_owned(),
            "function",
            path.to_owned(),
            SourceSpan {
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                end_line: 1,
            },
            name.to_owned(),
            format!("summary for {id}"),
        )
    }

    #[test]
    fn distinct_values_dedup_and_order_independent() {
        let records = vec![
            node("id-a", "alpha", "src/a.rs"),
            node("id-b", "beta", "src/b.rs"),
            node("id-b", "beta", "src/b.rs"), // exact duplicate
        ];
        let mut shuffled = records.clone();
        shuffled.reverse();

        let a = estimate_interned_strings(&records);
        let b = estimate_interned_strings(&shuffled);
        // Order-independent set cardinality; the per-write seq term depends only
        // on record_count, which is identical across the reordering.
        assert_eq!(a.distinct_string_count, b.distinct_string_count);
        assert_eq!(a.record_count, 3);
        assert_eq!(a.node_count, 3);
    }

    #[test]
    fn empty_graph_estimates_zero() {
        let estimate = estimate_interned_strings(&[]);
        assert_eq!(estimate.distinct_string_count, 0);
        assert_eq!(estimate.record_count, 0);
    }

    #[test]
    fn check_returns_ok_below_threshold() {
        let records = vec![node("id-a", "alpha", "src/a.rs")];
        let estimate = check_ingest_capacity(&records, false).expect("below threshold");
        assert!(estimate.distinct_string_count < MAX_INTERNED_STRINGS);
    }

    #[test]
    fn check_refuses_at_or_above_threshold() {
        let records = synthesize_above_threshold();
        let refusal = check_ingest_capacity(&records, false)
            .expect_err("above-threshold graph must be refused");
        assert!(refusal.estimate >= MAX_INTERNED_STRINGS);
        assert_eq!(refusal.limit, MAX_INTERNED_STRINGS);
        assert_eq!(refusal.record_count, records.len());
    }

    #[test]
    fn force_bypasses_refusal_even_above_threshold() {
        let records = synthesize_above_threshold();
        let estimate = check_ingest_capacity(&records, true).expect("force bypasses the estimate");
        assert!(estimate.distinct_string_count >= MAX_INTERNED_STRINGS);
    }

    /// Builds a cheap in-memory graph whose estimate exceeds the cap: each node
    /// contributes a distinct id + name + summary + the per-write seq term, so
    /// ~34k nodes clears `100_000`. Pure allocation, well under a second, no store.
    fn synthesize_above_threshold() -> Vec<GraphRecord> {
        let count = 34_000usize;
        let mut records = Vec::with_capacity(count);
        for index in 0..count {
            records.push(node(
                &format!("id-{index}"),
                &format!("name-{index}"),
                "src/x.rs",
            ));
        }
        records
    }
}
