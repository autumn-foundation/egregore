//! Byte-for-byte scan reproducibility comparison (issue #239).
//!
//! [`compare_scan_outputs`] answers "are these two scans of the same
//! repository identical?" at two levels: a byte-for-byte equality check over
//! the canonical JSONL, and — when the bytes differ — a handle-level record
//! diff that names each offending record handle and its first differing
//! field, so a nondeterminism regression points at the record instead of
//! merely "output differs". The `eg verify-scan` CLI drives two scans with
//! pinned transaction time and repository identity, then renders the
//! resulting [`ScanComparison`] as JSON or text.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Cap on the record-level differences carried in a [`ScanComparison`].
///
/// The total [`ScanComparison::difference_count`] is uncapped; only the
/// listed entries are bounded so reports stay small on large repositories.
pub const MAX_REPORTED_DIFFERENCES: usize = 25;

/// Character width for the first/second value previews in a
/// [`RecordDifference`]; longer values are truncated with an ellipsis.
const VALUE_PREVIEW_CHARS: usize = 160;

/// How one record differs between the two scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordDifferenceKind {
    /// The handle exists in both scans but at least one field value differs.
    FieldMismatch,
    /// The handle exists only in the first scan.
    MissingInSecond,
    /// The handle exists only in the second scan.
    MissingInFirst,
}

/// One record-level difference between two scans of the same repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordDifference {
    /// Stable record id (the "handle") identifying the offending record.
    /// When a handle occurs more than once in a scan, occurrences after the
    /// first are suffixed `#2`, `#3`, … so each entry still names one record.
    pub handle: String,
    /// How the record differs.
    pub kind: RecordDifferenceKind,
    /// 1-based JSONL line number where this record occurrence appears: in
    /// the first scan when the record is present there, otherwise in the
    /// second scan.
    pub line: u64,
    /// First differing field in sorted field order ([`RecordDifferenceKind::FieldMismatch`] only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Truncated first-scan value of [`RecordDifference::field`]
    /// ([`RecordDifferenceKind::FieldMismatch`] only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_value: Option<String>,
    /// Truncated second-scan value of [`RecordDifference::field`]
    /// ([`RecordDifferenceKind::FieldMismatch`] only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub second_value: Option<String>,
}

/// Record/node/edge counts for one scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanCounts {
    /// Total JSONL records (non-empty lines).
    pub records: usize,
    /// Records with `"record_type": "node"`.
    pub nodes: usize,
    /// Records with `"record_type": "edge"`.
    pub edges: usize,
}

/// Result of comparing two scans of the same repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanComparison {
    /// True when the two scans are byte-for-byte identical canonical JSONL.
    pub stable: bool,
    /// Counts for the first scan.
    pub first: ScanCounts,
    /// Counts for the second scan.
    pub second: ScanCounts,
    /// BLAKE3 hash (hex) of the first scan's JSONL bytes. It equals the second
    /// scan's hash exactly when [`ScanComparison::stable`] holds, so two
    /// checkouts of the same commit can compare hashes without exchanging
    /// full output.
    pub jsonl_hash: String,
    /// 1-based number of the first JSONL line where the two outputs differ.
    /// `None` when stable. Present on every unstable comparison — including
    /// byte-only divergences with no record-level differences (e.g. record
    /// reordering) — so an unstable verdict always names where the outputs
    /// diverged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_differing_line: Option<u64>,
    /// Byte offset of the first differing byte. `None` when stable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_differing_byte: Option<u64>,
    /// Total record-level differences (uncapped).
    pub difference_count: usize,
    /// True when [`ScanComparison::differences`] was capped at
    /// [`MAX_REPORTED_DIFFERENCES`].
    pub differences_truncated: bool,
    /// The differing records, in sorted handle order (capped).
    pub differences: Vec<RecordDifference>,
}

