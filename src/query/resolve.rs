//! Record-id handle resolution core (issue #160): the transport-agnostic
//! selection and drift-verdict logic behind `eg query resolve <record_id>`.
//!
//! This module owns the pure semantics — dereferencing a cited
//! `codegraph:vN:<suffix>` handle to its record and deriving the closed
//! `valid | drifted | dangling` verdict — shared by the `--graph`/`--data-dir`
//! CLI path (`crate::cli::resolve`) and the daemon's `resolve_record` query
//! verb. CLI rendering (JSON/text answers, error envelopes, exit codes) stays
//! in `crate::cli::resolve`; the daemon wire format stays in `crate::daemon`.

use std::collections::{BTreeSet, HashSet};

use chrono::DateTime;

use crate::ir::{GraphRecord, SourceSpan};

/// Closed drift verdict vocabulary for record-id resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveVerdict {
    /// The handle names a live record: in the current view (no selector), or
    /// the exact snapshot a temporal selector pinned.
    Valid,
    /// The record exists at the pinned `--at`/`--as-of` snapshot but its
    /// cited coordinates/content differ from the current view — or it is gone
    /// there. A successful dereference; exit 0.
    Drifted,
    /// No record matches the id in the requested view. Never constructed as a
    /// success value: the dangling outcome is surfaced through the structured
    /// `dangling_handle` not-found envelope (exit 2), never as an `ok: true`
    /// answer. The variant exists so the closed `valid | drifted | dangling`
    /// vocabulary is exhaustive in one place.
    #[allow(dead_code)]
    Dangling,
}

impl ResolveVerdict {
    /// The wire string for this verdict.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Drifted => "drifted",
            Self::Dangling => "dangling",
        }
    }
}

/// Why a handle was rejected before any store was touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveHandleError {
    /// Not a `codegraph:vN:<suffix>` handle at all.
    Malformed,
    /// A well-formed handle from another id domain. Out of scope for this
    /// lane: refused explicitly, never misread as a dangling codegraph id.
    UnsupportedDomain,
}

/// Non-codegraph id domains this lane refuses outright (issue #160 scope).
const NON_CODEGRAPH_HANDLE_PREFIXES: &[&str] = &[
    "agent_memory:v1:",
    "verification:v1:",
    "artifact:v1:",
    "project:v1:",
    "semantic:v1:",
    "user_context:v1:",
];

/// Validates a `query resolve` handle: a well-formed `codegraph:vN:<suffix>`
/// passes; anything else is a named rejection. No store is touched, so a
/// malformed id is an input error (exit 1), never a dangling verdict.
pub fn validate_resolve_handle(id: &str) -> Result<(), ResolveHandleError> {
    if crate::ir::parse_codegraph_id(id).is_some() {
        return Ok(());
    }
    if NON_CODEGRAPH_HANDLE_PREFIXES
        .iter()
        .any(|prefix| id.starts_with(prefix))
    {
        Err(ResolveHandleError::UnsupportedDomain)
    } else {
        Err(ResolveHandleError::Malformed)
    }
}

/// A record's stamped valid time, parsed — the temporal block's when present,
/// else a node's own `valid_time`.
fn record_valid_time_instant(record: &GraphRecord) -> Option<DateTime<chrono::FixedOffset>> {
    let raw = match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        }
        | GraphRecord::Edge {
            temporal: Some(t), ..
        } => t.valid_time.as_str(),
        GraphRecord::Node {
            valid_time: Some(vt),
            ..
        } => vt.as_str(),
        _ => return None,
    };
    DateTime::parse_from_rfc3339(raw).ok()
}

/// The commit SHA stamped on a record, when it starts with `prefix`.
fn temporal_commit_if_prefix<'a>(record: &'a GraphRecord, prefix: &str) -> Option<&'a str> {
    let commit = match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        }
        | GraphRecord::Edge {
            temporal: Some(t), ..
        } => t.git_commit.as_str(),
        _ => return None,
    };
    if commit.starts_with(prefix) {
        Some(commit)
    } else {
        None
    }
}

