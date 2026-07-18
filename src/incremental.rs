//! Incremental repository scanning.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use serde::{Deserialize, Serialize};

use chrono::Utc;

use crate::{
    PROCESS_STARTED_AT, code_graph_producer,
    error::{CodegraphError, Result},
    identity,
    ir::{Graph, GraphRecord, ProducerKind, SCHEMA_VERSION, stable_id, versioned_stable_id},
    languages::cross_file::{
        FileFacts, apply_out_of_line_test_scope, cross_file_call_records,
        cross_file_implements_records, label_same_file_call_resolutions,
    },
    repository_record_from_identity, scan_source_file_records,
    schema_version::validate_record_version,
};

/// Incremental cache schema for extractor output stored on disk.
///
/// v5: cached Rust `Symbol` records carry `visibility` / `signature` / `doc`
/// declaration-surface fields (issue #124); older caches rebuild so reused
/// records are never missing the new fields.
///
/// v6 adds per-file cross-file resolution facts and the previously emitted
/// cross-file record IDs (issue #152), and invalidates caches whose per-file
/// records still contain phantom comment/string-sourced reference edges
/// (issue #134); same-file resolution labels are recomputed per scan and are
/// never cached.
///
/// v7 adds per-file `PanicRiskSite` unwrap/expect call-site records
/// (issue #223); older caches rebuild so reused per-file records are never
/// missing the new sites.
///
/// v8: cached per-file records include `DebtMarker` debt-comment marker
/// nodes with their `note` field and `CONTAINS` edges (issue #218); older
/// caches rebuild so reused records are never missing the markers.
///
/// v9 adds per-file `UnsafeSite` records for `unsafe` blocks, `unsafe fn`
/// declarations, and `unsafe impl` blocks (issue #222); older caches rebuild
/// so reused per-file records are never missing the new sites.
///
/// v10 = #342 signature-only trait method Symbols: `function_signature_item`
/// declarations inside a trait body are now first-class `Symbol` records with
/// their `DEFINES` edges and `DefinitionFact`s; older caches rebuild so reused
/// per-file records are never missing the new symbols.
///
/// v11 deepens `IMPLEMENTS` extraction (issues #343, #344): same-file generic
/// trait impl headers now trait-edge-back (`impl<T> Trait for Type<T>` and
/// `impl GenP<u32> for Plain`), blanket impls (`impl<T> Trait for T`) are
/// bounded out, and inherent generic impls keep their self edge. #344 also
/// widens the serialized per-file `FileFacts` with `impl_targets` (exported
/// trait/type definitions) and `pending_impls` (locally-unresolved trait
/// impls) so the repo-wide pass can edge-back out-of-line cross-file impls;
/// older caches rebuild so reused per-file records carry both the new facts
/// and the new edges.
///
/// v12 adds the serde-default `shadowed_by_use` boolean to each per-file
/// `pending_impls` fact (issues #343/#344 round 9): the AST-derived
/// import-shadow verdict a bare trait/type impl carries into the repo-wide
/// cross-file pass, so a `use` (external/std or non-root local alias) that
/// shadows the bare name vetoes the wrong `IMPLEMENTS` edge. The field is
/// serde-default, but a bump forces older caches to rebuild so reused per-file
/// facts carry the verdict rather than defaulting it to `false`.
///
/// v15 adds the `content_signature` field to Node records: a Rust `Module` node
/// carries a compact BLAKE3 handle over its normalized body so a body change
/// with an unchanged name/path is content-detectable by evidence-freshness drift
/// (issue #206). (`Import` nodes intentionally do NOT carry it — an import's
/// stable ID already encodes its full `use ...;` declaration, so a body change
/// mints a new ID and surfaces as a handle-identity change, never a content
/// drift.) The field is serde-default, but an unchanged file served from an
/// older cache would lack it and hash inconsistently against freshly-rebuilt
/// neighbors, so the bump forces older caches to rebuild. (13/14 are reserved
/// for concurrent lanes; this lane takes 15.)
/// v13 adds crate-root partitioning of the cross-file `IMPLEMENTS` index
/// (issue #394: the serde-default `crate_root` on each `impl_targets` /
/// `pending_impls` fact) and import-aware bare-name resolution (issue #393: the
/// new `use_trait_imports` per-file vector capturing the resolved path each
/// module-item `use` binds). The fields are serde-default, but a bump forces
/// older caches to rebuild so reused per-file facts carry the crate root and
/// import paths rather than defaulting them, recovering the recall PR #389 left
/// conservatively unresolved.
///
/// v16 tracks the codegraph `SCHEMA_VERSION` 5→6 bump (issue #135, the
/// `ScanCoverage` node). Cached per-file records embed versioned record IDs
/// (e.g. a reused `File` and its `Repository —CONTAINS→ File` edge carry
/// `codegraph:v5:` IDs), so reusing a v5 cache after the bump would mint v6
/// repository/cross-file records that dangle against the reused v5 endpoints.
/// Bumping the cache version forces a full rebuild so every emitted record
/// shares the current `codegraph:v6:` identity. (This lane originally took 14;
/// renumbered to 16 after the v15 `content_signature` bump landed on trunk;
/// 16 -> 17 for the issue #390 `DefinitionFact.is_trait_method` field and the
/// trait-name-bearing `match_segments` shape, so per-file caches rebuild.
/// 17 -> 18 for the issue #414 `FileFacts.impl_trait_relations` vector AND the
/// issue #413 impl-side directness fix: a block-local `fn` nested inside an impl
/// method body is now a free function (`module::helper`), not `method`
/// (`Owner::helper`), so its cached symbol identity and edge set change and
/// per-file caches must rebuild rather than replay the mis-attributed records.)
/// 18 -> 19 for the issue #406 `ScanCoveragePayload.coverage_generation` field:
/// cached records embed serialized coverage shapes, so a bump forces older
/// caches to rebuild and re-emit the generation-bearing coverage node.
/// 19 -> 20 for the issue #438 non-UTF-8/unreadable source skip: such a file now
/// caches a `Diagnostic` (never a `File` node) and shifts the coverage counts,
/// so older caches must rebuild rather than replay the pre-skip record set.
///
/// Independent of this version, the cache records the writing binary's
/// producer signature (issue #234): a signature mismatch invalidates reuse
/// without a schema bump, and caches missing the signature always rebuild.
pub(crate) const CACHE_SCHEMA_VERSION: u32 = 20;

