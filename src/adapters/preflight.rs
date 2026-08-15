//! Ingest capacity preflight (issue #439, relaxed for `AletheiaDB` 0.2.0).
//!
//! At WRITE time only node/edge labels and property KEYS are interned (a small
//! bounded set), so tens of thousands of records write without complaint. At
//! index-PERSIST time the serializer interns every per-record property VALUE
//! string (record id, path, name, summary, signature, doc, boxed-payload JSON,
//! ...). A large graph mints far more distinct value strings than records, so
//! the interner — not the record count — is the binding limit on ingest size.
//!
//! WHAT CHANGED IN 0.2.0. Two upstream fixes retire the acute failure mode this
//! module was built for:
//!
//!   * The cap is no longer hardcoded at `100_000`. `PersistenceConfig` gained
//!     `max_interned_strings`, defaulting to `10_000_000` — a 100x relaxation.
//!     The embedded adapter sets it EXPLICITLY from [`MAX_INTERNED_STRINGS`]
//!     below rather than inheriting the upstream default, so this estimate and
//!     the store's real cap are the same number by construction and cannot
//!     drift apart across an upstream default change.
//!   * The background-persist infinite retry loop is gone. On 0.1.1 an overflow
//!     made the persistence thread hot-loop on `CapacityExceeded` forever — the
//!     observed "ingest hangs" symptom, and the reason a pre-open refusal was
//!     the only safe defense. An overflow now surfaces as an error instead of a
//!     hang.
//!
//! The preflight is therefore no longer load-bearing against a hang, but it is
//! RETAINED as a fast, honest refusal: at the 10M cap a graph large enough to
//! overflow would otherwise spend a long time writing before failing at persist
//! time, and a refusal naming the estimate is a better answer than a late error.
//! `--force` still bypasses the estimate, and a real overflow during
//! write/persist is still fatal.
//!
//! This module is feature-INDEPENDENT: it operates purely on [`GraphRecord`]s
//! and pulls in no `AletheiaDB` types, so it compiles and is unit-tested in
//! every feature configuration.

use std::collections::HashSet;

use crate::ir::GraphRecord;

/// The string-interner capacity Egregore configures on every embedded store,
/// mirrored here so the preflight can refuse before the store's own
/// persist-time check fires.
///
/// This is the value the embedded adapter passes to
/// `PersistenceConfig.max_interned_strings` at open (see
/// `adapters::aletheiadb::EmbeddedAletheiaSink::open_inner`), NOT merely a copy
/// of an upstream default — the same constant configures the store and bounds
/// this estimate, so the two cannot disagree.
///
/// It matches `AletheiaDB` 0.2.0's own
/// `PersistenceConfig::DEFAULT_MAX_INTERNED_STRINGS` (and its `MAX_STRING_COUNT`
/// load-path floor) of `10_000_000`, raised there from 0.1.1's hardcoded
/// `100_000`. The interner is process-global, monotonic/append-only, and read
/// once at open, so this is a per-PROCESS budget shared by every store the
/// process opens — not a per-store one. Raising it costs roughly 100 bytes of
/// resident memory per interned string (~1 GB at 10M).
pub const MAX_INTERNED_STRINGS: u64 = 10_000_000;

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
//     source_snapshot, dependency, log, scan_coverage, embedding_model,
//     body_handle, route, crate_attribution,
//     assignees, labels, target_files, patch_handle, arguments_handle,
//     result_handle, stdout_handle, stderr_handle, diff_hunk_handle,
//     user_context, producer.
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
//   these records are rare relative to the cap and their commit-shared
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
                route,
                crate_attribution,
                temporal,
                semantic_drift,
                evidence_links,
                repository_identity,
                source_snapshot,
                dependency,
                log,
                scan_coverage,
                embedding_model,
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
                insert_json(&mut values, embedding_model.as_deref());
                insert_json(&mut values, body_handle.as_deref());
                insert_json(&mut values, route.as_ref());
                insert_json(&mut values, crate_attribution.as_ref());
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
    check_ingest_capacity_against(records, force, MAX_INTERNED_STRINGS)
}

