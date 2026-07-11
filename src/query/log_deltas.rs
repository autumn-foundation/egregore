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
//! the resolved range (`range_commit_shas`), compared by parsed UTC instant
//! (never raw RFC 3339 string order — committer dates carry local offsets while
//! log times are Z-normalized, so a lexical comparison is wrong across offsets):
//!
//! * `window_start = min(commit_valid_time[sha])` over the range commits;
//! * `window_end   = max(commit_valid_time[sha])` over the range commits.
//!
//! # Signature coalescing
//!
//! `LogSource` is a **non-identity** input for signatures: a signature's stable
//! ID is `(repository_id, fingerprint_algorithm, template, severity)` only (see
//! `log_stable_id` in `crate::log_graph`). A graph combining multiple
//! `scan-logs` outputs for one repo therefore carries the same `ErrorSignature`
//! record ID more than once, each with its own scan-local `first_seen` /
//! `last_seen` / `occurrence_count`. Records are grouped by stable ID and merged
//! **before** classification so exactly one row per signature ID is emitted:
//! merged `first_seen` is the earliest and merged `last_seen` the latest across
//! the group (by parsed instant); occurrence buckets are unioned and deduped by
//! bucket record ID; the aggregate `occurrence_count` sums the group's per-scan
//! counts. Without this, one stable signature could split across conflicting
//! classes (an earlier scan → `ceased`, a later scan first-seen in-range →
//! `new`) and double-count its occurrences.
//!
//! # Classification (closed, mutually exclusive, precedence-ordered)
//!
//! Every in-scope `ErrorSignature`, after coalescing, is classified against the
//! window from its merged `first_seen` (`fs`) and `last_seen` (`ls`):
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
//! These per-window counts SUM every linked bucket across the coalesced group
//! and are never deduped by bucket record ID (issue #361). A `LogOccurrenceBucket`
//! record ID is `(repository/signature/hour/width)` and omits `LogSource`, so two
//! DISTINCT scan-logs sources observing the same signature in the same hour mint
//! the SAME bucket record ID with their own per-source counts; summing preserves
//! both sources and keeps these counts consistent with the aggregate
//! `occurrence_count`, which likewise sums the coalesced signatures. The symmetric
//! cost is that concatenating the IDENTICAL scan-logs output multiplies counts (a
//! degenerate, user-error input) — scan each source once, or use per-source
//! stores. Fully source-attributed counts require source-aware bucket identity, a
//! #320 log-graph schema change out of #326's scope (tracked in #361).
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

use chrono::{DateTime, Utc};

use super::RepositoryIndex;
use super::deltas::{RangeDeltasError, resolve_commit_range};
use crate::ir::{
    EdgeLabel, ErrorSignaturePayload, GraphRecord, LogPayload, NodeKind, parse_codegraph_id,
};

/// Always-present advisory label for [`log_deltas`] responses.
pub const LOG_DELTAS_DISCLAIMER: &str = "Rows are runtime error-signature observations classified \
     against the commit range's valid-time window. A signature first observed in-range is a \
     regression LEAD, not proof this range caused it; a ceased signature is not proof of a fix; \
     occurrence data only reflects the log sources that were scanned (a sampling artifact), never \
     the complete runtime behavior of the system.";

/// Envelope caveat text emitted with `--repo` when the store holds a SINGLE
/// repository — present-but-benign, since no cross-repository log signatures can
/// exist to bleed in.
pub const LOG_REPO_SCOPE_SINGLE_CAVEAT: &str = "`--repo` scopes only the code side (commit/window \
     resolution and the symbol-delta join). Log signatures are NOT repository-filtered: log records \
     carry no retrievable repository attribution (the repository ID is only hashed into their \
     stable IDs), so every in-window signature is classified regardless of `--repo`. This store \
     holds a single repository, so no other repository's log signatures can be included. Use \
     per-repository stores for log isolation.";