/// Ids carrying an ordinary `forget` tombstone (issue #231).
///
/// Repository-eviction tombstones (issue #472) are NOT included here; they
/// are computed separately via `crate::repo_evict::active_eviction_tombstoned_ids`
/// because they suppress temporal snapshots too, while `forget` tombstones
/// keep the temporal exemption.
pub fn forget_tombstoned_ids(records: &[GraphRecord]) -> BTreeSet<&str> {
    records
        .iter()
        .filter_map(|record| match record {
            GraphRecord::Tombstone { deleted_id, .. }
                if !crate::repo_evict::is_eviction_tombstone(record) =>
            {
                Some(deleted_id.as_str())
            }
            _ => None,
        })
        .collect()
}

/// Dereferences `id` in the current view: the head version wins by valid
/// time, with later file position breaking ties so the pick is deterministic.
///
/// `deleted` holds ordinary `forget`-tombstoned ids, which suppress
/// non-temporal records only (issue #231 temporal exemption); `evicted`
/// holds active repository-eviction targets, suppressed everywhere
/// (issue #472).
///
/// Where the id has several versions (history store), the one with the
/// greatest valid time wins — the head version.
pub fn find_current_record<'r>(
    records: &'r [GraphRecord],
    id: &str,
    deleted: &BTreeSet<&str>,
    evicted: &HashSet<String>,
) -> Option<&'r GraphRecord> {
    if evicted.contains(id) {
        return None;
    }
    records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.id() == id)
        .filter(|(_, record)| match record {
            GraphRecord::Node { temporal: None, .. } => !deleted.contains(id),
            _ => true,
        })
        .max_by(|(pos_a, record_a), (pos_b, record_b)| {
            record_valid_time_instant(record_a)
                .cmp(&record_valid_time_instant(record_b))
                .then(pos_a.cmp(pos_b))
        })
        .map(|(_, record)| record)
}

/// The record for `id` at the commit whose SHA starts with `commit_prefix`,
/// mirroring `query symbol --at`'s repository-wide ambiguity rule: a prefix
/// matching more than one commit is an error, never a guess. Forget
/// tombstones are ignored in temporal views (issue #231); eviction suppresses
/// everywhere (issue #472).
pub fn find_record_at_commit<'r>(
    records: &'r [GraphRecord],
    id: &str,
    commit_prefix: &str,
    evicted: &HashSet<String>,
) -> Result<Option<&'r GraphRecord>, String> {
    let matching_commits: BTreeSet<&str> = records
        .iter()
        .filter_map(|record| temporal_commit_if_prefix(record, commit_prefix))
        .collect();
    if matching_commits.len() > 1 {
        let named: Vec<String> = matching_commits
            .iter()
            .take(3)
            .map(|commit| (*commit).to_owned())
            .collect();
        return Err(format!(
            "commit prefix '{commit_prefix}' is ambiguous: matches {} commits ({})",
            matching_commits.len(),
            named.join(", ")
        ));
    }
    if evicted.contains(id) {
        return Ok(None);
    }
    Ok(records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.id() == id)
        .filter(|(_, record)| temporal_commit_if_prefix(record, commit_prefix).is_some())
        .max_by(|(pos_a, record_a), (pos_b, record_b)| {
            record_valid_time_instant(record_a)
                .cmp(&record_valid_time_instant(record_b))
                .then(pos_a.cmp(pos_b))
        })
        .map(|(_, record)| record))
}

/// The record for `id` at the most recent valid time at or before `as_of`
/// (RFC 3339). Forget tombstones are ignored in temporal views (issue #231);
/// eviction suppresses everywhere (issue #472).
pub fn find_record_as_of<'r>(
    records: &'r [GraphRecord],
    id: &str,
    as_of: &str,
    evicted: &HashSet<String>,
) -> Result<Option<&'r GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|error| format!("invalid --as-of timestamp '{as_of}': {error}"))?;
    if evicted.contains(id) {
        return Ok(None);
    }
    let mut best: Option<(&'r GraphRecord, DateTime<chrono::FixedOffset>)> = None;
    for record in records.iter().filter(|record| record.id() == id) {
        let Some(valid_time) = record_valid_time_instant(record) else {
            continue;
        };
        if valid_time > as_of_dt {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, previous)) => valid_time > previous,
        };
        if better {
            best = Some((record, valid_time));
        }
    }
    Ok(best.map(|(record, _)| record))
}