/// Result of an incremental repository scan.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IncrementalScan {
    /// Graph records for the current repository state plus tombstones.
    pub graph: Graph,
    /// Repository-relative files rebuilt in this scan.
    pub rebuilt_files: Vec<String>,
    /// Repository-relative files reused from cache.
    pub reused_files: Vec<String>,
    /// Repository-relative files emitted as tombstones.
    pub tombstoned_files: Vec<String>,
}

/// Scans a repository using a JSON file cache for unchanged source files.
///
/// # Errors
///
/// Returns an error when repository discovery, source parsing, cache parsing, or
/// cache persistence fails.
pub fn scan_repository_incremental(
    repo_path: impl AsRef<Path>,
    cache_path: impl AsRef<Path>,
) -> Result<IncrementalScan> {
    let transaction_time = Utc::now().to_rfc3339();
    scan_repository_incremental_at(repo_path, cache_path, &transaction_time)
}

/// Like [`scan_repository_incremental`] but excludes repo-relative paths from the
/// dirty probe when stamping the snapshot.
///
/// Pass the data-dir path (if inside the repository) so an existing embedded store
/// is not counted as a source change, keeping `eg freshness --data-dir` clean after
/// `eg refresh` on an uncommitted-edits tree (PR #186 A).
///
/// # Errors
///
/// Returns an error when repository discovery, source parsing, cache parsing, or
/// cache persistence fails.
pub fn scan_repository_incremental_excluding(
    repo_path: impl AsRef<Path>,
    cache_path: impl AsRef<Path>,
    snapshot_exclusions: &[String],
    raw_literals: bool,
) -> Result<IncrementalScan> {
    let transaction_time = Utc::now().to_rfc3339();
    scan_repository_incremental_at_inner(
        repo_path,
        cache_path,
        &transaction_time,
        snapshot_exclusions,
        raw_literals,
    )
}

/// Like [`scan_repository_incremental`] but accepts an explicit `transaction_time` (RFC 3339).
///
/// # Errors
///
/// Returns an error when repository discovery, source parsing, cache parsing, or
/// cache persistence fails.
pub fn scan_repository_incremental_at(
    repo_path: impl AsRef<Path>,
    cache_path: impl AsRef<Path>,
    transaction_time: &str,
) -> Result<IncrementalScan> {
    scan_repository_incremental_at_inner(repo_path, cache_path, transaction_time, &[], false)
}

