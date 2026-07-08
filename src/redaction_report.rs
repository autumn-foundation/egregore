//! At-import redaction report — issue #266.
//!
//! Builds a verifiable, secret-free summary of exactly what an import pass
//! redacted, so an operator can confirm the redaction pipeline actually fired
//! rather than grepping output JSONL for markers. The report is derived from
//! the `<REDACTED:secret_class:hash_prefix>` markers stored on the emitted
//! records themselves, so every entry provably ties to a persisted marker:
//! a field that was absent is never reported as redacted, and a field that
//! carries a marker is always reported.
//!
//! The report is redaction-safe by construction: it carries record IDs, field
//! paths, secret-class names, marker hash prefixes, source-artifact handles,
//! and counts — never raw payloads or secret values.
//!
//! Determinism: records are scanned in emission order, entries are sorted into
//! canonical order, and per-class counts use a [`BTreeMap`], so re-running the
//! same import produces a byte-identical serialized report.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::{
    ir::GraphRecord,
    redaction::{parse_redaction_markers, sensitive_fields},
};

/// Schema version of the redaction report envelope.
pub const REDACTION_REPORT_SCHEMA_VERSION: u32 = 1;

/// `redaction` value for an import that applied a redaction policy.
pub const REDACTION_ENABLED: &str = "enabled";

/// `redaction` value for a passthrough (no-redaction) import.
///
/// Distinguishes "redaction ran and found nothing" (enabled, total `0`) from
/// "redaction never ran" so an empty report cannot be mistaken for a clean
/// redacted pass.
pub const REDACTION_DISABLED: &str = "disabled";

/// One redaction accounted for by the report.
///
/// The derived `Ord` doubles as the canonical output order: entries sort by
/// owning record ID, then field path, then class, then hash prefix.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct RedactionReportEntry {
    /// Stable ID of the record carrying the redacted field.
    pub record_id: String,
    /// Redacted field path (e.g. `stdout_handle.inline`), matching the
    /// sensitive-field index in `docs/cli/redaction.md`.
    pub field_path: String,
    /// Named secret class from `docs/schema/redaction.md §Secret Classes`.
    pub secret_class: &'static str,
    /// BLAKE3 hash prefix from the stored marker — ties the entry to the
    /// `<REDACTED:secret_class:hash_prefix>` marker without revealing the
    /// secret.
    pub hash_prefix: String,
}

/// Structured JSON redaction summary for one import pass.
///
/// Serialization is deterministic: fixed field order, sorted entries, and
/// sorted per-class counts. `policy_version` is serialized explicitly (as
/// `null` when disabled) so the enabled/disabled distinction is machine-
/// readable without field-presence heuristics.
#[derive(Debug, Clone, Serialize)]
pub struct RedactionReport {
    /// Report envelope schema version.
    pub schema_version: u32,
    /// [`REDACTION_ENABLED`] or [`REDACTION_DISABLED`].
    pub redaction: &'static str,
    /// Redaction policy version applied by the import (`None` when disabled).
    pub policy_version: Option<String>,
    /// Source artifact path of the import, from the emitted records.
    pub source_artifact_path: Option<String>,
    /// BLAKE3 hash of the raw source artifact bytes, from the emitted records.
    pub source_artifact_hash: Option<String>,
    /// Total number of redaction markers accounted for.
    pub total: u64,
    /// Marker count per secret class, sorted by class name.
    pub counts_by_class: BTreeMap<&'static str, u64>,
    /// One entry per stored marker occurrence, in canonical order.
    pub entries: Vec<RedactionReportEntry>,
}

