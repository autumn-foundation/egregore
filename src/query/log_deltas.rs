//! Runtime error-signature deltas across a commit range (`eg query
//! log-deltas`, issue #326).
//!
//! Answers "did this commit range introduce new runtime error signatures?" by
//! composing three already-shipped mechanics rather than re-deriving any of
//! them:
//!
//! * the issue #118 range mechanics ([`resolve_commit_range`] and
//!   [`range_deltas`](super::range_deltas)) for endpoint resolution, the
//!   commit valid-time window, and the symbol-level delta join;
//! * the issue #319/#320 [`ErrorSignature`](crate::ir::NodeKind::ErrorSignature)
//!   valid-time model (`first_seen` / `last_seen`) and its per-signature hourly
//!   [`LogOccurrenceBucket`](crate::ir::NodeKind::LogOccurrenceBucket) records,
//!   linked back through `AGGREGATES` edges;
//! * the issue #322 `FRAME_RESOLVES_TO` edges that bind a signature's backtrace
//!   frames to code-graph targets.
//!
//! # Window derivation
//!
//! The valid-time window is derived from the committer dates of the commits in
//! the resolved range (`range_commit_shas`):
//!
//! * `window_start = min(commit_valid_time[sha])` over the range commits;
//! * `window_end   = max(commit_valid_time[sha])` over the range commits.
//!
//! # Classification (closed, mutually exclusive, precedence-ordered)
//!
//! Every in-scope `ErrorSignature` is classified against the window from its
//! `first_seen` (`fs`) and `last_seen` (`ls`):
//!
//! 1. `new_signatures`   — `window_start <= fs <= window_end`. The primary
//!    regression signal: a signature first observed inside the window, even if
//!    it also ceased inside the window.
//! 2. `ceased_signatures` — not new, `fs < window_start` and `ls < window_end`
//!    (existed before the range and went silent by/within it; a signature last
//!    seen before the range trivially satisfies `ls < window_end`).
//! 3. `continuing_signatures` — `fs < window_start` and `ls >= window_end`
//!    (existed before the range and still occurring through its end).
//!
//! A signature whose first observation falls strictly after the window
//! (`fs > window_end`) is **out of range** and excluded from all three classes
//! — it belongs to a future range, not this one.
//!
//! # Occurrence counts
//!
//! Per-window occurrence counts are computed from the signature's own
//! `LogOccurrenceBucket` records, discovered through the `AGGREGATES`
//! (bucket → signature) edges (issue #320). For a signature with at least one
//! linked bucket:
//!
//! * `base_window_occurrences` = sum of bucket counts whose `bucket_start`
//!   is `<= commit_valid_time[base]`;
//! * `head_window_occurrences` = sum of bucket counts whose `bucket_start`
//!   is `<= commit_valid_time[head]`.
//!
//! When a signature carries no linked buckets (e.g. a log graph ingested
//! without buckets), per-window bucketization is unavailable: the two window
//! fields are omitted and `occurrence_source` is `aggregate_only`, exposing the
//! signature's aggregate `occurrence_count` as the only honest count. Counts
//! are never fabricated.
//!
//! Read-only over the supplied records; never touches Git state or the working
//! tree. Output is deterministic and byte-identical across runs. No raw log
//! payload text ever enters the response — only bounded template excerpts,
//! record IDs, severities, counts, commit handles, and valid times.

use std::collections::{BTreeMap, BTreeSet};

use super::RepositoryIndex;
use super::deltas::{RangeDeltasError, resolve_commit_range};
use crate::ir::{EdgeLabel, GraphRecord, LogPayload, NodeKind};

/// Always-present advisory label for [`log_deltas`] responses.
pub const LOG_DELTAS_DISCLAIMER: &str = "Rows are runtime error-signature observations classified \
     against the commit range's valid-time window. A signature first observed in-range is a \
     regression LEAD, not proof this range caused it; a ceased signature is not proof of a fix; \
     occurrence data only reflects the log sources that were scanned (a sampling artifact), never \
     the complete runtime behavior of the system.";

/// The valid-time window a [`LogDeltas`] response classifies against, derived
/// from the committer dates of the commits in the resolved range.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogDeltaWindow {
    /// Earliest committer date across the range commits (RFC 3339).
    pub window_start: String,
    /// Latest committer date across the range commits (RFC 3339).
    pub window_end: String,
}

/// One resolved backtrace frame handle carried on a signature row (issue #322).
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct ResolvedFrameHandle {
    /// Zero-based backtrace frame index the resolution applies to.
    pub frame_index: u32,
    /// Closed frame-resolution class: `resolved` / `ambiguous` / `path_only` /
    /// `unresolved`.
    pub frame_resolution: String,
    /// Stable record ID of the code-graph target the frame resolved to.
    pub target_record_id: String,
}