/// Envelope caveat text emitted with `--repo` when the store holds MORE THAN ONE
/// repository — the elevated case.
///
/// This is exactly when cross-repository log bleed can occur, since an unrelated
/// repository's in-window signature is reported here as if scoped.
pub const LOG_REPO_SCOPE_MULTI_CAVEAT: &str = "`--repo` scopes only the code side (commit/window \
     resolution and the symbol-delta join). Log signatures are NOT repository-filtered: log records \
     carry no retrievable repository attribution (the repository ID is only hashed into their \
     stable IDs), so every in-window signature is classified regardless of `--repo`. This store \
     holds MULTIPLE repositories, so in-window log signatures from OTHER repositories may be \
     reported here as if scoped to this one. Use per-repository stores for log isolation.";

/// Advisory disclosure attached to a [`LogDeltas`] response whenever `--repo`
/// scopes the query, stating that log signatures are never repository-filtered.
///
/// `--repo` scopes only the CODE side (commit/window resolution and the
/// symbol-delta join). Because log records carry no retrievable repository
/// attribution, every in-window signature is always classified regardless of
/// `--repo`; the query cannot separate one repository's log signatures from
/// another's. This field surfaces that limitation in the machine-readable
/// envelope (not only the docs), and is ELEVATED when the store actually holds
/// more than one distinct `Repository` node — exactly the condition under which
/// an unrelated repository's signature can bleed into a scoped answer. It is
/// present only when `--repo` is set; single- and multi-repository stores both
/// carry it, differing only in `multi_repository_store` and `message`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogRepoScopeCaveat {
    /// The repository selector that was applied to the code side.
    pub repo_scope: String,
    /// Number of distinct repositories present in the store (schema-version
    /// duplicates of one repository collapsed). `> 1` is exactly when
    /// cross-repository log bleed can occur.
    pub distinct_repository_count: usize,
    /// True when the store holds more than one distinct repository.
    pub multi_repository_store: bool,
    /// Fixed advisory text: [`LOG_REPO_SCOPE_SINGLE_CAVEAT`] for a single-repo
    /// store, [`LOG_REPO_SCOPE_MULTI_CAVEAT`] (elevated) for a multi-repo store.
    pub message: &'static str,
}

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
    /// Repository-scope caveat, present only when `--repo` is set (issue #326
    /// follow-on): log signatures are never repository-filtered — see
    /// [`LogRepoScopeCaveat`]. Absent (omitted from JSON) for unscoped queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_scope_caveat: Option<LogRepoScopeCaveat>,
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
    // Repository scoping mirrors `range_deltas` for the CODE side only: in a
    // shared store two repositories can carry the same commit SHA, so commit
    // resolution, the valid-time window, and the symbol-delta join are gated by
    // owning repository when a scope is set.
    //
    // Log-domain records (`ErrorSignature`, `LogOccurrenceBucket`, and the
    // `AGGREGATES` / `FRAME_RESOLVES_TO` edges keyed on their IDs) carry NO
    // retrievable repository attribution: `scan-logs` only hashes the repository
    // ID into the stable record IDs (see `log_stable_id` / `scan_log_records`)
    // and never stores it on any payload, node, or edge field, so
    // `RepositoryIndex::owner_of(<signature-id>)` is always `None`. Filtering
    // signatures by `--repo` would therefore drop EVERY signature and return
    // empty groups even for the correct repository (Codex P2). Because log nodes
    // cannot be attributed, `--repo` scopes only the code side (commit/window
    // resolution and the symbol-delta join); all log signatures are included.
    //
    // A scoped run over a SHARED multi-repository store can consequently surface
    // an unrelated repository's in-window signature as if it were repo-scoped
    // (Codex P2, `src/query/log_deltas.rs:421`). We DIAGNOSE rather than reject:
    // rejecting `--repo` when logs are present would break the common
    // single-repository store, where including every signature is correct.
    // Instead the response envelope carries `repo_scope_caveat` whenever `--repo`
    // is set, disclosing that logs are unfiltered and ELEVATING the message when
    // the store holds more than one distinct `Repository` node (see the caveat
    // construction near the return). The schema-level fix — persisting repository
    // attribution on log records — is tracked in issue #362. This limitation is
    // documented in `docs/cli/log-deltas.md`.
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

    // Window bounds are derived by parsed INSTANT, never by raw RFC 3339 string
    // order: commit committer dates carry local UTC offsets (`%cI`), while
    // scan-logs normalizes signature/bucket times to UTC `Z`, so a lexical
    // comparison is wrong across offsets (Codex P1). A commit whose committer
    // date carries no parseable timestamp cannot bound the window and is dropped
    // from the derivation; an empty window (no range commit carried a parseable
    // valid time) degenerately excludes every signature rather than fabricating
    // bounds. The EMITTED window strings stay the original RFC 3339 text — only
    // the ordering is by instant.
    let mut range_times: Vec<(DateTime<Utc>, &str)> = range
        .range_commit_shas
        .iter()
        .filter_map(|sha| range.commit_valid_time.get(sha).copied())
        .filter_map(|s| parse_instant(s).map(|dt| (dt, s)))
        .collect();
    range_times.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    let window_start = range_times
        .first()
        .map_or_else(String::new, |(_, s)| (*s).to_owned());
    let window_end = range_times
        .last()
        .map_or_else(String::new, |(_, s)| (*s).to_owned());
    let window_start_instant = range_times.first().map(|(dt, _)| *dt);
    let window_end_instant = range_times.last().map(|(dt, _)| *dt);
    let base_instant = base_valid_time.and_then(parse_instant);
    let head_instant = head_valid_time.and_then(parse_instant);

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

    // ── Per-signature bucket / frame indices ─────────────────────────────────
    // bucket_id → the set of signature IDs it AGGREGATES to (issue #320). Targets
    // are deduped per bucket ID so a rescan's duplicate edge cannot inflate the
    // link set; per-source COUNTS are preserved by iterating bucket NODES below,
    // not by counting edges.
    let mut bucket_targets: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
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
                    // No `in_scope` gate: the target is a log signature, which
                    // carries no retrievable repository attribution (Codex P2).
                    bucket_targets
                        .entry(source.as_str())
                        .or_default()
                        .insert(target.as_str());
                }
                EdgeLabel::FrameResolvesTo => {
                    // No `in_scope` gate: the source is a log signature, which
                    // carries no retrievable repository attribution (Codex P2).
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

    // signature_id → linked (bucket_start, occurrence_count) pairs, ONE entry per
    // bucket NODE record (issue #361). A `LogOccurrenceBucket` record ID is
    // (repository/signature/hour/width) and omits `LogSource`, so two DISTINCT
    // scan-logs sources observing the same signature in the same hour emit
    // separate bucket nodes that share a bucket record ID. Iterating NODES here —
    // rather than resolving edges through a by-ID index that would collapse those
    // duplicates, or deduping by bucket ID — preserves each source's own count and
    // keeps the window sums consistent with the summed aggregate `occurrence_count`.
    // The symmetric cost is that an identical rescanned source's duplicate bucket
    // node is counted again (a degenerate, user-error input); source-aware bucket
    // identity is the true fix, out of #326's scope (tracked in #361).
    let mut buckets_by_sig: BTreeMap<&str, Vec<(&str, u64)>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            log: Some(payload), ..
        } = r
        {
            if let LogPayload::LogOccurrenceBucket(bucket) = payload.as_ref() {
                if let Some(sigs) = bucket_targets.get(r.id()) {
                    for sig in sigs {
                        buckets_by_sig
                            .entry(*sig)
                            .or_default()
                            .push((bucket.bucket_start.as_str(), bucket.occurrence_count));
                    }
                }
            }
        }
    }

    // ── Coalesce ErrorSignature records by stable ID ─────────────────────────
    // `LogSource` is a NON-identity input for signatures: the signature ID is
    // `(repository_id, fingerprint_algorithm, template, severity)` only (see
    // `log_stable_id` in `src/log_graph.rs`). A graph combining multiple
    // scan-logs outputs for one repo therefore carries the SAME signature record
    // ID more than once, each carrying its own scan-local `first_seen` /
    // `last_seen` / `occurrence_count`. Iterating node records would split one
    // stable signature across conflicting classes (e.g. an earlier scan observed
    // before the range → `ceased`, a later scan first-seen in-range → `new`) and
    // double its counts. Group by ID and merge BEFORE classifying so exactly one
    // row per stable signature ID is emitted. `schema_version` and `severity` are
    // identity-derived, hence identical within a group.
    let mut sig_groups: BTreeMap<&str, Vec<(u32, &ErrorSignaturePayload)>> = BTreeMap::new();
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
        // No `in_scope` gate on the signature: log records carry no retrievable
        // repository attribution, so `--repo` scopes only the code side (Codex
        // P2). See the module-level scoping note above.
        let LogPayload::ErrorSignature(sig) = payload.as_ref() else {
            continue;
        };
        sig_groups
            .entry(id.as_str())
            .or_default()
            .push((*schema_version, sig));
    }

    // ── Classify each coalesced signature group ──────────────────────────────
    // BTreeMap iteration is in sorted `record_id` order and every merge below
    // (min/max/sum/dedupe) is order-independent, so output is byte-stable.
    let mut new_signatures: Vec<LogSignatureDelta> = Vec::new();
    let mut ceased_signatures: Vec<LogSignatureDelta> = Vec::new();
    let mut continuing_signatures: Vec<LogSignatureDelta> = Vec::new();

    for (id, group) in &sig_groups {
        // Merged valid-time bounds: earliest first_seen and latest last_seen
        // across the group, compared by parsed UTC INSTANT (never raw RFC 3339
        // string order — the same cross-offset reason as `classify`). The emitted
        // strings keep the original RFC 3339 text of the winning bound.
        let firsts: Vec<(Option<DateTime<Utc>>, &str)> = group
            .iter()
            .map(|(_, s)| (parse_instant(&s.first_seen), s.first_seen.as_str()))
            .collect();
        let lasts: Vec<(Option<DateTime<Utc>>, &str)> = group
            .iter()
            .map(|(_, s)| (parse_instant(&s.last_seen), s.last_seen.as_str()))
            .collect();
        let (first_seen, first_instant) = merge_bound(&firsts, BoundKind::Earliest);
        let (last_seen, last_instant) = merge_bound(&lasts, BoundKind::Latest);

        // Aggregate count sums the group's per-scan occurrence counts. Summing
        // across DISTINCT log sources is intended (each contributes its own
        // observations); re-scanning the IDENTICAL source is a degenerate
        // double-count — the bucket path (deduped by bucket ID) is the robust one.
        let occurrence_count: u64 = group.iter().map(|(_, s)| s.occurrence_count).sum();
        // Identity-derived, identical within the group; take the first entry.
        let (schema_version, severity) = (group[0].0, group[0].1.severity.clone());

        let class = classify(
            window_start_instant,
            window_end_instant,
            first_instant,
            last_instant,
        );
        let change_class = match class {
            LogDeltaClass::New => "new_signature",
            LogDeltaClass::Ceased => "ceased_signature",
            LogDeltaClass::Continuing => "continuing_signature",
            LogDeltaClass::OutOfRange => continue,
        };

        // Per-window occurrence counts SUM every linked bucket at/before the
        // endpoint, WITHOUT deduping by bucket record ID (issue #361). Bucket
        // identity is (repository/signature/hour/width) and omits `LogSource`, so
        // two DISTINCT scan-logs sources observing the same signature in the same
        // hour mint the SAME bucket record ID with their own per-source counts;
        // summing preserves both sources and keeps these counts consistent with
        // the aggregate `occurrence_count`, which already sums the coalesced
        // signatures. The symmetric cost is that concatenating the IDENTICAL
        // scan-logs output multiplies counts (a degenerate, user-error input);
        // source-aware bucket identity is the true fix, tracked in #361.
        let (occurrence_source, base_window, head_window) = match buckets_by_sig.get(*id) {
            Some(buckets) if !buckets.is_empty() => {
                let base_sum = base_instant.map(|bt| window_bucket_sum(buckets, bt));
                let head_sum = head_instant.map(|ht| window_bucket_sum(buckets, ht));
                ("occurrence_buckets", base_sum, head_sum)
            }
            _ => ("aggregate_only", None, None),
        };

        // Resolved frames are keyed on the shared signature ID (issue #322), so
        // the index already unions the group's frame targets; sort + dedupe makes
        // the union deterministic and drops re-scanned duplicate frame edges.
        let mut resolved_frames = frames_by_sig.get(*id).cloned().unwrap_or_default();
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
            record_id: (*id).to_owned(),
            schema_version,
            change_class,
            severity,
            first_seen,
            last_seen,
            occurrence_count,
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

    // Repository-scope caveat: whenever `--repo` is set, disclose in the
    // envelope that log signatures are NOT repository-filtered (they carry no
    // retrievable attribution), and ELEVATE the message when the store actually
    // holds more than one distinct repository — the only condition under which an
    // unrelated repository's signature can bleed into this scoped answer. The
    // count collapses schema-version duplicates of one repository, mirroring the
    // `RepositoryIndex` version remap. Deterministic: fixed strings, no wall
    // clock. See docs/cli/log-deltas.md and the follow-up tracking issue.
    let repo_scope_caveat = match (repo_scope, repo_index.as_ref()) {
        (Some(scope), Some(index)) => {
            let distinct_repository_count = distinct_repository_count(index);
            let multi_repository_store = distinct_repository_count > 1;
            Some(LogRepoScopeCaveat {
                repo_scope: scope.to_owned(),
                distinct_repository_count,
                multi_repository_store,
                message: if multi_repository_store {
                    LOG_REPO_SCOPE_MULTI_CAVEAT
                } else {
                    LOG_REPO_SCOPE_SINGLE_CAVEAT
                },
            })
        }
        _ => None,
    };

    Ok(LogDeltas {
        base: base_sha,
        head: head_sha,
        window: LogDeltaWindow {
            window_start,
            window_end,
        },
        range_commit_count: range.range_commit_shas.len(),
        disclaimer: LOG_DELTAS_DISCLAIMER,
        repo_scope_caveat,
        new_signatures,
        ceased_signatures,
        continuing_signatures,
    })
}