/// Builds the redaction report for one import pass over its emitted records.
///
/// `policy_version` is the policy the import applied (for example
/// `ImportOptions::policy_version`): `Some` marks the report
/// [`REDACTION_ENABLED`] and scans every record's sensitive fields for stored
/// markers; `None` marks it [`REDACTION_DISABLED`] and reports no entries,
/// because no redaction pass fired.
///
/// The scan walks the same sensitive-field index the redaction gate enforces
/// ([`crate::redaction::sensitive_fields`]), so absent fields are never
/// reported and every reported entry corresponds to a marker present in a
/// stored field. The output is deterministic: byte-identical serialized
/// reports across repeated imports of the same source artifact.
#[must_use]
pub fn build_redaction_report(
    records: &[GraphRecord],
    policy_version: Option<&str>,
) -> RedactionReport {
    let (source_artifact_path, source_artifact_hash) = source_identity(records);

    let mut entries: Vec<RedactionReportEntry> = Vec::new();
    if policy_version.is_some() {
        for record in records {
            let GraphRecord::Node { id, .. } = record else {
                continue;
            };
            for (field_path, value) in sensitive_fields(record) {
                for (class, hash_prefix) in parse_redaction_markers(value) {
                    entries.push(RedactionReportEntry {
                        record_id: id.clone(),
                        field_path: field_path.clone(),
                        secret_class: class.as_str(),
                        hash_prefix,
                    });
                }
            }
        }
    }
    entries.sort();

    let mut counts_by_class: BTreeMap<&'static str, u64> = BTreeMap::new();
    for entry in &entries {
        *counts_by_class.entry(entry.secret_class).or_insert(0) += 1;
    }

    RedactionReport {
        schema_version: REDACTION_REPORT_SCHEMA_VERSION,
        redaction: if policy_version.is_some() {
            REDACTION_ENABLED
        } else {
            REDACTION_DISABLED
        },
        policy_version: policy_version.map(str::to_owned),
        source_artifact_path,
        source_artifact_hash,
        total: entries.len() as u64,
        counts_by_class,
        entries,
    }
}

/// Extracts the source artifact path and hash from the first node that
/// carries them (identical across all records of one import).
fn source_identity(records: &[GraphRecord]) -> (Option<String>, Option<String>) {
    records
        .iter()
        .find_map(|record| match record {
            GraphRecord::Node {
                source_artifact_path: Some(path),
                source_artifact_hash: Some(hash),
                ..
            } => Some((Some(path.clone()), Some(hash.clone()))),
            _ => None,
        })
        .unwrap_or((None, None))
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::ir::NodeKind;

    fn node_with_text(id: &str, text: &str) -> GraphRecord {
        let mut record = GraphRecord::node(
            id.to_owned(),
            NodeKind::CommandRun,
            None,
            None,
            None,
            "summary".to_owned(),
        );
        if let GraphRecord::Node { text: t, .. } = &mut record {
            *t = Some(text.to_owned());
        }
        record
    }

    #[test]
    fn enabled_report_counts_stored_markers() {
        let records = vec![node_with_text("rec-1", "<REDACTED:api_token:abc123def456>")];
        let report = build_redaction_report(&records, Some("v1"));
        assert_eq!(report.redaction, REDACTION_ENABLED);
        assert_eq!(report.total, 1);
        assert_eq!(report.counts_by_class.get("api_token"), Some(&1));
        assert_eq!(report.entries[0].record_id, "rec-1");
        assert_eq!(report.entries[0].field_path, "text");
        assert_eq!(report.entries[0].hash_prefix, "abc123def456");
    }

    #[test]
    fn disabled_report_has_no_entries() {
        let records = vec![node_with_text("rec-1", "<REDACTED:api_token:abc123def456>")];
        let report = build_redaction_report(&records, None);
        assert_eq!(report.redaction, REDACTION_DISABLED);
        assert!(report.policy_version.is_none());
        assert_eq!(report.total, 0);
        assert!(report.entries.is_empty());
    }

    #[test]
    fn clean_records_report_zero_total() {
        let records = vec![node_with_text("rec-1", "no markers here")];
        let report = build_redaction_report(&records, Some("v1"));
        assert_eq!(report.redaction, REDACTION_ENABLED);
        assert_eq!(report.total, 0);
        assert!(report.entries.is_empty());
        assert!(report.counts_by_class.is_empty());
    }

    #[test]
    fn entries_sort_into_canonical_order() {
        let records = vec![
            node_with_text("rec-b", "<REDACTED:email:bbbb22223333>"),
            node_with_text("rec-a", "<REDACTED:env_secret:aaaa11112222>"),
        ];
        let report = build_redaction_report(&records, Some("v1"));
        assert_eq!(report.entries[0].record_id, "rec-a");
        assert_eq!(report.entries[1].record_id, "rec-b");
    }
}
