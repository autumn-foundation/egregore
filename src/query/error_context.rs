//! Cross-domain runtime-error context bundle (`eg query error-context`,
//! issue #324).
//!
//! Answers ONE question — "what does the graph know about this error signature?"
//! — by resolving an [`ErrorSignature`](crate::ir::NodeKind::ErrorSignature)
//! handle and assembling a single deterministic, trust-separated cross-domain
//! envelope: the signature identity plus its occurrence buckets and resolved
//! backtrace frames (`runtime_observation`), the code source facts its frames
//! name (`source_fact`), the agent runs/commands it was `EMITTED_DURING`
//! (`agent_observation` / `verification`), the tasks it `REFERENCES_TASK`
//! (`project_state`), a history `first_seen_range`, and — behind an opt-in
//! `--protected-store` — the protected raw-payload handles whose content hash
//! matches the signature's captured `LogSource`.
//!
//! This lane is a **read-time join** of three shipped cores — it mints no edge
//! and introduces no new node kind, edge label, or trust class:
//!
//! * [`record_context`](super::record_context) supplies the code half's
//!   trust-separated cross-domain bundle from each resolved frame target;
//! * the `link-logs` / `resolve-frames` log-edge topology (`CAPTURED_FROM`,
//!   `FRAME_RESOLVES_TO`, `AGGREGATES`, `EMITTED_DURING`, `REFERENCES_TASK`)
//!   supplies the runtime half;
//! * the issue #118 range mechanics ([`range_deltas`](super::range_deltas))
//!   supply the `first_seen_range` symbol-delta overlap.
//!
//! Every row is a CORRELATION LEAD, never proof of cause: a resolved frame
//! proves the backtrace NAMES a symbol, not that it is at fault; an
//! `EMITTED_DURING` edge is a content-hash or temporal correlation, never
//! causation. Output is deterministic and byte-identical across runs; no raw
//! log/transcript/command/patch text ever enters the envelope beyond the
//! signature's bounded, post-redaction `template_excerpt`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{
    LOG_EMBEDDED_RETENTION_CAVEAT, LogEmbeddedRetentionCaveat, OverlappingSymbolDelta,
    ResolvedFrameHandle, UnresolvedRef, range_deltas,
};
use crate::ir::{
    CorrelationBasis, EdgeLabel, ErrorSignaturePayload, GraphRecord, LogPayload, NodeKind,
    SourceSpan,
};
use crate::log_resolve;
use crate::protected::{PROTECTED_HANDLE_PREFIX, ProtectedStore};
use crate::temporal_status::{SupersessionMode, TemporalResolver};

/// Always-present advisory stamped on every [`ErrorContext`]. NOT proof of cause.
pub const ERROR_CONTEXT_DISCLAIMER: &str = "Rows are CORRELATION LEADS, never proof of cause: a \
     resolved frame proves the backtrace NAMES a symbol, not that it is at fault; an EMITTED_DURING \
     edge is a content-hash or temporal correlation, never causation; and the absence of a lead is \
     not proof of unrelatedness. Occurrence data reflects only the log sources that were scanned.";

/// Fixed advisory emitted in `repo_scope_caveat` whenever `--repo` is set.
///
/// `--repo` scopes only the code-side `first_seen_range` symbol-delta join. Log
/// records carry no retrievable repository attribution (their repository ID is
/// only hashed into their stable IDs), so the runtime sections
/// (`signatures`/frames/buckets and their `EMITTED_DURING` observations) are
/// NEVER repository-filtered — mirroring `eg query log-deltas`.
pub const REPO_SCOPE_CAVEAT: &str = "--repo scopes only the code-side first_seen_range \
     symbol-delta join; log records carry no retrievable repository attribution, so the signature, \
     frame, bucket, and EMITTED_DURING observation sections are NOT repository-filtered.";

/// Strips a version-agnostic `log:v<N>:` stable-ID prefix (see
/// [`log_stable_id`](crate::ir::log_stable_id)), returning the hex tail (which
/// may be a partial prefix). Accepts ANY positive integer version so opaque
/// `log:v1:` fixtures and real `log:v2:` handles (issue #361 schema bump) both
/// resolve; the exact-record-ID resolution semantics are otherwise unchanged.
fn strip_log_prefix(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("log:v")?;
    let (version, hex) = rest.split_once(':')?;
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(hex)
}

/// A universal, redaction-safe projection of one `&GraphRecord` for a
/// trust-separated context section.
///
/// Carries ONLY bounded handles: record ID, schema version, domain, the derived
/// `trust_class`, node kind, a repo-relative path + span (code rows), valid
/// time / commit provenance, an optional `correlation_basis` (on
/// `EMITTED_DURING` observation rows), and per-row supersession flags (under
/// `--supersession include-but-flag`). No free-text summary field is ever
/// emitted, so no raw payload can escape through a row.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// Stable record ID (citation handle).
    pub record_id: String,
    /// Schema version stamped on the record.
    pub schema_version: u32,
    /// Record domain (`codegraph`, `agent_memory`, `project`, …).
    pub domain: String,
    /// Trust class derived from the domain — REQUIRED on every row.
    pub trust_class: &'static str,
    /// Node kind name (`Symbol`, `Observation`, `CommandRun`, …).
    pub kind: String,
    /// Repo-relative path when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Source span when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Valid time when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_time: Option<String>,
    /// Introducing commit SHA for a history-backed (temporal) record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// Correlation basis when this row entered via an `EMITTED_DURING` edge:
    /// `content_hash_join` or `temporal_correlation`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_basis: Option<String>,
    /// Supersession status under `--supersession include-but-flag`:
    /// `superseded` / `contradicted`. Absent when the row is current.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersession_status: Option<String>,
    /// Forward supersession/contradiction handles, present alongside
    /// `supersession_status`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub superseded_by: Vec<String>,
}

/// One `CAPTURED_FROM` `LogSource` handle carried on a [`SignatureBlock`].
#[derive(Debug, Clone, Serialize)]
pub struct SourceHandle {
    /// Stable `LogSource` record ID.
    pub record_id: String,
    /// Repo-relative path of the captured log artifact.
    pub source_relative_path: String,
    /// BLAKE3 hex of the newline-normalized artifact bytes.
    pub source_artifact_hash: String,
}

