use super::*;

pub(crate) fn print_counts_text(counts: &InspectCounts) {
    println!("records: {}", counts.records);
    println!("nodes: {}", counts.nodes);
    println!("edges: {}", counts.edges);
    println!("tombstones: {}", counts.tombstones);
    println!("diagnostics: {}", counts.diagnostics);

    // Group counts by domain category
    let mut domain_groups: BTreeMap<&str, Vec<(&RecordVersion, usize)>> = BTreeMap::new();
    for (version, count) in &counts.schema_versions {
        domain_groups
            .entry(version.domain.as_str())
            .or_default()
            .push((version, *count));
    }

    let ordered_domains = vec![
        ("codegraph", "Deterministic Source Facts (codegraph)"),
        ("semantic", "Derived Measurements (semantic)"),
        ("agent_memory", "Agent-Authored Claims (agent_memory)"),
        ("project", "Project/Work State (project)"),
        ("artifact", "Artifacts (artifact)"),
        ("verification", "Verification Evidence (verification)"),
        ("user_context", "User Context (user_context)"),
        ("log", "Runtime Observations (log)"),
    ];

    for (dom_name, category) in ordered_domains {
        if let Some(mut items) = domain_groups.remove(dom_name) {
            println!("{category}:");
            items.sort_by_key(|(v, _)| (&v.kind, v.version));
            for (version, count) in items {
                println!("  {} v{}: {}", version.kind, version.version, count);
            }
        }
    }

    for (dom_name, mut items) in domain_groups {
        println!("{dom_name} (unknown domain):");
        items.sort_by_key(|(v, _)| (&v.kind, v.version));
        for (version, count) in items {
            println!("  {} v{}: {}", version.kind, version.version, count);
        }
    }

    if !counts.unknown_schema_versions.is_empty() {
        println!("unknown schema versions:");
        let mut unknown_items: Vec<_> = counts.unknown_schema_versions.iter().collect();
        unknown_items.sort_by_key(|(v, _)| (&v.domain, &v.kind, v.version));
        for (version, count) in unknown_items {
            println!(
                "  {} {} v{}: {}",
                version.domain, version.kind, version.version, count
            );
        }
    }

    for repo in &counts.repositories {
        println!("repository: {} ({})", repo.id, repo.identity_summary);
    }
    for cov in &counts.coverage {
        let skipped_total: usize = cov.skipped_by_extension.values().sum();
        println!(
            "coverage: {} files walked, {} indexed, {} skipped (complete: {})",
            cov.files_walked, cov.files_indexed, skipped_total, cov.coverage_complete
        );
        for (ext, count) in &cov.skipped_by_extension {
            let label = if ext.is_empty() { "(no-ext)" } else { ext };
            println!("  skipped {label}: {count}");
        }
        println!("  indexed languages: {}", cov.indexed_languages.join(", "));
    }
    // Semantic vector-index identity (issue #104): the human-readable form of
    // the `semantic_index` JSON block.
    #[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
    if let Some(index) = &counts.semantic_index {
        match index.index_dimensions {
            None => println!("semantic index: absent (store never ingested with --embed)"),
            Some(dim) => {
                println!("semantic index: {dim}-dimensional vectors");
                if index.indexed_models.is_empty() {
                    println!(
                        "  embedding model: NOT RECORDED — compatibility with a query embedder \
                         is unverifiable; re-ingest with --embed to record it"
                    );
                }
                for model in &index.indexed_models {
                    println!(
                        "  embedding model: {}/{}@{} dim={} hash={}",
                        model.provider, model.name, model.version, model.dim, model.content_hash
                    );
                }
            }
        }
    }
    for (kind, count) in &counts.producer_kinds {
        println!("producer_kind {kind}: {count}");
    }
    for (version, count) in &counts.egregore_versions {
        println!("egregore_version {version}: {count}");
    }
}

