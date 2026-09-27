//! Ingestion adapters for graph records.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    evidence_link_audit::{edge_label_from_wire, is_integrity_checked_evidence_edge},
    ir::GraphRecord,
    query::liveness::Liveness,
    schema_version::{
        RecordLineRead, RecordVersion, UnknownSchemaVersion, domain_from_record_id,
        read_record_line, validate_record_version,
    },
};

#[cfg(feature = "embedded-aletheiadb")]
mod aletheiadb;

pub mod preflight;

#[cfg(feature = "embedded-aletheiadb")]
pub use aletheiadb::EmbeddedAletheiaSink;
#[cfg(feature = "embeddings")]
pub use aletheiadb::SemanticMatch;
#[cfg(feature = "embeddings")]
pub use aletheiadb::compare_semantic_matches;
#[cfg(feature = "embeddings")]
pub use aletheiadb::{ReembedNode, ReembedVectorUpdate, remove_persisted_vector_index};
/// Result type for adapter operations.
pub type AdapterResult<T> = std::result::Result<T, AdapterError>;

/// Stable machine code carried by every embedded write-lease contention refusal.
///
/// Documented in `docs/cli/embedded-concurrency.md` (issue #200); agents match
/// on this code to decide between routing through the daemon and retrying.
pub const STORE_CONTENDED_CODE: &str = "store_contended";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) enum ExpectedRecordState {
    Matched,
    Mismatched,
    Missing,
}

/// Adapter-layer errors.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
pub enum AdapterError {
    /// JSONL parsing failed.
    #[error("failed to parse graph JSONL line {line}: {message}")]
    Parse {
        /// One-based line number.
        line: usize,
        /// Parser error message.
        message: String,
    },

    /// A sink rejected a record.
    #[error("sink rejected record {record_id}: {message}")]
    Rejected {
        /// Graph record ID.
        record_id: String,
        /// Rejection reason.
        message: String,
    },

    /// Another live writer (embedded peer or daemon) holds the embedded
    /// store's exclusive write lease (issue #200).
    ///
    /// The refused open performed no partial or interleaved write. The message
    /// names the holder when it is identifiable and always names the remedy:
    /// route concurrent writers through the daemon, or retry after the current
    /// writer releases the store. The display form is prefixed with the stable
    /// [`STORE_CONTENDED_CODE`] machine code.
    #[error("store_contended: {message}")]
    Contended {
        /// Data directory whose write lease is held.
        data_dir: String,
        /// Diagnosis naming the holder (when known) and the remedy.
        message: String,
    },

    /// A write succeeded but read-back did not return the same record.
    #[error("read-back verification failed for record {record_id}: {message}")]
    ReadBack {
        /// Graph record ID.
        record_id: String,
        /// Verification failure.
        message: String,
    },

    /// A read-back operation exceeded its caller-supplied budget.
    #[error("read-back timed out for record {record_id}")]
    TimedOut {
        /// Graph record ID.
        record_id: String,
    },

    /// A record carried a schema-version tuple unknown to this reader.
    #[error("unknown_schema_version: {version}")]
    UnknownSchemaVersion {
        /// One-based JSONL line number, when available.
        line: Option<usize>,
        /// Unknown schema-version tuple.
        version: RecordVersion,
    },

    /// A backing store refused a write or a persist because a hard capacity
    /// limit was exceeded (issue #439).
    ///
    /// The motivating case is `AletheiaDB` 0.1.1's process-global string
    /// interner, whose non-overridable `MAX_STRING_COUNT` (`100_000`) is hit at
    /// index-persist time by the per-record property-value strings a large
    /// graph interns. Unlike [`AdapterError::Rejected`], this is a fatal,
    /// non-retryable class: the store cannot accept the workload as-is, so the
    /// CLI refuses fast rather than letting the store's background persistence
    /// thread hot-loop on the same error forever.
    #[error("capacity exceeded for {resource} (limit {limit:?}): {detail}")]
    CapacityExceeded {
        /// Human-readable name of the exhausted resource (e.g. `string interner`).
        resource: String,
        /// The exceeded limit when known, `None` when the store did not report it.
        limit: Option<u64>,
        /// The upstream error text, preserved verbatim for diagnostics.
        detail: String,
    },
}

/// Classifies a backing-store error message as an `AletheiaDB` string-interner
/// capacity overflow (issue #439).
///
/// `AletheiaDB` raises `StorageError::CapacityExceeded` from its
/// process-global interner (`core/interning.rs`) with the fixed Display
/// `"Capacity exceeded for {resource}: current={current}, limit={limit} (DoS
/// protection)"` and `resource == "string interner"`. This matcher returns
/// `true` when the message names that exact interner overflow — either by
/// carrying both `"Capacity exceeded"` and `"string interner"`, or by carrying
/// the DoS-protection marker the interner (and only the size/capacity `DoS`
/// guards) emits. Matching is case-sensitive against the strings upstream
/// actually emits.
#[must_use]
pub fn is_string_interner_capacity_error(message: &str) -> bool {
    (message.contains("Capacity exceeded") && message.contains("string interner"))
        || message.contains("DoS protection")
}