/// One overlapping symbol-delta join row on a `new_signatures` entry: a
/// resolved-frame target that also appears in the range's symbol deltas.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct OverlappingSymbolDelta {
    /// Stable record ID of the overlapping symbol delta.
    pub record_id: String,
    /// The symbol-delta change class it landed in: `added_symbol` /
    /// `modified_symbol` / `removed_symbol`.
    pub change_class: String,
}

/// One classified runtime error-signature delta across the commit range.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogSignatureDelta {
    /// Stable `ErrorSignature` record ID.
    pub record_id: String,
    /// Schema version stamped on the signature record.
    pub schema_version: u32,
    /// Stable change class: `new_signature` / `ceased_signature` /
    /// `continuing_signature`.
    pub change_class: &'static str,
    /// Closed severity class of the signature: `fatal` / `error` / `warn`.
    pub severity: String,
    /// Valid time of the signature's earliest occurrence (RFC 3339).
    pub first_seen: String,
    /// Valid time of the signature's latest occurrence (RFC 3339).
    pub last_seen: String,
    /// The signature's aggregate occurrence count (all scanned sources).
    pub occurrence_count: u64,
    /// How the occurrence figures were derived: `occurrence_buckets` when
    /// per-window bucket counts are available, `aggregate_only` when the
    /// signature carries no linked buckets.
    pub occurrence_source: &'static str,
    /// Occurrences observed at or before the base endpoint's committer date,
    /// summed over linked buckets. Absent when no buckets are linked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_window_occurrences: Option<u64>,
    /// Occurrences observed at or before the head endpoint's committer date,
    /// summed over linked buckets. Absent when no buckets are linked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_window_occurrences: Option<u64>,
    /// Resolved backtrace-frame handles (issue #322), canonically ordered.
    pub resolved_frames: Vec<ResolvedFrameHandle>,
    /// Symbol deltas overlapping this signature's resolved frames. Populated
    /// only for `new_signatures`; always empty for the other classes.
    pub overlapping_symbol_deltas: Vec<OverlappingSymbolDelta>,
}

/// Structured runtime error-signature deltas across a commit range, grouped by
/// stable change class. Returned by [`log_deltas`].
///
/// Every group is always present (empty vecs, never omitted) and canonically
/// ordered by `(first_seen, record_id)` so repeated queries are byte-equivalent
/// after serialization.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogDeltas {
    /// Resolved full SHA of the base (older) endpoint.
    pub base: String,
    /// Resolved full SHA of the head (newer) endpoint.
    pub head: String,
    /// The valid-time window derived from the range commits.
    pub window: LogDeltaWindow,
    /// Number of commits in the range (reachable from head, not from base).
    pub range_commit_count: usize,
    /// Always-present advisory disclaimer ([`LOG_DELTAS_DISCLAIMER`]).
    pub disclaimer: &'static str,
    /// Signatures first observed inside the window (the regression signal).
    pub new_signatures: Vec<LogSignatureDelta>,
    /// Signatures that existed before the range and went silent by its end.
    pub ceased_signatures: Vec<LogSignatureDelta>,
    /// Signatures that existed before the range and still occur through its end.
    pub continuing_signatures: Vec<LogSignatureDelta>,
}

/// Internal per-signature classification against the window.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LogDeltaClass {
    New,
    Ceased,
    Continuing,
    /// First observed strictly after the window — belongs to a future range.
    OutOfRange,
}