pub(crate) fn inspect(
    graph: Option<&Path>,
    daemon: bool,
    data_dir: Option<&Path>,
    format: Option<OutputFormat>,
) -> Result<()> {
    #[cfg(feature = "embedded-aletheiadb")]
    if daemon {
        let format = format.unwrap_or(OutputFormat::Text);
        let default_path = PathBuf::from(".egregore");
        let data_dir = data_dir.unwrap_or(&default_path);
        let client = DaemonClient::from_data_dir(data_dir).with_context(|| {
            format!(
                "failed to inspect data-dir {}: daemon metadata is missing or invalid",
                data_dir.display()
            )
        })?;
        client.health().with_context(|| {
            format!(
                "failed to inspect data-dir {}: daemon is not running or unresponsive",
                data_dir.display()
            )
        })?;
        let (records, unknown_versions, snapshot_timestamp) =
            client.get_all_records().with_context(|| {
                format!(
                "failed to inspect data-dir {}: invalid authorization token or daemon read error",
                data_dir.display()
            )
            })?;

        let counts = InspectCounts::from_records(&records, &unknown_versions);

        match format {
            OutputFormat::Json => {
                let json_val = counts.to_json(&snapshot_timestamp);
                println!("{}", serde_json::to_string_pretty(&json_val)?);
            }
            OutputFormat::Text => {
                print_counts_text(&counts);
            }
        }
        return Ok(());
    }

    #[cfg(not(feature = "embedded-aletheiadb"))]
    if daemon {
        anyhow::bail!("daemon inspection requires 'embedded-aletheiadb' feature");
    }

    // Daemon-free embedded-store inspection (issue #125): `--data-dir` without
    // `--daemon` reads the store through the same embedded read path the query
    // surface uses, defaulting to newline-delimited JSON.
    if let Some(data_dir) = data_dir {
        return inspect_embedded_store(data_dir, format.unwrap_or(OutputFormat::Json));
    }

    let format = format.unwrap_or(OutputFormat::Text);
    let graph = graph
        .ok_or_else(|| anyhow::anyhow!("graph file path, --data-dir, or --daemon is required"))?;
    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    let counts = InspectCounts::from_jsonl(&jsonl)?;
    let snapshot_timestamp = chrono::Utc::now().to_rfc3339();

    match format {
        OutputFormat::Json => {
            let json_val = counts.to_json(&snapshot_timestamp);
            println!("{}", serde_json::to_string_pretty(&json_val)?);
        }
        OutputFormat::Text => {
            print_counts_text(&counts);
        }
    }
    Ok(())
}

/// Inspects an embedded `--data-dir` store directly, without a daemon (issue #125).
///
/// Reads through the same read-only embedded path the audit surfaces use (the
/// store is copied to a throwaway temporary directory first, so the original is
/// never re-persisted or otherwise mutated), then reports the same totals and
/// per-domain/per-kind/per-schema-version trust-class counts as `eg inspect`
/// over a graph JSONL file. Unknown `(domain, kind, schema_version)` tuples are
/// counted under `unknown_schema_versions`, never folded into known versions.
///
/// JSON output is a single deterministic line (newline-delimited JSON) that is
/// byte-identical across runs on an unchanged store; it carries no timestamp
/// and no raw record payloads — counts, domains, kinds, schema versions,
/// repository handles, scan-coverage tallies, and the semantic vector index's
/// bounded embedding-model identity fields (issue #104) only. The shape is
/// documented in `docs/cli/inspect.md`.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn inspect_embedded_store(data_dir: &Path, format: OutputFormat) -> Result<()> {
    let (store_root, _readonly_guard) = readonly_audit_store(data_dir)?;
    let sink = EmbeddedAletheiaSink::open_unleased(&store_root)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
    let report = sink.inspect_all_records().map_err(|error| {
        anyhow::anyhow!(
            "failed to inspect embedded store {}: {error}",
            data_dir.display()
        )
    })?;

    // A directory can hold engine index/runtime files while containing zero
    // Egregore records (empty ingest, or a non-Egregore AletheiaDB dir). That
    // is a wrong-store diagnostic naming the path, never successful zero
    // counts (issue #125).
    if report.records.is_empty() && report.unknown_schema_versions.is_empty() {
        anyhow::bail!(
            "error: embedded store at {} contains no Egregore records - \
             run `eg ingest --adapter embedded --data-dir <path>` first",
            data_dir.display()
        );
    }

    let mut counts = InspectCounts::from_records(&report.records, &report.unknown_schema_versions);
    // Canonical ordering: repository summaries sort by stable record ID so the
    // output is deterministic regardless of physical store iteration order.
    counts.repositories.sort_by(|a, b| a.id.cmp(&b.id));

    // Both detail blocks below must reflect the transaction-time-CURRENT version
    // of each stable ID. `inspect_all_records` above returns EVERY physical
    // version (including superseded ones) so the record/node totals stay
    // accurate, but a re-ingested store then holds several equal-ID versions;
    // resolving the blocks through the current serving view guarantees the
    // latest is reported, never a stale earlier one. Read ONCE and shared, so
    // adding the second block did not add a third full deserialization of the
    // store (issues #135 / #104).
    let current = sink.inspect_current_records().map_err(|error| {
        anyhow::anyhow!(
            "failed to read current records from embedded store {}: {error}",
            data_dir.display()
        )
    })?;
    counts.coverage = current_coverage_summaries(&current.records);

    // Semantic-index identity block (issue #104): the documented `eg` workflow
    // for reading which embedding model produced a store's queryable vector
    // index, so an operator can see WHY a semantic query was refused and decide
    // to re-ingest.
    #[cfg(feature = "embeddings")]
    {
        counts.semantic_index = Some(SemanticIndexSummary {
            index_dimensions: sink.embedding_index_dimensions(),
            indexed_models: crate::embeddings::indexed_identities(&current.records),
        });
    }

    match format {
        OutputFormat::Json => {
            let json_val = counts.to_json_embedded(&data_dir.display().to_string());
            println!("{}", serde_json::to_string(&json_val)?);
        }
        OutputFormat::Text => print_counts_text(&counts),
    }
    Ok(())
}