/// One hourly occurrence bucket carried on a [`SignatureBlock`] (issue #320).
#[derive(Debug, Clone, Serialize)]
pub struct BucketRow {
    /// Stable `LogOccurrenceBucket` record ID.
    pub record_id: String,
    /// RFC 3339 UTC hour-aligned bucket start.
    pub bucket_start: String,
    /// Bucket width token (`1h`).
    pub bucket_width: String,
    /// Occurrences of the signature within this bucket.
    pub occurrence_count: u64,
}

/// One resolved-anchor signature block (trust class `runtime_observation`).
#[derive(Debug, Clone, Serialize)]
pub struct SignatureBlock {
    /// Stable `ErrorSignature` record ID.
    pub record_id: String,
    /// Schema version stamped on the signature.
    pub schema_version: u32,
    /// Always `runtime_observation`.
    pub trust_class: &'static str,
    /// Closed severity class: `fatal` / `error` / `warn`.
    pub severity: String,
    /// Fingerprint algorithm identifier (`template-v1`).
    pub fingerprint_algorithm: String,
    /// Bounded, post-redaction template excerpt — the ONLY free text emitted.
    pub template_excerpt: String,
    /// Valid time of the earliest occurrence (coalesced earliest).
    pub first_seen: String,
    /// Valid time of the latest occurrence (coalesced latest).
    pub last_seen: String,
    /// Aggregate occurrence count across scanned sources (coalesced sum).
    pub occurrence_count: u64,
    /// `CAPTURED_FROM` `LogSource` handles, sorted.
    pub source_handles: Vec<SourceHandle>,
    /// `AGGREGATES` occurrence buckets, sorted by `(bucket_start, record_id)`.
    pub buckets: Vec<BucketRow>,
    /// `FRAME_RESOLVES_TO` resolved-frame handles, canonically ordered.
    pub frames: Vec<ResolvedFrameHandle>,
}

/// The narrowest commit window bracketing a signature's `first_seen`.
#[derive(Debug, Clone, Serialize)]
pub struct FirstSeenWindow {
    /// Which resolved signature's `first_seen` anchored the window.
    pub anchor_signature_id: String,
    /// The `first_seen` valid time being bracketed.
    pub first_seen: String,
    /// Newest commit with valid_time `<= first_seen` (`None` if it precedes all).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// Oldest commit with valid_time `>= first_seen` (`None` if it follows all).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    /// Base commit valid time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_start: Option<String>,
    /// Head commit valid time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_end: Option<String>,
    /// Range symbol deltas overlapping the signatures' resolved frame targets.
    pub overlapping_symbol_deltas: Vec<OverlappingSymbolDelta>,
}

/// History `first_seen_range` block: a bracketing commit window, or an explicit
/// `history_unavailable` verdict on a plain `scan` graph.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FirstSeenRange {
    /// A history-backed graph yielded a bracketing window.
    History(FirstSeenWindow),
    /// No `Commit` timeline present — never a fabricated window.
    Unavailable {
        /// Fixed diagnostic string `history_unavailable`.
        diagnostic: &'static str,
    },
}

/// A record removed from (or flagged within) a trust-separated section by the
/// supersession policy.
#[derive(Debug, Clone, Serialize)]
pub struct ExcludedRef {
    /// Stable record ID that was superseded or contradicted.
    pub record_id: String,
    /// Reason: `superseded` or `contradicted`.
    pub reason: String,
    /// Forward supersession/contradiction handles.
    pub superseded_by: Vec<String>,
}

/// A protected raw-payload handle matched at read time to a signature's
/// captured `LogSource` (issue #60/#321). Raw bytes are NEVER read.
#[derive(Debug, Clone, Serialize)]
pub struct ProtectedPayloadRef {
    /// The matched content hash (LogSource `source_artifact_hash`).
    pub source_artifact_hash: String,
    /// Which signature's `LogSource` matched.
    pub signature_id: String,
    /// Protected handle string (`protected:v1:<hex>`).
    pub handle: String,
    /// Protected payload class (e.g. `log_payload`).
    pub source_class: String,
    /// Exact payload byte length (metadata only).
    pub byte_len: u64,
}

/// The assembled cross-domain error-context envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorContext {
    /// The handle as supplied (echoed for citation).
    pub handle: String,
    /// Resolved anchor signature IDs, sorted (`>1` when a symbol named many).
    pub signature_ids: Vec<String>,
    /// One block per resolved signature (`runtime_observation`).
    pub signatures: Vec<SignatureBlock>,
    /// History window, or `history_unavailable`.
    pub first_seen_range: FirstSeenRange,
    /// Code `Symbol`/`File`/`Module`/`Import` facts (`source_fact`).
    pub source_facts: Vec<Row>,
    /// Agent observations (`agent_observation`), incl. `EMITTED_DURING` runs.
    pub observations: Vec<Row>,
    /// Project/task state (`project_state`).
    pub project_state: Vec<Row>,
    /// Artifacts (`artifact`).
    pub artifacts: Vec<Row>,
    /// Verification evidence (`verification`), incl. `EMITTED_DURING` commands.
    pub verification_evidence: Vec<Row>,
    /// Evidence-link targets absent from the store slice.
    pub unresolved: Vec<UnresolvedRef>,
    /// Records dropped/flagged by the supersession policy.
    pub excluded: Vec<ExcludedRef>,
    /// Protected payload handles (present only under `--protected-store`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protected_payloads: Option<Vec<ProtectedPayloadRef>>,
    /// Repository-scope caveat, present only when `--repo` is set: discloses
    /// that the scope filters only the code-side `first_seen_range` join, never
    /// the log/runtime sections ([`REPO_SCOPE_CAVEAT`]). Omitted when unscoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_scope_caveat: Option<&'static str>,
    /// Embedded-store retention caveat, present only when the query ran over the
    /// embedded (`--data-dir`) read path AND the store holds at least one
    /// `ErrorSignature` record (issue #363): the embedded lane now loads through
    /// the log-retained read surface, which surfaces every superseded
    /// non-temporal `ErrorSignature` / `LogOccurrenceBucket` version, so the
    /// `--graph` cross-scan view IS reconstructed here for differing-content
    /// scans — leaving only the byte-identical-re-ingest idempotency divergence
    /// to disclose — see [`LogEmbeddedRetentionCaveat`]. Absent (omitted from
    /// JSON) for `--graph` queries and for embedded stores with no
    /// `ErrorSignature` records (a plain `scan` store, where no disclosure is
    /// warranted). Reuses the `log-deltas` (#326) disclosure verbatim; the string
    /// is lane-agnostic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedded_log_retention_caveat: Option<LogEmbeddedRetentionCaveat>,
    /// Always-present advisory ([`ERROR_CONTEXT_DISCLAIMER`]).
    pub disclaimer: &'static str,
}