/// The identity-adjacent stored fields a drift verdict compares: the
/// coordinates a citation points at (`repo_relative_path`, `span`) plus the
/// content fingerprints that change when the source moves or is edited
/// (`signature`, `content_signature`).
///
/// The STABLE ID itself is deliberately excluded: symbol identity (ADR-0004)
/// excludes span and content precisely so an id survives coordinate/content
/// changes — comparing the recomputed id would always agree with itself and
/// could never report drift.
fn resolve_coordinates(
    record: &GraphRecord,
) -> (Option<&str>, Option<SourceSpan>, Option<&str>, Option<&str>) {
    match record {
        GraphRecord::Node {
            repo_relative_path,
            span,
            signature,
            content_signature,
            ..
        } => (
            repo_relative_path.as_deref(),
            *span,
            signature.as_deref(),
            content_signature.as_deref(),
        ),
        GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => (None, None, None, None),
        // Tombstones carry no coordinates: two tombstone records for one id
        // compare equal, which is the only sane verdict for the id domain
        // this core serves (tombstone ids are never cited handles).
    }
}

/// Derives the drift verdict for a record found under a temporal selector.
/// `pinned` is the record at the pinned snapshot; `current` its current-view
/// counterpart (`None` when the id has no live record now).
pub fn drift_verdict_for(
    pinned: &GraphRecord,
    current: Option<&GraphRecord>,
) -> (ResolveVerdict, Option<String>) {
    match current {
        None => (
            ResolveVerdict::Drifted,
            Some(
                "the record exists at the pinned snapshot but has no live record \
                 in the current view"
                    .to_owned(),
            ),
        ),
        Some(current) if resolve_coordinates(pinned) == resolve_coordinates(current) => {
            (ResolveVerdict::Valid, None)
        }
        Some(_) => (
            ResolveVerdict::Drifted,
            Some(
                "the record's cited coordinates or content differ between the \
                 pinned snapshot and the current view"
                    .to_owned(),
            ),
        ),
    }
}

/// The citation fields of a dereferenced record, shared by the CLI answer
/// body and the daemon `resolve_record` verb so both transports report the
/// same coordinates for the same record.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedRecordFields<'a> {
    pub kind: &'a str,
    pub name: Option<&'a str>,
    pub repo_relative_path: Option<&'a str>,
    pub span: Option<SourceSpan>,
    pub git_commit: Option<&'a str>,
    pub valid_time: Option<&'a str>,
}