/// Semantic vector-index identity block reported by `eg inspect --data-dir`
/// (issue #104).
///
/// Allow-list only: dimensions, the identity-recorded flag, and the bounded
/// `EmbeddingModel` identity fields. Never vectors, model bytes, or payloads.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[derive(Debug, Clone, Default)]
pub(crate) struct SemanticIndexSummary {
    /// Dimensionality of the persisted vector index; `None` when the store has
    /// no semantic index (never ingested with `--embed`).
    index_dimensions: Option<usize>,
    /// Distinct live embedding-model identities recorded for the index.
    indexed_models: Vec<crate::ir::EmbeddingModel>,
}

/// Resolves the transaction-time-current `ScanCoverage` summary for each stable
/// coverage ID in an embedded store (issue #135).
///
/// `inspect_current_records` collapses each non-temporal stable ID to its
/// latest physical version (superseded prior versions are dropped), so a store
/// re-ingested after files changed yields exactly one — the current — coverage
/// summary per ID. Unlike `inspect_all_records` (which the totals need), it
/// serves only the current serving view; unlike `read_all_records`, it
/// tolerates unknown-schema-version physical records rather than erroring, so
/// it is safe on any store the totals path can inspect. Ordering by record ID
/// keeps the block byte-identical across runs.
#[cfg(feature = "embedded-aletheiadb")]
fn current_coverage_summaries(current_records: &[GraphRecord]) -> Vec<CoverageSummary> {
    let mut latest: BTreeMap<String, CoverageSummary> = BTreeMap::new();
    for record in current_records {
        if let Some(summary) = coverage_summary_from_record(record) {
            latest.insert(summary.id.clone(), summary);
        }
    }
    latest.into_values().collect()
}

/// Feature-off stub: `--data-dir` inspection needs the embedded adapter.
#[cfg(not(feature = "embedded-aletheiadb"))]
pub(crate) fn inspect_embedded_store(data_dir: &Path, _format: OutputFormat) -> Result<()> {
    anyhow::bail!(
        "inspecting {} requires the 'embedded-aletheiadb' feature",
        data_dir.display()
    )
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct RepositorySummary {
    id: String,
    identity_summary: String,
}

/// One `ScanCoverage` node's file-level indexing accounting (issue #135),
/// surfaced in `eg inspect` so an agent can tell "0 results because absent"
/// from "0 results because that language was never indexed" (AC3).
#[derive(Debug, Serialize, Clone)]
pub(crate) struct CoverageSummary {
    id: String,
    files_walked: usize,
    files_indexed: usize,
    skipped_by_extension: BTreeMap<String, usize>,
    indexed_languages: Vec<String>,
    coverage_complete: bool,
}

/// Deterministic freshness ordering for collapsing equal-ID `ScanCoverage`
/// versions on the `--graph` path (issue #135).
///
/// `instant` is the coverage node's `valid_time` (= the scan's transaction
/// time, stamped by `with_valid_time_inferred`) parsed to a UTC instant — a
/// serialized, export-preserved signal that is monotonic with scan recency, so
/// a later re-scan sorts fresher regardless of physical line order. A record
/// whose `valid_time` is absent or unparseable sorts as `None`, strictly older
/// than any parseable instant (so a real scan always wins over a signal-less
/// version). `generation` is the payload's full-precision `coverage_generation`
/// (issue #406) parsed to a UTC instant — the same-UTC-second tie-break: two
/// full scans within one UTC second share a seconds-precision `valid_time` yet
/// carry DISTINCT nanosecond generations, so the newer one wins. A missing or
/// unparseable generation sorts as `None` (older than any real generation), so
/// a legacy record without the field never beats a generation-bearing one.
/// `tiebreak` is the canonical serialization of the summary, a fully-ordered
/// field that keeps selection byte-identical across runs when two versions
/// share both an instant AND a generation (a degenerate tie carrying no
/// freshness signal). Field order matters: the derived `Ord` compares
/// `instant`, then `generation`, then `tiebreak`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CoverageFreshness {
    instant: Option<chrono::DateTime<chrono::Utc>>,
    generation: Option<chrono::DateTime<chrono::Utc>>,
    tiebreak: String,
}

