//! Integration tests for `eg query log-deltas` (issue #326): runtime
//! error-signature classification across a commit range.
//!
//! Two layers, mirroring `tests/integration/range_deltas.rs`:
//!   * synthetic-record tests that build a `Vec<GraphRecord>` with three linear
//!     commits, a modified symbol, four `ErrorSignature` nodes exercising the
//!     new / ceased / continuing / out-of-range classes, hourly
//!     `LogOccurrenceBucket` records linked through `AGGREGATES`, and a
//!     `FRAME_RESOLVES_TO` edge from the new signature onto the modified symbol
//!     (the AC4 join);
//!   * a seeded end-to-end CLI test that scans a real git fixture with
//!     `scan-history`, augments the graph with the same synthetic log records,
//!     and drives `egregore query log-deltas`, asserting byte-identical output
//!     across five runs, the documented exit codes, and a clean working tree
//!     before and after.

#![allow(missing_docs)]

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use aletheia_egregore::{
    GraphRecord, LOG_SCHEMA_VERSION, NodeKind, TemporalMetadata,
    ir::{
        EdgeLabel, ErrorSignaturePayload, EvidenceLink, FrameResolution,
        LogOccurrenceBucketPayload, LogPayload,
    },
    query::{LogDeltas, RangeDeltasError, log_deltas},
    scan_repository_history, stable_id,
};
use assert_cmd::Command as CargoCommand;

// ---------------------------------------------------------------------------
// Timestamps: commit committer dates and signature valid times.
// ---------------------------------------------------------------------------

const T1: &str = "2026-01-01T00:00:00Z"; // base commit (c1)
const T2: &str = "2026-01-02T00:00:00Z"; // c2  → window_start for c1..c3
const T3: &str = "2026-01-03T00:00:00Z"; // c3  → window_end   for c1..c3

// New signature: first observed inside the [T2, T3] window.
const NEW_FIRST: &str = "2026-01-02T12:00:00Z";
const NEW_LAST: &str = "2026-01-02T13:00:00Z";
const NEW_BUCKET: &str = "2026-01-02T12:00:00Z";
// Ceased: existed before the range, last seen before the window end.
const CEASED_FIRST: &str = "2026-01-01T00:00:00Z";
const CEASED_LAST: &str = "2026-01-01T05:00:00Z";
// Continuing: existed before the range, still occurring through the end.
const CONT_FIRST: &str = "2026-01-01T00:00:00Z";
const CONT_LAST: &str = "2026-01-05T00:00:00Z";
// Out of range: first observed strictly after the window end.
const FUTURE_FIRST: &str = "2026-02-01T00:00:00Z";
const FUTURE_LAST: &str = "2026-02-01T01:00:00Z";

// ── Timezone-offset window (Codex P1) ──────────────────────────────────────
// Commit committer dates carry LOCAL offsets (`%cI`), but scan-logs normalizes
// signature valid times and bucket starts to UTC `Z`. Comparing the two as raw
// RFC 3339 strings is wrong across offsets; the comparisons must be by parsed
// instant. Here the head commit's committer date is `00:30:00-05:00`, i.e.
// `05:30:00Z`, while the in-window signature is at `05:00:00Z`. Lexically
// `"...05:00:00Z" > "...00:30:00-05:00"`, so the buggy string comparison drops
// the signature as out-of-range even though its instant is inside the window.
const TZ_BASE: &str = "2025-12-31T00:00:00Z"; // c1 base
const TZ_MID: &str = "2026-01-01T00:00:00Z"; // c2 → window_start
const TZ_HEAD: &str = "2026-01-01T00:30:00-05:00"; // c3 head = 2026-01-01T05:30:00Z → window_end
const TZ_SIG_FIRST: &str = "2026-01-01T05:00:00Z"; // 05:00Z, inside [00:00Z, 05:30Z]
const TZ_SIG_LAST: &str = "2026-01-01T05:10:00Z";
const TZ_BUCKET: &str = "2026-01-01T05:00:00Z"; // 05:00Z ≤ head 05:30Z by instant

// ---------------------------------------------------------------------------
// Synthetic record helpers (mirrors tests/integration/range_deltas.rs).
// ---------------------------------------------------------------------------

fn temporal(commit: &str, parents: &[&str], valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|s| (*s).to_owned()).collect(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

fn commit(sha: &str, parents: &[&str], valid_time: &str) -> GraphRecord {
    let id = stable_id(&["node", "commit", "repo_test", sha]);
    GraphRecord::node(
        id,
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("Commit {sha}"),
    )
    .with_temporal(temporal(sha, parents, valid_time))
}

fn symbol_snapshot(
    name: &str,
    path: &str,
    body: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    let id = symbol_id(name, path);
    GraphRecord::node(
        id,
        NodeKind::Symbol,
        Some(path.to_owned()),
        None,
        Some(name.to_owned()),
        format!("Symbol {name} in {path}\nSource:\n{body}"),
    )
    .with_temporal(temporal(commit, &[], valid_time))
}

fn symbol_id(name: &str, path: &str) -> String {
    stable_id(&["node", "symbol", "repo_test", path, name])
}

/// Builds an `ErrorSignature` node with the given valid-time bounds. The seed
/// keeps distinct signatures from colliding on their content-addressed ID.
fn error_signature(
    seed: &str,
    severity: &str,
    first_seen: &str,
    last_seen: &str,
    occurrence_count: u64,
) -> GraphRecord {
    error_signature_attributed(
        seed,
        severity,
        first_seen,
        last_seen,
        occurrence_count,
        "repo_test",
    )
}

/// Like [`error_signature`] but stamps an explicit `repository_id` on the payload
/// (issue #362, schema v3). An empty `repository_id` models a legacy `log:v2:`
/// record that carries no persisted attribution.
fn error_signature_attributed(
    seed: &str,
    severity: &str,
    first_seen: &str,
    last_seen: &str,
    occurrence_count: u64,
    repository_id: &str,
) -> GraphRecord {
    let id = log_sig_id(seed);
    GraphRecord::node(
        id,
        NodeKind::ErrorSignature,
        None,
        None,
        Some(format!("{severity} signature")),
        format!("Error signature ({severity}) x{occurrence_count}: {seed}"),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
        fingerprint_algorithm: "template-v1".to_owned(),
        template_excerpt: format!("template {seed}"),
        severity: severity.to_owned(),
        occurrence_count,
        first_seen: first_seen.to_owned(),
        last_seen: last_seen.to_owned(),
        frames: None,
        repository_id: repository_id.to_owned(),
    }))
    .with_valid_time(first_seen, "log_event_timestamp")
}

fn log_sig_id(seed: &str) -> String {
    aletheia_egregore::log_stable_id(&["error_signature", "repo_test", seed])
}

/// The default `LogSource` handle folded into a single-source fixture bucket ID.
const DEFAULT_SOURCE: &str = "log:v2:source-default";

/// A `LogOccurrenceBucket` node plus its `AGGREGATES` edge to the signature,
/// attributed to a single default source (issue #361: bucket identity is now
/// source-aware, so a fixture modelling ONE source uses one source handle).
fn bucket_with_edge(
    signature_id: &str,
    bucket_start: &str,
    count: u64,
) -> (GraphRecord, GraphRecord) {
    bucket_with_source(signature_id, bucket_start, count, DEFAULT_SOURCE)
}

/// A `LogOccurrenceBucket` node plus its `AGGREGATES` edge, attributed to an
/// explicit `source_id` (issue #361). The `source_id` is folded LAST into the
/// bucket's stable ID, mirroring production ordering, so distinct sources mint
/// DISTINCT bucket IDs while a rescan of the same source mints the SAME ID.
fn bucket_with_source(
    signature_id: &str,
    bucket_start: &str,
    count: u64,
    source_id: &str,
) -> (GraphRecord, GraphRecord) {
    let bucket_id = aletheia_egregore::log_stable_id(&[
        "log_occurrence_bucket",
        signature_id,
        bucket_start,
        source_id,
    ]);
    let node = GraphRecord::node(
        bucket_id.clone(),
        NodeKind::LogOccurrenceBucket,
        None,
        None,
        Some(format!("bucket {bucket_start}")),
        format!("Occurrence bucket {bucket_start} x{count}"),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::LogOccurrenceBucket(
        LogOccurrenceBucketPayload {
            bucket_start: bucket_start.to_owned(),
            bucket_width: "1h".to_owned(),
            occurrence_count: count,
            source_id: source_id.to_owned(),
            repository_id: "repo_test".to_owned(),
            occurrence_timestamps: Vec::new(),
        },
    ))
    .with_valid_time(bucket_start, "log_event_timestamp");
    let edge = log_edge(
        EdgeLabel::Aggregates,
        &bucket_id,
        signature_id,
        "LogOccurrenceBucket aggregates ErrorSignature",
    );
    (node, edge)
}