/// Failure modes of [`error_context`], mapped to CLI exit codes by the caller.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ErrorContextError {
    /// A fingerprint prefix matched two or more signatures (exit 1).
    Ambiguous {
        /// Candidate signature record IDs, sorted ascending.
        candidates: Vec<String>,
    },
    /// The handle matched no signature ID, prefix, or symbol frame target (exit 2).
    NoMatch {
        /// The handle as supplied.
        handle: String,
    },
    /// `--protected-store` set AND the graph carries a protected handle (exit 1).
    ProtectedHandleInGraph,
    /// `--protected-store` set but its manifest could not be read — a tampered
    /// or unreadable manifest must fail loudly, never degrade to an empty list
    /// indistinguishable from "no match" (exit 1).
    ProtectedStoreUnreadable {
        /// Human-readable diagnostic naming the store path and the I/O cause.
        message: String,
    },
}

/// Outcome of handle resolution (§1).
enum HandleResolution {
    /// One or more resolved anchor signature IDs (sorted, deduped).
    Signatures(Vec<String>),
    /// A fingerprint-prefix collision — candidate IDs sorted ascending.
    Ambiguous(Vec<String>),
    /// No signature ID, prefix, or symbol frame target matched.
    NoMatch,
}

/// Parses an RFC 3339 timestamp to a UTC instant for ordering.
///
/// All log/commit timestamps compared here originate from Egregore's scanners
/// and are parseable in practice; `None` is a deterministic exclude fallback,
/// never a silent misordering. Comparison is always by parsed instant because
/// commit committer dates carry local offsets while log times are Z-normalized.
fn parse_instant(rfc3339: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Maps a record domain to its trust class.
const fn trust_class_for_domain(domain: &str) -> &'static str {
    match domain.as_bytes() {
        b"codegraph" => "source_fact",
        b"agent_memory" => "agent_observation",
        b"verification" => "verification",
        b"artifact" => "artifact",
        b"project" => "project_state",
        b"log" => "runtime_observation",
        b"semantic" => "semantic",
        b"user_context" => "user_context",
        _ => "unknown",
    }
}

/// Resolves a record's domain from its stamped `domain` field, falling back to
/// its stable-ID prefix.
fn record_domain(record: &GraphRecord) -> String {
    if let GraphRecord::Node {
        domain: Some(domain),
        ..
    } = record
    {
        return domain.clone();
    }
    crate::schema_version::domain_from_record_id(record.id())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Projects one node record into a redaction-safe [`Row`].
///
/// Returns `None` for non-node records (edges/tombstones never become rows).
fn project_row(
    record: &GraphRecord,
    basis: Option<String>,
    supersession: Option<(String, Vec<String>)>,
) -> Option<Row> {
    let GraphRecord::Node {
        id,
        kind,
        schema_version,
        repo_relative_path,
        span,
        valid_time,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    let domain = record_domain(record);
    let trust_class = trust_class_for_domain(&domain);
    let (supersession_status, superseded_by) = match supersession {
        Some((status, handles)) => (Some(status), handles),
        None => (None, Vec::new()),
    };
    Some(Row {
        record_id: id.clone(),
        schema_version: *schema_version,
        domain,
        trust_class,
        kind: kind.as_str().to_owned(),
        repo_relative_path: repo_relative_path.clone(),
        span: *span,
        valid_time: valid_time.clone(),
        git_commit: temporal.as_ref().map(|t| t.git_commit.clone()),
        correlation_basis: basis,
        supersession_status,
        superseded_by,
    })
}

/// True when the graph slice carries any protected handle. The graph must never
/// store protected handles (#60/#321); they are resolved at read time only.
fn graph_has_protected_handle(records: &[GraphRecord]) -> bool {
    for record in records {
        if record.id().starts_with(PROTECTED_HANDLE_PREFIX) {
            return true;
        }
        match record {
            GraphRecord::Node {
                evidence_links: Some(links),
                ..
            } => {
                for link in links {
                    if link.target_domain == "protected"
                        || link
                            .target_record_id
                            .as_deref()
                            .is_some_and(|t| t.starts_with(PROTECTED_HANDLE_PREFIX))
                    {
                        return true;
                    }
                }
            }
            GraphRecord::Edge { source, target, .. } => {
                if source.starts_with(PROTECTED_HANDLE_PREFIX)
                    || target.starts_with(PROTECTED_HANDLE_PREFIX)
                {
                    return true;
                }
            }
            GraphRecord::Node { .. } | GraphRecord::Tombstone { .. } => {}
        }
    }
    false
}

/// Resolves a handle to anchor `ErrorSignature` IDs (§1: record ID → fingerprint
/// prefix → symbol name).
///
/// `frame_records` supplies the `FRAME_RESOLVES_TO` edges consulted by symbol-name
/// mode (§1c). Under `--at` it is the commit-view-re-resolved frame set, so a
/// symbol named only by a frame that resolves at that commit still matches;
/// without `--at` it is the graph's own edges. Signature and symbol NODE indices
/// always come from `records` (the re-resolved set never re-emits code-graph
/// symbols).
fn resolve_handle(
    records: &[GraphRecord],
    frame_records: &[GraphRecord],
    handle: &str,
) -> HandleResolution {
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();

    let sig_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::ErrorSignature,
                temporal,
                ..
            } if temporal.is_some() || !tombstoned.contains(id.as_str()) => Some(id.as_str()),
            _ => None,
        })
        .collect();

    // ── 1a: exact record-ID match ────────────────────────────────────────────
    if let Some(needle) = strip_log_prefix(handle) {
        if sig_ids.contains(handle) {
            return HandleResolution::Signatures(vec![handle.to_owned()]);
        }
        // A `log:v<N>:`-shaped handle only ever names a log record; resolve it as
        // a fingerprint prefix over signature hex tails, never as a symbol name.
        return prefix_resolution(&sig_ids, needle);
    }

    // ── 1b: bare fingerprint / template-hash prefix ──────────────────────────
    if !handle.is_empty() && handle.bytes().all(|b| b.is_ascii_hexdigit()) {
        match prefix_resolution(&sig_ids, handle) {
            HandleResolution::NoMatch => {} // fall through to symbol-name mode
            resolved => return resolved,
        }
    }

    // ── 1c: exact symbol name via FRAME_RESOLVES_TO targets ──────────────────
    let symbol_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                name: Some(name),
                temporal,
                ..
            } if name == handle && (temporal.is_some() || !tombstoned.contains(id.as_str())) => {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect();

    if !symbol_ids.is_empty() {
        let mut resolved: BTreeSet<&str> = BTreeSet::new();
        for r in frame_records {
            if let GraphRecord::Edge {
                label: EdgeLabel::FrameResolvesTo,
                source,
                target,
                ..
            } = r
                && symbol_ids.contains(target.as_str())
                && sig_ids.contains(source.as_str())
            {
                resolved.insert(source.as_str());
            }
        }
        if !resolved.is_empty() {
            return HandleResolution::Signatures(resolved.into_iter().map(str::to_owned).collect());
        }
    }

    HandleResolution::NoMatch
}