impl CoverageFreshness {
    fn new(
        valid_time: Option<&str>,
        coverage_generation: Option<&str>,
        summary: &CoverageSummary,
    ) -> Self {
        let parse = |s: Option<&str>| {
            s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
        };
        let instant = parse(valid_time);
        let generation = parse(coverage_generation);
        let tiebreak = serde_json::to_string(summary).unwrap_or_default();
        Self {
            instant,
            generation,
            tiebreak,
        }
    }
}

/// Extracts the `coverage_generation` freshness signal (issue #406) from a
/// `ScanCoverage` node record, returning `None` for any other record kind or a
/// coverage node produced before the field existed.
fn coverage_generation_from_record(record: &GraphRecord) -> Option<&str> {
    if let GraphRecord::Node {
        kind: NodeKind::ScanCoverage,
        scan_coverage: Some(payload),
        ..
    } = record
    {
        return payload.coverage_generation.as_deref();
    }
    None
}

/// Builds a [`CoverageSummary`] from a `ScanCoverage` node record, returning
/// `None` for any other record kind or a coverage node missing its payload.
fn coverage_summary_from_record(record: &GraphRecord) -> Option<CoverageSummary> {
    if let GraphRecord::Node {
        id,
        kind: NodeKind::ScanCoverage,
        scan_coverage: Some(payload),
        ..
    } = record
    {
        return Some(CoverageSummary {
            id: id.clone(),
            files_walked: payload.files_walked,
            files_indexed: payload.files_indexed,
            skipped_by_extension: payload.skipped_by_extension.clone(),
            indexed_languages: payload.indexed_languages.clone(),
            coverage_complete: payload.coverage_complete,
        });
    }
    None
}

#[derive(Debug, Default)]
pub(crate) struct InspectCounts {
    records: usize,
    nodes: usize,
    edges: usize,
    tombstones: usize,
    diagnostics: usize,
    schema_versions: BTreeMap<RecordVersion, usize>,
    unknown_schema_versions: BTreeMap<RecordVersion, usize>,
    repositories: Vec<RepositorySummary>,
    /// Scan-coverage summaries, one per `ScanCoverage` node (issue #135),
    /// sorted by record ID for deterministic output.
    coverage: Vec<CoverageSummary>,
    /// Semantic vector-index identity block (issue #104). Only populated on the
    /// `--data-dir` path, where the physical vector index is inspectable; a
    /// `--graph` JSONL carries no index.
    #[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
    semantic_index: Option<SemanticIndexSummary>,
    /// Per-`producer_kind` breakdown; legacy records use key `"legacy_pre_v1"`.
    producer_kinds: BTreeMap<String, usize>,
    /// Per-`egregore_version` breakdown; legacy records use key `"legacy_pre_v1"`.
    egregore_versions: BTreeMap<String, usize>,
}