/// A schema-v3 `LogOccurrenceBucket` node (issue #364) carrying per-occurrence
/// `occurrence_timestamps`, plus its `AGGREGATES` edge to the signature. The
/// bucket's `occurrence_count` equals the number of timestamps (the payload
/// invariant), letting consumers bound window counts endpoint-exactly.
fn bucket_with_timestamps(
    signature_id: &str,
    bucket_start: &str,
    timestamps: &[&str],
    source_id: &str,
) -> (GraphRecord, GraphRecord) {
    let bucket_id = aletheia_egregore::log_stable_id(&[
        "log_occurrence_bucket",
        signature_id,
        bucket_start,
        source_id,
    ]);
    let node = GraphRecord::node(
        bucket_id.clone(),
        NodeKind::LogOccurrenceBucket,
        None,
        None,
        Some(format!("bucket {bucket_start}")),
        format!("Occurrence bucket {bucket_start} x{}", timestamps.len()),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::LogOccurrenceBucket(
        LogOccurrenceBucketPayload {
            bucket_start: bucket_start.to_owned(),
            bucket_width: "1h".to_owned(),
            occurrence_count: timestamps.len() as u64,
            source_id: source_id.to_owned(),
            repository_id: "repo_test".to_owned(),
            occurrence_timestamps: timestamps.iter().map(|t| (*t).to_owned()).collect(),
        },
    ))
    .with_valid_time(bucket_start, "log_event_timestamp");
    let edge = log_edge(
        EdgeLabel::Aggregates,
        &bucket_id,
        signature_id,
        "LogOccurrenceBucket aggregates ErrorSignature",
    );
    (node, edge)
}

/// A `FRAME_RESOLVES_TO` edge from a signature onto a code-graph target.
fn frame_edge(
    signature_id: &str,
    target_id: &str,
    frame_index: u32,
    resolution: FrameResolution,
) -> GraphRecord {
    GraphRecord::Edge {
        id: aletheia_egregore::log_stable_id(&[
            "edge",
            "FRAME_RESOLVES_TO",
            signature_id,
            &frame_index.to_string(),
            target_id,
            resolution.as_str(),
        ]),
        schema_version: LOG_SCHEMA_VERSION,
        label: EdgeLabel::FrameResolvesTo,
        source: signature_id.to_owned(),
        target: target_id.to_owned(),
        confidence: Some("1.0".to_owned()),
        resolution: None,
        frame_resolution: Some(resolution),
        frame_index: Some(frame_index),
        temporal: None,
        summary: format!("frame {frame_index} of {signature_id} resolves to {target_id}"),
        producer: None,
        basis: None,
    }
}

fn log_edge(label: EdgeLabel, source: &str, target: &str, summary: &str) -> GraphRecord {
    GraphRecord::Edge {
        id: aletheia_egregore::log_stable_id(&["edge", label.as_str(), source, target]),
        schema_version: LOG_SCHEMA_VERSION,
        label,
        source: source.to_owned(),
        target: target.to_owned(),
        confidence: None,
        resolution: None,
        frame_resolution: None,
        frame_index: None,
        temporal: None,
        summary: summary.to_owned(),
        producer: None,
        basis: None,
    }
}

/// Three linear commits (`c1`→`c2`→`c3`) with `tweaked` modified between c1 and
/// c2, plus four signatures and the new signature's bucket + frame join to the
/// modified symbol.
fn synthetic_log_delta_records() -> Vec<GraphRecord> {
    let tweaked = symbol_id("tweaked", "src/lib.rs");
    let new_sig = log_sig_id("new-boom");
    let (bucket_node, bucket_edge) = bucket_with_edge(&new_sig, NEW_BUCKET, 5);
    vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        // Modified symbol so the AC4 join has a `modified_symbol` to hit.
        symbol_snapshot("keep", "src/lib.rs", "A", "c1sha0000", T1),
        symbol_snapshot("tweaked", "src/lib.rs", "C1", "c1sha0000", T1),
        symbol_snapshot("keep", "src/lib.rs", "A", "c2sha0000", T2),
        symbol_snapshot("tweaked", "src/lib.rs", "C2", "c2sha0000", T2),
        symbol_snapshot("keep", "src/lib.rs", "A", "c3sha0000", T3),
        symbol_snapshot("tweaked", "src/lib.rs", "C2", "c3sha0000", T3),
        // Signatures across the four classes.
        error_signature("new-boom", "error", NEW_FIRST, NEW_LAST, 5),
        error_signature("ceased-warn", "warn", CEASED_FIRST, CEASED_LAST, 3),
        error_signature("cont-error", "error", CONT_FIRST, CONT_LAST, 9),
        error_signature("future-fatal", "fatal", FUTURE_FIRST, FUTURE_LAST, 1),
        // New signature's occurrence bucket + frame resolution onto `tweaked`.
        bucket_node,
        bucket_edge,
        frame_edge(&new_sig, &tweaked, 0, FrameResolution::Resolved),
    ]
}

// ---------------------------------------------------------------------------
// Diagnostics: the taxonomy is reused from issue #118 verbatim.
// ---------------------------------------------------------------------------

#[test]
fn log_deltas_empty_history_errors() {
    let records = vec![error_signature("lonely", "error", NEW_FIRST, NEW_LAST, 1)];
    let err = log_deltas(&records, "c1", "c2", None, false).unwrap_err();
    assert!(matches!(err, RangeDeltasError::EmptyHistory));
}

#[test]
fn log_deltas_missing_commit_errors() {
    let records = synthetic_log_delta_records();
    let err = log_deltas(&records, "ffff", "c3sha0000", None, false).unwrap_err();
    match err {
        RangeDeltasError::MissingCommit { commit_prefix } => assert_eq!(commit_prefix, "ffff"),
        other => panic!("expected MissingCommit, got {other:?}"),
    }
}

#[test]
fn log_deltas_identical_endpoints_error() {
    let records = synthetic_log_delta_records();
    let err = log_deltas(&records, "c2", "c2sha0000", None, false).unwrap_err();
    match err {
        RangeDeltasError::IdenticalEndpoints { commit } => assert_eq!(commit, "c2sha0000"),
        other => panic!("expected IdenticalEndpoints, got {other:?}"),
    }
}

#[test]
fn log_deltas_reversed_range_errors() {
    let records = synthetic_log_delta_records();
    let err = log_deltas(&records, "c3", "c1", None, false).unwrap_err();
    assert!(matches!(err, RangeDeltasError::ReversedRange { .. }));
}

// ---------------------------------------------------------------------------
// Classification across the closed 3-class set + the out-of-range exclusion.
// ---------------------------------------------------------------------------

fn record_ids(rows: &[aletheia_egregore::query::LogSignatureDelta]) -> Vec<&str> {
    rows.iter().map(|r| r.record_id.as_str()).collect()
}

#[test]
fn log_deltas_classifies_four_signature_cases() {
    let records = synthetic_log_delta_records();
    let deltas = log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");

    assert_eq!(deltas.base, "c1sha0000");
    assert_eq!(deltas.head, "c3sha0000");
    assert_eq!(deltas.range_commit_count, 2);
    // Window derived from the range commits (c2, c3).
    assert_eq!(deltas.window.window_start, T2);
    assert_eq!(deltas.window.window_end, T3);

    // (a) first_seen inside the window → new.
    assert_eq!(
        record_ids(&deltas.new_signatures),
        vec![log_sig_id("new-boom")]
    );
    assert_eq!(deltas.new_signatures[0].change_class, "new_signature");
    // (b) last_seen before the range → ceased.
    assert_eq!(
        record_ids(&deltas.ceased_signatures),
        vec![log_sig_id("ceased-warn")]
    );
    assert_eq!(deltas.ceased_signatures[0].change_class, "ceased_signature");
    // (c) spanning the window → continuing.
    assert_eq!(
        record_ids(&deltas.continuing_signatures),
        vec![log_sig_id("cont-error")]
    );
    assert_eq!(
        deltas.continuing_signatures[0].change_class,
        "continuing_signature"
    );

    // (d) first observed after the window is excluded from every class.
    let future = log_sig_id("future-fatal");
    for group in [
        &deltas.new_signatures,
        &deltas.ceased_signatures,
        &deltas.continuing_signatures,
    ] {
        assert!(group.iter().all(|r| r.record_id != future));
    }

    // Every row carries citable identity + valid-time bounds.
    for row in deltas
        .new_signatures
        .iter()
        .chain(&deltas.ceased_signatures)
        .chain(&deltas.continuing_signatures)
    {
        assert!(!row.record_id.is_empty());
        assert!(row.schema_version >= 1);
        assert!(!row.severity.is_empty());
        assert!(!row.first_seen.is_empty());
        assert!(!row.last_seen.is_empty());
    }

    // The disclaimer labels rows as leads with the sampling caveat.
    assert!(deltas.disclaimer.contains("regression LEAD"));
    assert!(deltas.disclaimer.contains("sampling"));
}

#[test]
fn log_deltas_new_signature_joins_overlapping_symbol_delta() {
    let records = synthetic_log_delta_records();
    let deltas = log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");

    let new_row = &deltas.new_signatures[0];
    // The resolved frame binds the signature to the `tweaked` symbol.
    assert_eq!(new_row.resolved_frames.len(), 1);
    assert_eq!(new_row.resolved_frames[0].frame_resolution, "resolved");
    let tweaked = symbol_id("tweaked", "src/lib.rs");
    assert_eq!(new_row.resolved_frames[0].target_record_id, tweaked);

    // AC4: the overlap names exactly the modified symbol, from the reused
    // range_deltas mechanics.
    assert_eq!(new_row.overlapping_symbol_deltas.len(), 1);
    assert_eq!(new_row.overlapping_symbol_deltas[0].record_id, tweaked);
    assert_eq!(
        new_row.overlapping_symbol_deltas[0].change_class,
        "modified_symbol"
    );

    // Only new signatures carry the join.
    for row in deltas
        .ceased_signatures
        .iter()
        .chain(&deltas.continuing_signatures)
    {
        assert!(row.overlapping_symbol_deltas.is_empty());
    }
}