/// Compare two canonical scan JSONL outputs.
///
/// The primary verdict is byte-for-byte equality of the two outputs;
/// [`ScanComparison::differences`] is the explanation layer, keying records
/// by their stable id ("handle") and reporting the first differing field in
/// sorted field order plus the JSONL line number for each mismatched record.
/// [`ScanComparison::first_differing_line`] is the fallback that locates the
/// divergence even when the bytes differ with no record-level differences
/// (e.g. record reordering).
#[must_use]
pub fn compare_scan_outputs(first_jsonl: &str, second_jsonl: &str) -> ScanComparison {
    let (first_counts, first_records) = index_records(first_jsonl);
    let (second_counts, second_records) = index_records(second_jsonl);

    let mut differences: Vec<RecordDifference> = Vec::new();
    let mut difference_count: usize = 0;
    let mut push_difference = |difference: RecordDifference| {
        difference_count += 1;
        if differences.len() < MAX_REPORTED_DIFFERENCES {
            differences.push(difference);
        }
    };

    let mut handles: BTreeSet<&str> = BTreeSet::new();
    handles.extend(first_records.keys().map(String::as_str));
    handles.extend(second_records.keys().map(String::as_str));
    for handle in handles {
        match (first_records.get(handle), second_records.get(handle)) {
            (Some(first_list), Some(second_list)) => {
                let occurrences = first_list.len().max(second_list.len());
                for index in 0..occurrences {
                    let occurrence_handle = OccurrenceHandle::new(handle, index, occurrences);
                    match (first_list.get(index), second_list.get(index)) {
                        (Some(first_record), Some(second_record)) => {
                            if first_record.value != second_record.value {
                                push_difference(field_mismatch(
                                    &occurrence_handle,
                                    first_record.line,
                                    &first_record.value,
                                    &second_record.value,
                                ));
                                break;
                            }
                        }
                        (Some(first_record), None) => {
                            push_difference(RecordDifference {
                                handle: occurrence_handle.label(),
                                kind: RecordDifferenceKind::MissingInSecond,
                                line: first_record.line,
                                field: None,
                                first_value: None,
                                second_value: None,
                            });
                            break;
                        }
                        (None, Some(second_record)) => {
                            push_difference(RecordDifference {
                                handle: occurrence_handle.label(),
                                kind: RecordDifferenceKind::MissingInFirst,
                                line: second_record.line,
                                field: None,
                                first_value: None,
                                second_value: None,
                            });
                            break;
                        }
                        (None, None) => {}
                    }
                }
            }
            (Some(first_list), None) => {
                for (index, record) in first_list.iter().enumerate() {
                    push_difference(RecordDifference {
                        handle: OccurrenceHandle::new(handle, index, first_list.len()).label(),
                        kind: RecordDifferenceKind::MissingInSecond,
                        line: record.line,
                        field: None,
                        first_value: None,
                        second_value: None,
                    });
                }
            }
            (None, Some(second_list)) => {
                for (index, record) in second_list.iter().enumerate() {
                    push_difference(RecordDifference {
                        handle: OccurrenceHandle::new(handle, index, second_list.len()).label(),
                        kind: RecordDifferenceKind::MissingInFirst,
                        line: record.line,
                        field: None,
                        first_value: None,
                        second_value: None,
                    });
                }
            }
            (None, None) => {}
        }
    }

    let (first_differing_line, first_differing_byte) = first_divergence(first_jsonl, second_jsonl)
        .map_or((None, None), |(line, byte)| (Some(line), Some(byte)));

    // Byte equality is the primary verdict; the record diff above can only
    // explain a byte difference, never overrule byte identity.
    let stable = first_jsonl == second_jsonl && difference_count == 0;
    ScanComparison {
        stable,
        first: first_counts,
        second: second_counts,
        jsonl_hash: blake3::hash(first_jsonl.as_bytes()).to_hex().to_string(),
        first_differing_line,
        first_differing_byte,
        difference_count,
        differences_truncated: difference_count > differences.len(),
        differences,
    }
}