/// Counts the distinct repositories a [`RepositoryIndex`] knows, collapsing the
/// schema-version duplicates of one repository into a single count.
///
/// A repository re-scanned under a newer schema version mints a new
/// `Repository` record whose stable-ID suffix (identity) is unchanged; grouping
/// by that suffix mirrors the index's own highest-version remap so a versioned
/// re-scan is not miscounted as two repositories. An ID that does not parse as a
/// code-graph handle counts as its own distinct repository (defensive — real
/// `Repository` IDs are always code-graph handles).
fn distinct_repository_count(index: &RepositoryIndex) -> usize {
    let mut identities: BTreeSet<String> = BTreeSet::new();
    for id in index.repository_ids() {
        match parse_codegraph_id(id) {
            Some((_, suffix)) => identities.insert(suffix.to_owned()),
            None => identities.insert(id.to_owned()),
        };
    }
    identities.len()
}

/// Parses an RFC 3339 timestamp to a UTC instant for ordering, or `None` when it
/// cannot be parsed.
///
/// All timestamps compared here (commit committer dates, signature
/// `first_seen`/`last_seen`, bucket `bucket_start`) originate from Egregore's own
/// scanners and are always parseable in practice; `None` is a defensive,
/// deterministic fallback that callers treat as "exclude", never a silent
/// misclassification.
fn parse_instant(rfc3339: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Which end of a coalesced signature group's valid-time bounds to keep.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum BoundKind {
    /// The earliest bound (merged `first_seen`).
    Earliest,
    /// The latest bound (merged `last_seen`).
    Latest,
}