#[test]
fn log_deltas_per_window_occurrences_from_buckets() {
    let records = synthetic_log_delta_records();
    let deltas = log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");
    let new_row = &deltas.new_signatures[0];
    assert_eq!(new_row.occurrence_source, "occurrence_buckets");
    // The single bucket sits after the base endpoint (T1) but at/before head (T3).
    assert_eq!(new_row.base_window_occurrences, Some(0));
    assert_eq!(new_row.head_window_occurrences, Some(5));

    // A signature with no linked buckets falls back to the aggregate count.
    let ceased = &deltas.ceased_signatures[0];
    assert_eq!(ceased.occurrence_source, "aggregate_only");
    assert_eq!(ceased.base_window_occurrences, None);
    assert_eq!(ceased.head_window_occurrences, None);
    assert_eq!(ceased.occurrence_count, 3);
}

#[test]
fn log_deltas_window_occurrences_are_hour_bucket_granular() {
    // HEAD's committer date falls MID-BUCKET: head is 12:30 but the linked
    // `LogOccurrenceBucket` is hour-aligned at 12:00 and holds an aggregate count
    // for the whole 12:00–13:00 hour (occurrences after 12:30 are indistinguishable
    // from ones before — the bucket retains no per-occurrence timestamps). The sum
    // predicate includes any bucket whose start is at/before the endpoint, so the
    // WHOLE 12:00 bucket counts toward head_window_occurrences even though part of
    // the hour is past the exact head instant. We cannot sub-divide the bucket, so
    // (a) the count stays whole-bucket-granular (behavior unchanged) and (b) the
    // envelope DISCLOSES that window occurrence counts are hour-bucket-granular,
    // not endpoint-exact.
    const BASE_T: &str = "2026-01-01T00:00:00Z"; // c1 base
    const START_T: &str = "2026-01-02T00:00:00Z"; // c2 → window_start
    const HEAD_T: &str = "2026-01-02T12:30:00Z"; // c3 head → window_end, MID 12:00 bucket
    const SIG_FIRST: &str = "2026-01-02T12:00:00Z"; // inside [00:00, 12:30]
    const SIG_LAST: &str = "2026-01-02T12:55:00Z";
    const BUCKET_HOUR: &str = "2026-01-02T12:00:00Z"; // hour-aligned; spans 12:00–13:00

    let sig = log_sig_id("mid-bucket-head");
    let (bucket_node, bucket_edge) = bucket_with_edge(&sig, BUCKET_HOUR, 5);
    let records = vec![
        commit("c1sha0000", &[], BASE_T),
        commit("c2sha0000", &["c1sha0000"], START_T),
        commit("c3sha0000", &["c2sha0000"], HEAD_T),
        error_signature("mid-bucket-head", "error", SIG_FIRST, SIG_LAST, 5),
        bucket_node,
        bucket_edge,
    ];
    let deltas = log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");

    // (a) Behavior unchanged: the whole hour-aligned 12:00 bucket is counted at head
    // even though head is 12:30 — occurrences in 12:30–13:00 are included.
    assert_eq!(record_ids(&deltas.new_signatures), vec![sig]);
    let row = &deltas.new_signatures[0];
    assert_eq!(row.occurrence_source, "occurrence_buckets");
    assert_eq!(
        row.head_window_occurrences,
        Some(5),
        "the whole hour-aligned bucket is counted even though head falls mid-hour"
    );

    // (b) Disclosure: the counts are explicitly labeled hour-bucket-granular, not
    // endpoint-exact — both as a compact machine-readable marker and in the
    // always-present disclaimer sentence.
    assert_eq!(deltas.occurrence_count_granularity, "hourly_bucket");
    assert!(
        deltas.disclaimer.contains("hour-bucket-granular"),
        "disclaimer must disclose hour-bucket granularity, got: {}",
        deltas.disclaimer
    );
    assert!(
        deltas.disclaimer.contains("endpoint-exact"),
        "disclaimer must clarify counts are not endpoint-exact, got: {}",
        deltas.disclaimer
    );
}

#[test]
fn log_deltas_window_occurrences_are_endpoint_exact_with_v3_timestamps() {
    // Issue #364: HEAD's committer date falls MID-BUCKET (12:30), and the linked
    // schema-v3 `LogOccurrenceBucket` (hour-aligned at 12:00) now carries
    // per-occurrence timestamps [12:15, 12:45]. The window count is the number of
    // timestamps at or before the exact head instant, so only 12:15 counts — the
    // post-head 12:45 occurrence is excluded, where the legacy hour-bucket rule
    // would have over-counted it. The marker and disclaimer flip to endpoint-exact.
    const BASE_T: &str = "2026-01-01T00:00:00Z"; // c1 base
    const START_T: &str = "2026-01-02T00:00:00Z"; // c2 → window_start
    const HEAD_T: &str = "2026-01-02T12:30:00Z"; // c3 head → MID 12:00 bucket
    const SIG_FIRST: &str = "2026-01-02T12:00:00Z"; // inside [00:00, 12:30]
    const SIG_LAST: &str = "2026-01-02T12:55:00Z";
    const BUCKET_HOUR: &str = "2026-01-02T12:00:00Z"; // spans 12:00–13:00
    const OCC_BEFORE_HEAD: &str = "2026-01-02T12:15:00Z"; // <= 12:30 → counted
    const OCC_AFTER_HEAD: &str = "2026-01-02T12:45:00Z"; // > 12:30 → excluded

    let sig = log_sig_id("exact-head");
    let (bucket_node, bucket_edge) = bucket_with_timestamps(
        &sig,
        BUCKET_HOUR,
        &[OCC_BEFORE_HEAD, OCC_AFTER_HEAD],
        DEFAULT_SOURCE,
    );
    let records = vec![
        commit("c1sha0000", &[], BASE_T),
        commit("c2sha0000", &["c1sha0000"], START_T),
        commit("c3sha0000", &["c2sha0000"], HEAD_T),
        error_signature("exact-head", "error", SIG_FIRST, SIG_LAST, 2),
        bucket_node,
        bucket_edge,
    ];
    let deltas = log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");

    assert_eq!(record_ids(&deltas.new_signatures), vec![sig]);
    let row = &deltas.new_signatures[0];
    assert_eq!(row.occurrence_source, "occurrence_buckets");
    assert_eq!(
        row.head_window_occurrences,
        Some(1),
        "endpoint-exact: only the 12:15 occurrence is at/before the 12:30 head; \
         the post-head 12:45 occurrence is excluded"
    );
    assert_eq!(row.base_window_occurrences, Some(0));

    // The marker and disclaimer flip to endpoint-exact and drop the legacy wording.
    assert_eq!(deltas.occurrence_count_granularity, "endpoint_exact");
    assert!(
        deltas.disclaimer.contains("endpoint-exact"),
        "endpoint-exact disclaimer must state the counts are endpoint-exact, got: {}",
        deltas.disclaimer
    );
    assert!(
        !deltas.disclaimer.contains("hour-bucket-granular"),
        "endpoint-exact disclaimer must not carry the legacy hour-bucket wording, got: {}",
        deltas.disclaimer
    );
}

#[test]
fn log_deltas_mixed_v3_and_legacy_buckets_degrade_to_hourly() {
    // A response with even ONE contributing legacy `log:v2:` bucket (no
    // timestamps) degrades the whole granularity marker to `hourly_bucket`, even
    // though a sibling signature's bucket is schema-v3 endpoint-exact (issue #364).
    const BASE_T: &str = "2026-01-01T00:00:00Z";
    const START_T: &str = "2026-01-02T00:00:00Z";
    const HEAD_T: &str = "2026-01-02T12:30:00Z";
    const HOUR: &str = "2026-01-02T12:00:00Z";

    let v3_sig = log_sig_id("v3-exact");
    let legacy_sig = log_sig_id("v2-legacy");
    let (v3_node, v3_edge) =
        bucket_with_timestamps(&v3_sig, HOUR, &["2026-01-02T12:15:00Z"], DEFAULT_SOURCE);
    // `bucket_with_edge` mints a legacy bucket with empty `occurrence_timestamps`.
    let (legacy_node, legacy_edge) = bucket_with_edge(&legacy_sig, HOUR, 4);
    let records = vec![
        commit("c1sha0000", &[], BASE_T),
        commit("c2sha0000", &["c1sha0000"], START_T),
        commit("c3sha0000", &["c2sha0000"], HEAD_T),
        error_signature(
            "v3-exact",
            "error",
            "2026-01-02T12:00:00Z",
            "2026-01-02T12:20:00Z",
            1,
        ),
        error_signature(
            "v2-legacy",
            "error",
            "2026-01-02T12:00:00Z",
            "2026-01-02T12:55:00Z",
            4,
        ),
        v3_node,
        v3_edge,
        legacy_node,
        legacy_edge,
    ];
    let deltas = log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");
    assert_eq!(
        deltas.occurrence_count_granularity, "hourly_bucket",
        "one legacy bucket degrades the whole response to hour-bucket granularity"
    );
    assert!(deltas.disclaimer.contains("hour-bucket-granular"));
}

