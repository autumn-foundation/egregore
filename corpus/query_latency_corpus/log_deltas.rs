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
//! Coalescing works on BOTH read paths. The embedded (`--data-dir`) lane loads
//! records through the log-retained read surface
//! ([`EmbeddedAletheiaSink::read_all_records_log_retained`](crate::adapters)),
//! which surfaces every superseded non-temporal `ErrorSignature` /
//! `LogOccurrenceBucket` version that differing-content `scan-logs` ingests
//! append — so the same duplicate slice the coalescer needs is present, and
//! `first_seen` / `last_seen` / occurrence counts are reconstructed exactly as on
//! the concatenated `--graph` JSONL (issue #363, which landed the adapter-level
//! retention). `log_deltas`'s `embedded_source` argument still gates a
//! [`LogEmbeddedRetentionCaveat`] disclosing the one residual divergence: a
//! byte-identical re-ingest of the same `scan-logs` output is an idempotent no-op
//! (deduped to one physical record) rather than multiplied, so identical rescans
//! do not inflate `--data-dir` counts the way concatenating identical JSONL does
//! on `--graph`. A single ingest is exact either way.
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
//! * `base_window_occurrences` = occurrences at or before `commit_valid_time[base]`;
//! * `head_window_occurrences` = occurrences at or before `commit_valid_time[head]`.
//!
//! Since issue #364 (schema v3) a `LogOccurrenceBucket` carries the sorted
//! per-occurrence `occurrence_timestamps` that fell in its hour, so a bucket
//! straddling the base/head commit instant CAN be sub-divided: the per-window
//! count is the number of timestamps at or before the endpoint instant — bounded
//! precisely at the commit instant even when the endpoint falls mid-hour. When
//! every contributing bucket carries timestamps the response reports
//! `occurrence_count_granularity == "endpoint_exact"`
//! ([`OCCURRENCE_COUNT_GRANULARITY_ENDPOINT_EXACT`]) and the endpoint-exact
//! disclaimer.
//!
//! A legacy `log:v2:` bucket carries no timestamps (they deserialize empty), so it
//! FALLS BACK to the hour-bucket predicate — its whole `occurrence_count` is summed
//! whenever `bucket_start <= endpoint`, which may pull in occurrences up to one
//! bucket width (1 hour) past the exact instant. The granularity marker is
//! PER-RESPONSE: a response degrades to `occurrence_count_granularity ==
//! "hourly_bucket"` ([`OCCURRENCE_COUNT_GRANULARITY`]) and the legacy
//! hour-bucket disclaimer the moment ANY contributing bucket falls back, honestly
//! disclosing the mixed/legacy case rather than fabricating sub-hour precision. The
//! "fully-before" predicate (`bucket_start + width <= endpoint`) was rejected for
//! the fallback because it would UNDER-count by dropping pre-endpoint occurrences.
//! Re-run `eg scan-logs` to regenerate buckets under schema v3 for exact counts.
//!
//! These per-window counts SUM every DISTINCT linked bucket across the coalesced
//! group, deduped by bucket record ID (issue #361, source-aware identity). A
//! `LogOccurrenceBucket` record ID is now `(repository/signature/hour/width/SOURCE)`,
//! so two DISTINCT scan-logs sources observing the same signature in the same hour
//! mint DISTINCT bucket IDs whose per-source counts each sum in — preserving both
//! sources and keeping these counts consistent with the aggregate `occurrence_count`,
//! which likewise sums the coalesced signatures. A genuine rescan of the IDENTICAL
//! source mints the SAME bucket ID (byte-identical) and is collapsed by the
//! dedup-by-ID, so concatenating the IDENTICAL scan-logs output no longer
//! double-counts buckets.
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
use crate::ir::{EdgeLabel, ErrorSignaturePayload, GraphRecord, LogPayload, NodeKind};