/// Merges the valid-time bound across a coalesced signature group to the
/// earliest or latest value by parsed UTC instant, returning the winning RFC
/// 3339 string and its instant.
///
/// Comparison is by parsed instant, never raw RFC 3339 string order, for the
/// same cross-offset reason as [`classify`] (Codex P1). Parseable values are
/// preferred: a real timestamp always wins over an unparseable one, and only
/// when nothing in the group parses does the lexically smallest/largest raw
/// string win with a `None` instant (defensive — all values here originate from
/// Egregore's own scanners and are parseable in practice). The input slice is
/// non-empty by construction (a group exists only once a payload is pushed);
/// the empty fallback is unreachable but deterministic.
fn merge_bound(
    values: &[(Option<DateTime<Utc>>, &str)],
    kind: BoundKind,
) -> (String, Option<DateTime<Utc>>) {
    let parseable = values.iter().filter_map(|(dt, s)| dt.map(|d| (d, *s)));
    let winner = match kind {
        BoundKind::Earliest => parseable.min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1))),
        BoundKind::Latest => parseable.max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1))),
    };
    if let Some((dt, s)) = winner {
        return (s.to_owned(), Some(dt));
    }
    // Nothing parseable: deterministic lexical fallback over the raw strings.
    let raw = values.iter().map(|(_, s)| *s);
    let s = match kind {
        BoundKind::Earliest => raw.min(),
        BoundKind::Latest => raw.max(),
    };
    (s.unwrap_or_default().to_owned(), None)
}