#[test]
fn log_deltas_sums_distinct_source_buckets_for_the_same_hour() {
    // Two DISTINCT scan-logs sources observe the same signature in the same hour
    // inside the head window. Since issue #361 made `LogOccurrenceBucket` identity
    // source-aware (repository/signature/hour/width/SOURCE), the two sources mint
    // DISTINCT bucket record IDs with their own per-source counts (3 and 4).
    // Per-window counts SUM across the distinct sources (7), staying consistent
    // with the aggregate `occurrence_count`, which sums the coalesced signatures.
    let sig = log_sig_id("dual-source");
    let (first_node, first_edge) = bucket_with_source(&sig, NEW_BUCKET, 3, "log:v2:source-a");
    let (second_node, second_edge) = bucket_with_source(&sig, NEW_BUCKET, 4, "log:v2:source-b");
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        // Two scan outputs, each contributing its own per-source occurrence count.
        error_signature("dual-source", "error", NEW_FIRST, NEW_LAST, 3),
        error_signature("dual-source", "error", NEW_FIRST, NEW_LAST, 4),
        first_node,
        first_edge,
        second_node,
        second_edge,
    ];
    let deltas = log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");
    assert_eq!(record_ids(&deltas.new_signatures), vec![sig]);
    let row = &deltas.new_signatures[0];
    assert_eq!(row.occurrence_source, "occurrence_buckets");
    // The shared hour is after the base endpoint (T1) and at/before head (T3).
    assert_eq!(row.base_window_occurrences, Some(0));
    assert_eq!(
        row.head_window_occurrences,
        Some(7),
        "distinct source-aware bucket IDs must SUM (3 + 4)"
    );
    // Aggregate count sums the two per-source signature payloads, matching.
    assert_eq!(row.occurrence_count, 7);
}

#[test]
fn log_deltas_is_byte_stable_across_runs() {
    let records = synthetic_log_delta_records();
    let baseline = serde_json::to_string(
        &log_deltas(&records, "c1", "c3", None, false).expect("range should resolve"),
    )
    .expect("serialize");
    for _ in 0..4 {
        let again: LogDeltas =
            log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");
        assert_eq!(baseline, serde_json::to_string(&again).expect("serialize"));
    }
}

// ---------------------------------------------------------------------------
// Coalescing: a graph combining multiple scan-logs outputs for one repo carries
// the SAME ErrorSignature record ID more than once. `LogSource` is a NON-identity
// input for signatures — identity is (repo, fingerprint algorithm, template,
// severity) — so a fingerprint re-observed in a second scan-logs output for the
// same repo mints another ErrorSignature record with the SAME stable ID but its
// own first_seen / last_seen / occurrence_count. log_deltas must group by record
// ID and merge BEFORE classifying, emitting exactly ONE row per stable signature
// ID, never split across conflicting classes.
// ---------------------------------------------------------------------------

/// Two `ErrorSignature` records with the SAME content-addressed record ID
/// (identical seed → identical stable ID), as two `scan-logs` outputs for one
/// repo produce: scan 1 observed the signature entirely BEFORE the `[T2, T3]`
/// range window (alone → `ceased`); scan 2 first observed it INSIDE the window
/// (alone → `new`). Buckets: an H1 hour before the window, an H2 hour inside it,
/// and a re-scanned duplicate of H2 from the SAME source (identical source-aware
/// bucket record ID). Per-window bucket counts DEDUP by record ID (#361), so this
/// identical H2 rescan collapses and is counted ONCE. Merged bounds (earliest
/// `first_seen`, latest `last_seen`) drive a single `ceased` classification.
fn split_signature_records() -> Vec<GraphRecord> {
    let sig = log_sig_id("split-boom");
    let (h1_node, h1_edge) = bucket_with_edge(&sig, "2026-01-01T00:00:00Z", 3);
    let (h2_node, h2_edge) = bucket_with_edge(&sig, NEW_BUCKET, 5);
    // A second scan re-emits the identical H2 bucket from the SAME source: same
    // source-aware bucket record ID and identical bytes. Per-window counts dedup
    // by record ID (#361), so this rescan collapses and is counted once.
    let (h2_dup_node, h2_dup_edge) = bucket_with_edge(&sig, NEW_BUCKET, 5);
    vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        // Same seed → same stable ErrorSignature record ID, emitted twice.
        error_signature(
            "split-boom",
            "error",
            "2026-01-01T00:00:00Z",
            "2026-01-01T05:00:00Z",
            3,
        ),
        error_signature("split-boom", "error", NEW_FIRST, NEW_LAST, 5),
        h1_node,
        h1_edge,
        h2_node,
        h2_edge,
        h2_dup_node,
        h2_dup_edge,
    ]
}

#[test]
fn log_deltas_coalesces_split_signature_across_scan_outputs() {
    let records = split_signature_records();
    let deltas = log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");
    let sig = log_sig_id("split-boom");

    // (1) Exactly ONE row for the stable signature ID across all classes — never
    // split into a `new` + `ceased` pair by iterating both node records.
    let rows: Vec<&str> = deltas
        .new_signatures
        .iter()
        .chain(&deltas.ceased_signatures)
        .chain(&deltas.continuing_signatures)
        .filter(|r| r.record_id == sig)
        .map(|r| r.change_class)
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "one row per stable signature ID, never split across classes (got {rows:?})"
    );

    // (2) Merged first_seen (earliest = before the window) → NOT `new`; merged
    // last_seen (latest = before window_end) → `ceased`.
    assert!(
        deltas.new_signatures.iter().all(|r| r.record_id != sig),
        "the merged signature must not classify as new"
    );
    assert_eq!(record_ids(&deltas.ceased_signatures), vec![sig.clone()]);
    let row = &deltas.ceased_signatures[0];
    assert_eq!(row.change_class, "ceased_signature");
    assert_eq!(
        row.first_seen, "2026-01-01T00:00:00Z",
        "merged first_seen is the earliest across the group"
    );
    assert_eq!(
        row.last_seen, NEW_LAST,
        "merged last_seen is the latest across the group"
    );

    // (3) Deduped per-window bucket counts (#361): the re-scanned H2 hour shares a
    // source-aware bucket record ID and collapses, so it is counted ONCE.
    assert_eq!(row.occurrence_source, "occurrence_buckets");
    assert_eq!(
        row.base_window_occurrences,
        Some(3),
        "only the H1 bucket is at/before the base endpoint (T1)"
    );
    assert_eq!(
        row.head_window_occurrences,
        Some(8),
        "H1 (3) + H2 (5); the identical H2 rescan is deduped by bucket ID"
    );
    // (4) Aggregate occurrence_count sums the two scan payloads.
    assert_eq!(
        row.occurrence_count, 8,
        "aggregate count sums the group's per-scan occurrence counts"
    );
}

// ---------------------------------------------------------------------------
// Embedded coalescing parity (issue #363): a `--data-dir` store ingested with
// two differing-content `scan-logs` outputs for the SAME signature ID must
// coalesce exactly as the `--graph` path does over the concatenated JSONL. Two
// distinct (non-identical) buckets avoid the byte-identical-re-ingest dedup, so
// occurrence sums match too.
// ---------------------------------------------------------------------------

/// Same seed → same stable `ErrorSignature` ID, emitted twice with DIFFERING
/// content: scan 1 entirely before the `[T2, T3]` window (alone → `ceased`),
/// scan 2 first observed inside it (alone → `new`). Distinct buckets (H1 before,
/// H2 inside) so nothing dedupes. Coalesced: merged `first_seen` (earliest,
/// before the window) → NOT new; merged `last_seen` (latest, before
/// `window_end`) → `ceased`.
#[cfg(feature = "embedded-aletheiadb")]
fn split_signature_distinct_bucket_records() -> Vec<GraphRecord> {
    let sig = log_sig_id("split-boom");
    let (h1_node, h1_edge) = bucket_with_edge(&sig, "2026-01-01T00:00:00Z", 3);
    let (h2_node, h2_edge) = bucket_with_edge(&sig, NEW_BUCKET, 5);
    vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        error_signature(
            "split-boom",
            "error",
            "2026-01-01T00:00:00Z",
            "2026-01-01T05:00:00Z",
            3,
        ),
        error_signature("split-boom", "error", NEW_FIRST, NEW_LAST, 5),
        h1_node,
        h1_edge,
        h2_node,
        h2_edge,
    ]
}

/// A comparable, order-independent projection of every classified signature row.
#[cfg(feature = "embedded-aletheiadb")]
#[allow(clippy::type_complexity)]
fn signature_rows(
    d: &LogDeltas,
) -> Vec<(
    String,
    &'static str,
    String,
    String,
    u64,
    Option<u64>,
    Option<u64>,
)> {
    let mut rows: Vec<_> = d
        .new_signatures
        .iter()
        .chain(&d.ceased_signatures)
        .chain(&d.continuing_signatures)
        .map(|r| {
            (
                r.record_id.clone(),
                r.change_class,
                r.first_seen.clone(),
                r.last_seen.clone(),
                r.occurrence_count,
                r.base_window_occurrences,
                r.head_window_occurrences,
            )
        })
        .collect();
    rows.sort();
    rows
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn log_deltas_embedded_data_dir_coalesces_like_graph_for_differing_scans() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};

    let records = split_signature_distinct_bucket_records();

    // Build an embedded store by writing both scan outputs (same signature ID,
    // differing content) plus their distinct buckets.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("split-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
    for record in &records {
        sink.write_record(record).expect("record should write");
    }
    let embedded_records = sink
        .read_all_records_log_retained()
        .expect("log-retained read should surface both signature versions");
    drop(sink);

    // `--graph` (concatenated JSONL) vs `--data-dir` (log-retained embedded read).
    let graph = log_deltas(&records, "c1", "c3", None, false).expect("graph range should resolve");
    let embedded = log_deltas(&embedded_records, "c1", "c3", None, true)
        .expect("embedded range should resolve");

    // The coalesced classification + occurrence data match byte-for-byte.
    assert_eq!(
        signature_rows(&embedded),
        signature_rows(&graph),
        "the embedded --data-dir result must coalesce exactly like the --graph result"
    );

    // And concretely: the merged signature is CEASED (the split case) — NOT the
    // last-write-wins `new` a non-retaining read would have produced.
    let sig = log_sig_id("split-boom");
    assert_eq!(record_ids(&embedded.ceased_signatures), vec![sig.clone()]);
    assert!(
        embedded.new_signatures.iter().all(|r| r.record_id != sig),
        "the coalesced signature must not classify as new on the embedded path"
    );
    let row = &embedded.ceased_signatures[0];
    assert_eq!(row.first_seen, "2026-01-01T00:00:00Z");
    assert_eq!(row.last_seen, NEW_LAST);
    assert_eq!(row.occurrence_count, 8, "aggregate sums both scan payloads");
    assert_eq!(row.base_window_occurrences, Some(3));
    assert_eq!(row.head_window_occurrences, Some(8));

    // The embedded path discloses the residual byte-identical-re-ingest caveat.
    assert!(
        embedded.embedded_log_retention_caveat.is_some(),
        "the embedded path must still disclose the retention caveat"
    );
    assert!(
        graph.embedded_log_retention_caveat.is_none(),
        "the --graph path must not carry the retention caveat"
    );
}

