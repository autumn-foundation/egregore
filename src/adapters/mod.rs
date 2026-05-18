//! Ingestion adapters for graph records.

use std::collections::BTreeMap;

use crate::ir::GraphRecord;

#[cfg(feature = "embedded-aletheiadb")]
mod aletheiadb;

#[cfg(feature = "embedded-aletheiadb")]
pub use aletheiadb::EmbeddedAletheiaSink;

/// Result type for adapter operations.
pub type AdapterResult<T> = std::result::Result<T, AdapterError>;

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

    /// A write succeeded but read-back did not return the same record.
    #[error("read-back verification failed for record {record_id}: {message}")]
    ReadBack {
        /// Graph record ID.
        record_id: String,
        /// Verification failure.
        message: String,
    },
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
    jsonl
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str::<GraphRecord>(line).map_err(|error| AdapterError::Parse {
                line: index + 1,
                message: error.to_string(),
            })
        })
        .collect()
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
    sink.write_record(record)?;
    sink.verify_record(record)
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