impl InspectCounts {
    fn from_records(
        records: &[GraphRecord],
        unknown_schema_versions: &[crate::schema_version::UnknownSchemaVersion],
    ) -> Self {
        let mut counts = Self::default();
        // Collapse equal-ID `ScanCoverage` versions to the FRESHEST version on
        // the `--graph` path (issue #135). The freshness key is each coverage
        // node's `valid_time` (= the scan's transaction time, stamped by
        // `with_valid_time_inferred`), parsed to a UTC instant — a serialized,
        // export-preserved, scan-recency-monotonic signal. Physical input/file
        // order is NOT a valid freshness signal: `eg export` (issue #402) writes
        // canonical JSONL after a LEXICOGRAPHIC `lines.sort_unstable()`, so a
        // stale line can sort after the current one and a "keep-last-by-order"
        // rule would then report stale coverage. The tie-break (fully-ordered
        // canonical serialization of the summary) keeps output byte-identical
        // when two versions share a `valid_time` and carry no freshness signal.
        //
        // SAME-SECOND TIES (issue #406, resolved): the full-scan path stamps
        // `valid_time` at SECONDS precision (`src/lib.rs`), so two full scans of
        // the same repo within one UTC second share a `valid_time`. The
        // serialized full-precision `coverage_generation` key on
        // `ScanCoveragePayload` breaks that tie — `CoverageFreshness` compares
        // `valid_time`, THEN `coverage_generation` (both parsed to UTC
        // instants), so the newer of two same-second versions wins
        // deterministically regardless of physical line order (immune to
        // `eg export`'s lexicographic `lines.sort_unstable()`). Residual ties
        // fall through to the content tie-break: two scans within the SAME
        // NANOSECOND, or a legacy coverage record produced before this field
        // (only seconds `valid_time`, `coverage_generation` absent). For those
        // (and any sub-second re-scan on the store side) `eg inspect --data-dir`
        // remains authoritative — it orders by store physical write-order
        // (`inspect_current_records`).
        let mut coverage_latest: BTreeMap<String, (CoverageFreshness, CoverageSummary)> =
            BTreeMap::new();
        for unknown in unknown_schema_versions {
            counts.records += 1;
            *counts
                .unknown_schema_versions
                .entry(unknown.version.clone())
                .or_default() += 1;
        }
        for record in records {
            counts.records += 1;
            if let Err(unknown) = crate::schema_version::validate_record_version(record) {
                *counts
                    .unknown_schema_versions
                    .entry(unknown.version.clone())
                    .or_default() += 1;
                continue;
            }
            *counts
                .schema_versions
                .entry(record_version(record))
                .or_default() += 1;
            match record {
                GraphRecord::Node {
                    id,
                    kind,
                    repository_identity,
                    valid_time,
                    ..
                } => {
                    counts.nodes += 1;
                    if *kind == NodeKind::Diagnostic {
                        counts.diagnostics += 1;
                    }
                    if *kind == NodeKind::ScanCoverage
                        && let Some(summary) = coverage_summary_from_record(record)
                    {
                        let freshness = CoverageFreshness::new(
                            valid_time.as_deref(),
                            coverage_generation_from_record(record),
                            &summary,
                        );
                        match coverage_latest.entry(summary.id.clone()) {
                            std::collections::btree_map::Entry::Occupied(mut slot) => {
                                if freshness > slot.get().0 {
                                    slot.insert((freshness, summary));
                                }
                            }
                            std::collections::btree_map::Entry::Vacant(slot) => {
                                slot.insert((freshness, summary));
                            }
                        }
                    }
                    if *kind == NodeKind::Repository {
                        let identity_summary = repository_identity.as_deref().map_or_else(
                            || "unknown".to_owned(),
                            |p| {
                                use crate::ir::IdentitySource;
                                let source_str = match p.identity_source {
                                    IdentitySource::Remote => "remote",
                                    IdentitySource::LocalRootCommit => "local_root_commit",
                                    IdentitySource::LocalPath => "local_path",
                                    IdentitySource::OperatorOverride => "operator_override",
                                };
                                let canonical = p
                                    .remote_url
                                    .as_deref()
                                    .or(p.root_commit_sha.as_deref())
                                    .or(p.canonical_path.as_deref())
                                    .unwrap_or(p.basename.as_str());
                                format!("{source_str}: {canonical}")
                            },
                        );
                        counts.repositories.push(RepositorySummary {
                            id: id.clone(),
                            identity_summary,
                        });
                    }
                }
                GraphRecord::Edge { .. } => counts.edges += 1,
                GraphRecord::Tombstone { .. } => counts.tombstones += 1,
            }
            // Producer breakdown — legacy records (no `producer` field) go under "legacy_pre_v1".
            let (kind_key, version_key) = record.producer().map_or_else(
                || ("legacy_pre_v1".to_owned(), "legacy_pre_v1".to_owned()),
                |p| {
                    (
                        p.producer_kind.as_str().to_owned(),
                        p.egregore_version.clone(),
                    )
                },
            );
            *counts.producer_kinds.entry(kind_key).or_default() += 1;
            *counts.egregore_versions.entry(version_key).or_default() += 1;
        }
        // Emit the freshest version per stable ScanCoverage ID (see the
        // `coverage_latest` freshness contract above). The BTreeMap orders the
        // result by record ID, so the output is byte-identical across runs
        // regardless of physical iteration order.
        counts.coverage = coverage_latest
            .into_values()
            .map(|(_, summary)| summary)
            .collect();
        counts
    }