/// First divergence between two JSONL outputs as a 1-based line number and a
/// byte offset, or `None` when the outputs are byte-identical.
///
/// When one output is a strict prefix of the other, the divergence is the
/// line after the shorter output's last line, at the byte offset where the
/// shorter output ends. Newlines are counted over bytes so multi-byte
/// characters can never break the line computation.
fn first_divergence(first_jsonl: &str, second_jsonl: &str) -> Option<(u64, u64)> {
    if first_jsonl == second_jsonl {
        return None;
    }
    let first_bytes = first_jsonl.as_bytes();
    let second_bytes = second_jsonl.as_bytes();
    let byte = first_bytes
        .iter()
        .zip(second_bytes.iter())
        .position(|(first, second)| first != second)
        .unwrap_or_else(|| first_bytes.len().min(second_bytes.len()));
    // The naive newline count is deliberate: this is a cold diagnostic path
    // (it runs only after the two scans already diverged), so a SIMD
    // byte-count dependency is not worth it.
    #[allow(clippy::naive_bytecount)]
    let line = first_bytes[..byte]
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count() as u64
        + 1;
    Some((line, byte as u64))
}

/// Determinism contract for `eg verify-scan` (issue #239).
///
/// Determinism is guaranteed only relative to a fixed producer — this
/// `egregore` binary — plus the transaction time and repository identity the
/// command pins for both scans. A producer upgrade can change extraction
/// output for identical source; that cross-version axis is the complementary
/// `eg query producer-drift` check (issue #234), not this one.
pub const VERIFY_SCAN_DISCLAIMER: &str = "Determinism is guaranteed only relative to a fixed \
    producer (this egregore binary) plus the pinned transaction time and repository identity. \
    Cross-version extraction drift is checked by `eg query producer-drift` (issue #234).";

/// Inputs pinned once per [`run_verify_scan`] invocation and shared by both
/// scans, so the wall clock and the repository identity contribute zero
/// nondeterminism to the check.
#[derive(Debug, Clone)]
pub struct VerifyScanInputs {
    /// The UTC instant captured once and shared by both scans.
    pub transaction_time: String,
    /// Repository id the scan's redaction is scoped to.
    pub repository_id: String,
    /// Operator-supplied repository id override, passed through to the scan.
    pub repo_id_override: Option<String>,
}

/// Run the `eg verify-scan` check with an injectable scan step (issue #239).
///
/// The closure is invoked twice with the same [`VerifyScanInputs`]; the
/// production command passes the real scan/redaction pipeline, while tests
/// inject nondeterminism (e.g. mutate the second scan's output) to exercise
/// the unstable path without touching production behavior. Returns the full
/// report; the caller renders it ([`VerifyScanReport::render_json`] or
/// [`VerifyScanReport::render_text`]) and maps [`VerifyScanReport::ok`] to a
/// process exit code with [`verify_scan_exit_code`].
///
/// # Errors
///
/// Propagates the scan closure's error unchanged when either scan fails; no
/// report is produced in that case.
pub fn run_verify_scan<E>(
    repo_path: &Path,
    repo_id_override: Option<&str>,
    scan: &mut dyn FnMut(&VerifyScanInputs) -> Result<String, E>,
) -> Result<VerifyScanReport, E> {
    // Pin the transaction instant ONCE for the whole invocation: both scans
    // share it, so the wall clock contributes zero nondeterminism.
    let transaction_time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let repo_identity = crate::identity::compute_repository_identity(repo_path, repo_id_override);
    let (repository_id, _) = crate::repository_record_from_identity(&repo_identity);
    let inputs = VerifyScanInputs {
        transaction_time: transaction_time.clone(),
        repository_id: repository_id.clone(),
        repo_id_override: repo_id_override.map(str::to_owned),
    };
    let first_jsonl = scan(&inputs)?;
    let second_jsonl = scan(&inputs)?;
    let comparison = compare_scan_outputs(&first_jsonl, &second_jsonl);
    Ok(VerifyScanReport {
        ok: comparison.stable,
        repo_path: repo_path.to_path_buf(),
        repository_id,
        identity_source: identity_source_name(&repo_identity),
        transaction_time,
        disclaimer: VERIFY_SCAN_DISCLAIMER,
        comparison,
    })
}