/// Classifies runtime error signatures across a commit range (issue #326).
///
/// The two endpoints are full SHAs or unique prefixes resolved against the
/// store's `Commit` nodes with the exact issue #118 endpoint resolution and
/// error taxonomy. See the module documentation for the window derivation,
/// the closed classification set, and the occurrence-count semantics.
///
/// Purely read-time: reads only the provided records, never Git state or the
/// working tree.
///
/// # Errors
///
/// Returns a [`RangeDeltasError`] when the history is empty, a commit handle
/// is missing or ambiguous, the endpoints are identical, the range is
/// reversed, or no ancestor path connects the endpoints — the same taxonomy as
/// [`range_deltas`](super::range_deltas).
#[allow(clippy::missing_panics_doc)]
pub fn log_deltas(
    records: &[GraphRecord],
    base_prefix: &str,
    head_prefix: &str,
    repo_scope: Option<&str>,
) -> Result<LogDeltas, RangeDeltasError> {
    // Repository scoping mirrors `range_deltas`: in a shared store two
    // repositories can carry the same commit SHA, so commit resolution and
    // signature selection are gated by owning repository when a scope is set.
    let repo_index = repo_scope.map(|_| RepositoryIndex::build(records));
    let in_scope = |id: &str| -> bool {
        match (repo_scope, repo_index.as_ref()) {
            (Some(scope), Some(index)) => index.owner_of(id) == Some(scope),
            _ => true,
        }
    };

    // ── Range resolution + valid-time window (reused issue #118 mechanics) ───
    let range = resolve_commit_range(records, base_prefix, head_prefix, &in_scope)?;
    let base_sha = range.base_sha.to_owned();
    let head_sha = range.head_sha.to_owned();
    let base_valid_time = range.commit_valid_time.get(range.base_sha).copied();
    let head_valid_time = range.commit_valid_time.get(range.head_sha).copied();

    let mut range_times: Vec<&str> = range
        .range_commit_shas
        .iter()
        .filter_map(|sha| range.commit_valid_time.get(sha).copied())
        .collect();
    range_times.sort_unstable();
    // A commit that carries no committer date cannot bound the window; an empty
    // window (no range commit carried a valid time) degenerately excludes every
    // signature rather than fabricating bounds.
    let window_start = range_times.first().copied().unwrap_or("").to_owned();
    let window_end = range_times.last().copied().unwrap_or("").to_owned();

    // ── Symbol-delta join set (reused issue #118 `range_deltas`) ─────────────
    // The intersection is computed from the existing delta mechanics, never
    // re-derived ad hoc.
    let symbol_deltas = super::range_deltas(records, base_prefix, head_prefix, repo_scope)?;
    let mut symbol_delta_class: BTreeMap<&str, &'static str> = BTreeMap::new();
    for item in symbol_deltas
        .added_symbols
        .iter()
        .chain(&symbol_deltas.modified_symbols)
        .chain(&symbol_deltas.removed_symbols)
    {
        symbol_delta_class.insert(item.record_id, item.change_class);
    }

    // ── Node-by-ID index + per-signature bucket / frame indices ──────────────
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

    // signature_id → linked (bucket_start, occurrence_count) pairs, via
    // AGGREGATES (bucket → signature) edges (issue #320).
    let mut buckets_by_sig: BTreeMap<&str, Vec<(&str, u64)>> = BTreeMap::new();
    // signature_id → resolved-frame handles, via FRAME_RESOLVES_TO edges (#322).
    let mut frames_by_sig: BTreeMap<&str, Vec<ResolvedFrameHandle>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            label,
            source,
            target,
            frame_resolution,
            frame_index,
            ..
        } = r
        {
            match label {
                EdgeLabel::Aggregates => {
                    if !in_scope(target.as_str()) {
                        continue;
                    }
                    if let Some(GraphRecord::Node {
                        log: Some(payload), ..
                    }) = by_id.get(source.as_str())
                    {
                        if let LogPayload::LogOccurrenceBucket(bucket) = payload.as_ref() {
                            buckets_by_sig
                                .entry(target.as_str())
                                .or_default()
                                .push((bucket.bucket_start.as_str(), bucket.occurrence_count));
                        }
                    }
                }
                EdgeLabel::FrameResolvesTo => {
                    if !in_scope(source.as_str()) {
                        continue;
                    }
                    if let (Some(resolution), Some(index)) = (frame_resolution, frame_index) {
                        frames_by_sig.entry(source.as_str()).or_default().push(
                            ResolvedFrameHandle {
                                frame_index: *index,
                                frame_resolution: resolution.as_str().to_owned(),
                                target_record_id: target.clone(),
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    // ── Classify every in-scope ErrorSignature ───────────────────────────────
    let mut new_signatures: Vec<LogSignatureDelta> = Vec::new();
    let mut ceased_signatures: Vec<LogSignatureDelta> = Vec::new();
    let mut continuing_signatures: Vec<LogSignatureDelta> = Vec::new();

    for r in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::ErrorSignature,
            schema_version,
            log: Some(payload),
            ..
        } = r
        else {
            continue;
        };
        if !in_scope(id.as_str()) {
            continue;
        }
        let LogPayload::ErrorSignature(sig) = payload.as_ref() else {
            continue;
        };

        let class = classify(&window_start, &window_end, &sig.first_seen, &sig.last_seen);
        let change_class = match class {
            LogDeltaClass::New => "new_signature",
            LogDeltaClass::Ceased => "ceased_signature",
            LogDeltaClass::Continuing => "continuing_signature",
            LogDeltaClass::OutOfRange => continue,
        };

        // Per-window occurrence counts from linked buckets, when available.
        let (occurrence_source, base_window, head_window) = match buckets_by_sig.get(id.as_str()) {
            Some(buckets) if !buckets.is_empty() => {
                let base_sum = base_valid_time.map(|bt| window_bucket_sum(buckets, bt));
                let head_sum = head_valid_time.map(|ht| window_bucket_sum(buckets, ht));
                ("occurrence_buckets", base_sum, head_sum)
            }
            _ => ("aggregate_only", None, None),
        };

        let mut resolved_frames = frames_by_sig.get(id.as_str()).cloned().unwrap_or_default();
        resolved_frames.sort_by(|a, b| {
            a.frame_index
                .cmp(&b.frame_index)
                .then_with(|| a.frame_resolution.cmp(&b.frame_resolution))
                .then_with(|| a.target_record_id.cmp(&b.target_record_id))
        });
        resolved_frames.dedup();

        // AC4: only `new_signatures` carry the symbol-delta join.
        let overlapping_symbol_deltas = if matches!(class, LogDeltaClass::New) {
            overlaps(&resolved_frames, &symbol_delta_class)
        } else {
            Vec::new()
        };

        let row = LogSignatureDelta {
            record_id: id.clone(),
            schema_version: *schema_version,
            change_class,
            severity: sig.severity.clone(),
            first_seen: sig.first_seen.clone(),
            last_seen: sig.last_seen.clone(),
            occurrence_count: sig.occurrence_count,
            occurrence_source,
            base_window_occurrences: base_window,
            head_window_occurrences: head_window,
            resolved_frames,
            overlapping_symbol_deltas,
        };

        match class {
            LogDeltaClass::New => new_signatures.push(row),
            LogDeltaClass::Ceased => ceased_signatures.push(row),
            LogDeltaClass::Continuing => continuing_signatures.push(row),
            LogDeltaClass::OutOfRange => {}
        }
    }

    for group in [
        &mut new_signatures,
        &mut ceased_signatures,
        &mut continuing_signatures,
    ] {
        group.sort_by(|a, b| {
            a.first_seen
                .cmp(&b.first_seen)
                .then_with(|| a.record_id.cmp(&b.record_id))
        });
    }

    Ok(LogDeltas {
        base: base_sha,
        head: head_sha,
        window: LogDeltaWindow {
            window_start,
            window_end,
        },
        range_commit_count: range.range_commit_shas.len(),
        disclaimer: LOG_DELTAS_DISCLAIMER,
        new_signatures,
        ceased_signatures,
        continuing_signatures,
    })
}

/// Classifies one signature against the window from its `first_seen` /
/// `last_seen` valid times. RFC 3339 strings compare lexicographically in
/// chronological order for the Z-normalized UTC form the extractors emit,
/// matching the repository's existing temporal-selector comparisons.
fn classify(
    window_start: &str,
    window_end: &str,
    first_seen: &str,
    last_seen: &str,
) -> LogDeltaClass {
    if window_start <= first_seen && first_seen <= window_end {
        LogDeltaClass::New
    } else if first_seen > window_end {
        LogDeltaClass::OutOfRange
    } else {
        // Not new and not after the window ⇒ first observed before the window.
        if last_seen < window_end {
            LogDeltaClass::Ceased
        } else {
            LogDeltaClass::Continuing
        }
    }
}

/// Sums the occurrence counts of the linked buckets whose `bucket_start` is at
/// or before `endpoint_valid_time` (the endpoint's committer date).
fn window_bucket_sum(buckets: &[(&str, u64)], endpoint_valid_time: &str) -> u64 {
    buckets
        .iter()
        .filter(|(bucket_start, _)| *bucket_start <= endpoint_valid_time)
        .map(|(_, count)| *count)
        .sum()
}

/// Builds the deterministic overlapping symbol-delta list for a new signature:
/// each resolved-frame target that also appears in the range's symbol deltas.
fn overlaps(
    resolved_frames: &[ResolvedFrameHandle],
    symbol_delta_class: &BTreeMap<&str, &'static str>,
) -> Vec<OverlappingSymbolDelta> {
    let mut seen: BTreeSet<(&'static str, String)> = BTreeSet::new();
    for frame in resolved_frames {
        if let Some(class) = symbol_delta_class.get(frame.target_record_id.as_str()) {
            seen.insert((*class, frame.target_record_id.clone()));
        }
    }
    seen.into_iter()
        .map(|(class, record_id)| OverlappingSymbolDelta {
            record_id,
            change_class: class.to_owned(),
        })
        .collect()
}