/// Advisory disclaimer for [`log_deltas`] responses that fell back to hour-bucket
/// granularity (issue #364).
///
/// Emitted when at least one contributing bucket is a legacy `log:v2:` record
/// carrying no per-occurrence timestamps. Selected at response build time; see
/// [`LOG_DELTAS_DISCLAIMER_ENDPOINT_EXACT`] for the fully-attributed case.
pub const LOG_DELTAS_DISCLAIMER: &str = "Rows are runtime error-signature observations classified \
     against the commit range's valid-time window. A signature first observed in-range is a \
     regression LEAD, not proof this range caused it; a ceased signature is not proof of a fix; \
     occurrence data only reflects the log sources that were scanned (a sampling artifact), never \
     the complete runtime behavior of the system. Per-window occurrence counts \
     (`base_window_occurrences`/`head_window_occurrences`) are hour-bucket-granular, not \
     endpoint-exact: at least one contributing `LogOccurrenceBucket` is a legacy record carrying \
     no per-occurrence timestamps, so its whole hourly bucket is summed whenever its start is at \
     or before the endpoint, and when the endpoint falls mid-hour a count may include occurrences \
     up to one bucket width (1 hour) past the exact commit instant. Re-run `eg scan-logs` to \
     regenerate the buckets under schema v3 (issue #364) for endpoint-exact counts.";

/// Advisory disclaimer for [`log_deltas`] responses with endpoint-exact counts
/// (issue #364).
///
/// Emitted when every contributing `LogOccurrenceBucket` carries per-occurrence
/// timestamps (schema v3), so a count includes only occurrences at or before the
/// exact commit instant even when the endpoint falls mid-hour. Selected at response
/// build time; the legacy hour-bucket wording is [`LOG_DELTAS_DISCLAIMER`].
pub const LOG_DELTAS_DISCLAIMER_ENDPOINT_EXACT: &str = "Rows are runtime error-signature \
     observations classified against the commit range's valid-time window. A signature first \
     observed in-range is a regression LEAD, not proof this range caused it; a ceased signature is \
     not proof of a fix; occurrence data only reflects the log sources that were scanned (a \
     sampling artifact), never the complete runtime behavior of the system. Per-window occurrence \
     counts (`base_window_occurrences`/`head_window_occurrences`) are endpoint-exact: every \
     contributing `LogOccurrenceBucket` carries per-occurrence timestamps (schema v3, issue #364), \
     so a count includes only occurrences at or before the exact commit instant, even when the \
     endpoint falls mid-hour.";

/// Granularity marker for hour-bucket-granular per-window occurrence counts.
///
/// Emitted when at least one contributing bucket is a legacy `log:v2:` record with
/// no per-occurrence timestamps, so its whole hourly bucket is summed and a
/// mid-hour endpoint may over-count by up to one bucket width. The marker is
/// PER-RESPONSE and conditional (issue #364): a response reports
/// [`OCCURRENCE_COUNT_GRANULARITY_ENDPOINT_EXACT`] when every contributing bucket
/// carried timestamps (or no buckets contributed), and this value otherwise.
pub const OCCURRENCE_COUNT_GRANULARITY: &str = "hourly_bucket";

/// Granularity marker for endpoint-exact per-window occurrence counts.
///
/// Emitted when every contributing `LogOccurrenceBucket` carries per-occurrence
/// timestamps (schema v3, issue #364), so each count is bounded precisely at the
/// commit instant. Also reported (vacuously) when no bucket contributed a
/// per-window count. See [`OCCURRENCE_COUNT_GRANULARITY`].
pub const OCCURRENCE_COUNT_GRANULARITY_ENDPOINT_EXACT: &str = "endpoint_exact";