/// Destination for graph records.
pub trait GraphSink {
    /// Writes one graph record.
    ///
    /// # Errors
    ///
    /// Returns an error if the sink cannot durably accept the record.
    fn write_record(&mut self, record: &GraphRecord) -> AdapterResult<()>;

    /// Reads a graph record back by its stable ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the sink cannot perform read-back verification.
    fn read_back(&self, record_id: &str) -> AdapterResult<Option<GraphRecord>>;

    /// Verifies that a just-written graph record can be reconstructed.
    ///
    /// # Errors
    ///
    /// Returns an error if read-back is missing or does not match the record.
    fn verify_record(&self, record: &GraphRecord) -> AdapterResult<()> {
        match self.read_back(record.id())? {
            Some(read_back) if read_back == *record => Ok(()),
            Some(_) => Err(AdapterError::ReadBack {
                record_id: record.id().to_owned(),
                message: "record mismatch".to_owned(),
            }),
            None => Err(AdapterError::ReadBack {
                record_id: record.id().to_owned(),
                message: "record missing after write".to_owned(),
            }),
        }
    }
}

/// Summary of an ingest attempt.
#[derive(Debug, Clone, Eq, PartialEq, Default)]
pub struct IngestReport {
    /// Number of records attempted.
    pub attempted: usize,
    /// Number of records written and read back.
    pub succeeded: usize,
    /// Number of records that failed.
    pub failed: usize,
    /// Per-record failures.
    pub failures: Vec<IngestFailure>,
}

impl IngestReport {
    /// Returns true when every attempted record succeeded.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.failed == 0
    }
}

/// One ingest failure.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IngestFailure {
    /// Stable record ID.
    pub record_id: String,
    /// Failure message.
    pub message: String,
}

use serde::{Deserialize, Serialize};

/// Version-aware JSONL read report used by inspect-style commands.
#[derive(Debug, Clone, Eq, PartialEq, Default)]
pub struct JsonlRecordReport {
    /// Records whose `(domain, kind, schema_version)` tuple is known.
    pub records: Vec<GraphRecord>,
    /// Unknown-version records that were rejected before concrete deserialization.
    pub unknown_schema_versions: Vec<UnknownSchemaVersion>,
}

/// Version-aware store inspection report returned by the daemon.
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct InspectStoreReport {
    /// Records whose `(domain, kind, schema_version)` tuple is known.
    pub records: Vec<GraphRecord>,
    /// Unknown-version records that were rejected before concrete deserialization.
    pub unknown_schema_versions: Vec<UnknownSchemaVersion>,
}

/// Ingests graph records into a sink with read-back verification.
///
/// Dangling cross-domain evidence citations are enforced at the default
/// [`DanglingCitationPolicy::Quarantine`]: records whose evidence links cite a
/// target that resolves to no live node are skipped (with a machine-readable
/// diagnostic) while the rest of the batch ingests. See
/// [`ingest_records_with_policy`] for the policy-selected behavior.
#[must_use]
pub fn ingest_records<S: GraphSink>(records: &[GraphRecord], sink: &mut S) -> IngestReport {
    ingest_records_with_policy(records, sink, DanglingCitationPolicy::default())
}

/// Stable machine code carried by every ingest-time dangling-citation
/// diagnostic (issue #241). Agents match on this code to distinguish a
/// provenance rejection from a write failure.
pub const DANGLING_EVIDENCE_CITATION_CODE: &str = "dangling_evidence_citation";

/// Selects how ingest handles a record whose cross-domain evidence citation
/// does not resolve to a live node (issue #241).
///
/// The offline `eg write` step is unchanged — it cannot see the store — so this
/// policy is enforced at the ingest boundary (embedded and daemon adapters),
/// which can see both the destination store and the batch.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum DanglingCitationPolicy {
    /// Skip the citing record and ingest the rest of the batch. This is the
    /// default: it never corrupts an otherwise-valid batch.
    #[default]
    Quarantine,
    /// Write nothing: every record in the batch fails when any evidence
    /// citation dangles.
    RejectBatch,
}

impl DanglingCitationPolicy {
    /// Stable wire string used by the `--dangling-citation-policy` CLI flag
    /// and the daemon ingest payload (`dangling_citation_policy`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Quarantine => "quarantine",
            Self::RejectBatch => "reject-batch",
        }
    }
}