// ---------------------------------------------------------------------------
// Source-aware bucket identity convergence (issue #361). Two cases prove that
// per-window occurrence counts now CONVERGE between `--graph` and `--data-dir`:
//   (a) a RESCAN of the identical source mints IDENTICAL source-aware bucket IDs,
//       which collapse on BOTH paths (`--graph` dedups by record ID, `--data-dir`
//       idempotent-write-dedups) — no divergence in bucket counts;
//   (b) two DISTINCT sources mint DISTINCT source-aware bucket IDs, which SUM on
//       BOTH paths.
// The `--data-dir` retention caveat remains ONLY to disclose the residual
// idempotent-write behavior (a byte-identical whole-output re-ingest doubles the
// signature aggregate `occurrence_count` on `--graph` but not on `--data-dir`) —
// NOT a bucket-identity gap.
// ---------------------------------------------------------------------------

/// A RESCAN of the identical source: the SAME `ErrorSignature` (byte-identical
/// content) and the SAME buckets (default source → identical source-aware bucket
/// IDs) emitted TWICE, exactly as `cat scan.jsonl scan.jsonl` would produce.
/// Buckets: 12:00 x4 and 13:00 x3, both inside the `[T2, T3]` window.
#[cfg(feature = "embedded-aletheiadb")]
fn rescan_identical_source_records() -> Vec<GraphRecord> {
    const HOUR_A: &str = "2026-01-02T12:00:00Z";
    const HOUR_B: &str = "2026-01-02T13:00:00Z";

    let sig = log_sig_id("rescan-boom");
    let (a_node, a_edge) = bucket_with_edge(&sig, HOUR_A, 4);
    let (b_node, b_edge) = bucket_with_edge(&sig, HOUR_B, 3);
    vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        // Byte-identical signature emitted twice (identical rescan).
        error_signature("rescan-boom", "error", NEW_FIRST, NEW_LAST, 5),
        error_signature("rescan-boom", "error", NEW_FIRST, NEW_LAST, 5),
        a_node.clone(),
        a_edge.clone(),
        b_node.clone(),
        b_edge.clone(),
        a_node,
        a_edge,
        b_node,
        b_edge,
    ]
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn log_deltas_rescan_buckets_converge_across_graph_and_data_dir() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};

    let records = rescan_identical_source_records();

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("rescan-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
    for record in &records {
        sink.write_record(record).expect("record should write");
    }
    let embedded_records = sink
        .read_all_records_log_retained()
        .expect("log-retained read");
    drop(sink);

    let sig = log_sig_id("rescan-boom");

    // --data-dir: identical buckets are idempotent-write-deduped → counted once:
    // 4 + 3 = 7.
    let embedded = log_deltas(&embedded_records, "c1", "c3", None, true)
        .expect("embedded range should resolve");
    assert_eq!(record_ids(&embedded.new_signatures), vec![sig.clone()]);
    let embedded_row = &embedded.new_signatures[0];
    assert_eq!(embedded_row.occurrence_source, "occurrence_buckets");
    assert_eq!(embedded_row.base_window_occurrences, Some(0));
    assert_eq!(embedded_row.head_window_occurrences, Some(7));

    // --graph: the identical rescan buckets share source-aware IDs and are deduped
    // by record ID before summing → ALSO 7. Per-window counts CONVERGE.
    let graph = log_deltas(&records, "c1", "c3", None, false).expect("graph range should resolve");
    assert_eq!(record_ids(&graph.new_signatures), vec![sig]);
    let graph_row = &graph.new_signatures[0];
    assert_eq!(
        graph_row.head_window_occurrences,
        Some(7),
        "identical rescan buckets collapse on --graph too (source-aware identity, #361)"
    );
    assert_eq!(
        graph_row.head_window_occurrences, embedded_row.head_window_occurrences,
        "per-window occurrence counts CONVERGE for a rescan"
    );

    // The ONLY residual: the byte-identical whole-output re-ingest doubles the
    // signature aggregate occurrence_count on --graph (10) but not on --data-dir
    // (5, idempotent-write dedup). This is idempotent-write behavior, not a
    // bucket-identity gap — exactly what the caveat now discloses.
    assert_eq!(graph_row.occurrence_count, 10);
    assert_eq!(embedded_row.occurrence_count, 5);
    assert!(embedded.embedded_log_retention_caveat.is_some());
    let caveat = embedded.embedded_log_retention_caveat.as_ref().unwrap();
    assert!(
        caveat.message.contains("converge"),
        "the caveat must now disclose that per-window counts converge, got: {}",
        caveat.message
    );
    assert!(
        caveat.message.contains("idempotent"),
        "the caveat must attribute the residual to idempotent-write dedup, got: {}",
        caveat.message
    );

    // Determinism.
    let embedded_again = log_deltas(&embedded_records, "c1", "c3", None, true)
        .expect("embedded range should resolve again");
    assert_eq!(signature_rows(&embedded), signature_rows(&embedded_again));
}

/// Two DISTINCT sources observing the SAME signature in the SAME 12:00 hour, with
/// DIFFERING signature content (distinct `last_seen`) so both are retained on
/// `--data-dir`. Distinct sources mint DISTINCT source-aware bucket IDs.
#[cfg(feature = "embedded-aletheiadb")]
fn distinct_source_same_hour_records() -> Vec<GraphRecord> {
    const HOUR: &str = "2026-01-02T12:00:00Z";
    const LAST_A: &str = "2026-01-02T12:30:00Z";
    const LAST_B: &str = "2026-01-02T13:30:00Z";

    let sig = log_sig_id("multi-source");
    let (a_node, a_edge) = bucket_with_source(&sig, HOUR, 4, "log:v2:source-a");
    let (b_node, b_edge) = bucket_with_source(&sig, HOUR, 6, "log:v2:source-b");
    vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        error_signature("multi-source", "error", NEW_FIRST, LAST_A, 4),
        error_signature("multi-source", "error", NEW_FIRST, LAST_B, 6),
        a_node,
        a_edge,
        b_node,
        b_edge,
    ]
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn log_deltas_distinct_sources_sum_across_graph_and_data_dir() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};

    let records = distinct_source_same_hour_records();

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("multi-source-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
    for record in &records {
        sink.write_record(record).expect("record should write");
    }
    let embedded_records = sink
        .read_all_records_log_retained()
        .expect("log-retained read");
    drop(sink);

    let sig = log_sig_id("multi-source");

    // --data-dir: distinct source-aware bucket IDs both survive → 4 + 6 = 10.
    let embedded = log_deltas(&embedded_records, "c1", "c3", None, true)
        .expect("embedded range should resolve");
    assert_eq!(record_ids(&embedded.new_signatures), vec![sig.clone()]);
    let embedded_row = &embedded.new_signatures[0];
    assert_eq!(embedded_row.head_window_occurrences, Some(10));

    // --graph: distinct bucket IDs both sum → 10. CONVERGES with --data-dir.
    let graph = log_deltas(&records, "c1", "c3", None, false).expect("graph range should resolve");
    assert_eq!(record_ids(&graph.new_signatures), vec![sig]);
    let graph_row = &graph.new_signatures[0];
    assert_eq!(
        graph_row.head_window_occurrences,
        Some(10),
        "distinct sources sum on --graph too (source-aware identity, #361)"
    );
    assert_eq!(
        graph_row.head_window_occurrences, embedded_row.head_window_occurrences,
        "distinct-source per-window occurrence counts CONVERGE"
    );
}