#[allow(clippy::too_many_lines)]
fn scan_repository_incremental_at_inner(
    repo_path: impl AsRef<Path>,
    cache_path: impl AsRef<Path>,
    transaction_time: &str,
    snapshot_exclusions: &[String],
    raw_literals: bool,
) -> Result<IncrementalScan> {
    std::sync::LazyLock::force(&PROCESS_STARTED_AT);
    let repo_root = repo_path.as_ref();
    crate::validate_repository(repo_root)?;

    let repo_identity = identity::compute_repository_identity(repo_root, None);
    let (repository_id, repository) = repository_record_from_identity(&repo_identity);
    let previous_cache = CacheFile::load(cache_path.as_ref())?;
    // Cache reuse also keys on the producer signature of the binary that
    // wrote the cache (issue #234 / PR #317 review). The assembled graph is
    // re-stamped with the running binary's producer envelope, so reusing
    // records extracted by a different binary/grammar would launder them
    // into `current` and turn the grammar-upgrade partial-refresh scenario
    // into a producer-drift false negative. A signature mismatch (or a cache
    // written before signatures existed) degrades to a full rebuild.
    let current_producer_identity = crate::query::CurrentProducerIdentity::of_running_binary();
    let mut can_reuse_cache_records = previous_cache.schema_version == CACHE_SCHEMA_VERSION
        && previous_cache.repository_id == repository_id
        && previous_cache.producer_egregore_version == current_producer_identity.egregore_version
        && previous_cache.producer_components == current_producer_identity.producer_components
        && previous_cache.raw_literals == Some(raw_literals);
    if can_reuse_cache_records && previous_cache.validate_record_versions().is_err() {
        can_reuse_cache_records = false;
    }
    let mut next_cache = CacheFile::default();
    let mut graph = Graph::new();
    let mut rebuilt_files = Vec::new();
    let mut reused_files = Vec::new();
    let mut seen_files = BTreeSet::new();
    let mut run_redaction_counts = std::collections::HashMap::new();
    let mut run_redacted_nodes = 0_usize;

    // Stamp the store-level source-snapshot identity (issue #82) on the Repository
    // node, mirroring the full-scan path. Without this, refreshing a stale store
    // would replace the stamped Repository node with one whose snapshot is absent,
    // so a follow-up `eg freshness --data-dir` would report `unknown` instead of
    // confirming the refreshed store is fresh.
    let (head, dirty) = identity::working_tree_snapshot_excluding(repo_root, snapshot_exclusions);
    let snapshot = crate::ir::SourceSnapshotPayload {
        head,
        dirty,
        repository_id: repository_id.clone(),
        scanned_at: transaction_time.to_owned(),
    };
    graph.push(
        repository
            .with_valid_time_inferred(transaction_time)
            .with_source_snapshot(snapshot),
    );

    // If the repository identity changed from a previous scan, tombstone the old Repository node
    // so it does not remain live in persisted stores alongside the new identity.  Without this,
    // a store first scanned with local_path identity keeps the stale Repository node even after
    // the identity changes to Remote, which causes the daemon's shared-store guard to keep
    // rejecting otherwise valid writes.
    if !previous_cache.repository_id.is_empty() && previous_cache.repository_id != repository_id {
        let old_repo_id = &previous_cache.repository_id;
        graph.push(GraphRecord::Tombstone {
            id: stable_id(&["tombstone", "repository-identity-changed", old_repo_id]),
            schema_version: SCHEMA_VERSION,
            deleted_id: old_repo_id.clone(),
            summary: format!("Repository identity changed; stale Repository {old_repo_id} removed"),
            producer: None,
        });
    } else if previous_cache.repository_id.is_empty() && !previous_cache.files.is_empty() {
        // Legacy cache written before the repository_id field existed: infer the old
        // basename-derived ID and tombstone it so persisted stores can retire stale records.
        let basename = repo_root
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .unwrap_or("repository");
        // Use the old cache's schema version so the deleted_id matches what the old
        // extractor actually wrote (e.g. `codegraph:v1:…` for a v1 cache).
        let legacy_repo_id = versioned_stable_id(
            previous_cache.schema_version,
            &["node", "repository", basename],
        );
        if legacy_repo_id != repository_id {
            graph.push(GraphRecord::Tombstone {
                id: stable_id(&["tombstone", "repository-identity-changed", &legacy_repo_id]),
                schema_version: SCHEMA_VERSION,
                deleted_id: legacy_repo_id.clone(),
                summary: format!(
                    "Repository identity changed; stale Repository {legacy_repo_id} removed"
                ),
                producer: None,
            });
        }
    }

    let (source_files, mut coverage_tally) =
        crate::fs::discover_source_files_with_coverage(repo_root)?;
    for source_file in source_files {
        let hash = file_hash(&source_file.path)?;
        seen_files.insert(source_file.repo_relative_path.clone());
        let previous_entry = previous_cache.files.get(&source_file.repo_relative_path);
        let cached = previous_entry.filter(|_| can_reuse_cache_records);

        let (records, facts) = if let Some(cached) = cached.filter(|entry| entry.hash == hash) {
            reused_files.push(source_file.repo_relative_path.clone());
            // Restamp reused records so valid_time reflects this scan's transaction time,
            // not the prior scan's time when they were first extracted.
            let mut records = cached
                .records
                .iter()
                .cloned()
                .map(|r| r.with_valid_time_inferred(transaction_time))
                .collect::<Vec<_>>();
            if !raw_literals {
                for record in &mut records {
                    if let GraphRecord::Node {
                        kind,
                        summary,
                        signature,
                        doc,
                        ..
                    } = record
                        && crate::redaction::is_code_graph_kind(*kind)
                    {
                        let counts = crate::redaction::redact_node_text_fields(
                            summary,
                            signature,
                            doc,
                            "«redacted:secret»",
                        );
                        if !counts.is_empty() {
                            run_redacted_nodes += 1;
                            for (class, count) in counts {
                                *run_redaction_counts.entry(class).or_insert(0) += count;
                            }
                        }
                    }
                }
            }
            (records, cached.facts.clone())
        } else {
            rebuilt_files.push(source_file.repo_relative_path.clone());
            let (records, facts) = match scan_source_file_records(&source_file, &repository_id)? {
                crate::SourceFileScanOutcome::Extracted { records, facts } => (records, facts),
                // A non-UTF-8 or unreadable file is skipped (issue #438): cache
                // the diagnostic as this file's sole record so an unchanged file
                // reuses it next refresh. Byte hashing above already succeeds
                // for such files. The coverage skip is threaded below, uniformly
                // for the rebuild and reuse paths.
                crate::SourceFileScanOutcome::Skipped { diagnostic, .. } => {
                    (vec![*diagnostic], FileFacts::default())
                }
            };
            let mut records = records
                .into_iter()
                .map(|r| r.with_valid_time_inferred(transaction_time))
                .collect::<Vec<_>>();
            if let Some(invalidated) = previous_entry {
                for tombstone in invalidated_record_tombstones(
                    &source_file.repo_relative_path,
                    &invalidated.records,
                    &records,
                    transaction_time,
                ) {
                    graph.push(tombstone);
                }
            }
            if !raw_literals {
                for record in &mut records {
                    if let GraphRecord::Node {
                        kind,
                        summary,
                        signature,
                        doc,
                        ..
                    } = record
                        && crate::redaction::is_code_graph_kind(*kind)
                    {
                        let counts = crate::redaction::redact_node_text_fields(
                            summary,
                            signature,
                            doc,
                            "«redacted:secret»",
                        );
                        if !counts.is_empty() {
                            run_redacted_nodes += 1;
                            for (class, count) in counts {
                                *run_redaction_counts.entry(class).or_insert(0) += count;
                            }
                        }
                    }
                }
            }
            (records, facts)
        };

        // Thread a decode/read skip (issue #438) into the coverage tally,
        // uniformly for the rebuild and reuse paths: a skipped file produces its
        // diagnostic but no `File` node, so an absent `File` node for this path
        // means it must be counted UNINDEXED. Doing it here (rather than only on
        // the rebuild arm) keeps `reconcile_scan_coverage` honest even when the
        // undecodable file is unchanged and its cached diagnostic is reused.
        let has_file_node = records.iter().any(|record| {
            matches!(
                record,
                GraphRecord::Node {
                    kind: crate::ir::NodeKind::File,
                    repo_relative_path: Some(path),
                    ..
                } if *path == source_file.repo_relative_path
            )
        });
        if !has_file_node {
            coverage_tally.record_unindexed_skip(
                source_file.repo_relative_path.clone(),
                crate::fs::lowercased_extension(&source_file.path),
            );
        }

        for record in &records {
            graph.push(record.clone());
        }
        next_cache.files.insert(
            source_file.repo_relative_path.clone(),
            CachedFile {
                hash,
                records,
                facts,
            },
        );
    }

    // Repo-wide cross-file call resolution (issue #152): recompute the pass
    // from every file's cached or freshly extracted facts, and tombstone any
    // cross-file record from the previous scan that no longer exists so
    // persisted stores can retire it.
    let facts_by_file: BTreeMap<String, FileFacts> = next_cache
        .files
        .iter()
        .filter(|(_, cached_file)| !cached_file.facts.is_empty())
        .map(|(path, cached_file)| (path.clone(), cached_file.facts.clone()))
        .collect();
    let mut cross_file_records = cross_file_call_records(&repository_id, &facts_by_file);
    // Repo-wide cross-file trait resolution (issue #344): recomputed from the
    // same `facts_by_file` as the CALLS pass, so a change on either side of an
    // out-of-line impl re-derives its IMPLEMENTS edge. Folded into the same
    // recomputed-record stream and ID set so its edges are tombstoned on
    // removal exactly like cross-file CALLS edges.
    cross_file_records.extend(cross_file_implements_records(
        &repository_id,
        &facts_by_file,
    ));
    let cross_file_ids: BTreeSet<String> = cross_file_records
        .iter()
        .map(|record| record.id().to_owned())
        .collect();
    // Previous cross-file record IDs are tombstoned even when cache reuse is
    // disabled (repository identity or cache schema mismatch): those IDs can
    // embed the old repository identity, so the recomputed pass never re-emits
    // them and a persisted store would otherwise keep them live forever. This
    // mirrors the per-file path, which tombstones invalidated cached records
    // regardless of reuse eligibility.
    for stale_id in previous_cache
        .cross_file_record_ids
        .iter()
        .filter(|previous_id| !cross_file_ids.contains(*previous_id))
    {
        graph.push(invalidated_record_tombstone(
            "cross-file",
            stale_id,
            transaction_time,
        ));
    }
    for record in cross_file_records {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }
    next_cache.cross_file_record_ids = cross_file_ids.into_iter().collect();
    // Same-file resolution labeling (issue #134): recomputed over the whole
    // assembled graph every scan — never cached — so a definition added or
    // removed in another file re-labels an unchanged file's edges correctly.
    label_same_file_call_resolutions(graph.records_mut(), &facts_by_file);
    // Out-of-line `#[cfg(test)] mod x;` test-scope marking (issue #223):
    // recomputed over the whole assembled graph every scan — never cached —
    // so a gating change in a parent file re-contexts an unchanged module
    // file's cached panic-risk sites correctly.
    apply_out_of_line_test_scope(graph.records_mut(), &facts_by_file);

    let mut tombstoned_files = Vec::new();
    for (removed, cached_file) in &previous_cache.files {
        if !seen_files.contains(removed) {
            tombstoned_files.push(removed.clone());
            // Tombstone every cached record (File node, Symbol nodes, DEFINES edges) so nothing
            // from the deleted file remains live in persisted stores.
            for tombstone in
                invalidated_record_tombstones(removed, &cached_file.records, &[], transaction_time)
            {
                graph.push(tombstone);
            }
        }
    }

    // Declared Cargo dependencies (issue #180), mirrored from the full-scan path
    // (issue #403). Manifest records are regenerated every refresh — they are not
    // per-file cached — so a dependency-declaring `Cargo.toml` mints a `File` node
    // here exactly as a full scan would. Without this, coverage reconciliation
    // below would misclassify that manifest as skipped `toml`, diverging from a
    // full scan of the same tree and re-introducing the stale/flip-flop the
    // coverage node is meant to eliminate.
    for record in crate::manifest_deps::scan_dependency_records(repo_root, &repository_id)? {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }

    // Scan-coverage reconciliation (issue #135), previously emitted only by the
    // full-scan path (issue #403). Finalize the tally against the COMPLETE set of
    // `File` nodes the graph now carries — after every File-producing extractor
    // (per-file source extraction and manifest extraction above) has run — so a
    // manifest indexed with a `File` node is counted under `files_indexed`, never
    // mislabeled under `skipped_by_extension`. The `ScanCoverage` node is repo-keyed
    // (`stable_id(["node","scan_coverage",repository_id])`), so re-emitting it here
    // supersedes the prior full-scan version in the store, keeping
    // `eg inspect --data-dir` coverage current after a refresh.
    crate::reconcile_scan_coverage(&graph, &mut coverage_tally);
    // `coverage_generation` (issue #406) is the same-UTC-second recency
    // tie-break. The refresh entry points stamp `transaction_time` at full
    // `to_rfc3339()` precision (nanoseconds in production, deterministic under a
    // fixed override), so it is the correct generation signal here — two
    // refreshes within one UTC second get distinct nanosecond values while a
    // fixed-time refresh stays byte-identical.
    for record in crate::scan_coverage_records(&repository_id, &coverage_tally, transaction_time) {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }

    next_cache.repository_id.clone_from(&repository_id);
    next_cache.producer_egregore_version = current_producer_identity.egregore_version;
    next_cache.producer_components = current_producer_identity.producer_components;
    next_cache.raw_literals = Some(raw_literals);
    next_cache.save(cache_path.as_ref())?;

    // Evidence lives while masked literals remain in the assembled graph, not
    // merely while THIS run redacted something new (Codex C8). When every
    // secret-bearing file is unchanged, its reused cache records already carry
    // the redaction marker, so `run_redaction_counts` is empty even though the
    // graph still holds masked literals; tombstoning here would silently erase
    // the audit evidence on a second `eg refresh`.
    let (masked_nodes, masked_literals) = graph
        .records()
        .iter()
        .filter_map(|record| match record {
            GraphRecord::Node {
                kind,
                summary,
                signature,
                doc,
                ..
            } if crate::redaction::is_code_graph_kind(*kind) => {
                let mut hits = summary.matches(crate::redaction::REDACTION_MARKER).count();
                if let Some(sig) = signature {
                    hits += sig.matches(crate::redaction::REDACTION_MARKER).count();
                }
                if let Some(d) = doc {
                    hits += d.matches(crate::redaction::REDACTION_MARKER).count();
                }
                (hits > 0).then_some(hits)
            }
            _ => None,
        })
        .fold((0_usize, 0_usize), |(nodes, literals), hits| {
            (nodes + 1, literals + hits)
        });

    if !raw_literals && masked_nodes > 0 {
        let summary = if run_redaction_counts.is_empty() {
            // No new redactions this run: the masked literals were carried over
            // from a prior redaction via reused cache records.
            format!(
                "Redaction evidence present: {masked_literals} masked literals across {masked_nodes} nodes carried from a prior redaction."
            )
        } else {
            let total_literals: usize = run_redaction_counts.values().sum();
            let mut class_details = run_redaction_counts
                .iter()
                .map(|(class, count)| format!("{}: {}", class.as_str(), count))
                .collect::<Vec<_>>();
            class_details.sort();
            let class_details_str = class_details.join(", ");
            format!(
                "Redacted {total_literals} literals across {run_redacted_nodes} nodes. Detector classes: {class_details_str}"
            )
        };

        let diag_id =
            crate::stable_id(&["node", "diagnostic", "redaction_evidence", &repository_id]);
        // Stamp valid_time from this refresh's transaction time (Codex C9) so the
        // diagnostic is temporally placed with the Repository/File/Symbol nodes
        // emitted earlier in the same refresh batch.
        let mut diag = GraphRecord::node(
            diag_id,
            crate::ir::NodeKind::Diagnostic,
            None,
            None,
            Some("redaction_evidence".to_owned()),
            summary,
        )
        .with_valid_time_inferred(transaction_time);

        if let GraphRecord::Node {
            redaction_policy_version,
            ..
        } = &mut diag
        {
            *redaction_policy_version = Some(crate::redaction::REDACTION_POLICY_VERSION.to_owned());
        }

        graph.push(diag);
    } else {
        let diag_id =
            crate::stable_id(&["node", "diagnostic", "redaction_evidence", &repository_id]);
        graph.push(GraphRecord::Tombstone {
            id: crate::stable_id(&[
                "tombstone",
                "redaction_evidence",
                &repository_id,
                transaction_time,
            ]),
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: diag_id,
            summary: "Stale redaction evidence removed".to_owned(),
            producer: None,
        });
    }

    let languages = crate::languages_in_graph(&graph);
    let mut producer = code_graph_producer(&languages);
    producer.producer_kind = ProducerKind::IncrementalCache;
    producer.producer_components.insert(
        "cache_format_version".to_owned(),
        CACHE_SCHEMA_VERSION.to_string(),
    );
    Ok(IncrementalScan {
        graph: graph.stamp_producer(&producer),
        rebuilt_files,
        reused_files,
        tombstoned_files,
    })
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
struct CacheFile {
    schema_version: u32,
    #[serde(default)]
    repository_id: String,
    files: BTreeMap<String, CachedFile>,
    /// Cross-file resolution records emitted by the previous scan (issue #152),
    /// kept so a later scan can tombstone the ones that disappear.
    #[serde(default)]
    cross_file_record_ids: Vec<String>,
    /// `CARGO_PKG_VERSION` of the binary that wrote this cache (issue #234).
    /// Empty for caches written before producer signatures were recorded;
    /// those always rebuild because their extractor identity is unknown.
    #[serde(default)]
    producer_egregore_version: String,
    /// Every producer-component version the writing binary would stamp
    /// (issue #234). Any difference from the running binary invalidates
    /// reuse so re-stamped records never misreport their producer.
    #[serde(default)]
    producer_components: BTreeMap<String, String>,
    /// Whether the cache was written with `--raw-literals` (issue #101). A
    /// mismatch with the current run invalidates reuse so redacted and raw
    /// record sets never mix.
    #[serde(default)]
    raw_literals: Option<bool>,
}

impl Default for CacheFile {
    fn default() -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            repository_id: String::new(),
            files: BTreeMap::new(),
            cross_file_record_ids: Vec::new(),
            producer_egregore_version: String::new(),
            producer_components: BTreeMap::new(),
            raw_literals: None,
        }
    }
}