/// Resolves a hex prefix over signature stable-ID hex tails.
fn prefix_resolution(sig_ids: &BTreeSet<&str>, needle: &str) -> HandleResolution {
    if needle.is_empty() || !needle.bytes().all(|b| b.is_ascii_hexdigit()) {
        return HandleResolution::NoMatch;
    }
    let candidates: Vec<String> = sig_ids
        .iter()
        .filter(|id| strip_log_prefix(id).is_some_and(|hex| hex.starts_with(needle)))
        .map(|id| (*id).to_owned())
        .collect();
    match candidates.len() {
        0 => HandleResolution::NoMatch,
        1 => HandleResolution::Signatures(candidates),
        _ => HandleResolution::Ambiguous(candidates),
    }
}

/// Assembles the cross-domain error-context envelope for `handle` (issue #324).
///
/// `at_commit` re-resolves backtrace frames against a commit view; `as_of`
/// bounds the occurrence-bucket view on the valid axis. `repo_scope` scopes only
/// the code side of the `first_seen_range` symbol-delta join (log records carry
/// no retrievable repository attribution). `supersession` chooses whether
/// superseded rows are excluded or flagged. `protected_store`, when set,
/// resolves protected payload handles at read time. `embedded_source` is `true`
/// only when the caller loaded the records from an embedded (`--data-dir`) store
/// rather than a `--graph` JSONL: that path now loads through the log-retained
/// read surface, which surfaces every superseded non-temporal `ErrorSignature` /
/// `LogOccurrenceBucket` version, so cross-scan coalescing IS reconstructed for
/// differing-content scans; when it is set AND the store holds at least one
/// `ErrorSignature`, the response still carries [`LogEmbeddedRetentionCaveat`]
/// disclosing the one residual divergence (byte-identical re-ingests are deduped,
/// not multiplied) (issue #363). It never changes resolution or section
/// contents — only whether the caveat is emitted.
///
/// # Errors
///
/// Returns [`ErrorContextError`] on an ambiguous fingerprint prefix, an
/// unmatched handle, or a graph that already carries a protected handle while
/// `--protected-store` is set.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn error_context(
    records: &[GraphRecord],
    handle: &str,
    repo_scope: Option<&str>,
    at_commit: Option<&str>,
    as_of: Option<&str>,
    supersession: SupersessionMode,
    protected_store: Option<&Path>,
    embedded_source: bool,
) -> Result<ErrorContext, ErrorContextError> {
    // §5: the graph must carry ZERO protected handles when the store is set.
    if protected_store.is_some() && graph_has_protected_handle(records) {
        return Err(ErrorContextError::ProtectedHandleInGraph);
    }

    // Frame view: re-resolved against a commit view when `--at` is set, else the
    // graph's existing FRAME_RESOLVES_TO edges. Computed BEFORE handle resolution
    // so symbol-name mode (§1c) resolves against the SAME commit-view frames that
    // Stage A reports — resolving symbol handles from the current graph while
    // Stage A re-resolves would make `error-context <symbol> --at <commit>` miss a
    // signature whose frame only resolves to that symbol at the commit view.
    let stripped;
    let reresolved;
    let frame_records: &[GraphRecord] = if at_commit.is_some() {
        // Drop any pre-existing FRAME_RESOLVES_TO edges (carried through from a
        // prior `eg resolve-frames` / `link-logs` run — the normal combined-graph
        // input) BEFORE re-resolving, so the at-commit view REPLACES the HEAD
        // frame view rather than merging with it. Without the strip, a frame that
        // resolves to a DIFFERENT target at the old commit than at HEAD would
        // report BOTH the stale HEAD target and the fresh at-commit target.
        // `resolve_frames` reads frames from `ErrorSignature` NODES and rebuilds
        // its own `RepositoryIndex`, both of which survive dropping the edges.
        stripped = records
            .iter()
            .filter(|r| {
                !matches!(
                    r,
                    GraphRecord::Edge {
                        label: EdgeLabel::FrameResolvesTo,
                        ..
                    }
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        reresolved = log_resolve::resolve_frames(&stripped, at_commit);
        &reresolved.records
    } else {
        records
    };

    // §1: resolve the handle to anchor signature IDs.
    let signature_ids: Vec<String> = match resolve_handle(records, frame_records, handle) {
        HandleResolution::Signatures(ids) => ids,
        HandleResolution::Ambiguous(candidates) => {
            return Err(ErrorContextError::Ambiguous { candidates });
        }
        HandleResolution::NoMatch => {
            return Err(ErrorContextError::NoMatch {
                handle: handle.to_owned(),
            });
        }
    };
    let sig_set: BTreeSet<&str> = signature_ids.iter().map(String::as_str).collect();
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

    // ── Stage A: signature blocks (direct edge reads) ────────────────────────
    let mut frames_by_sig: BTreeMap<&str, Vec<ResolvedFrameHandle>> = BTreeMap::new();
    for r in frame_records {
        if let GraphRecord::Edge {
            label: EdgeLabel::FrameResolvesTo,
            source,
            target,
            frame_resolution: Some(resolution),
            frame_index: Some(index),
            ..
        } = r
            && let Some(key) = sig_set.get(source.as_str()).copied()
        {
            frames_by_sig
                .entry(key)
                .or_default()
                .push(ResolvedFrameHandle {
                    frame_index: *index,
                    frame_resolution: resolution.as_str().to_owned(),
                    target_record_id: target.clone(),
                });
        }
    }

    // CAPTURED_FROM LogSource handles.
    let mut sources_by_sig: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    // AGGREGATES bucket → signatures.
    let mut bucket_targets: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    // EMITTED_DURING signature → run (basis), and REFERENCES_TASK signature → task.
    let mut emitted_runs: BTreeMap<&str, CorrelationBasis> = BTreeMap::new();
    let mut sig_task_targets: BTreeSet<&str> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Edge {
            label,
            source,
            target,
            basis,
            ..
        } = r
        {
            match label {
                EdgeLabel::CapturedFrom if sig_set.contains(source.as_str()) => {
                    sources_by_sig
                        .entry(source.as_str())
                        .or_default()
                        .insert(target.as_str());
                }
                EdgeLabel::Aggregates => {
                    bucket_targets
                        .entry(source.as_str())
                        .or_default()
                        .insert(target.as_str());
                }
                EdgeLabel::EmittedDuring if sig_set.contains(source.as_str()) => {
                    // A basis-less EMITTED_DURING edge should not occur; default
                    // to the weaker correlation for safety.
                    let incoming = basis.unwrap_or(CorrelationBasis::TemporalCorrelation);
                    // When two signatures link the SAME run under different bases,
                    // the stronger `content_hash_join` (confidence 1.0) always
                    // wins over `temporal_correlation` (0.5) regardless of record
                    // order — never a file-order downgrade.
                    emitted_runs
                        .entry(target.as_str())
                        .and_modify(|existing| {
                            if incoming == CorrelationBasis::ContentHashJoin {
                                *existing = CorrelationBasis::ContentHashJoin;
                            }
                        })
                        .or_insert(incoming);
                }
                EdgeLabel::ReferencesTask if sig_set.contains(source.as_str()) => {
                    sig_task_targets.insert(target.as_str());
                }
                _ => {}
            }
        }
    }

    // Occurrence buckets per signature, bounded by `--as-of` on the valid axis.
    // Buckets are DEDUPED by record ID (issue #361, source-aware identity): a
    // bucket ID is now (repository/signature/hour/width/SOURCE), so distinct
    // sources mint distinct bucket IDs (each listed once) while a genuine rescan
    // of identical bytes mints the SAME bucket ID (collapsed as a duplicate).
    let as_of_instant = as_of.and_then(parse_instant);
    let mut buckets_by_sig: BTreeMap<&str, Vec<BucketRow>> = BTreeMap::new();
    let mut seen_bucket_ids: BTreeSet<&str> = BTreeSet::new();
    for r in records {
        let GraphRecord::Node {
            id,
            log: Some(payload),
            ..
        } = r
        else {
            continue;
        };
        let LogPayload::LogOccurrenceBucket(bucket) = payload.as_ref() else {
            continue;
        };
        if !seen_bucket_ids.insert(id.as_str()) {
            continue;
        }
        let Some(sigs) = bucket_targets.get(id.as_str()) else {
            continue;
        };
        if let (Some(cutoff), Some(start)) = (as_of_instant, parse_instant(&bucket.bucket_start))
            && start > cutoff
        {
            continue;
        }
        for sig in sigs {
            if sig_set.contains(sig) {
                buckets_by_sig.entry(*sig).or_default().push(BucketRow {
                    record_id: id.clone(),
                    bucket_start: bucket.bucket_start.clone(),
                    bucket_width: bucket.bucket_width.clone(),
                    occurrence_count: bucket.occurrence_count,
                });
            }
        }
    }

    // Coalesce ErrorSignature records by stable ID (LogSource is non-identity).
    let mut sig_payloads: BTreeMap<&str, Vec<(u32, &ErrorSignaturePayload)>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::ErrorSignature,
            schema_version,
            log: Some(payload),
            ..
        } = r
            && sig_set.contains(id.as_str())
            && let LogPayload::ErrorSignature(sig) = payload.as_ref()
        {
            sig_payloads
                .entry(id.as_str())
                .or_default()
                .push((*schema_version, sig));
        }
    }

    let mut signatures: Vec<SignatureBlock> = Vec::new();
    for sig_id in &signature_ids {
        let Some(group) = sig_payloads.get(sig_id.as_str()) else {
            continue;
        };
        let (schema_version, first) = (group[0].0, group[0].1);
        let first_seen = group
            .iter()
            .filter_map(|(_, s)| parse_instant(&s.first_seen).map(|dt| (dt, s.first_seen.clone())))
            .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            .map_or_else(|| first.first_seen.clone(), |(_, s)| s);
        let last_seen = group
            .iter()
            .filter_map(|(_, s)| parse_instant(&s.last_seen).map(|dt| (dt, s.last_seen.clone())))
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            .map_or_else(|| first.last_seen.clone(), |(_, s)| s);
        let occurrence_count: u64 = group.iter().map(|(_, s)| s.occurrence_count).sum();

        let mut source_handles: Vec<SourceHandle> = sources_by_sig
            .get(sig_id.as_str())
            .into_iter()
            .flatten()
            .filter_map(|src| match by_id.get(src) {
                Some(GraphRecord::Node {
                    log: Some(payload), ..
                }) => match payload.as_ref() {
                    LogPayload::LogSource(p) => Some(SourceHandle {
                        record_id: (*src).to_owned(),
                        source_relative_path: p.source_relative_path.clone(),
                        source_artifact_hash: p.source_artifact_hash.clone(),
                    }),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        source_handles.sort_by(|a, b| {
            a.source_artifact_hash
                .cmp(&b.source_artifact_hash)
                .then_with(|| a.record_id.cmp(&b.record_id))
        });

        let mut buckets = buckets_by_sig
            .get(sig_id.as_str())
            .cloned()
            .unwrap_or_default();
        buckets.sort_by(|a, b| {
            a.bucket_start
                .cmp(&b.bucket_start)
                .then_with(|| a.record_id.cmp(&b.record_id))
        });

        let mut frames = frames_by_sig
            .get(sig_id.as_str())
            .cloned()
            .unwrap_or_default();
        frames.sort_by(|a, b| {
            a.frame_index
                .cmp(&b.frame_index)
                .then_with(|| a.frame_resolution.cmp(&b.frame_resolution))
                .then_with(|| a.target_record_id.cmp(&b.target_record_id))
        });
        frames.dedup();

        signatures.push(SignatureBlock {
            record_id: sig_id.clone(),
            schema_version,
            trust_class: "runtime_observation",
            severity: first.severity.clone(),
            fingerprint_algorithm: first.fingerprint_algorithm.clone(),
            template_excerpt: first.template_excerpt.clone(),
            first_seen,
            last_seen,
            occurrence_count,
            source_handles,
            buckets,
            frames,
        });
    }
    signatures.sort_by(|a, b| a.record_id.cmp(&b.record_id));

    // ── Stage B: cross-domain sections ───────────────────────────────────────
    // Code half: seed the shared BFS from every codegraph frame target.
    let frame_target_seeds: BTreeSet<String> = signatures
        .iter()
        .flat_map(|s| &s.frames)
        .filter(|f| f.frame_resolution != "unresolved")
        .map(|f| f.target_record_id.clone())
        .filter(|t| crate::schema_version::domain_from_record_id(t).as_deref() == Some("codegraph"))
        .collect();

    // Keyed by (record ID, git_commit) so EVERY temporal version of a stable ID
    // survives. `record_context` deliberately returns all versions of a symbol
    // (see its by-ID collapse comment); an ID-only key here would re-collapse
    // them to the lexically-largest-`git_commit` version, dropping the historical
    // fact that `error-context <sig> --at <old commit>` exists to surface (the
    // frame correctly re-resolves to the symbol's stable ID at that commit, but
    // the cited version would otherwise be an arbitrary later one). Confined to
    // the frame-target source_facts — the log-half sections keep their own by-ID
    // single-version maps.
    let mut source_facts: BTreeMap<(String, Option<String>), &GraphRecord> = BTreeMap::new();
    let mut observations: BTreeMap<String, &GraphRecord> = BTreeMap::new();
    let mut project_state: BTreeMap<String, &GraphRecord> = BTreeMap::new();
    let mut artifacts: BTreeMap<String, &GraphRecord> = BTreeMap::new();
    let mut verification: BTreeMap<String, &GraphRecord> = BTreeMap::new();
    let mut unresolved: Vec<UnresolvedRef> = Vec::new();

    for seed in &frame_target_seeds {
        let ctx = super::record_context(records, seed);
        for r in ctx.source_facts {
            let git_commit = match r {
                GraphRecord::Node {
                    temporal: Some(t), ..
                } => Some(t.git_commit.clone()),
                _ => None,
            };
            source_facts.insert((r.id().to_owned(), git_commit), r);
        }
        for r in ctx.observations {
            observations.insert(r.id().to_owned(), r);
        }
        for r in ctx.project_state {
            project_state.insert(r.id().to_owned(), r);
        }
        for r in ctx.artifacts {
            artifacts.insert(r.id().to_owned(), r);
        }
        for r in ctx.verification_evidence {
            verification.insert(r.id().to_owned(), r);
        }
        unresolved.extend(ctx.unresolved);
    }

    // Log half: classify EMITTED_DURING runs, their one-hop cross-domain
    // neighbors, and REFERENCES_TASK targets. Runs are classified locally
    // (AgentRun/AgentTurn → observations, CommandRun → verification) because
    // `classify_node` has no mapping for run kinds.
    let mut basis_by_run: BTreeMap<String, CorrelationBasis> = BTreeMap::new();
    for (run, basis) in &emitted_runs {
        basis_by_run.insert((*run).to_owned(), *basis);
        if let Some(record) = by_id.get(run) {
            match record {
                GraphRecord::Node {
                    kind: NodeKind::AgentRun | NodeKind::AgentTurn,
                    ..
                } => {
                    observations.insert((*run).to_owned(), record);
                }
                GraphRecord::Node {
                    kind: NodeKind::CommandRun,
                    ..
                } => {
                    verification.insert((*run).to_owned(), record);
                }
                _ => {}
            }
        }
    }
    // One hop from each run: classify its direct cross-domain neighbors.
    for r in records {
        if let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = r
            && emitted_runs.contains_key(source.as_str())
            && super::is_cross_domain_label(*label)
            && let Some(node) = by_id.get(target.as_str())
        {
            insert_by_kind(
                node,
                &mut observations,
                &mut project_state,
                &mut artifacts,
                &mut verification,
            );
        }
    }
    // REFERENCES_TASK targets minted directly onto the signature.
    for task in &sig_task_targets {
        if let Some(node) = by_id.get(task) {
            insert_by_kind(
                node,
                &mut observations,
                &mut project_state,
                &mut artifacts,
                &mut verification,
            );
        }
    }

    // Never leak the anchor signatures or their code seeds across sections.
    for sig in &sig_set {
        observations.remove(*sig);
        project_state.remove(*sig);
        artifacts.remove(*sig);
        verification.remove(*sig);
    }

    // Supersession policy over the agent/project/artifact/verification sections.
    let resolver = TemporalResolver::build(records);
    let mut excluded: Vec<ExcludedRef> = Vec::new();
    let project = |section: BTreeMap<String, &GraphRecord>,
                   basis: &BTreeMap<String, CorrelationBasis>,
                   excluded: &mut Vec<ExcludedRef>|
     -> Vec<Row> {
        let mut rows: Vec<Row> = Vec::new();
        for (id, record) in section {
            let (status, superseded_refs, contradicted_refs) = resolver.resolve_status(&id);
            let flagged = match status {
                "superseded" => Some(("superseded", superseded_refs)),
                "contradicted" => Some(("contradicted", contradicted_refs)),
                _ => None,
            };
            let correlation_basis = basis.get(&id).map(|b| b.as_str().to_owned());
            match (flagged, supersession) {
                (Some((reason, refs)), SupersessionMode::Exclude) => {
                    excluded.push(ExcludedRef {
                        record_id: id.clone(),
                        reason: reason.to_owned(),
                        superseded_by: refs.into_iter().map(|r| r.record_id).collect(),
                    });
                }
                (Some((reason, refs)), SupersessionMode::IncludeButFlag) => {
                    if let Some(mut row) = project_row(
                        record,
                        correlation_basis,
                        Some((
                            reason.to_owned(),
                            refs.into_iter().map(|r| r.record_id).collect(),
                        )),
                    ) {
                        row.superseded_by.sort();
                        rows.push(row);
                    }
                }
                (None, _) => {
                    if let Some(row) = project_row(record, correlation_basis, None) {
                        rows.push(row);
                    }
                }
            }
        }
        rows
    };

    let empty_basis: BTreeMap<String, CorrelationBasis> = BTreeMap::new();
    let mut source_facts_rows: Vec<Row> = source_facts
        .into_values()
        .filter_map(|r| project_row(r, None, None))
        .collect();
    let mut observations_rows = project(observations, &basis_by_run, &mut excluded);
    let mut project_state_rows = project(project_state, &empty_basis, &mut excluded);
    let mut artifacts_rows = project(artifacts, &empty_basis, &mut excluded);
    // `EMITTED_DURING` `CommandRun` targets land in verification carrying their
    // `content_hash_join` basis, so the basis index applies here too.
    let mut verification_rows = project(verification, &basis_by_run, &mut excluded);

    let sort_rows = |rows: &mut Vec<Row>| {
        rows.sort_by(|a, b| {
            a.record_id
                .cmp(&b.record_id)
                .then_with(|| a.git_commit.cmp(&b.git_commit))
        });
    };
    sort_rows(&mut source_facts_rows);
    sort_rows(&mut observations_rows);
    sort_rows(&mut project_state_rows);
    sort_rows(&mut artifacts_rows);
    sort_rows(&mut verification_rows);

    unresolved.sort_by(|a, b| {
        a.source_record_id
            .cmp(&b.source_record_id)
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
            .then_with(|| a.target_domain.cmp(&b.target_domain))
    });
    unresolved.dedup();
    excluded.sort_by(|a, b| {
        a.record_id
            .cmp(&b.record_id)
            .then_with(|| a.reason.cmp(&b.reason))
    });
    for e in &mut excluded {
        e.superseded_by.sort();
    }

    // ── first_seen_range (§4) ────────────────────────────────────────────────
    let first_seen_range =
        build_first_seen_range(records, &signatures, &frame_target_seeds, repo_scope);

    // ── protected payloads (§5) ──────────────────────────────────────────────
    // A tampered or unreadable manifest fails loudly (exit 1) rather than
    // silently degrading to an empty list indistinguishable from "no match".
    let protected_payloads = match protected_store {
        None => None,
        Some(dir) => {
            let handles = ProtectedStore::new(dir).list().map_err(|e| {
                ErrorContextError::ProtectedStoreUnreadable {
                    message: format!("failed to read protected store at {}: {e}", dir.display()),
                }
            })?;
            // A content hash can back MORE THAN ONE protected handle: handle
            // identity is (source_class, content_hash, source_path), so one
            // captured artifact may legitimately appear under several classes or
            // paths. Collect EVERY handle per hash — a plain `BTreeMap<&str, &_>`
            // would be last-write-wins per key and silently drop all but the
            // lexically-largest-serialized handle, omitting the actual
            // `log_payload` or surfacing the wrong class.
            let mut by_hash: BTreeMap<&str, Vec<&crate::protected::ProtectedHandle>> =
                BTreeMap::new();
            for h in &handles {
                by_hash.entry(h.content_hash.as_str()).or_default().push(h);
            }
            let mut refs: Vec<ProtectedPayloadRef> = Vec::new();
            for block in &signatures {
                for source in &block.source_handles {
                    if let Some(hs) = by_hash.get(source.source_artifact_hash.as_str()) {
                        for h in hs {
                            refs.push(ProtectedPayloadRef {
                                source_artifact_hash: source.source_artifact_hash.clone(),
                                signature_id: block.record_id.clone(),
                                handle: h.handle.clone(),
                                source_class: h.source_class.as_str().to_owned(),
                                byte_len: h.byte_len,
                            });
                        }
                    }
                }
            }
            refs.sort_by(|a, b| {
                a.source_artifact_hash
                    .cmp(&b.source_artifact_hash)
                    .then_with(|| a.signature_id.cmp(&b.signature_id))
                    .then_with(|| a.handle.cmp(&b.handle))
            });
            Some(refs)
        }
    };

    // Embedded-store retention caveat (issue #363), mirroring `log-deltas`
    // (#326): the embedded `--data-dir` current-state read surface returns one
    // record per stable ID, and `ErrorSignature` / `LogOccurrenceBucket` are
    // non-temporal, so multiple `scan-logs` ingests of the same stable ID are
    // collapsed (last-write-wins) BEFORE this query runs — the `--graph`
    // cross-scan view is not reconstructable here. DIAGNOSE rather than reject:
    // a single-ingest store is correct and must keep working, and error-context
    // also runs over pure `scan` graphs with zero log records, so the caveat is
    // gated on at least one `ErrorSignature` node being present in the store.
    // The `--graph` path preserves every ingested line, so it never carries this
    // caveat. Fixed string, no wall clock — byte-stable. See issue #363 and
    // docs/cli/error-context.md.
    let embedded_log_retention_caveat = if embedded_source
        && records.iter().any(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::ErrorSignature,
                    ..
                }
            )
        }) {
        Some(LogEmbeddedRetentionCaveat {
            message: LOG_EMBEDDED_RETENTION_CAVEAT,
        })
    } else {
        None
    };

    Ok(ErrorContext {
        handle: handle.to_owned(),
        signature_ids,
        signatures,
        first_seen_range,
        source_facts: source_facts_rows,
        observations: observations_rows,
        project_state: project_state_rows,
        artifacts: artifacts_rows,
        verification_evidence: verification_rows,
        unresolved,
        excluded,
        protected_payloads,
        repo_scope_caveat: repo_scope.map(|_| REPO_SCOPE_CAVEAT),
        embedded_log_retention_caveat,
        disclaimer: ERROR_CONTEXT_DISCLAIMER,
    })
}