/// Residual repository-scope caveat text emitted with `--repo` (issue #362).
///
/// Emitted when the store holds at least one legacy `log:v2:` log signature that
/// carries no persisted repository attribution and was therefore excluded from the
/// scoped run. Since issue #362 (schema v3) persisted `repository_id` on every log payload,
/// `--repo` filters log signatures by their attribution: a signature attributed
/// to a different repository is soundly excluded. The ONLY residual honesty gap is
/// a legacy `log:v2:` record whose `repository_id` deserializes empty: it cannot
/// be proven to belong to the scoped repository, so it is EXCLUDED (conservative,
/// possible under-report) rather than bled in (never a cross-repository false
/// lead). This message discloses that residual and never claims full isolation or
/// a guaranteed-complete scoped result. The excluded count rides the caveat's
/// `excluded_unattributed_signature_count` field.
pub const LOG_REPO_SCOPE_RESIDUAL_CAVEAT: &str = "`--repo` now filters log signatures by their \
     persisted repository attribution (schema v3, issue #362): a signature attributed to a \
     different repository is excluded. This store additionally holds legacy log signatures (schema \
     v2, see `excluded_unattributed_signature_count`) that carry NO persisted repository \
     attribution; because they cannot be proven to belong to the scoped repository, they were \
     EXCLUDED from this scoped run rather than bled in. This is a conservative exclusion — the \
     scoped result may UNDER-report for those legacy records until they are re-scanned — never a \
     cross-repository bleed and never a full-isolation guarantee. Re-run `eg scan-logs` to \
     regenerate them under schema v3 with retrievable attribution, or keep per-repository stores.";

/// Residual repository-scope disclosure attached to a [`LogDeltas`] response
/// (issue #362, schema v3).
///
/// Since `repository_id` is persisted on every log payload, `--repo` now filters
/// log signatures by their attribution: a signature attributed to a different
/// repository is soundly excluded, so the former "logs are never
/// repository-filtered" caveat no longer applies. The ONLY residual honesty gap is
/// a legacy `log:v2:` signature whose `repository_id` deserializes empty: it
/// cannot be proven to belong to the scoped repository, so it is EXCLUDED from a
/// scoped run rather than bled in. This caveat is emitted ONLY when `--repo` is set
/// AND at least one such legacy signature was actually excluded, disclosing that
/// conservative exclusion (possible under-report, never a cross-repository bleed)
/// and pointing to a re-scan remedy. A fully schema-v3 store (every log signature
/// attributed) carries NO caveat — the scoped filtering is sound.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogRepoScopeCaveat {
    /// The repository selector that was applied.
    pub repo_scope: String,
    /// Count of legacy (schema v2, empty `repository_id`) log signatures that
    /// could not be attributed and were EXCLUDED from this scoped run. Always
    /// `> 0` when this caveat is present (the caveat is omitted otherwise).
    pub excluded_unattributed_signature_count: usize,
    /// Fixed advisory text ([`LOG_REPO_SCOPE_RESIDUAL_CAVEAT`]). Discloses the
    /// conservative exclusion of unattributed legacy signatures and the re-scan
    /// remedy; never claims full isolation or a guaranteed-complete result.
    pub message: &'static str,
}