/// Four signatures across the classes plus the new signature's bucket, with NO
/// `FRAME_RESOLVES_TO` edge onto a multi-snapshot symbol (which the stricter
/// embedded write path rejects). Exercises the single-scan embedded parity.
#[cfg(feature = "embedded-aletheiadb")]
fn single_scan_records() -> Vec<GraphRecord> {
    let new_sig = log_sig_id("new-boom");
    let (bucket_node, bucket_edge) = bucket_with_edge(&new_sig, NEW_BUCKET, 5);
    vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        error_signature("new-boom", "error", NEW_FIRST, NEW_LAST, 5),
        error_signature("ceased-warn", "warn", CEASED_FIRST, CEASED_LAST, 3),
        error_signature("cont-error", "error", CONT_FIRST, CONT_LAST, 9),
        error_signature("future-fatal", "fatal", FUTURE_FIRST, FUTURE_LAST, 1),
        bucket_node,
        bucket_edge,
    ]
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn log_deltas_embedded_data_dir_single_scan_matches_graph() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};

    // Regression: a single-scan embedded store is unaffected by the retention
    // change — its result equals the `--graph` result over the same records.
    let records = single_scan_records();
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("single-scan-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
    for record in &records {
        sink.write_record(record).expect("record should write");
    }
    let embedded_records = sink
        .read_all_records_log_retained()
        .expect("log-retained read");
    drop(sink);

    let graph = log_deltas(&records, "c1", "c3", None, false).expect("graph range should resolve");
    let embedded = log_deltas(&embedded_records, "c1", "c3", None, true)
        .expect("embedded range should resolve");
    assert_eq!(
        signature_rows(&embedded),
        signature_rows(&graph),
        "a single-scan embedded store must match the --graph result"
    );
}

/// The same `ErrorSignature` re-emitted with evidence links added but an
/// IDENTICAL log payload, exactly as `resolve-frames` / `link-logs` do (they
/// re-emit the signature node enriched with `FRAME_RESOLVES_TO` /
/// `EMITTED_DURING` / `REFERENCES_TASK` evidence links, leaving
/// `first_seen`/`last_seen`/`occurrence_count` untouched).
#[cfg(feature = "embedded-aletheiadb")]
fn error_signature_enriched(
    seed: &str,
    severity: &str,
    first_seen: &str,
    last_seen: &str,
    occurrence_count: u64,
) -> GraphRecord {
    error_signature(seed, severity, first_seen, last_seen, occurrence_count).with_evidence_links(
        vec![EvidenceLink {
            target_record_id: Some(symbol_id("tweaked", "src/lib.rs")),
            target_domain: "codegraph".to_owned(),
            relation: EdgeLabel::FrameResolvesTo.as_str().to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: Some("src/lib.rs".to_owned()),
            target_span: None,
            target_git_commit: None,
        }],
    )
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn log_deltas_embedded_does_not_double_count_enrichment_rewrite() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};

    // Issue #363 regression: the STANDARD pipeline `scan-logs -> resolve-frames ->
    // link-logs` ingests the SAME `ErrorSignature` node twice into one embedded
    // store — first bare (scan-logs), then enriched with evidence links
    // (resolve-frames / link-logs) but with an IDENTICAL log payload
    // (`first_seen`/`last_seen`/`occurrence_count` unchanged). The enriched node's
    // differing content appends a new physical version. The log-retained read must
    // treat these as ONE scan observation and NOT re-count the occurrence total:
    // an enrichment rewrite is not a distinct scan.
    let (bucket_node, bucket_edge) = bucket_with_edge(&log_sig_id("enrich-boom"), NEW_BUCKET, 5);
    let records: Vec<GraphRecord> = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        // scan-logs: the bare signature.
        error_signature("enrich-boom", "error", NEW_FIRST, NEW_LAST, 5),
        // resolve-frames / link-logs: the SAME signature, identical payload, only
        // evidence links added.
        error_signature_enriched("enrich-boom", "error", NEW_FIRST, NEW_LAST, 5),
        // The occurrence bucket for the new signature.
        bucket_node,
        bucket_edge,
    ];

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("enrichment-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
    for record in &records {
        sink.write_record(record).expect("record should write");
    }
    let embedded_records = sink
        .read_all_records_log_retained()
        .expect("log-retained read");
    drop(sink);

    let sig = log_sig_id("enrich-boom");

    // Exactly ONE `ErrorSignature` for that ID: the enrichment rewrite is not a
    // distinct scan observation, so it is not re-emitted.
    let sig_count = embedded_records
        .iter()
        .filter(|r| r.id() == sig && r.node_kind_name() == Some("ErrorSignature"))
        .count();
    assert_eq!(
        sig_count, 1,
        "the enrichment rewrite must not surface a second signature version"
    );

    // And log-deltas reports the single-scan occurrence truth (5), NOT 10.
    let embedded = log_deltas(&embedded_records, "c1", "c3", None, true)
        .expect("embedded range should resolve");
    assert_eq!(record_ids(&embedded.new_signatures), vec![sig.clone()]);
    let row = &embedded.new_signatures[0];
    assert_eq!(
        row.occurrence_count, 5,
        "the aggregate occurrence must not double-count the enrichment rewrite"
    );
    assert_eq!(
        row.head_window_occurrences,
        Some(5),
        "the per-window bucket total must not double-count"
    );

    // The enriched (current) version's evidence link survives: the retained read
    // keeps the enriched current node, not the bare one.
    let current_sig = embedded_records
        .iter()
        .find(|r| r.id() == sig && r.node_kind_name() == Some("ErrorSignature"))
        .expect("the signature must be present");
    assert!(
        current_sig.evidence_links().is_some_and(|l| !l.is_empty()),
        "the retained signature must be the enriched (evidence-link-carrying) version"
    );
}

// ---------------------------------------------------------------------------
// Timezone-offset window: commit committer dates carry LOCAL offsets while log
// valid times are Z-normalized (Codex P1). Classification and bucket cutoffs
// must compare by parsed instant, not raw RFC 3339 string order.
// ---------------------------------------------------------------------------

/// Three linear commits where the head commit's committer date carries a
/// non-UTC offset, plus one signature whose Z-normalized `first_seen` lands
/// just inside the window by instant but sorts AFTER the head string lexically.
fn timezone_window_records() -> Vec<GraphRecord> {
    let sig = log_sig_id("tz-boom");
    let (bucket_node, bucket_edge) = bucket_with_edge(&sig, TZ_BUCKET, 7);
    vec![
        commit("c1sha0000", &[], TZ_BASE),
        commit("c2sha0000", &["c1sha0000"], TZ_MID),
        commit("c3sha0000", &["c2sha0000"], TZ_HEAD),
        error_signature("tz-boom", "error", TZ_SIG_FIRST, TZ_SIG_LAST, 7),
        bucket_node,
        bucket_edge,
    ]
}

#[test]
fn log_deltas_classifies_across_timezone_offsets() {
    let records = timezone_window_records();
    let deltas = log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");

    // window_end is the head commit's committer date (05:30:00Z as an instant).
    assert_eq!(deltas.window.window_start, TZ_MID);
    assert_eq!(deltas.window.window_end, TZ_HEAD);

    // The signature's first_seen (05:00:00Z) is inside [00:00Z, 05:30Z] by
    // instant, so it must classify as `new` — never dropped as out-of-range by
    // a lexical string comparison against the offset-bearing head string.
    assert_eq!(
        record_ids(&deltas.new_signatures),
        vec![log_sig_id("tz-boom")],
        "an in-window signature must not be dropped across a non-UTC commit offset"
    );
    assert!(deltas.ceased_signatures.is_empty());
    assert!(deltas.continuing_signatures.is_empty());

    // Bucket cutoff is the same instant comparison: the 05:00Z bucket is at or
    // before the head's 05:30Z instant, so it counts toward the head window.
    let new_row = &deltas.new_signatures[0];
    assert_eq!(new_row.occurrence_source, "occurrence_buckets");
    assert_eq!(new_row.base_window_occurrences, Some(0));
    assert_eq!(
        new_row.head_window_occurrences,
        Some(7),
        "a bucket at/before the head instant must count despite the offset"
    );
}

// ---------------------------------------------------------------------------
// Seeded end-to-end CLI test (AC1, AC8, AC9): scan-history + augmented graph.
// ---------------------------------------------------------------------------

/// Seeds a three-commit git repo where `tweaked` is modified between the first
/// and second commit, returning the three full SHAs.
fn seed_repo(repo: &Path) -> [String; 3] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(
        repo,
        "src/lib.rs",
        "pub fn keep() -> u32 { 1 }\npub fn tweaked() -> u32 { 4 }\n",
    );
    let first = commit_fixture(repo, "seed", T1);

    write(
        repo,
        "src/lib.rs",
        "pub fn keep() -> u32 { 1 }\npub fn tweaked() -> u32 { 44 }\n",
    );
    let second = commit_fixture(repo, "modify tweaked", T2);

    write(repo, "src/notes.txt", "third commit marker\n");
    let third = commit_fixture(repo, "third", T3);

    [first, second, third]
}

/// Scans the repo and appends the synthetic log records, returning the combined
/// record set and the `tweaked` symbol record ID. The scan-history records carry
/// a real `Repository` node plus its `CONTAINS` commit/file/symbol topology, so
/// `--repo` scoping can attribute the code side; the log records deliberately
/// carry no repository attribution (as `scan-logs` emits them).
fn augmented_records(repo: &Path) -> (Vec<GraphRecord>, String) {
    let jsonl = scan_repository_history(repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let mut records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();

    // The modified symbol's stable record ID, as reported by range_deltas.
    let tweaked_id = records
        .iter()
        .find_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                name: Some(name),
                ..
            } if name == "tweaked" => Some(id.clone()),
            _ => None,
        })
        .expect("tweaked symbol should be scanned");

    let new_sig = log_sig_id("new-boom");
    let (bucket_node, bucket_edge) = bucket_with_edge(&new_sig, NEW_BUCKET, 5);
    records.push(error_signature("new-boom", "error", NEW_FIRST, NEW_LAST, 5));
    records.push(error_signature(
        "ceased-warn",
        "warn",
        CEASED_FIRST,
        CEASED_LAST,
        3,
    ));
    records.push(error_signature(
        "cont-error",
        "error",
        CONT_FIRST,
        CONT_LAST,
        9,
    ));
    records.push(error_signature(
        "future-fatal",
        "fatal",
        FUTURE_FIRST,
        FUTURE_LAST,
        1,
    ));
    records.push(bucket_node);
    records.push(bucket_edge);
    records.push(frame_edge(
        &new_sig,
        &tweaked_id,
        0,
        FrameResolution::Resolved,
    ));

    (records, tweaked_id)
}