impl std::fmt::Display for DanglingCitationPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One cross-domain evidence citation collected from a record at ingest.
///
/// Mirrors the #217 audit's checked set: standalone edge records whose label is
/// in the closed evidence vocabulary, plus inline [`EvidenceLink`]s (node
/// `evidence_links` and the `user_context` supporting/contradicting evidence
/// fields) whose `relation` names a checked label. Triple-only links (no
/// `target_record_id`) are not ID references and are never collected.
struct EvidenceCitation<'a> {
    /// Stable ID of the record making the claim.
    citing_record_id: &'a str,
    /// Cited target's stable record ID (verbatim, never inferred).
    target_record_id: &'a str,
    /// Wire relation name (e.g. `"OBSERVES"`).
    relation: &'a str,
    /// Target domain: declared by an inline link, derived from the target ID
    /// prefix for standalone edge records.
    target_domain: String,
}

/// Collects every checked evidence citation in the batch, in input order.
fn collect_evidence_citations(records: &[GraphRecord]) -> Vec<EvidenceCitation<'_>> {
    let mut citations = Vec::new();
    for record in records {
        match record {
            GraphRecord::Edge {
                id, label, target, ..
            } => {
                if is_integrity_checked_evidence_edge(*label) {
                    citations.push(EvidenceCitation {
                        citing_record_id: id.as_str(),
                        target_record_id: target.as_str(),
                        relation: label.as_str(),
                        target_domain: domain_from_record_id(target)
                            .unwrap_or_else(|| "unknown".to_owned()),
                    });
                }
            }
            GraphRecord::Node {
                id,
                evidence_links,
                user_context,
                ..
            } => {
                let inline = evidence_links
                    .iter()
                    .flatten()
                    .chain(user_context.supporting_evidence.iter().flatten())
                    .chain(user_context.contradicting_evidence.iter().flatten());
                for link in inline {
                    let Some(label) = edge_label_from_wire(&link.relation) else {
                        continue;
                    };
                    if !is_integrity_checked_evidence_edge(label) {
                        continue;
                    }
                    let Some(target) = link
                        .target_record_id
                        .as_deref()
                        .filter(|target| !target.is_empty())
                    else {
                        // Triple-only link: not an ID reference, out of scope
                        // (mirrors the #217 audit's `unresolvable_link_no_target_id`
                        // tally — reported there, never a broken edge).
                        continue;
                    };
                    citations.push(EvidenceCitation {
                        citing_record_id: id.as_str(),
                        target_record_id: target,
                        relation: link.relation.as_str(),
                        target_domain: link.target_domain.clone(),
                    });
                }
            }
            GraphRecord::Tombstone { .. } => {}
        }
    }
    citations
}

/// Renders the machine-readable dangling-citation diagnostic.
///
/// Names exactly the `(citing_record_id, target_record_id, relation,
/// target_domain)` tuple under the stable [`DANGLING_EVIDENCE_CITATION_CODE`].
/// The diagnostic never echoes observation text or payload values: it is built
/// solely from IDs, the relation wire name, and the domain string.
fn dangling_citation_diagnostic(citation: &EvidenceCitation<'_>) -> String {
    serde_json::json!({
        "code": DANGLING_EVIDENCE_CITATION_CODE,
        "citing_record_id": citation.citing_record_id,
        "target_record_id": citation.target_record_id,
        "relation": citation.relation,
        "target_domain": citation.target_domain,
    })
    .to_string()
}

/// Batch-aware target resolution state for the ingest citation check.
struct CitationResolution<'a> {
    /// Node IDs live in the batch: present as a node and not deleted by a
    /// batch tombstone (latest-write-wins, mirroring the #217 audit).
    batch_live_nodes: BTreeSet<&'a str>,
    /// Batch liveness view (tombstone precedence, revival rule).
    batch_liveness: Liveness<'a>,
    /// Record IDs quarantined by the fixpoint: a batch node that was
    /// quarantined no longer resolves citations, even though it is "present".
    quarantined: BTreeSet<&'a str>,
    /// Memoized store liveness per cited target ID.
    store_cache: BTreeMap<String, bool>,
}

impl<'a> CitationResolution<'a> {
    fn new(records: &'a [GraphRecord]) -> Self {
        let batch_liveness = Liveness::new(records);
        let mut batch_live_nodes: BTreeSet<&'a str> = BTreeSet::new();
        for record in records {
            if let GraphRecord::Node { id, .. } = record {
                batch_live_nodes.insert(id.as_str());
            }
        }
        batch_live_nodes.retain(|id| !batch_liveness.deleted(id));
        Self {
            batch_live_nodes,
            batch_liveness,
            quarantined: BTreeSet::new(),
            store_cache: BTreeMap::new(),
        }
    }