/// Splits a [`GraphRecord`] into the citation fields a resolve answer carries.
pub fn resolved_record_fields(record: &GraphRecord) -> ResolvedRecordFields<'_> {
    match record {
        GraphRecord::Node {
            kind,
            name,
            repo_relative_path,
            span,
            temporal,
            valid_time,
            ..
        } => ResolvedRecordFields {
            kind: kind.as_str(),
            name: name.as_deref(),
            repo_relative_path: repo_relative_path.as_deref(),
            span: *span,
            git_commit: temporal.as_ref().map(|block| block.git_commit.as_str()),
            valid_time: temporal.as_ref().map_or_else(
                || valid_time.as_deref(),
                |block| Some(block.valid_time.as_str()),
            ),
        },
        GraphRecord::Edge { label, .. } => ResolvedRecordFields {
            kind: label.as_str(),
            name: None,
            repo_relative_path: None,
            span: None,
            git_commit: None,
            valid_time: None,
        },
        GraphRecord::Tombstone { .. } => ResolvedRecordFields {
            kind: "Tombstone",
            name: None,
            repo_relative_path: None,
            span: None,
            git_commit: None,
            valid_time: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a code-graph node fixture from JSON. Only the fields under
    /// test are spelled out; serde fills the rest.
    fn node_fixture(value: serde_json::Value) -> GraphRecord {
        serde_json::from_value(value).expect("node fixture deserializes")
    }

    fn symbol_node(
        id: &str,
        path: &str,
        start_line: u32,
        commit: &str,
        valid_time: &str,
    ) -> GraphRecord {
        node_fixture(serde_json::json!({
            "record_type": "node",
            "id": id,
            "kind": "Symbol",
            "schema_version": 1,
            "summary": "test fixture symbol",
            "user_context": {},
            "name": "target",
            "repo_relative_path": path,
            "span": { "start_byte": 0, "end_byte": 10, "start_line": start_line, "end_line": start_line, "start_column": 1, "end_column": 10 },
            "signature": "pub fn target() -> u32",
            "content_signature": format!("content@{start_line}"),
            "temporal": { "git_commit": commit, "git_parent_commits": [], "valid_time": valid_time, "observed_at": valid_time },
        }))
    }

    const ID: &str =
        "codegraph:v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn verdict_strings_are_snake_case() {
        assert_eq!(ResolveVerdict::Valid.as_str(), "valid");
        assert_eq!(ResolveVerdict::Drifted.as_str(), "drifted");
        assert_eq!(ResolveVerdict::Dangling.as_str(), "dangling");
    }

    #[test]
    fn current_record_picks_the_head_version() {
        let older = symbol_node(ID, "src/lib.rs", 1, "aaa", "2026-03-01T00:00:00Z");
        let newer = symbol_node(ID, "src/lib.rs", 3, "bbb", "2026-03-02T00:00:00Z");
        let records = vec![older, newer];
        let deleted = BTreeSet::new();
        let evicted = HashSet::new();
        let found = find_current_record(&records, ID, &deleted, &evicted).unwrap();
        assert_eq!(
            record_valid_time_instant(found).unwrap().to_string(),
            "2026-03-02 00:00:00 +00:00"
        );
    }

    #[test]
    fn current_record_suppresses_forget_tombstoned_ids() {
        // The issue #231 exemption: a record WITH a temporal block survives
        // a forget tombstone, so the suppression test uses a non-temporal
        // node — exactly the `query symbol` current-view rule.
        let mut record = symbol_node(ID, "src/lib.rs", 1, "aaa", "2026-03-01T00:00:00Z");
        if let GraphRecord::Node { temporal, .. } = &mut record {
            *temporal = None;
        }
        let records = vec![record];
        let deleted: BTreeSet<&str> = std::iter::once(ID).collect();
        let evicted = HashSet::new();
        assert!(find_current_record(&records, ID, &deleted, &evicted).is_none());
    }

    #[test]
    fn current_record_keeps_temporal_versions_despite_forget_tombstone() {
        // Issue #231 temporal exemption: a forget tombstone does NOT suppress
        // the temporal (historical) versions of an id.
        let record = symbol_node(ID, "src/lib.rs", 1, "aaa", "2026-03-01T00:00:00Z");
        let records = vec![record];
        let deleted: BTreeSet<&str> = std::iter::once(ID).collect();
        let evicted = HashSet::new();
        assert!(find_current_record(&records, ID, &deleted, &evicted).is_some());
    }

    #[test]
    fn current_record_suppresses_evicted_ids_everywhere() {
        let record = symbol_node(ID, "src/lib.rs", 1, "aaa", "2026-03-01T00:00:00Z");
        let records = vec![record];
        let deleted = BTreeSet::new();
        let evicted: HashSet<String> = std::iter::once(ID.to_owned()).collect();
        assert!(find_current_record(&records, ID, &deleted, &evicted).is_none());
    }

    #[test]
    fn at_commit_selects_the_matching_snapshot() {
        let first = symbol_node(ID, "src/lib.rs", 1, "aaa", "2026-03-01T00:00:00Z");
        let second = symbol_node(ID, "src/lib.rs", 5, "bbb", "2026-03-02T00:00:00Z");
        let records = vec![first, second];
        let evicted = HashSet::new();
        let found = find_record_at_commit(&records, ID, "aaa", &evicted)
            .unwrap()
            .unwrap();
        assert_eq!(
            record_valid_time_instant(found).unwrap().to_string(),
            "2026-03-01 00:00:00 +00:00"
        );
        assert!(
            find_record_at_commit(&records, ID, "zzz", &evicted)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn at_commit_rejects_ambiguous_prefixes() {
        let first = symbol_node(ID, "src/lib.rs", 1, "aaa111", "2026-03-01T00:00:00Z");
        let second = symbol_node(ID, "src/lib.rs", 5, "aaa222", "2026-03-02T00:00:00Z");
        let records = vec![first, second];
        let evicted = HashSet::new();
        let error = find_record_at_commit(&records, ID, "aaa", &evicted).unwrap_err();
        assert!(error.contains("ambiguous"), "unexpected error: {error}");
    }

    #[test]
    fn as_of_selects_the_latest_snapshot_at_or_before_the_instant() {
        let first = symbol_node(ID, "src/lib.rs", 1, "aaa", "2026-03-01T00:00:00Z");
        let second = symbol_node(ID, "src/lib.rs", 5, "bbb", "2026-03-03T00:00:00Z");
        let records = vec![first, second];
        let evicted = HashSet::new();
        let found = find_record_as_of(&records, ID, "2026-03-02T00:00:00Z", &evicted)
            .unwrap()
            .unwrap();
        assert_eq!(
            record_valid_time_instant(found).unwrap().to_string(),
            "2026-03-01 00:00:00 +00:00"
        );
        assert!(
            find_record_as_of(&records, ID, "2026-01-01T00:00:00Z", &evicted)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn as_of_rejects_malformed_timestamps() {
        let records = vec![symbol_node(
            ID,
            "src/lib.rs",
            1,
            "aaa",
            "2026-03-01T00:00:00Z",
        )];
        let evicted = HashSet::new();
        let error = find_record_as_of(&records, ID, "not-a-time", &evicted).unwrap_err();
        assert!(
            error.contains("invalid --as-of"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn drift_verdict_is_valid_when_coordinates_match() {
        let pinned = symbol_node(ID, "src/lib.rs", 3, "bbb", "2026-03-02T00:00:00Z");
        let current = symbol_node(ID, "src/lib.rs", 3, "bbb", "2026-03-02T00:00:00Z");
        let (verdict, detail) = drift_verdict_for(&pinned, Some(&current));
        assert_eq!(verdict, ResolveVerdict::Valid);
        assert!(detail.is_none());
    }

    #[test]
    fn drift_verdict_is_drifted_when_coordinates_move() {
        let pinned = symbol_node(ID, "src/lib.rs", 1, "aaa", "2026-03-01T00:00:00Z");
        let current = symbol_node(ID, "src/lib.rs", 3, "bbb", "2026-03-02T00:00:00Z");
        let (verdict, detail) = drift_verdict_for(&pinned, Some(&current));
        assert_eq!(verdict, ResolveVerdict::Drifted);
        assert!(detail.is_some_and(|d| d.contains("differ")));
    }

    #[test]
    fn drift_verdict_is_drifted_when_the_record_is_gone() {
        let pinned = symbol_node(ID, "src/lib.rs", 1, "aaa", "2026-03-01T00:00:00Z");
        let (verdict, detail) = drift_verdict_for(&pinned, None);
        assert_eq!(verdict, ResolveVerdict::Drifted);
        assert!(detail.is_some_and(|d| d.contains("no live record")));
    }

    #[test]
    fn drift_verdict_ignores_the_stable_id_itself() {
        // ADR-0004: the id deliberately excludes span/content, so two
        // versions share one id; the verdict must still see the moved span.
        let pinned = symbol_node(ID, "src/lib.rs", 1, "aaa", "2026-03-01T00:00:00Z");
        let current = symbol_node(ID, "src/lib.rs", 9, "bbb", "2026-03-02T00:00:00Z");
        assert_eq!(pinned.id(), current.id());
        let (verdict, _) = drift_verdict_for(&pinned, Some(&current));
        assert_eq!(verdict, ResolveVerdict::Drifted);
    }

    #[test]
    fn forget_tombstoned_ids_collects_deleted_ids() {
        let record = symbol_node(ID, "src/lib.rs", 1, "aaa", "2026-03-01T00:00:00Z");
        let tombstone = node_fixture(serde_json::json!({
            "record_type": "tombstone",
            "id": "codegraph:v1:tombstone0000000000000000000000000000000000000000000000000000",
            "schema_version": 1,
            "deleted_id": ID,
            "summary": "forget",
        }));
        let records = vec![record, tombstone];
        let deleted = forget_tombstoned_ids(&records);
        assert!(deleted.contains(ID));
        assert_eq!(deleted.len(), 1);
    }
}