/// [`check_ingest_capacity`] against an explicit `limit`.
///
/// Exists so the refusal/bypass logic stays unit-testable after the 0.2.0
/// upgrade raised [`MAX_INTERNED_STRINGS`] to `10_000_000`: synthesizing a graph
/// that genuinely clears 10M distinct strings would need millions of records and
/// gigabytes of allocation, which is not a unit test. Tests drive the threshold
/// behaviour through this function with a small limit and separately assert that
/// the public wrapper passes [`MAX_INTERNED_STRINGS`].
///
/// # Errors
///
/// Returns [`PreflightRefusal`] when the estimate reaches `limit` and `force` is
/// not set.
fn check_ingest_capacity_against(
    records: &[GraphRecord],
    force: bool,
    limit: u64,
) -> Result<InternEstimate, PreflightRefusal> {
    let estimate = estimate_interned_strings(records);
    if force || estimate.distinct_string_count < limit {
        Ok(estimate)
    } else {
        Err(PreflightRefusal {
            estimate: estimate.distinct_string_count,
            limit,
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
        let records = synthesize_above(TEST_LIMIT);
        let refusal = check_ingest_capacity_against(&records, false, TEST_LIMIT)
            .expect_err("above-threshold graph must be refused");
        assert!(refusal.estimate >= TEST_LIMIT);
        assert_eq!(refusal.limit, TEST_LIMIT);
        assert_eq!(refusal.record_count, records.len());
    }

    #[test]
    fn force_bypasses_refusal_even_above_threshold() {
        let records = synthesize_above(TEST_LIMIT);
        let estimate = check_ingest_capacity_against(&records, true, TEST_LIMIT)
            .expect("force bypasses the estimate");
        assert!(estimate.distinct_string_count >= TEST_LIMIT);
    }

    /// The public wrapper must gate on [`MAX_INTERNED_STRINGS`], not on some
    /// other constant — this is what ties the tests above (which use a small
    /// injected limit) to the cap the adapter actually configures.
    #[test]
    fn public_check_gates_on_the_configured_cap() {
        let records = synthesize_above(TEST_LIMIT);
        let refusal = check_ingest_capacity_against(&records, false, TEST_LIMIT)
            .expect_err("refused against the small test limit");
        // The very same graph passes the public check, because the real cap is
        // two orders of magnitude higher.
        let estimate =
            check_ingest_capacity(&records, false).expect("must pass under the configured cap");
        assert_eq!(estimate.distinct_string_count, refusal.estimate);
        assert!(estimate.distinct_string_count < MAX_INTERNED_STRINGS);
    }

    /// `AletheiaDB` 0.2.0 upgrade: the cap is the value Egregore configures on the
    /// store (`PersistenceConfig.max_interned_strings`), raised from 0.1.1's
    /// hardcoded `100_000` to `10_000_000`.
    #[test]
    fn configured_cap_is_ten_million() {
        assert_eq!(MAX_INTERNED_STRINGS, 10_000_000);
    }

    /// Regression the upgrade exists to fix: a graph in the 100k–10M band was
    /// refused outright on 0.1.1 and must now ingest without `--force`. ~34k
    /// nodes estimate well past the old `100_000` cap.
    #[test]
    fn graph_over_the_old_100k_cap_is_no_longer_refused() {
        const OLD_CAP: u64 = 100_000;
        let records = synthesize_above(OLD_CAP);
        let estimate = check_ingest_capacity(&records, false)
            .expect("a graph over the retired 100k cap must ingest without --force");
        assert!(
            estimate.distinct_string_count >= OLD_CAP,
            "fixture must actually clear the old cap to be a regression test"
        );
        assert!(estimate.distinct_string_count < MAX_INTERNED_STRINGS);
    }

    /// A small stand-in for the real cap, so the refusal path is exercised
    /// without allocating the millions of records `MAX_INTERNED_STRINGS` would
    /// now require.
    const TEST_LIMIT: u64 = 1_000;

    /// Builds a cheap in-memory graph whose estimate exceeds `limit`: each node
    /// contributes a distinct id + name + summary + the per-write seq term, so
    /// roughly `limit / 3` nodes clears it. Pure allocation, no store.
    fn synthesize_above(limit: u64) -> Vec<GraphRecord> {
        let count = usize::try_from(limit).unwrap_or(usize::MAX) / 3 + 64;
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