impl CacheFile {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path).map_err(|source| CodegraphError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_str(&raw).map_err(CodegraphError::from)
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| CodegraphError::WriteFile {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(path, raw).map_err(|source| CodegraphError::WriteFile {
            path: path.to_path_buf(),
            source,
        })
    }

    fn validate_record_versions(&self) -> Result<()> {
        for cached_file in self.files.values() {
            for record in &cached_file.records {
                validate_record_version(record).map_err(|unknown| {
                    CodegraphError::UnsupportedSchemaVersion {
                        message: format!(
                            "incremental cache contains unsupported record schema version: {}",
                            unknown.version
                        ),
                    }
                })?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
struct CachedFile {
    hash: String,
    records: Vec<GraphRecord>,
    /// Cross-file resolution facts for the file (issue #152).
    #[serde(default, skip_serializing_if = "FileFacts::is_empty")]
    facts: FileFacts,
}

fn file_hash(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|source| CodegraphError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn invalidated_record_tombstones(
    repo_relative_path: &str,
    old_records: &[GraphRecord],
    rebuilt_records: &[GraphRecord],
    transaction_time: &str,
) -> Vec<GraphRecord> {
    let rebuilt_ids = rebuilt_records
        .iter()
        .map(GraphRecord::id)
        .collect::<BTreeSet<_>>();
    old_records
        .iter()
        .filter(|record| !rebuilt_ids.contains(record.id()))
        .map(|record| {
            invalidated_record_tombstone(repo_relative_path, record.id(), transaction_time)
        })
        .collect()
}

fn invalidated_record_tombstone(
    repo_relative_path: &str,
    deleted_id: &str,
    transaction_time: &str,
) -> GraphRecord {
    GraphRecord::Tombstone {
        // Include transaction_time so re-removing the same record after a re-add produces
        // a fresh tombstone ID that gets a higher egregore_seq and supersedes the re-added node.
        id: stable_id(&[
            "tombstone",
            "cache-schema",
            repo_relative_path,
            deleted_id,
            transaction_time,
        ]),
        schema_version: SCHEMA_VERSION,
        deleted_id: deleted_id.to_owned(),
        summary: format!("Invalidated stale cached record {deleted_id} from {repo_relative_path}"),
        producer: None,
    }
}