/// Classifies a node into the correct cross-domain section by kind.
fn insert_by_kind<'a>(
    node: &'a GraphRecord,
    observations: &mut BTreeMap<String, &'a GraphRecord>,
    project_state: &mut BTreeMap<String, &'a GraphRecord>,
    artifacts: &mut BTreeMap<String, &'a GraphRecord>,
    verification: &mut BTreeMap<String, &'a GraphRecord>,
) {
    let GraphRecord::Node { id, kind, .. } = node else {
        return;
    };
    match kind {
        NodeKind::Observation | NodeKind::Decision | NodeKind::Failure => {
            observations.insert(id.clone(), node);
        }
        NodeKind::Task
        | NodeKind::AcceptanceCriterion
        | NodeKind::LocalTask
        | NodeKind::GitHubIssue
        | NodeKind::PR => {
            project_state.insert(id.clone(), node);
        }
        NodeKind::Artifact | NodeKind::PatchArtifact | NodeKind::FileEdit => {
            artifacts.insert(id.clone(), node);
        }
        NodeKind::Verification
        | NodeKind::CommandEvidence
        | NodeKind::TestRun
        | NodeKind::CommandRun
        | NodeKind::CIStatus
        | NodeKind::BenchmarkRun
        | NodeKind::CoverageReport
        | NodeKind::ProofResult => {
            verification.insert(id.clone(), node);
        }
        _ => {}
    }
}