/// Process exit code for a verify-scan verdict: 0 when the scans are stable,
/// 1 when they differ.
///
/// The CLI maps [`VerifyScanReport::ok`] through this so the unstable path's
/// exit code is covered by unit tests rather than only by process exit.
#[must_use]
pub const fn verify_scan_exit_code(stable: bool) -> i32 {
    if stable { 0 } else { 1 }
}

/// The `eg verify-scan` report: pinned-scan metadata plus the byte-stability
/// comparison.
#[derive(Debug, Serialize)]
pub struct VerifyScanReport {
    /// Verification verdict; mirrors the flattened comparison's `stable`,
    /// following the repo's `ok` envelope convention (`producer-drift`,
    /// `scan-logs`).
    pub ok: bool,
    /// Repository path that was scanned.
    pub repo_path: PathBuf,
    /// Repository id both scans were pinned to.
    pub repository_id: String,
    /// Snake-case name of the repository identity source.
    pub identity_source: &'static str,
    /// The wall-clock instant captured once and shared by both scans.
    pub transaction_time: String,
    /// The determinism contract this check enforces.
    pub disclaimer: &'static str,
    /// The byte-stability comparison, flattened into the report.
    #[serde(flatten)]
    pub comparison: ScanComparison,
}

impl VerifyScanReport {
    /// Serialize the report as pretty JSON for stdout.
    ///
    /// # Errors
    ///
    /// Returns the [`serde_json::Error`] when serialization fails.
    pub fn render_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deterministic human-readable rendering of the report.
    #[must_use]
    pub fn render_text(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        let _ = writeln!(
            out,
            "verify-scan {}: stable: {}",
            self.repo_path.display(),
            self.comparison.stable
        );
        let _ = writeln!(
            out,
            "  scans compared: 2, transaction_time: {}, identity: {} ({})",
            self.transaction_time, self.identity_source, self.repository_id
        );
        let first = &self.comparison.first;
        let second = &self.comparison.second;
        let _ = writeln!(
            out,
            "  first scan:  {} records ({} nodes, {} edges)",
            first.records, first.nodes, first.edges
        );
        let _ = writeln!(
            out,
            "  second scan: {} records ({} nodes, {} edges)",
            second.records, second.nodes, second.edges
        );
        let _ = writeln!(out, "  differences: {}", self.comparison.difference_count);
        if !self.comparison.stable {
            if let (Some(line), Some(byte)) = (
                self.comparison.first_differing_line,
                self.comparison.first_differing_byte,
            ) {
                let _ = writeln!(out, "  first differing line: {line} (byte offset {byte})");
            }
            if self.comparison.differences.is_empty() {
                let _ = writeln!(
                    out,
                    "  no record-level differences: divergence is in line ordering or formatting, not record values"
                );
            }
        }
        for difference in &self.comparison.differences {
            match difference.kind {
                RecordDifferenceKind::FieldMismatch => {
                    let _ = writeln!(
                        out,
                        "  ~ {} (line {}): field '{}' differs",
                        difference.handle,
                        difference.line,
                        difference.field.as_deref().unwrap_or("?")
                    );
                    if let Some(first_value) = &difference.first_value {
                        let _ = writeln!(out, "      first:  {first_value}");
                    }
                    if let Some(second_value) = &difference.second_value {
                        let _ = writeln!(out, "      second: {second_value}");
                    }
                }
                RecordDifferenceKind::MissingInSecond => {
                    let _ = writeln!(
                        out,
                        "  - {} (line {}): present in first scan, missing in second",
                        difference.handle, difference.line
                    );
                }
                RecordDifferenceKind::MissingInFirst => {
                    let _ = writeln!(
                        out,
                        "  + {} (line {}): missing in first scan, present in second",
                        difference.handle, difference.line
                    );
                }
            }
        }
        if self.comparison.differences_truncated {
            let _ = writeln!(
                out,
                "  ... and {} more",
                self.comparison
                    .difference_count
                    .saturating_sub(self.comparison.differences.len())
            );
        }
        let _ = writeln!(out, "{VERIFY_SCAN_DISCLAIMER}");
        out
    }
}