    /// True when `target` resolves to a live node in the union of the batch
    /// and the destination store.
    ///
    /// Precedence is deliberate and matches the post-ingest #217 audit:
    /// 1. a live, non-quarantined batch node resolves;
    /// 2. a batch tombstone deletes the target even when the store still holds
    ///    it live (the tombstone is about to be written);
    /// 3. otherwise the store's current view decides — a live node resolves,
    ///    anything else (absent, tombstoned, an edge sharing the ID, or a
    ///    read failure) does not. Fail-closed: a target whose liveness cannot
    ///    be proven is dangling.
    ///
    /// The sink is passed per call (read-only existence checks) rather than
    /// held in the struct, so the write path can reborrow it mutably after
    /// the fixpoint without fighting the borrow checker.
    fn target_is_live<S: GraphSink>(&mut self, sink: &S, target: &str) -> bool {
        if self.batch_live_nodes.contains(target) && !self.quarantined.contains(target) {
            return true;
        }
        if self.batch_liveness.deleted(target) {
            return false;
        }
        *self
            .store_cache
            .entry(target.to_owned())
            .or_insert_with(|| matches!(sink.read_back(target), Ok(Some(GraphRecord::Node { .. }))))
    }
}

/// Ingests graph records into a sink with read-back verification and
/// ingest-time dangling-citation enforcement (issue #241).
///
/// Every cross-domain evidence citation in the batch is resolved against the
/// union of (a) the destination store and (b) the batch itself, batch-aware and
/// order-independent: a forward reference to a target defined later in the
/// batch resolves. Tombstoned targets are non-resolving, consistent with the
/// #217 audit's definition of "dangling", so ingest and audit agree.
///
/// A citation whose target resolves to nothing quarantines its citing record
/// under [`DanglingCitationPolicy::Quarantine`] (the record never enters the
/// store; the rest of the batch proceeds) or fails the whole batch under
/// [`DanglingCitationPolicy::RejectBatch`] (nothing is written). Quarantine
/// cascades: a record citing a quarantined record is itself quarantined, so no
/// dangling edge can enter the store through a chain of in-batch citations.
/// Every rejection carries the machine-readable
/// [`DANGLING_EVIDENCE_CITATION_CODE`] diagnostic.
#[must_use]
pub fn ingest_records_with_policy<S: GraphSink>(
    records: &[GraphRecord],
    sink: &mut S,
    policy: DanglingCitationPolicy,
) -> IngestReport {
    let citations = collect_evidence_citations(records);
    let mut resolution = CitationResolution::new(records);

    // Group citations by citing record (deterministic order); remember each
    // record's first citation for its diagnostic.
    let mut by_citer: BTreeMap<&str, Vec<&EvidenceCitation<'_>>> = BTreeMap::new();
    for citation in &citations {
        by_citer
            .entry(citation.citing_record_id)
            .or_default()
            .push(citation);
    }

    // Quarantine fixpoint: a record is quarantined when any of its citations
    // fails to resolve; quarantining a batch node can dangle citations that
    // previously resolved through it, so iterate to a fixed point. Each
    // quarantined record keeps the diagnostic for its first dangling citation
    // in input order.
    let mut diagnostics: BTreeMap<&str, String> = BTreeMap::new();
    loop {
        let mut newly: Vec<(&str, String)> = Vec::new();
        for (&citer, cites) in &by_citer {
            if resolution.quarantined.contains(citer) {
                continue;
            }
            if let Some(citation) = cites
                .iter()
                .find(|citation| !resolution.target_is_live(sink, citation.target_record_id))
            {
                newly.push((citer, dangling_citation_diagnostic(citation)));
            }
        }
        if newly.is_empty() {
            break;
        }
        for (citer, diagnostic) in newly {
            resolution.quarantined.insert(citer);
            diagnostics.insert(citer, diagnostic);
        }
    }

    let diagnostic_for = |citer: &str| -> String {
        diagnostics.get(citer).cloned().unwrap_or_else(|| {
            // Unreachable: `citer` is quarantined only via a dangling citation.
            format!("{DANGLING_EVIDENCE_CITATION_CODE}: no dangling citation found")
        })
    };

    if policy == DanglingCitationPolicy::RejectBatch && !resolution.quarantined.is_empty() {
        let mut report = IngestReport::default();
        let first_citer = resolution
            .quarantined
            .iter()
            .next()
            .copied()
            .unwrap_or("<unknown>");
        for record in ordered_records(records) {
            report.attempted += 1;
            report.failed += 1;
            let message = if resolution.quarantined.contains(record.id()) {
                diagnostic_for(record.id())
            } else {
                format!(
                    "batch rejected under reject-batch policy: record '{first_citer}' cites \
                     dangling cross-domain evidence; no records were written"
                )
            };
            report.failures.push(IngestFailure {
                record_id: record.id().to_owned(),
                message,
            });
        }
        return report;
    }

    let mut report = IngestReport::default();
    for record in ordered_records(records) {
        report.attempted += 1;
        if resolution.quarantined.contains(record.id()) {
            report.failed += 1;
            report.failures.push(IngestFailure {
                record_id: record.id().to_owned(),
                message: diagnostic_for(record.id()),
            });
            continue;
        }
        match write_and_verify(record, sink) {
            Ok(()) => report.succeeded += 1,
            Err(error) => {
                report.failed += 1;
                report.failures.push(IngestFailure {
                    record_id: record.id().to_owned(),
                    message: error.to_string(),
                });
            }
        }
    }
    report
}

