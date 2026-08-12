//! Ingestion adapters for graph records.

use std::collections::BTreeMap;

use crate::{
    ir::GraphRecord,
    schema_version::{
        RecordLineRead, RecordVersion, UnknownSchemaVersion, read_record_line,
        validate_record_version,
    },
};

#[cfg(feature = "embedded-aletheiadb")]
mod aletheiadb;

pub mod preflight;

#[cfg(feature = "embedded-aletheiadb")]
pub use aletheiadb::EmbeddedAletheiaSink;
#[cfg(feature = "embeddings")]
pub use aletheiadb::SemanticMatch;
/// Test-only fixture affordances (issue #486). Not part of the supported API.
#[cfg(feature = "embedded-aletheiadb")]
#[doc(hidden)]
pub use aletheiadb::fixtures;

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
#[must_use]
pub fn ingest_records<S: GraphSink>(records: &[GraphRecord], sink: &mut S) -> IngestReport {
    let mut report = IngestReport::default();
    for record in ordered_records(records) {
        report.attempted += 1;
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