    fn from_jsonl(jsonl: &str) -> Result<Self> {
        let report = crate::adapters::records_from_jsonl_report(jsonl)?;
        Ok(Self::from_records(
            &report.records,
            &report.unknown_schema_versions,
        ))
    }

    fn to_json(&self, snapshot_timestamp: &str) -> serde_json::Value {
        let mut json_val = self.counts_json();
        json_val["snapshot_timestamp"] = serde_json::Value::from(snapshot_timestamp);
        json_val
    }

    /// Deterministic embedded-store envelope (issue #125): the shared count
    /// fields plus a `source` descriptor, and deliberately no timestamp so the
    /// output is byte-identical across runs on an unchanged store.
    #[cfg(feature = "embedded-aletheiadb")]
    fn to_json_embedded(&self, data_dir: &str) -> serde_json::Value {
        let mut json_val = self.counts_json();
        json_val["source"] = serde_json::json!({
            "mode": "embedded",
            "data_dir": data_dir,
        });
        // Semantic vector-index identity (issue #104). Allow-list only, and
        // deterministic: identities arrive already deduplicated and sorted.
        #[cfg(feature = "embeddings")]
        if let Some(index) = &self.semantic_index {
            json_val["semantic_index"] = serde_json::json!({
                "index_present": index.index_dimensions.is_some(),
                "index_dimensions": index.index_dimensions,
                "identity_recorded": !index.indexed_models.is_empty(),
                "indexed_models": index
                    .indexed_models
                    .iter()
                    .map(|m| serde_json::json!({
                        "provider": m.provider,
                        "name": m.name,
                        "version": m.version,
                        "dim": m.dim,
                        "content_hash": m.content_hash,
                    }))
                    .collect::<Vec<_>>(),
            });
        }
        json_val
    }

    fn counts_json(&self) -> serde_json::Value {
        let mut schema_versions_obj = serde_json::Map::new();
        for (version, count) in &self.schema_versions {
            let key = format!("{}:{}:{}", version.domain, version.kind, version.version);
            schema_versions_obj.insert(key, serde_json::Value::from(*count));
        }

        let mut unknown_schema_versions_obj = serde_json::Map::new();
        for (version, count) in &self.unknown_schema_versions {
            let key = format!("{}:{}:{}", version.domain, version.kind, version.version);
            unknown_schema_versions_obj.insert(key, serde_json::Value::from(*count));
        }

        let mut structured_counts = serde_json::Map::new();
        for (version, count) in &self.schema_versions {
            let category = domain_category(&version.domain);
            let category_obj = structured_counts
                .entry(category.to_owned())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let serde_json::Value::Object(map) = category_obj {
                let key = format!("{} v{}", version.kind, version.version);
                map.insert(key, serde_json::Value::from(*count));
            }
        }

        serde_json::json!({
            "records": self.records,
            "nodes": self.nodes,
            "edges": self.edges,
            "tombstones": self.tombstones,
            "diagnostics": self.diagnostics,
            "domain_counts": structured_counts,
            "schema_versions": schema_versions_obj,
            "unknown_schema_versions": unknown_schema_versions_obj,
            "repositories": self.repositories.iter().map(|r| serde_json::json!({
                "id": r.id,
                "identity_summary": r.identity_summary
            })).collect::<Vec<_>>(),
            "coverage": self.coverage.iter().map(|c| serde_json::json!({
                "id": c.id,
                "files_walked": c.files_walked,
                "files_indexed": c.files_indexed,
                "skipped_by_extension": c.skipped_by_extension,
                "indexed_languages": c.indexed_languages,
                "coverage_complete": c.coverage_complete
            })).collect::<Vec<_>>(),
            "producer_kinds": self.producer_kinds,
            "egregore_versions": self.egregore_versions
        })
    }
}