/// Snake-case name of the repository identity source, matching the
/// `identity_source` values stored on `Repository` nodes.
const fn identity_source_name(identity: &crate::identity::RepositoryIdentity) -> &'static str {
    match identity.payload.identity_source {
        crate::ir::IdentitySource::Remote => "remote",
        crate::ir::IdentitySource::LocalRootCommit => "local_root_commit",
        crate::ir::IdentitySource::LocalPath => "local_path",
        crate::ir::IdentitySource::OperatorOverride => "operator_override",
    }
}

/// A record handle qualified by its occurrence index when a handle appears
/// more than once in a single scan.
struct OccurrenceHandle<'a> {
    handle: &'a str,
    index: usize,
    occurrences: usize,
}

impl<'a> OccurrenceHandle<'a> {
    const fn new(handle: &'a str, index: usize, occurrences: usize) -> Self {
        Self {
            handle,
            index,
            occurrences,
        }
    }

    /// The report label for this occurrence: the bare handle, or the handle
    /// suffixed `#2`, `#3`, … when it occurs more than once in a scan.
    fn label(&self) -> String {
        if self.occurrences > 1 {
            format!("{}#{}", self.handle, self.index + 1)
        } else {
            self.handle.to_owned()
        }
    }
}

/// Build the [`RecordDifferenceKind::FieldMismatch`] entry for two records
/// sharing a handle, naming the first differing field in sorted field order.
fn field_mismatch(
    handle: &OccurrenceHandle<'_>,
    line: u64,
    first_record: &serde_json::Value,
    second_record: &serde_json::Value,
) -> RecordDifference {
    let field = first_differing_field(first_record, second_record);
    let (first_value, second_value) = field.as_deref().map_or((None, None), |field_name| {
        (
            first_record
                .as_object()
                .and_then(|object| object.get(field_name))
                .map(value_preview),
            second_record
                .as_object()
                .and_then(|object| object.get(field_name))
                .map(value_preview),
        )
    });
    RecordDifference {
        handle: handle.label(),
        kind: RecordDifferenceKind::FieldMismatch,
        line,
        field,
        first_value,
        second_value,
    }
}

/// First field, in sorted field order, whose value differs between two
/// records. Returns `None` when both records are non-objects (or identical).
fn first_differing_field(
    first_record: &serde_json::Value,
    second_record: &serde_json::Value,
) -> Option<String> {
    let (Some(first_object), Some(second_object)) =
        (first_record.as_object(), second_record.as_object())
    else {
        return None;
    };
    let mut fields: BTreeSet<&str> = BTreeSet::new();
    fields.extend(first_object.keys().map(String::as_str));
    fields.extend(second_object.keys().map(String::as_str));
    fields
        .into_iter()
        .find(|field| first_object.get(*field) != second_object.get(*field))
        .map(str::to_owned)
}

/// Render a JSON value for a diff preview, truncating long values with an
/// ellipsis so one record can never blow up the report.
fn value_preview(value: &serde_json::Value) -> String {
    let full = value.to_string();
    if full.chars().count() > VALUE_PREVIEW_CHARS {
        let truncated: String = full.chars().take(VALUE_PREVIEW_CHARS).collect();
        format!("{truncated}…")
    } else {
        full
    }
}

/// One JSONL record with its 1-based line number in the scan output.
struct IndexedRecord {
    line: u64,
    value: serde_json::Value,
}