/// Builds the `first_seen_range` block: the narrowest commit window bracketing
/// the earliest resolved-signature `first_seen`, with the overlapping range
/// symbol deltas that intersect the resolved frame targets.
fn build_first_seen_range(
    records: &[GraphRecord],
    signatures: &[SignatureBlock],
    frame_target_seeds: &BTreeSet<String>,
    repo_scope: Option<&str>,
) -> FirstSeenRange {
    // Commit timeline: (sha, instant, raw_valid_time). A graph is history-backed
    // iff it carries at least one Commit with a parseable valid_time. The emitted
    // window strings keep the original RFC 3339 text; only the ordering is by
    // instant (committer dates carry local offsets, log times are Z-normalized).
    //
    // When `--repo` is set the timeline is scoped to commits OWNED by that
    // repository (via the CONTAINS topology, like `range_deltas`/`log_deltas`):
    // in a shared multi-repository store an unrelated repository's commits could
    // otherwise bracket `first_seen`, citing a foreign window and, because the
    // reused `range_deltas` join below is itself repo-scoped, dropping the real
    // overlap. Commits ARE attributable (`owner_of`), so bracketing must be
    // scoped too, not built from every Commit node.
    let repo_index = repo_scope.map(|_| super::RepositoryIndex::build(records));
    let commit_in_scope = |id: &str| -> bool {
        match (repo_scope, repo_index.as_ref()) {
            (Some(scope), Some(index)) => index.owner_of(id) == Some(scope),
            _ => true,
        }
    };
    let mut commits: Vec<(String, DateTime<Utc>, String)> = Vec::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Commit,
            name: Some(sha),
            temporal: Some(t),
            ..
        } = r
            && commit_in_scope(id)
            && let Some(instant) = parse_instant(&t.valid_time)
        {
            commits.push((sha.clone(), instant, t.valid_time.clone()));
        }
    }
    if commits.is_empty() {
        return FirstSeenRange::Unavailable {
            diagnostic: "history_unavailable",
        };
    }

    // Anchor: the earliest first_seen across the resolved signatures.
    let Some((anchor_signature_id, first_seen, first_instant)) = signatures
        .iter()
        .filter_map(|s| {
            parse_instant(&s.first_seen).map(|dt| (s.record_id.clone(), s.first_seen.clone(), dt))
        })
        .min_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)))
    else {
        // No parseable first_seen — an honest empty window anchored on nothing.
        return FirstSeenRange::History(FirstSeenWindow {
            anchor_signature_id: signatures
                .first()
                .map_or_else(String::new, |s| s.record_id.clone()),
            first_seen: signatures
                .first()
                .map_or_else(String::new, |s| s.first_seen.clone()),
            base_commit: None,
            head_commit: None,
            window_start: None,
            window_end: None,
            overlapping_symbol_deltas: Vec::new(),
        });
    };

    // base = newest commit at/before first_seen; head = oldest commit at/after.
    // Ties break to the ascending SHA for determinism.
    let base = commits
        .iter()
        .filter(|(_, t, _)| *t <= first_instant)
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .cloned();
    let head = commits
        .iter()
        .filter(|(_, t, _)| *t >= first_instant)
        .min_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)))
        .cloned();

    // Overlapping symbol deltas: only when both endpoints resolve to distinct
    // commits and the reused range mechanics succeed (a point bracket, not a
    // user range, so identical/reversed endpoints are an empty overlap here).
    let mut overlapping_symbol_deltas: Vec<OverlappingSymbolDelta> = Vec::new();
    if let (Some((base_sha, _, _)), Some((head_sha, _, _))) = (base.as_ref(), head.as_ref())
        && base_sha != head_sha
        && let Ok(deltas) = range_deltas(records, base_sha, head_sha, repo_scope)
    {
        let mut classes: BTreeMap<&str, &'static str> = BTreeMap::new();
        for item in deltas
            .added_symbols
            .iter()
            .chain(&deltas.modified_symbols)
            .chain(&deltas.removed_symbols)
        {
            classes.insert(item.record_id, item.change_class);
        }
        let mut seen: BTreeSet<(String, &'static str)> = BTreeSet::new();
        for target in frame_target_seeds {
            if let Some(class) = classes.get(target.as_str()) {
                seen.insert((target.clone(), *class));
            }
        }
        overlapping_symbol_deltas = seen
            .into_iter()
            .map(|(record_id, change_class)| OverlappingSymbolDelta {
                record_id,
                change_class: change_class.to_owned(),
            })
            .collect();
        overlapping_symbol_deltas.sort_by(|a, b| {
            a.change_class
                .cmp(&b.change_class)
                .then_with(|| a.record_id.cmp(&b.record_id))
        });
    }

    FirstSeenRange::History(FirstSeenWindow {
        anchor_signature_id,
        first_seen,
        base_commit: base.as_ref().map(|(sha, _, _)| sha.clone()),
        head_commit: head.as_ref().map(|(sha, _, _)| sha.clone()),
        window_start: base.as_ref().map(|(_, _, vt)| vt.clone()),
        window_end: head.as_ref().map(|(_, _, vt)| vt.clone()),
        overlapping_symbol_deltas,
    })
}

#[cfg(test)]
mod prefix_tests {
    use super::strip_log_prefix;

    #[test]
    fn strip_log_prefix_is_version_agnostic() {
        // Both the superseded v1 and current v2 (issue #361) prefixes resolve,
        // returning the hex tail unchanged so exact-ID and prefix resolution work.
        assert_eq!(strip_log_prefix("log:v1:deadbeef"), Some("deadbeef"));
        assert_eq!(strip_log_prefix("log:v2:deadbeef"), Some("deadbeef"));
        // Multi-digit versions are accepted (future-proof).
        assert_eq!(strip_log_prefix("log:v10:abc"), Some("abc"));
        // A partial hex tail (prefix-resolution needle) round-trips.
        assert_eq!(strip_log_prefix("log:v2:dead"), Some("dead"));
    }

    #[test]
    fn strip_log_prefix_rejects_non_log_and_malformed() {
        assert_eq!(strip_log_prefix("codegraph:v5:abc"), None);
        assert_eq!(strip_log_prefix("log:abc"), None);
        assert_eq!(strip_log_prefix("log:v:abc"), None);
        assert_eq!(strip_log_prefix("log:vx:abc"), None);
        assert_eq!(strip_log_prefix("some_symbol"), None);
    }
}