/// Parses graph records from JSON Lines.
///
/// # Errors
///
/// Returns an error when any non-empty line is not a graph record.
pub fn records_from_jsonl(jsonl: &str) -> AdapterResult<Vec<GraphRecord>> {
    let mut records = Vec::new();
    for (index, line) in jsonl
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        match read_record_line(line).map_err(|error| AdapterError::Parse {
            line: index + 1,
            message: error.to_string(),
        })? {
            RecordLineRead::Record(record) => records.push(*record),
            RecordLineRead::UnknownSchemaVersion(unknown) => {
                return Err(AdapterError::UnknownSchemaVersion {
                    line: Some(index + 1),
                    version: unknown.version,
                });
            }
        }
    }
    Ok(records)
}

/// Parses graph records from JSON Lines while preserving unknown-version counts.
///
/// # Errors
///
/// Returns an error when a non-empty line is not valid graph-record JSON. Lines
/// with unknown future schema versions are reported in the returned summary.
pub fn records_from_jsonl_report(jsonl: &str) -> AdapterResult<JsonlRecordReport> {
    let mut report = JsonlRecordReport::default();
    for (index, line) in jsonl
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        match read_record_line(line).map_err(|error| AdapterError::Parse {
            line: index + 1,
            message: error.to_string(),
        })? {
            RecordLineRead::Record(record) => report.records.push(*record),
            RecordLineRead::UnknownSchemaVersion(unknown) => {
                report.unknown_schema_versions.push(unknown);
            }
        }
    }
    Ok(report)
}

/// Sink that records successful writes without touching external storage.
#[derive(Debug, Default)]
pub struct DryRunSink {
    records: BTreeMap<String, GraphRecord>,
}

impl GraphSink for DryRunSink {
    fn write_record(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        self.records.insert(record.id().to_owned(), record.clone());
        Ok(())
    }

    fn read_back(&self, record_id: &str) -> AdapterResult<Option<GraphRecord>> {
        Ok(self.records.get(record_id).cloned())
    }
}

/// Test sink that can simulate partial write failures.
#[derive(Debug, Default)]
pub struct FakeSink {
    records: BTreeMap<String, GraphRecord>,
    fail_after: Option<usize>,
    written: usize,
}

impl FakeSink {
    /// Creates a fake sink that rejects every write after `limit` successes.
    #[must_use]
    pub const fn fail_after(limit: usize) -> Self {
        Self {
            records: BTreeMap::new(),
            fail_after: Some(limit),
            written: 0,
        }
    }
}

impl GraphSink for FakeSink {
    fn write_record(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        if self.fail_after.is_some_and(|limit| self.written >= limit) {
            return Err(AdapterError::Rejected {
                record_id: record.id().to_owned(),
                message: "fake adapter failure".to_owned(),
            });
        }

        self.records.insert(record.id().to_owned(), record.clone());
        self.written += 1;
        Ok(())
    }

    fn read_back(&self, record_id: &str) -> AdapterResult<Option<GraphRecord>> {
        Ok(self.records.get(record_id).cloned())
    }
}

fn write_and_verify<S: GraphSink>(record: &GraphRecord, sink: &mut S) -> AdapterResult<()> {
    validate_adapter_record_version(record)?;
    sink.write_record(record)?;
    sink.verify_record(record)
}

pub(crate) fn validate_adapter_record_version(record: &GraphRecord) -> AdapterResult<()> {
    validate_record_version(record).map_err(|unknown| AdapterError::UnknownSchemaVersion {
        line: None,
        version: unknown.version,
    })
}