/// Index one scan's JSONL by record handle, counting records/nodes/edges.
///
/// A line that is not a JSON object (or lacks a string `id`) still counts as
/// a record; it is keyed by its raw bytes so a corrupt line can never
/// silently match a well-formed one. Line numbers name lines in the raw
/// output, so blank lines still advance the count even though they hold no
/// record.
fn index_records(jsonl: &str) -> (ScanCounts, BTreeMap<String, Vec<IndexedRecord>>) {
    let mut counts = ScanCounts::default();
    let mut by_handle: BTreeMap<String, Vec<IndexedRecord>> = BTreeMap::new();
    for (index, line) in jsonl.lines().enumerate() {
        let line_number = index as u64 + 1;
        if line.trim().is_empty() {
            continue;
        }
        counts.records += 1;
        let (is_node, is_edge, handle, parsed) = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .map_or((false, false, None, None), |value| {
                let object = value.as_object();
                let record_type = object
                    .and_then(|object| object.get("record_type"))
                    .and_then(serde_json::Value::as_str);
                let handle = object
                    .and_then(|object| object.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                (
                    record_type == Some("node"),
                    record_type == Some("edge"),
                    handle,
                    Some(value),
                )
            });
        if is_node {
            counts.nodes += 1;
        } else if is_edge {
            counts.edges += 1;
        }
        let key = handle.unwrap_or_else(|| line.to_owned());
        let value = parsed.unwrap_or_else(|| serde_json::Value::String(line.to_owned()));
        by_handle.entry(key).or_default().push(IndexedRecord {
            line: line_number,
            value,
        });
    }
    (counts, by_handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn comparison_of(first: &str, second: &str) -> ScanComparison {
        compare_scan_outputs(first, second)
    }

    #[test]
    fn empty_inputs_compare_stable() {
        let comparison = comparison_of("", "");
        assert!(comparison.stable);
        assert_eq!(comparison.difference_count, 0);
        assert_eq!(comparison.first.records, 0);
    }

    #[test]
    fn field_mismatch_names_first_field_in_sorted_order() {
        let first = "{\"id\":\"r1\",\"record_type\":\"node\",\"b\":1,\"a\":1}\n";
        let second = "{\"id\":\"r1\",\"record_type\":\"node\",\"b\":2,\"a\":0}\n";
        let comparison = comparison_of(first, second);
        assert!(!comparison.stable);
        assert_eq!(comparison.difference_count, 1);
        let difference = &comparison.differences[0];
        assert_eq!(difference.handle, "r1");
        assert_eq!(difference.kind, RecordDifferenceKind::FieldMismatch);
        // "a" sorts before "b": the report names the first differing field.
        assert_eq!(difference.field.as_deref(), Some("a"));
        assert_eq!(difference.first_value.as_deref(), Some("1"));
        assert_eq!(difference.second_value.as_deref(), Some("0"));
        // The offending record is on JSONL line 1 of the first scan.
        assert_eq!(difference.line, 1);
    }

    #[test]
    fn duplicate_handles_get_occurrence_suffixes() {
        let first = "{\"id\":\"r1\",\"v\":1}\n{\"id\":\"r1\",\"v\":1}\n";
        let second = "{\"id\":\"r1\",\"v\":1}\n{\"id\":\"r1\",\"v\":2}\n";
        let comparison = comparison_of(first, second);
        assert!(!comparison.stable);
        assert_eq!(comparison.differences.len(), 1);
        assert_eq!(comparison.differences[0].handle, "r1#2");
        assert_eq!(comparison.differences[0].line, 2);
    }

    #[test]
    fn long_values_are_truncated_in_previews() {
        let long = "x".repeat(VALUE_PREVIEW_CHARS + 50);
        let first = format!("{{\"id\":\"r1\",\"big\":\"{long}\"}}\n");
        let second = "{\"id\":\"r1\",\"big\":\"short\"}\n";
        let comparison = comparison_of(&first, second);
        let difference = &comparison.differences[0];
        let preview = difference.first_value.as_deref().expect("preview");
        assert!(
            preview.ends_with('…'),
            "preview should end with an ellipsis"
        );
        assert!(preview.chars().count() <= VALUE_PREVIEW_CHARS + 3);
    }

    #[test]
    fn differences_are_capped_but_counted() {
        let mut first = String::new();
        let mut second = String::new();
        for index in 0..(MAX_REPORTED_DIFFERENCES + 5) {
            let _ = writeln!(first, "{{\"id\":\"r{index}\",\"v\":1}}");
            let _ = writeln!(second, "{{\"id\":\"r{index}\",\"v\":2}}");
        }
        let comparison = comparison_of(&first, &second);
        assert!(!comparison.stable);
        assert_eq!(comparison.difference_count, MAX_REPORTED_DIFFERENCES + 5);
        assert_eq!(comparison.differences.len(), MAX_REPORTED_DIFFERENCES);
        assert!(comparison.differences_truncated);
    }

    #[test]
    fn corrupt_lines_never_match_well_formed_ones() {
        let first = "{\"id\":\"r1\",\"record_type\":\"node\"}\nnot json at all\n";
        let second = "{\"id\":\"r1\",\"record_type\":\"node\"}\n";
        let comparison = comparison_of(first, second);
        assert!(!comparison.stable);
        assert_eq!(comparison.difference_count, 1);
        assert_eq!(
            comparison.differences[0].kind,
            RecordDifferenceKind::MissingInSecond
        );
        // The corrupt line is JSONL line 2 of the first scan.
        assert_eq!(comparison.differences[0].line, 2);
    }

    #[test]
    fn first_divergence_names_line_and_byte() {
        let first = "{\"id\":\"r1\"}\n{\"id\":\"r2\"}\n";
        let second = "{\"id\":\"r1\"}\n{\"id\":\"rX\"}\n";
        let comparison = comparison_of(first, second);
        assert!(!comparison.stable);
        assert_eq!(comparison.first_differing_line, Some(2));
        let expected_byte = "{\"id\":\"r1\"}\n{\"id\":\"r".len() as u64;
        assert_eq!(comparison.first_differing_byte, Some(expected_byte));
    }

    #[test]
    fn first_divergence_handles_prefix_outputs() {
        let first = "{\"id\":\"r1\"}\n";
        let second = "{\"id\":\"r1\"}\n{\"id\":\"r2\"}\n";
        let comparison = comparison_of(first, second);
        assert!(!comparison.stable);
        // The first output ends after line 1: divergence is at line 2,
        // the byte offset where the first output ends.
        assert_eq!(comparison.first_differing_line, Some(2));
        assert_eq!(comparison.first_differing_byte, Some(first.len() as u64));
    }

    #[test]
    fn stable_comparison_has_no_divergence_location() {
        let jsonl = "{\"id\":\"r1\"}\n";
        let comparison = comparison_of(jsonl, jsonl);
        assert!(comparison.stable);
        assert_eq!(comparison.first_differing_line, None);
        assert_eq!(comparison.first_differing_byte, None);
    }

    #[test]
    fn reordered_records_report_divergence_location_without_record_differences() {
        // Same records, different order: byte-unstable, but the
        // handle-indexed diff is empty. The first-diverging-line/byte
        // fallback must still say where the outputs diverged.
        let first = "{\"id\":\"r1\",\"v\":1}\n{\"id\":\"r2\",\"v\":2}\n";
        let second = "{\"id\":\"r2\",\"v\":2}\n{\"id\":\"r1\",\"v\":1}\n";
        let comparison = comparison_of(first, second);
        assert!(!comparison.stable);
        assert_eq!(comparison.difference_count, 0);
        assert!(comparison.differences.is_empty());
        assert_eq!(comparison.first_differing_line, Some(1));
        // `{"id":"r` is 8 bytes; the ninth byte ('1' vs '2') differs.
        assert_eq!(comparison.first_differing_byte, Some(8));
    }

    #[test]
    fn missing_records_carry_their_jsonl_line_number() {
        let first = "{\"id\":\"r1\"}\n{\"id\":\"r2\"}\n";
        let second = "{\"id\":\"r1\"}\n";
        let comparison = comparison_of(first, second);
        assert_eq!(comparison.differences.len(), 1);
        assert_eq!(
            comparison.differences[0].kind,
            RecordDifferenceKind::MissingInSecond
        );
        assert_eq!(comparison.differences[0].line, 2);

        // A record missing from the first scan is located in the second.
        let comparison = comparison_of(second, first);
        assert_eq!(
            comparison.differences[0].kind,
            RecordDifferenceKind::MissingInFirst
        );
        assert_eq!(comparison.differences[0].line, 2);
    }

    #[test]
    fn verify_scan_exit_code_is_zero_only_when_stable() {
        assert_eq!(verify_scan_exit_code(true), 0);
        assert_eq!(verify_scan_exit_code(false), 1);
    }
}