/// Scans the repo, appends the synthetic log records, and writes a combined
/// JSONL graph. Returns the graph path and the `tweaked` symbol record ID.
fn build_augmented_graph(repo: &Path, graph_path: &Path) -> String {
    let (records, tweaked_id) = augmented_records(repo);
    let mut out = String::new();
    for r in &records {
        out.push_str(&serde_json::to_string(r).expect("record should serialize"));
        out.push('\n');
    }
    fs::write(graph_path, out).expect("graph should write");
    tweaked_id
}

/// Scans the repo's history into records and returns them alongside the real
/// `Repository` record ID and the `tweaked` symbol record ID, WITHOUT appending
/// any log signatures — so a test can attach log records with whatever
/// `repository_id` attribution it wants to exercise (issue #362).
fn scanned_code_records(repo: &Path) -> (Vec<GraphRecord>, String, String) {
    let jsonl = scan_repository_history(repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();
    let repo_id = records
        .iter()
        .find_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::Repository,
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .expect("scan-history should emit a Repository node");
    let tweaked_id = records
        .iter()
        .find_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                name: Some(name),
                ..
            } if name == "tweaked" => Some(id.clone()),
            _ => None,
        })
        .expect("tweaked symbol should be scanned");
    (records, repo_id, tweaked_id)
}

#[test]
fn log_deltas_repo_scope_keeps_attributed_and_excludes_foreign() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, _second, third] = seed_repo(&repo);
    let (mut records, repo_id, tweaked_id) = scanned_code_records(&repo);
    let base_prefix = &first[..12];

    // In-repo signatures attributed to the scanned repository (schema v3, #362).
    let new_sig = log_sig_id("new-boom");
    let (bucket_node, bucket_edge) = bucket_with_edge(&new_sig, NEW_BUCKET, 5);
    records.push(error_signature_attributed(
        "new-boom", "error", NEW_FIRST, NEW_LAST, 5, &repo_id,
    ));
    records.push(error_signature_attributed(
        "ceased-warn",
        "warn",
        CEASED_FIRST,
        CEASED_LAST,
        3,
        &repo_id,
    ));
    records.push(error_signature_attributed(
        "cont-error",
        "error",
        CONT_FIRST,
        CONT_LAST,
        9,
        &repo_id,
    ));
    records.push(bucket_node);
    records.push(bucket_edge);
    records.push(frame_edge(
        &new_sig,
        &tweaked_id,
        0,
        FrameResolution::Resolved,
    ));

    // A FOREIGN repository's in-window signature: attributed to a different
    // repository id that owns no code here. Under #362 it must be EXCLUDED when
    // the query is scoped to the scanned repository (the cross-repository false
    // lead #326 could only disclose).
    let foreign = log_sig_id("foreign-boom");
    records.push(error_signature_attributed(
        "foreign-boom",
        "error",
        NEW_FIRST,
        NEW_LAST,
        7,
        "codegraph:v1:other-repo",
    ));

    // Unscoped: every in-window signature is classified, in-repo and foreign.
    let unscoped = log_deltas(&records, base_prefix, &third, None, false)
        .expect("unscoped range should resolve");
    assert!(record_ids(&unscoped.new_signatures).contains(&new_sig.as_str()));
    assert!(
        record_ids(&unscoped.new_signatures).contains(&foreign.as_str()),
        "an unscoped run classifies every in-window signature"
    );

    // Scoped to the scanned repository: the attributed in-repo signatures are
    // KEPT and the foreign signature is EXCLUDED (issue #362).
    let scoped = log_deltas(&records, base_prefix, &third, Some(&repo_id), false)
        .expect("scoped range should resolve");
    assert_eq!(
        record_ids(&scoped.new_signatures),
        vec![log_sig_id("new-boom")],
        "scoping keeps the repo's attributed signature and drops the foreign one"
    );
    assert!(
        !record_ids(&scoped.new_signatures).contains(&foreign.as_str()),
        "a signature attributed to another repository must be excluded when scoped"
    );
    assert_eq!(scoped.ceased_signatures.len(), 1);
    assert_eq!(scoped.continuing_signatures.len(), 1);

    // Buckets and the frame/overlap join survive scoping for the kept signature.
    assert_eq!(
        scoped.new_signatures[0].occurrence_source,
        "occurrence_buckets"
    );
    assert_eq!(scoped.new_signatures[0].head_window_occurrences, Some(5));
    assert_eq!(scoped.new_signatures[0].overlapping_symbol_deltas.len(), 1);
    assert_eq!(
        scoped.new_signatures[0].overlapping_symbol_deltas[0].record_id,
        tweaked_id
    );

    // Every log signature that reached the scoped result is attributed (schema
    // v3), so the residual caveat is ABSENT — the filtering is sound.
    assert!(
        scoped.repo_scope_caveat.is_none(),
        "a fully-attributed scoped store carries no residual caveat"
    );
}

#[test]
fn log_deltas_repo_scope_excludes_legacy_unattributed_and_discloses() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, _second, third] = seed_repo(&repo);
    let (mut records, repo_id, _tweaked_id) = scanned_code_records(&repo);
    let base_prefix = &first[..12];

    // One attributed in-repo signature (kept) plus one LEGACY `log:v2:` signature
    // whose `repository_id` deserializes empty (unattributed). Both would classify
    // `new` in-window, but the legacy one cannot be proven in-repo.
    records.push(error_signature_attributed(
        "new-boom", "error", NEW_FIRST, NEW_LAST, 5, &repo_id,
    ));
    records.push(error_signature_attributed(
        "legacy-boom",
        "error",
        NEW_FIRST,
        NEW_LAST,
        9,
        "",
    ));

    let scoped = log_deltas(&records, base_prefix, &third, Some(&repo_id), false)
        .expect("scoped range should resolve");

    // The attributed signature is kept; the legacy unattributed one is EXCLUDED.
    let legacy = log_sig_id("legacy-boom");
    assert_eq!(
        record_ids(&scoped.new_signatures),
        vec![log_sig_id("new-boom")]
    );
    assert!(
        !record_ids(&scoped.new_signatures).contains(&legacy.as_str()),
        "a legacy unattributed signature cannot be proven in-repo and is excluded"
    );

    // The residual caveat fires, naming the excluded count, and NEVER carries the
    // obsolete "logs are not repository-filtered" story or a full-isolation claim.
    let caveat = scoped
        .repo_scope_caveat
        .as_ref()
        .expect("excluding a legacy unattributed signature must disclose the residual caveat");
    assert_eq!(caveat.repo_scope, repo_id);
    assert_eq!(
        caveat.excluded_unattributed_signature_count, 1,
        "the caveat must name how many legacy unattributed signatures were excluded"
    );
    assert!(
        caveat.message.contains("EXCLUDED"),
        "the caveat must disclose the conservative exclusion"
    );
    assert!(
        caveat.message.contains("issue #362") && caveat.message.contains("schema v3"),
        "the caveat must attribute the v3 filtering to issue #362"
    );
    assert!(
        !caveat.message.contains("NOT repository-filtered"),
        "the shrunk caveat must not carry the obsolete unfiltered-logs disclosure"
    );
}

#[test]
fn log_deltas_unscoped_never_carries_repo_caveat() {
    let records = synthetic_log_delta_records();
    let unscoped =
        log_deltas(&records, "c1", "c3", None, false).expect("unscoped range should resolve");
    assert!(
        unscoped.repo_scope_caveat.is_none(),
        "unscoped log-deltas must not carry a repo-scope caveat"
    );
}

// ---------------------------------------------------------------------------
// Embedded-store retention caveat (issue #363).
//
// The embedded (`--data-dir`) lane loads through the log-retained read surface,
// which surfaces every superseded non-temporal `ErrorSignature` /
// `LogOccurrenceBucket` version that differing-content `scan-logs` ingests
// append, so the cross-scan coalescing performed on the `--graph` path IS
// reconstructed there. We still DIAGNOSE (disclose in the envelope) the one
// residual divergence: byte-identical re-ingests are deduped, not multiplied.
// ---------------------------------------------------------------------------