/// Envelope caveat emitted ONLY on the embedded (`--data-dir`) read path when
/// the store holds at least one `ErrorSignature` record (issue #363).
///
/// `ErrorSignature` and `LogOccurrenceBucket` are NON-temporal nodes, so
/// ingesting multiple `scan-logs` outputs of the SAME stable signature/bucket ID
/// appends a superseded physical version per differing-content scan. The
/// embedded log-retained read surface
/// ([`EmbeddedAletheiaSink::read_all_records_log_retained`](crate::adapters))
/// surfaces every one of those versions, so the cross-scan coalescing this query
/// performs on the `--graph` path — grouping duplicate signatures by stable ID
/// and merging earliest `first_seen` / latest `last_seen` / summed occurrence
/// counts BEFORE classification — IS reconstructed on `--data-dir` for scans
/// whose captured content differs.
///
/// Since issue #361 made `LogOccurrenceBucket` identity source-aware
/// (repository/signature/hour/width/SOURCE), the former same-hour/same-count
/// bucket divergence is GONE: distinct sources now mint DISTINCT bucket IDs
/// (summed on both paths) and a genuine rescan mints the SAME bucket ID (deduped
/// on both paths — `--graph` dedups by record ID before summing, `--data-dir`
/// dedups at write time), so per-window occurrence counts CONVERGE.
///
/// The caveat remains a disclosure because ONE residual divergence persists,
/// rooted purely in the idempotent-write dedup of byte-identical non-temporal
/// records — NOT a bucket-identity gap: a byte-identical re-ingest of the SAME
/// `scan-logs` output (the same signature record appended twice) is an idempotent
/// no-op on `--data-dir` (deduped to one physical record) but is summed on
/// `--graph` (which groups duplicate signatures by stable ID and sums their
/// aggregate `occurrence_count`), so concatenating identical JSONL inflates the
/// `--graph` aggregate count while an identical re-scan does not inflate
/// `--data-dir`. A SINGLE `scan-logs` ingest is exact either way.
pub const LOG_EMBEDDED_RETENTION_CAVEAT: &str = "Embedded (`--data-dir`) stores retain every \
     superseded non-temporal log observation (`ErrorSignature` / `LogOccurrenceBucket`), so the \
     cross-scan coalescing performed on the `--graph` path (earliest `first_seen`, latest \
     `last_seen`, summed occurrence counts) is reconstructed here for scans whose captured content \
     differs. Since bucket identity became source-aware (issue #361), distinct sources mint \
     distinct bucket IDs (summed on both paths) and genuine rescans mint identical bucket IDs \
     (deduped on both paths), so per-window occurrence counts converge. One residual divergence \
     from `--graph` persists, rooted only in idempotent-write dedup (not bucket identity): a \
     byte-identical re-ingest of the same `scan-logs` output is deduped to one physical record on \
     `--data-dir` but summed on `--graph`, so concatenating identical JSONL inflates the `--graph` \
     aggregate `occurrence_count` while an identical re-scan does not inflate `--data-dir`. Issue \
     #363.";