fn ordered_records(records: &[GraphRecord]) -> Vec<&GraphRecord> {
    let mut ordered = records.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|record| match record {
        GraphRecord::Node { .. } => (0_u8, record.id()),
        GraphRecord::Edge { .. } => (1_u8, record.id()),
        GraphRecord::Tombstone { .. } => (2_u8, record.id()),
    });
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, EvidenceLink, NodeKind, SCHEMA_VERSION,
    };

    /// An observation node carrying one inline evidence link (issue #241).
    fn citing_observation(id: &str, target: &str, relation: &str, body: &str) -> GraphRecord {
        let link = EvidenceLink {
            target_record_id: Some(target.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: relation.to_owned(),
            confidence: "0.9".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        };
        let mut record = GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "observation".to_owned(),
        );
        if let GraphRecord::Node {
            evidence_links,
            text,
            schema_version,
            ..
        } = &mut record
        {
            *evidence_links = Some(vec![link]);
            *text = Some(body.to_owned());
            // Agent-memory observations validate against
            // AGENT_MEMORY_SCHEMA_VERSION, not the codegraph SCHEMA_VERSION
            // that GraphRecord::node stamps.
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        }
        record
    }

    /// A plain node with no evidence links (a citation target).
    fn plain_node(id: &str, kind: NodeKind) -> GraphRecord {
        let mut record =
            GraphRecord::node(id.to_owned(), kind, None, None, None, "node".to_owned());
        if let GraphRecord::Node { schema_version, .. } = &mut record {
            *schema_version = match domain_from_record_id(id).as_deref() {
                Some("agent_memory") => AGENT_MEMORY_SCHEMA_VERSION,
                _ => SCHEMA_VERSION,
            };
        }
        record
    }

    /// A tombstone record deleting `deleted_id`.
    fn tombstone(id: &str, deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: id.to_owned(),
            schema_version: 1,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    /// A standalone edge record.
    fn edge_record(id: &str, label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::Edge {
            id: id.to_owned(),
            schema_version: 1,
            label,
            source: source.to_owned(),
            target: target.to_owned(),
            confidence: None,
            resolution: None,
            frame_resolution: None,
            frame_index: None,
            basis: None,
            call_site_spans: None,
            is_exhaustive: None,
            temporal: None,
            summary: "edge".to_owned(),
            producer: None,
        }
    }

    /// Extracts the machine-readable dangling-citation diagnostic from a
    /// failure message, asserting it names the expected tuple and never
    /// echoes payload text.
    fn assert_diagnostic(
        message: &str,
        citing: &str,
        target: &str,
        relation: &str,
        domain: &str,
        forbidden_payload: &str,
    ) {
        assert!(
            !message.contains(forbidden_payload),
            "diagnostic must never echo payload text: {message}"
        );
        let value: serde_json::Value =
            serde_json::from_str(message).expect("diagnostic must be machine-readable JSON");
        assert_eq!(value["code"], "dangling_evidence_citation");
        assert_eq!(value["citing_record_id"], citing);
        assert_eq!(value["target_record_id"], target);
        assert_eq!(value["relation"], relation);
        assert_eq!(value["target_domain"], domain);
    }

    #[test]
    fn quarantine_policy_skips_citing_record_and_continues_batch() {
        let mut sink = FakeSink::default();
        let observation = citing_observation(
            "agent_memory:v1:obs1",
            "codegraph:v1:ghost",
            "OBSERVES",
            "SENTINEL_SECRET_BODY",
        );
        let agent = plain_node("agent_memory:v1:agent1", NodeKind::Agent);
        let records = vec![observation, agent];

        let report =
            ingest_records_with_policy(&records, &mut sink, DanglingCitationPolicy::Quarantine);

        assert_eq!(report.attempted, 2);
        assert_eq!(report.succeeded, 1);
        assert_eq!(report.failed, 1);
        // The citing record never entered the store …
        assert!(sink.read_back("agent_memory:v1:obs1").unwrap().is_none());
        // … while the valid record did.
        assert!(sink.read_back("agent_memory:v1:agent1").unwrap().is_some());
        assert_eq!(report.failures.len(), 1);
        let failure = &report.failures[0];
        assert_eq!(failure.record_id, "agent_memory:v1:obs1");
        assert_diagnostic(
            &failure.message,
            "agent_memory:v1:obs1",
            "codegraph:v1:ghost",
            "OBSERVES",
            "codegraph",
            "SENTINEL_SECRET_BODY",
        );
    }

    #[test]
    fn reject_batch_policy_writes_nothing_on_dangling_citation() {
        let mut sink = FakeSink::default();
        let observation = citing_observation(
            "agent_memory:v1:obs1",
            "codegraph:v1:ghost",
            "OBSERVES",
            "SENTINEL_SECRET_BODY",
        );
        let agent = plain_node("agent_memory:v1:agent1", NodeKind::Agent);
        let records = vec![observation, agent];

        let report =
            ingest_records_with_policy(&records, &mut sink, DanglingCitationPolicy::RejectBatch);

        assert_eq!(report.attempted, 2);
        assert_eq!(report.succeeded, 0);
        assert_eq!(report.failed, 2);
        assert!(sink.read_back("agent_memory:v1:obs1").unwrap().is_none());
        assert!(sink.read_back("agent_memory:v1:agent1").unwrap().is_none());
        assert_eq!(report.failures.len(), 2);
        let citing = report
            .failures
            .iter()
            .find(|f| f.record_id == "agent_memory:v1:obs1")
            .expect("citing record must carry the diagnostic");
        assert_diagnostic(
            &citing.message,
            "agent_memory:v1:obs1",
            "codegraph:v1:ghost",
            "OBSERVES",
            "codegraph",
            "SENTINEL_SECRET_BODY",
        );
    }

    #[test]
    fn forward_reference_later_in_batch_resolves() {
        let mut sink = FakeSink::default();
        // Citing record FIRST, target later: batch-aware, not order-sensitive.
        let observation = citing_observation(
            "agent_memory:v1:obs1",
            "codegraph:v1:real",
            "OBSERVES",
            "body",
        );
        let target = plain_node("codegraph:v1:real", NodeKind::Symbol);
        let records = vec![observation, target];

        let report =
            ingest_records_with_policy(&records, &mut sink, DanglingCitationPolicy::Quarantine);

        assert!(report.is_success(), "{report:?}");
        assert!(sink.read_back("agent_memory:v1:obs1").unwrap().is_some());
    }

    #[test]
    fn target_present_in_store_resolves() {
        let mut sink = FakeSink::default();
        sink.write_record(&plain_node("codegraph:v1:real", NodeKind::Symbol))
            .unwrap();
        let observation = citing_observation(
            "agent_memory:v1:obs1",
            "codegraph:v1:real",
            "OBSERVES",
            "body",
        );

        let report = ingest_records_with_policy(
            std::slice::from_ref(&observation),
            &mut sink,
            DanglingCitationPolicy::Quarantine,
        );

        assert!(report.is_success(), "{report:?}");
    }

    #[test]
    fn tombstoned_target_is_dangling() {
        let mut sink = FakeSink::default();
        // The store holds only a tombstone for the cited target.
        sink.write_record(&tombstone("tombstone:v1:t1", "codegraph:v1:dead"))
            .unwrap();
        let observation = citing_observation(
            "agent_memory:v1:obs1",
            "codegraph:v1:dead",
            "OBSERVES",
            "body",
        );

        let report = ingest_records_with_policy(
            std::slice::from_ref(&observation),
            &mut sink,
            DanglingCitationPolicy::Quarantine,
        );

        assert_eq!(report.failed, 1);
        assert!(sink.read_back("agent_memory:v1:obs1").unwrap().is_none());
        assert_eq!(report.failures[0].record_id, "agent_memory:v1:obs1");
    }

    #[test]
    fn tombstone_in_batch_makes_target_dangling() {
        let mut sink = FakeSink::default();
        let observation = citing_observation(
            "agent_memory:v1:obs1",
            "codegraph:v1:doomed",
            "OBSERVES",
            "body",
        );
        let records = vec![
            observation,
            plain_node("codegraph:v1:doomed", NodeKind::Symbol),
            tombstone("tombstone:v1:t1", "codegraph:v1:doomed"),
        ];

        let report =
            ingest_records_with_policy(&records, &mut sink, DanglingCitationPolicy::Quarantine);

        assert_eq!(report.failed, 1);
        assert_eq!(report.failures[0].record_id, "agent_memory:v1:obs1");
    }

    #[test]
    fn standalone_evidence_edge_with_dangling_target_is_quarantined() {
        let mut sink = FakeSink::default();
        let edge = edge_record(
            "agent_memory:v1:edge1",
            EdgeLabel::Observes,
            "agent_memory:v1:obs1",
            "codegraph:v1:ghost",
        );
        let records = vec![
            edge,
            plain_node("agent_memory:v1:obs1", NodeKind::Observation),
        ];

        let report =
            ingest_records_with_policy(&records, &mut sink, DanglingCitationPolicy::Quarantine);

        assert_eq!(report.failed, 1);
        assert_eq!(report.failures[0].record_id, "agent_memory:v1:edge1");
        assert!(sink.read_back("agent_memory:v1:edge1").unwrap().is_none());
        // The edge's source node (no dangling citations of its own) still ingests.
        assert!(sink.read_back("agent_memory:v1:obs1").unwrap().is_some());
    }

    #[test]
    fn quarantine_cascades_to_records_citing_a_quarantined_record() {
        let mut sink = FakeSink::default();
        // obs2 cites obs1; obs1 cites a ghost. Both must be quarantined —
        // the edge to a never-persisted record is itself dangling.
        let obs1 = citing_observation(
            "agent_memory:v1:obs1",
            "codegraph:v1:ghost",
            "OBSERVES",
            "body",
        );
        let obs2 = citing_observation(
            "agent_memory:v1:obs2",
            "agent_memory:v1:obs1",
            "OBSERVES",
            "body",
        );
        let records = vec![obs1, obs2];

        let report =
            ingest_records_with_policy(&records, &mut sink, DanglingCitationPolicy::Quarantine);

        assert_eq!(report.failed, 2);
        assert!(sink.read_back("agent_memory:v1:obs1").unwrap().is_none());
        assert!(sink.read_back("agent_memory:v1:obs2").unwrap().is_none());
    }

    #[test]
    fn non_evidence_edges_are_not_citation_checked() {
        let mut sink = FakeSink::default();
        // Codegraph topology (#103's territory) is out of scope for #241.
        let edge = edge_record(
            "codegraph:v1:edge1",
            EdgeLabel::Contains,
            "codegraph:v1:parent",
            "codegraph:v1:ghost",
        );

        let report = ingest_records_with_policy(
            std::slice::from_ref(&edge),
            &mut sink,
            DanglingCitationPolicy::Quarantine,
        );

        assert!(report.is_success(), "{report:?}");
    }

    #[test]
    fn triple_only_links_without_target_id_are_skipped() {
        let mut sink = FakeSink::default();
        let mut record = plain_node("agent_memory:v1:obs1", NodeKind::Observation);
        if let GraphRecord::Node { evidence_links, .. } = &mut record {
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: None,
                target_domain: "codegraph".to_owned(),
                relation: "OBSERVES".to_owned(),
                confidence: "0.9".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: Some("src/main.rs".to_owned()),
                target_span: None,
                target_git_commit: Some("abc123".to_owned()),
            }]);
        }

        let report = ingest_records_with_policy(
            std::slice::from_ref(&record),
            &mut sink,
            DanglingCitationPolicy::Quarantine,
        );

        assert!(report.is_success(), "{report:?}");
    }

    #[test]
    fn no_dangling_links_enter_store_across_fixture_run() {
        // The #241 success metric: ingest, then re-run the #217 audit over
        // what the store holds and find zero dangling cross-domain links.
        let mut sink = FakeSink::default();
        let records = vec![
            citing_observation(
                "agent_memory:v1:obs1",
                "codegraph:v1:ghost",
                "OBSERVES",
                "SENTINEL_SECRET_BODY",
            ),
            citing_observation(
                "agent_memory:v1:obs2",
                "codegraph:v1:real",
                "VALIDATED_BY",
                "backed",
            ),
            plain_node("codegraph:v1:real", NodeKind::Symbol),
        ];

        let report =
            ingest_records_with_policy(&records, &mut sink, DanglingCitationPolicy::Quarantine);
        assert_eq!(report.failed, 1);

        let stored: Vec<GraphRecord> = sink.records.values().cloned().collect();
        let audit = crate::evidence_link_audit::run_evidence_link_audit(&stored);
        assert!(audit.ok, "{audit:?}");
        assert_eq!(audit.broken_edge_count, 0);
    }

    #[test]
    fn default_policy_is_quarantine() {
        assert_eq!(
            DanglingCitationPolicy::default(),
            DanglingCitationPolicy::Quarantine
        );
        assert_eq!(DanglingCitationPolicy::Quarantine.as_str(), "quarantine");
        assert_eq!(DanglingCitationPolicy::RejectBatch.as_str(), "reject-batch");
    }

    #[test]
    fn classifies_real_upstream_interner_overflow_string() {
        // The exact Display AletheiaDB 0.1.1 emits for a string-interner
        // overflow (core/interning.rs -> core/error.rs CapacityExceeded).
        let upstream =
            "Capacity exceeded for string interner: current=100000, limit=100000 (DoS protection)";
        assert!(is_string_interner_capacity_error(upstream));
    }

    #[test]
    fn classifies_on_dos_protection_marker_alone() {
        assert!(is_string_interner_capacity_error(
            "some wrapper: current=100000, limit=100000 (DoS protection)"
        ));
    }

    #[test]
    fn classifies_on_capacity_plus_interner_without_dos_marker() {
        assert!(is_string_interner_capacity_error(
            "Capacity exceeded for string interner (limit reached)"
        ));
    }

    #[test]
    fn rejects_unrelated_error_messages() {
        assert!(!is_string_interner_capacity_error(
            "sink rejected record codegraph:v1:abc: node label mismatch"
        ));
        assert!(!is_string_interner_capacity_error(
            "failed to parse graph JSONL line 3: expected value"
        ));
        // A different capacity error (not the interner, no DoS marker) is not
        // classified as the interner overflow.
        assert!(!is_string_interner_capacity_error(
            "Capacity exceeded for transaction operations: current=5, limit=5"
        ));
    }

    #[test]
    fn capacity_exceeded_display_names_resource_and_limit() {
        let error = AdapterError::CapacityExceeded {
            resource: "string interner".to_owned(),
            limit: Some(100_000),
            detail: "Capacity exceeded for string interner: current=100000, limit=100000 \
                     (DoS protection)"
                .to_owned(),
        };
        let rendered = error.to_string();
        assert!(rendered.contains("capacity exceeded for string interner"));
        assert!(rendered.contains("100000"));
    }
}