/// Classifies one signature against the window from its `first_seen` /
/// `last_seen` valid times, comparing by parsed UTC instant.
///
/// Comparing raw RFC 3339 strings is wrong across UTC offsets (Codex P1): commit
/// committer dates carry local offsets (`%cI`) while scan-logs normalizes
/// signature times to UTC `Z`, so `"...05:00:00Z"` sorts lexically after
/// `"...00:30:00-05:00"` even though its instant precedes it. Every comparison is
/// therefore on parsed instants.
///
/// An empty/unparseable window (either bound `None`) or an unparseable
/// `first_seen`/`last_seen` yields [`LogDeltaClass::OutOfRange`] — the signature
/// is excluded rather than misclassified. This preserves the documented
/// empty-window behavior (no parseable range-commit time excludes every
/// signature).
fn classify(
    window_start: Option<DateTime<Utc>>,
    window_end: Option<DateTime<Utc>>,
    first_seen: Option<DateTime<Utc>>,
    last_seen: Option<DateTime<Utc>>,
) -> LogDeltaClass {
    let (Some(window_start), Some(window_end), Some(first_seen)) =
        (window_start, window_end, first_seen)
    else {
        return LogDeltaClass::OutOfRange;
    };
    if window_start <= first_seen && first_seen <= window_end {
        LogDeltaClass::New
    } else if first_seen > window_end {
        LogDeltaClass::OutOfRange
    } else {
        // Not new and not after the window ⇒ first observed before the window.
        match last_seen {
            Some(last_seen) if last_seen < window_end => LogDeltaClass::Ceased,
            Some(_) => LogDeltaClass::Continuing,
            // Unparseable last_seen: exclude rather than misclassify.
            None => LogDeltaClass::OutOfRange,
        }
    }
}

/// Sums the occurrence counts of the linked buckets whose `bucket_start` instant
/// is at or before `endpoint` (the endpoint's committer date as a UTC instant).
///
/// The comparison is by parsed instant, not raw string, for the same
/// cross-offset reason as [`classify`] (Codex P1). A bucket whose `bucket_start`
/// cannot be parsed is excluded from the sum rather than compared incorrectly.
fn window_bucket_sum(buckets: &[(&str, u64)], endpoint: DateTime<Utc>) -> u64 {
    buckets
        .iter()
        .filter(|(bucket_start, _)| parse_instant(bucket_start).is_some_and(|b| b <= endpoint))
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