/// Advisory disclosure attached to a [`LogDeltas`] response whenever the query
/// runs over the embedded (`--data-dir`) read path AND the store holds at least
/// one `ErrorSignature` record (issue #363).
///
/// Discloses in the machine-readable envelope (not only the docs) that the
/// embedded read path now retains superseded non-temporal log observations, so
/// cross-scan coalescing is reconstructed to match `--graph` for differing-content
/// scans, leaving only a residual byte-identical-reingest idempotency divergence.
/// Present only on the embedded path with log
/// records; absent for `--graph` queries and for embedded stores with no log
/// records (where single-ingest results are exact and no disclosure is warranted).
/// The `message` is a fixed string ([`LOG_EMBEDDED_RETENTION_CAVEAT`]), so the
/// envelope stays deterministic and byte-stable.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogEmbeddedRetentionCaveat {
    /// Fixed advisory text ([`LOG_EMBEDDED_RETENTION_CAVEAT`]).
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
    /// Always-present advisory disclaimer, selected per-response:
    /// [`LOG_DELTAS_DISCLAIMER_ENDPOINT_EXACT`] when every contributing bucket
    /// carried per-occurrence timestamps, [`LOG_DELTAS_DISCLAIMER`] (the legacy
    /// hour-bucket wording) when at least one fell back (issue #364).
    pub disclaimer: &'static str,
    /// Always-present granularity marker for the per-window occurrence counts,
    /// PER-RESPONSE and conditional (issue #364):
    /// [`OCCURRENCE_COUNT_GRANULARITY_ENDPOINT_EXACT`] (`"endpoint_exact"`) when
    /// every contributing `LogOccurrenceBucket` carried per-occurrence timestamps
    /// (schema v3), so `base_window_occurrences`/`head_window_occurrences` are
    /// bounded precisely at the commit instant; [`OCCURRENCE_COUNT_GRANULARITY`]
    /// (`"hourly_bucket"`) when at least one contributing bucket was a legacy
    /// `log:v2:` record with no timestamps, so a count may include occurrences up
    /// to one bucket width (1 hour) past the exact endpoint.
    pub occurrence_count_granularity: &'static str,
    /// Residual repository-scope caveat (issue #362, schema v3), present only when
    /// `--repo` is set AND at least one legacy unattributed (`log:v2:`) signature
    /// was excluded from the scoped run — see [`LogRepoScopeCaveat`]. A
    /// fully-schema-v3 store (every log signature attributed) carries NO caveat:
    /// `--repo` filters log signatures soundly. Absent for unscoped queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_scope_caveat: Option<LogRepoScopeCaveat>,
    /// Embedded-store retention caveat, present only when the query ran over the
    /// embedded (`--data-dir`) read path AND the store holds at least one
    /// `ErrorSignature` record (issue #363): cross-scan coalescing IS
    /// reconstructed on the embedded path (the log-retained read surfaces every
    /// superseded log version), leaving only the byte-identical-re-ingest
    /// idempotency divergence to disclose — see [`LogEmbeddedRetentionCaveat`].
    /// Absent (omitted from JSON) for `--graph` queries and for embedded stores
    /// with no log records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedded_log_retention_caveat: Option<LogEmbeddedRetentionCaveat>,
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
/// `embedded_source` records whether the caller loaded `records` from the
/// embedded (`--data-dir`) read path rather than a `--graph` JSONL. That path now
/// loads through the log-retained read surface, which surfaces every superseded
/// non-temporal `ErrorSignature` / `LogOccurrenceBucket` version, so cross-scan
/// coalescing IS reconstructed for differing-content scans; when it is set AND
/// the store holds at least one `ErrorSignature`, the response still carries
/// [`LogEmbeddedRetentionCaveat`] disclosing the one residual divergence
/// (byte-identical re-ingests are deduped, not multiplied) (issue #363). It never
/// changes classification — only whether the caveat is emitted.
#[allow(clippy::missing_panics_doc)]
pub fn log_deltas(
    records: &[GraphRecord],
    base_prefix: &str,
    head_prefix: &str,
    repo_scope: Option<&str>,
    embedded_source: bool,
) -> Result<LogDeltas, RangeDeltasError> {
    // Repository scoping mirrors `range_deltas` for the CODE side: in a shared
    // store two repositories can carry the same commit SHA, so commit resolution,
    // the valid-time window, and the symbol-delta join are gated by owning
    // repository when a scope is set.
    //
    // Since issue #362 (schema v3) persisted a retrievable `repository_id` on
    // every log payload, `RepositoryIndex::owner_of(<signature-id>)` now RESOLVES,
    // so `--repo` also SOUNDLY filters the log side: a signature attributed to a
    // different repository is excluded (the cross-repository false-lead that #326
    // could only disclose). The one residual honesty gap is a legacy `log:v2:`
    // signature whose `repository_id` deserializes empty (serde default): it
    // cannot be proven to belong to the scoped repository, so it is EXCLUDED from a
    // scoped run rather than bled in — a conservative under-report, never a bleed.
    // When at least one such legacy signature is excluded, the response envelope
    // carries `repo_scope_caveat` (see the caveat construction near the return)
    // pointing to a re-scan remedy; a fully-schema-v3 store carries no caveat.
    // Documented in `docs/cli/log-deltas.md` and issue #362.
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

    // signature_id → linked (bucket_start, occurrence_count) pairs, DEDUPED by
    // bucket record ID (issue #361, source-aware identity). A `LogOccurrenceBucket`
    // record ID is now (repository/signature/hour/width/SOURCE), so two DISTINCT
    // scan-logs sources observing the same signature in the same hour mint DISTINCT
    // bucket IDs (each pushed once → summed naturally below), while a genuine rescan
    // of identical bytes mints the SAME bucket ID (byte-identical → collapsed here).
    // Iterating NODES and deduping by record ID therefore counts each real
    // observation exactly once and no longer double-counts a concatenated rescan.
    // Each linked bucket carries its hour-aligned `bucket_start`, aggregate
    // `occurrence_count`, and (schema v3, issue #364) the sorted per-occurrence
    // `occurrence_timestamps` that let [`window_bucket_sum`] bound a per-window
    // count endpoint-exactly. A legacy `log:v2:` bucket deserializes empty
    // timestamps and falls back to the hour-bucket predicate.
    let mut buckets_by_sig: BTreeMap<&str, Vec<BucketWindow>> = BTreeMap::new();
    let mut seen_bucket_ids: BTreeSet<&str> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            log: Some(payload), ..
        } = r
        {
            if let LogPayload::LogOccurrenceBucket(bucket) = payload.as_ref() {
                if !seen_bucket_ids.insert(r.id()) {
                    // A byte-identical rescan of the same source: same bucket ID,
                    // already counted. Collapse it (do not sum).
                    continue;
                }
                if let Some(sigs) = bucket_targets.get(r.id()) {
                    for sig in sigs {
                        buckets_by_sig.entry(*sig).or_default().push(BucketWindow {
                            bucket_start: bucket.bucket_start.as_str(),
                            occurrence_count: bucket.occurrence_count,
                            occurrence_timestamps: &bucket.occurrence_timestamps,
                        });
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
    // Legacy (`log:v2:`) signatures excluded from a scoped run because they carry
    // no persisted `repository_id` and cannot be proven in-repo (issue #362).
    // Deduped by stable ID so a coalesced group counts once.
    let mut excluded_unattributed: BTreeSet<&str> = BTreeSet::new();
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
        let LogPayload::ErrorSignature(sig) = payload.as_ref() else {
            continue;
        };
        // #362 repository filtering: `repository_id` (schema v3) makes the log side
        // attributable, so `--repo` filters signatures soundly. A signature
        // attributed to a DIFFERENT repository is excluded (the cross-repository
        // false lead #326 could only disclose). A legacy `log:v2:` signature with
        // an empty `repository_id` cannot be proven in-repo, so it is EXCLUDED
        // (conservative under-report, never a bleed) and tallied for the residual
        // caveat. `owner_of` reads the persisted `repository_id`, remapped to the
        // highest-version `Repository` id exactly like the `--repo` selector.
        if let (Some(scope), Some(index)) = (repo_scope, repo_index.as_ref()) {
            if sig.repository_id.is_empty() {
                excluded_unattributed.insert(id.as_str());
                continue;
            }
            if index.owner_of(id.as_str()) != Some(scope) {
                continue;
            }
        }
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

    // Per-response granularity flag (issue #364): stays true while every
    // contributing bucket carries per-occurrence timestamps (endpoint-exact) and
    // flips false the first time any window sum falls back to a legacy `log:v2:`
    // hour-bucket. A response with no contributing buckets stays true (vacuously
    // endpoint-exact — there are no hour-granular approximations to disclose).
    let mut all_buckets_endpoint_exact = true;

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

        // Per-window occurrence counts SUM every DISTINCT linked bucket (deduped
        // by record ID above) at/before the endpoint (issue #361, source-aware
        // identity). Bucket identity is now
        // (repository/signature/hour/width/SOURCE), so two DISTINCT scan-logs
        // sources observing the same signature in the same hour mint DISTINCT
        // bucket IDs whose per-source counts each sum in; a genuine rescan of
        // identical bytes mints the SAME bucket ID and was collapsed above, so
        // concatenating an identical scan-logs output no longer double-counts.
        // Each `window_bucket_sum` reports whether it was fully endpoint-exact
        // (no legacy hour-bucket fallback); the response-level flag is the AND
        // across both endpoints of every bucketed signature (issue #364).
        let (occurrence_source, base_window, head_window) = match buckets_by_sig.get(*id) {
            Some(buckets) if !buckets.is_empty() => {
                let base = base_instant.map(|bt| window_bucket_sum(buckets, bt));
                let head = head_instant.map(|ht| window_bucket_sum(buckets, ht));
                if base.is_some_and(|(_, exact)| !exact) || head.is_some_and(|(_, exact)| !exact) {
                    all_buckets_endpoint_exact = false;
                }
                (
                    "occurrence_buckets",
                    base.map(|(sum, _)| sum),
                    head.map(|(sum, _)| sum),
                )
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

    // Residual repository-scope caveat (issue #362, schema v3): `--repo` now
    // filters log signatures soundly by their persisted `repository_id`, so the
    // former "logs are never repository-filtered" disclosure is gone. The ONLY
    // residual honesty gap is a legacy `log:v2:` signature with an empty
    // `repository_id` that was EXCLUDED because it could not be proven in-repo
    // (tallied above). Emit the caveat ONLY when at least one such legacy signature
    // was excluded; a fully-schema-v3 scoped store carries no caveat. Fixed string
    // + a determined count — byte-stable, no wall clock. See docs/cli/log-deltas.md.
    let repo_scope_caveat = match repo_scope {
        Some(scope) if !excluded_unattributed.is_empty() => Some(LogRepoScopeCaveat {
            repo_scope: scope.to_owned(),
            excluded_unattributed_signature_count: excluded_unattributed.len(),
            message: LOG_REPO_SCOPE_RESIDUAL_CAVEAT,
        }),
        _ => None,
    };

    // Embedded-store retention caveat (issue #363): the embedded `--data-dir`
    // read path now retains superseded non-temporal log observations
    // (`ErrorSignature` / `LogOccurrenceBucket`), so multiple `scan-logs` ingests
    // of the same stable ID whose captured content differs are coalesced above
    // exactly like the `--graph` path rather than collapsed to a single
    // last-write-wins record. One residual divergence remains: byte-identical
    // re-ingests are idempotent (deduped to one physical record) instead of
    // multiplied, so identical re-scans do not inflate counts here the way
    // concatenating identical JSONL does on `--graph`. DIAGNOSE rather than
    // reject: a single-ingest store is exact, so the caveat is gated on log
    // records actually being present (`sig_groups` holds every `ErrorSignature`
    // node encountered, regardless of classification). The `--graph` path never
    // carries this caveat. Fixed string, no wall clock — byte-stable. See issue
    // #363 and docs/cli/log-deltas.md.
    let embedded_log_retention_caveat = if embedded_source && !sig_groups.is_empty() {
        Some(LogEmbeddedRetentionCaveat {
            message: LOG_EMBEDDED_RETENTION_CAVEAT,
        })
    } else {
        None
    };

    Ok(LogDeltas {
        base: base_sha,
        head: head_sha,
        window: LogDeltaWindow {
            window_start,
            window_end,
        },
        range_commit_count: range.range_commit_shas.len(),
        disclaimer: if all_buckets_endpoint_exact {
            LOG_DELTAS_DISCLAIMER_ENDPOINT_EXACT
        } else {
            LOG_DELTAS_DISCLAIMER
        },
        occurrence_count_granularity: if all_buckets_endpoint_exact {
            OCCURRENCE_COUNT_GRANULARITY_ENDPOINT_EXACT
        } else {
            OCCURRENCE_COUNT_GRANULARITY
        },
        repo_scope_caveat,
        embedded_log_retention_caveat,
        new_signatures,
        ceased_signatures,
        continuing_signatures,
    })
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

/// One linked occurrence bucket's window-relevant fields.
#[derive(Debug, Clone, Copy)]
struct BucketWindow<'a> {
    /// Hour-aligned RFC 3339 UTC bucket start.
    bucket_start: &'a str,
    /// Aggregate occurrences in the bucket (used only for the legacy fallback).
    occurrence_count: u64,
    /// Sorted per-occurrence RFC 3339 UTC valid times (schema v3, issue #364).
    /// Empty for a legacy `log:v2:` bucket.
    occurrence_timestamps: &'a [String],
}

/// Sums the occurrences of the linked buckets that fall at or before `endpoint`
/// (the endpoint's committer date as a UTC instant), returning the sum and
/// whether the sum was fully ENDPOINT-EXACT (issue #364).
///
/// A schema-v3 bucket carries per-occurrence `occurrence_timestamps`, so its
/// contribution is the count of timestamps at or before the endpoint — bounded
/// precisely at the commit instant even when the endpoint falls mid-hour. A legacy
/// `log:v2:` bucket carries no timestamps, so it FALLS BACK to the hour-bucket
/// predicate (its whole `occurrence_count` is summed whenever `bucket_start` is at
/// or before the endpoint) and the returned exact-flag is cleared, degrading the
/// response granularity to `hourly_bucket`.
///
/// Every comparison is by parsed instant, not raw string, for the same
/// cross-offset reason as [`classify`] (Codex P1). A bucket start or timestamp that
/// cannot be parsed is excluded from the sum rather than compared incorrectly.
fn window_bucket_sum(buckets: &[BucketWindow], endpoint: DateTime<Utc>) -> (u64, bool) {
    let mut sum: u64 = 0;
    let mut endpoint_exact = true;
    for bucket in buckets {
        let (count, exact) = bucket_occurrences_at_or_before(
            bucket.bucket_start,
            bucket.occurrence_count,
            bucket.occurrence_timestamps,
            endpoint,
        );
        sum += count;
        endpoint_exact &= exact;
    }
    (sum, endpoint_exact)
}

/// Endpoint-exact occurrence count for a SINGLE `LogOccurrenceBucket` at or
/// before `endpoint`, returning `(count, endpoint_exact)` (issue #364).
///
/// A schema-v3 bucket carries per-occurrence `occurrence_timestamps`, so its
/// contribution is the count of timestamps at or before the endpoint — bounded
/// precisely at the commit instant even when the endpoint falls mid-hour, and
/// `endpoint_exact` is `true`. A legacy `log:v2:` bucket carries no timestamps
/// (they deserialize empty), so it FALLS BACK to the hour-bucket predicate (its
/// whole `occurrence_count` counts iff `bucket_start` is at or before the
/// endpoint) and `endpoint_exact` is `false`, degrading the caller's response
/// granularity to `hourly_bucket`. Every comparison is by parsed UTC instant,
/// never raw RFC 3339 string order (Codex P1); an unparseable bucket start or
/// timestamp is excluded rather than compared incorrectly. Shared by
/// [`window_bucket_sum`] (log-deltas) and `error_context`'s `--as-of` bucket
/// view so the two lanes count identically.
#[must_use]
pub fn bucket_occurrences_at_or_before(
    bucket_start: &str,
    occurrence_count: u64,
    occurrence_timestamps: &[String],
    endpoint: DateTime<Utc>,
) -> (u64, bool) {
    if occurrence_timestamps.is_empty() && occurrence_count > 0 {
        // Legacy v2 bucket: no per-occurrence data. Whole-bucket predicate.
        let count = if parse_instant(bucket_start).is_some_and(|b| b <= endpoint) {
            occurrence_count
        } else {
            0
        };
        (count, false)
    } else {
        // v3 bucket: count only occurrences at or before the endpoint instant.
        let count = occurrence_timestamps
            .iter()
            .filter(|t| parse_instant(t).is_some_and(|ts| ts <= endpoint))
            .count() as u64;
        (count, true)
    }
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