#[test]
fn log_deltas_embedded_source_discloses_retention_when_log_records_present() {
    let records = synthetic_log_delta_records();
    let deltas = log_deltas(&records, "c1", "c3", None, true).expect("range should resolve");

    let caveat = deltas
        .embedded_log_retention_caveat
        .as_ref()
        .expect("embedded path with log records must disclose the retention caveat");
    // #363 landed the adapter-level retention and #361 made bucket identity
    // source-aware: the caveat now states coalescing is reconstructed, per-window
    // counts converge, and the only residual is the idempotent-write dedup of a
    // byte-identical whole-output re-ingest.
    assert!(
        caveat
            .message
            .contains("retain every superseded non-temporal log observation"),
        "the caveat must state embedded stores retain every superseded log observation"
    );
    assert!(
        caveat.message.contains("reconstructed here"),
        "the caveat must state cross-scan coalescing is reconstructed on the embedded path"
    );
    assert!(
        caveat.message.contains("`--graph`"),
        "the caveat must reference the `--graph` path it now matches"
    );
    assert!(
        caveat.message.contains("converge"),
        "the caveat must disclose that per-window counts now converge (issue #361)"
    );
    assert!(
        caveat.message.contains("byte-identical re-ingest"),
        "the caveat must disclose the residual idempotent-write re-ingest divergence"
    );
    assert!(
        caveat.message.contains("#363"),
        "the caveat must reference the issue tracking the fix"
    );

    // The flag is disclosure-only: classification is byte-for-byte the `--graph`
    // result — the embedded flag NEVER changes which class a signature lands in.
    assert_eq!(
        record_ids(&deltas.new_signatures),
        vec![log_sig_id("new-boom")]
    );
    assert_eq!(deltas.ceased_signatures.len(), 1);
    assert_eq!(deltas.continuing_signatures.len(), 1);
}

#[test]
fn log_deltas_graph_source_never_carries_retention_caveat() {
    let records = synthetic_log_delta_records();
    let deltas = log_deltas(&records, "c1", "c3", None, false).expect("range should resolve");
    assert!(
        deltas.embedded_log_retention_caveat.is_none(),
        "the --graph path preserves every ingested line and must not carry the retention caveat"
    );
}

#[test]
fn log_deltas_embedded_source_without_log_records_omits_retention_caveat() {
    // Commits + a modified symbol, but NO `ErrorSignature` records. An embedded
    // store with no log records yields exact results, so no disclosure is
    // warranted — the caveat is gated on log records actually being present.
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        symbol_snapshot("tweaked", "src/lib.rs", "C1", "c1sha0000", T1),
        symbol_snapshot("tweaked", "src/lib.rs", "C2", "c2sha0000", T2),
        symbol_snapshot("tweaked", "src/lib.rs", "C2", "c3sha0000", T3),
    ];
    let deltas = log_deltas(&records, "c1", "c3", None, true).expect("range should resolve");
    assert!(
        deltas.embedded_log_retention_caveat.is_none(),
        "an embedded store with no `ErrorSignature` records must not carry the retention caveat"
    );
}

/// End-to-end embedded path (issue #363): ingesting the combined graph into an
/// embedded store and querying it over `--data-dir` must carry the retention
/// caveat, while the same query over `--graph` must not.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_log_deltas_cli_embedded_data_dir_discloses_retention_caveat() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, _second, third] = seed_repo(&repo);
    let graph_path = temp.path().join("combined.graph.jsonl");

    // A history graph plus `ErrorSignature` nodes — no `FRAME_RESOLVES_TO` edge
    // onto a multi-snapshot symbol (which the stricter embedded ingest rejects).
    // The caveat only needs commits (for range resolution) plus at least one
    // signature, so this exercises the embedded read path faithfully.
    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let mut records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();
    records.push(error_signature("new-boom", "error", NEW_FIRST, NEW_LAST, 5));
    records.push(error_signature(
        "ceased-warn",
        "warn",
        CEASED_FIRST,
        CEASED_LAST,
        3,
    ));
    records.push(error_signature(
        "cont-error",
        "error",
        CONT_FIRST,
        CONT_LAST,
        9,
    ));
    let mut out = String::new();
    for r in &records {
        out.push_str(&serde_json::to_string(r).expect("record should serialize"));
        out.push('\n');
    }
    fs::write(&graph_path, out).expect("graph should write");

    let data_dir = temp.path().join("store");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let base_prefix = &first[..12];

    // `--data-dir`: the retention caveat is present because log records exist.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "log-deltas", base_prefix, &third])
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    let message = body["embedded_log_retention_caveat"]["message"]
        .as_str()
        .expect("the embedded --data-dir path must carry the retention caveat");
    assert!(message.contains("retain every superseded non-temporal log observation"));
    assert!(message.contains("#363"));

    // `--graph`: the same query must NOT carry the caveat.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "log-deltas", base_prefix, &third])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert!(
        body.get("embedded_log_retention_caveat").is_none(),
        "the --graph path must not carry the embedded retention caveat"
    );
}

/// Sorted `(relative path, bytes)` fingerprint of every file under `root`
/// (mirrors `tests/integration/manifest_deps.rs`).
#[cfg(feature = "embedded-aletheiadb")]
fn dir_fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let ft = entry.file_type().unwrap();
            let path = entry.path();
            if ft.is_dir() {
                walk(&path, base, out);
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// PR #356 review: the store-backed `log-deltas` query is strictly read-only.
/// Opening the embedded engine in place re-persists its on-disk index files, so
/// `--data-dir` must read from a throwaway copy and leave the live store
/// byte-for-byte untouched (same contract as the other read-only lanes).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_log_deltas_data_dir_is_strictly_read_only() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, _second, third] = seed_repo(&repo);
    let graph_path = temp.path().join("combined.graph.jsonl");

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let mut records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();
    records.push(error_signature("new-boom", "error", NEW_FIRST, NEW_LAST, 5));
    records.push(error_signature(
        "ceased-warn",
        "warn",
        CEASED_FIRST,
        CEASED_LAST,
        3,
    ));
    records.push(error_signature(
        "cont-error",
        "error",
        CONT_FIRST,
        CONT_LAST,
        9,
    ));
    let mut out = String::new();
    for r in &records {
        out.push_str(&serde_json::to_string(r).expect("record should serialize"));
        out.push('\n');
    }
    fs::write(&graph_path, out).expect("graph should write");

    let data_dir = temp.path().join("store");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let base_prefix = &first[..12];

    let before = dir_fingerprint(&data_dir);
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "log-deltas", base_prefix, &third])
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();
    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "query log-deltas must not modify any store file when reading --data-dir"
    );
}

#[test]
fn query_log_deltas_cli_is_deterministic_and_redaction_safe() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, _second, third] = seed_repo(&repo);
    let graph_path = temp.path().join("combined.graph.jsonl");

    let status_before = git_output(&repo, ["status", "--porcelain"]);
    assert!(status_before.is_empty(), "fixture tree must start clean");

    let tweaked_id = build_augmented_graph(&repo, &graph_path);

    let base_prefix = &first[..12];
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let assert = CargoCommand::cargo_bin("egregore")
            .expect("binary should run")
            .args(["query", "log-deltas", base_prefix, &third])
            .arg("--graph")
            .arg(&graph_path)
            .assert()
            .success();
        outputs.push(String::from_utf8(assert.get_output().stdout.clone()).unwrap());
    }
    for output in &outputs[1..] {
        assert_eq!(&outputs[0], output, "CLI output must be byte-identical");
    }

    let body: serde_json::Value = serde_json::from_str(&outputs[0]).expect("stdout should be JSON");
    assert_eq!(body["ok"], true);
    assert_eq!(body["base"], first);
    assert_eq!(body["head"], third);
    assert_eq!(body["window"]["window_start"], T2);
    assert_eq!(body["window"]["window_end"], T3);
    for group in [
        "new_signatures",
        "ceased_signatures",
        "continuing_signatures",
    ] {
        assert!(
            body[group].is_array(),
            "group {group} must always be present"
        );
    }
    // The new signature joins onto the modified `tweaked` symbol.
    let new_rows = body["new_signatures"].as_array().unwrap();
    assert_eq!(new_rows.len(), 1);
    let overlaps = new_rows[0]["overlapping_symbol_deltas"].as_array().unwrap();
    assert_eq!(overlaps.len(), 1);
    assert_eq!(overlaps[0]["record_id"], tweaked_id);
    assert_eq!(overlaps[0]["change_class"], "modified_symbol");
    assert_eq!(body["ceased_signatures"].as_array().unwrap().len(), 1);
    assert_eq!(body["continuing_signatures"].as_array().unwrap().len(), 1);

    // The `--graph` path preserves every ingested line, so it never carries the
    // embedded-store retention caveat (issue #363).
    assert!(
        body.get("embedded_log_retention_caveat").is_none(),
        "the --graph path must not carry the embedded retention caveat"
    );

    assert!(
        body["disclaimer"]
            .as_str()
            .unwrap()
            .contains("regression LEAD"),
        "disclaimer must label rows as leads"
    );
    // No raw log payload text leaks; only bounded excerpts / handles.
    assert!(
        !outputs[0].contains("Source:"),
        "raw snapshot bodies must never leak"
    );

    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert!(
        status_after.is_empty(),
        "CLI query must not mutate the tree"
    );
}

#[test]
fn query_log_deltas_cli_exit_codes_for_diagnostics() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, _second, third] = seed_repo(&repo);
    let graph_path = temp.path().join("combined.graph.jsonl");
    build_augmented_graph(&repo, &graph_path);

    // Identical endpoints: exit 1, machine-readable diagnostic.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "log-deltas", &first, &first])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["error_type"], "identical_endpoints");

    // Unknown commit: exit 2.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "log-deltas", "ffffffffffff", &third])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "missing_commit");

    // Reversed range: exit 1.
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "log-deltas", &third, &first])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);
}

// ---------------------------------------------------------------------------
// Git fixture helpers (mirrors tests/integration/range_deltas.rs).
// ---------------------------------------------------------------------------

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn commit_fixture(repo: &Path, message: &str, date: &str) -> String {
    git(repo, ["add", "."]);
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        status.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    git_output(repo, ["rev-parse", "HEAD"])
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}
