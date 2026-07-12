//! Integration tests for `eg query error-context` (issue #324): resolve an
//! `ErrorSignature` handle and assemble one deterministic, trust-separated
//! cross-domain envelope.
//!
//! Two layers, mirroring `tests/integration/log_deltas.rs`:
//!   * synthetic-record unit tests that build a `Vec<GraphRecord>` and call
//!     `aletheia_egregore::query::error_context(...)` directly, exercising
//!     handle resolution, trust separation, the log/history/protected sections,
//!     supersession, temporal selectors, redaction, and determinism;
//!   * a seeded end-to-end CLI byte-stability layer that writes a combined graph
//!     and drives the built `egregore` binary, asserting byte-identical output
//!     across five runs and the documented exit codes.

#![allow(missing_docs, clippy::similar_names)]

use std::{collections::BTreeMap, fs, path::Path};

use aletheia_egregore::{
    GraphRecord, LOG_SCHEMA_VERSION, NodeKind, SourceSpan, SupersessionMode, TemporalMetadata,
    ir::{
        CorrelationBasis, EdgeLabel, ErrorSignaturePayload, EvidenceLink, FrameResolution,
        LogOccurrenceBucketPayload, LogPayload, LogSourcePayload, OutputHandle, StackFrame,
    },
    log_stable_id,
    protected::{
        PROTECTED_HANDLE_PREFIX, PROTECTED_SCHEMA_VERSION, ProtectedHandle, ProtectedPayloadClass,
    },
    query::{ErrorContextError, FirstSeenRange, error_context},
    stable_id,
};
use assert_cmd::Command as CargoCommand;

const ANCHOR: &str = "codegraph:v1:repo_main";
const FINGERPRINT: &str = "template-v1";

// Commit timeline (committer dates) for the history-backed fixtures.
const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-01-02T00:00:00Z";
const T3: &str = "2026-01-03T00:00:00Z";
// A first_seen strictly between T2 and T3.
const SIG_FIRST: &str = "2026-01-02T12:00:00Z";
const SIG_LAST: &str = "2026-01-02T13:00:00Z";

// ---------------------------------------------------------------------------
// Fixture builders.
// ---------------------------------------------------------------------------

const fn span(start: usize, end: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 0,
        start_line: start,
        end_line: end,
    }
}

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
    GraphRecord::node(
        stable_id(&["node", "commit", "repo_test", sha]),
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("Commit {sha}"),
    )
    .with_temporal(temporal(sha, parents, valid_time))
}

fn error_signature(
    seed: &str,
    severity: &str,
    first_seen: &str,
    last_seen: &str,
    occurrence_count: u64,
    frames: Option<Vec<StackFrame>>,
) -> (String, GraphRecord) {
    let id = log_stable_id(&["error_signature", ANCHOR, FINGERPRINT, seed, severity]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::ErrorSignature,
        None,
        None,
        Some(format!("{severity} signature")),
        format!("error signature {seed}"),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
        fingerprint_algorithm: FINGERPRINT.to_owned(),
        template_excerpt: format!("template {seed}"),
        severity: severity.to_owned(),
        occurrence_count,
        first_seen: first_seen.to_owned(),
        last_seen: last_seen.to_owned(),
        frames,
    }))
    .with_valid_time(first_seen, "log_event_timestamp");
    (id, node)
}

fn log_source(anchor: &str, path: &str, hash: &str) -> (String, GraphRecord) {
    let id = log_stable_id(&["log_source", anchor, path, hash]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::LogSource,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        "log source".to_owned(),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::LogSource(LogSourcePayload {
        source_relative_path: path.to_owned(),
        source_format_version: "plain-v1".to_owned(),
        source_artifact_hash: hash.to_owned(),
        line_count: 1,
    }));
    (id, node)
}

fn captured_from(sig: &str, src: &str) -> GraphRecord {
    log_edge(EdgeLabel::CapturedFrom, sig, src, "captured from")
}

fn frame_resolves(sig: &str, target: &str, index: u32, resolution: FrameResolution) -> GraphRecord {
    GraphRecord::Edge {
        id: log_stable_id(&[
            "edge",
            "FRAME_RESOLVES_TO",
            sig,
            &index.to_string(),
            target,
            resolution.as_str(),
        ]),
        schema_version: LOG_SCHEMA_VERSION,
        label: EdgeLabel::FrameResolvesTo,
        source: sig.to_owned(),
        target: target.to_owned(),
        confidence: Some("1.0".to_owned()),
        resolution: None,
        frame_resolution: Some(resolution),
        frame_index: Some(index),
        basis: None,
        temporal: None,
        summary: format!("frame {index} of {sig} resolves to {target}"),
        producer: None,
    }
}

fn bucket_with_edge(sig: &str, bucket_start: &str, count: u64) -> (GraphRecord, GraphRecord) {
    let bucket_id = log_stable_id(&["log_occurrence_bucket", sig, bucket_start]);
    let node = GraphRecord::node(
        bucket_id.clone(),
        NodeKind::LogOccurrenceBucket,
        None,
        None,
        Some(format!("bucket {bucket_start}")),
        format!("occurrence bucket {bucket_start} x{count}"),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::LogOccurrenceBucket(
        LogOccurrenceBucketPayload {
            bucket_start: bucket_start.to_owned(),
            bucket_width: "1h".to_owned(),
            occurrence_count: count,
        },
    ))
    .with_valid_time(bucket_start, "log_event_timestamp");
    let edge = log_edge(EdgeLabel::Aggregates, &bucket_id, sig, "aggregates");
    (node, edge)
}

fn emitted_during(sig: &str, run: &str, basis: CorrelationBasis) -> GraphRecord {
    GraphRecord::Edge {
        id: log_stable_id(&["edge", "EMITTED_DURING", sig, run, basis.as_str()]),
        schema_version: LOG_SCHEMA_VERSION,
        label: EdgeLabel::EmittedDuring,
        source: sig.to_owned(),
        target: run.to_owned(),
        confidence: Some(basis.confidence().to_owned()),
        resolution: None,
        frame_resolution: None,
        frame_index: None,
        basis: Some(basis),
        temporal: None,
        summary: format!("{sig} emitted during {run}"),
        producer: None,
    }
}

fn references_task(sig: &str, task: &str) -> GraphRecord {
    log_edge(EdgeLabel::ReferencesTask, sig, task, "references task")
}

fn log_edge(label: EdgeLabel, source: &str, target: &str, summary: &str) -> GraphRecord {
    GraphRecord::Edge {
        id: log_stable_id(&["edge", label.as_str(), source, target]),
        schema_version: LOG_SCHEMA_VERSION,
        label,
        source: source.to_owned(),
        target: target.to_owned(),
        confidence: None,
        resolution: None,
        frame_resolution: None,
        frame_index: None,
        basis: None,
        temporal: None,
        summary: summary.to_owned(),
        producer: None,
    }
}

fn command_run(id: &str, hash: &str) -> (String, GraphRecord) {
    let rid = format!("verification:v1:{id}");
    let mut node = GraphRecord::node(
        rid.clone(),
        NodeKind::CommandRun,
        None,
        None,
        Some("cmd".to_owned()),
        format!("command run {id} SECRET_COMMAND_OUTPUT_MARKER"),
    );
    if let GraphRecord::Node { stderr_handle, .. } = &mut node {
        *stderr_handle = Some(Box::new(OutputHandle {
            inline: None,
            hash: hash.to_owned(),
            bytes: 10,
        }));
    }
    (rid, node.with_domain("verification", 1))
}

fn agent_run(id: &str, start: &str, end: &str) -> (String, GraphRecord) {
    let rid = format!("agent_memory:v1:{id}");
    let mut node = GraphRecord::node(
        rid.clone(),
        NodeKind::AgentRun,
        None,
        None,
        Some("run".to_owned()),
        format!("agent run {id} SECRET_TRANSCRIPT_MARKER"),
    );
    if let GraphRecord::Node {
        started_at,
        finished_at,
        ..
    } = &mut node
    {
        *started_at = Some(start.to_owned());
        *finished_at = Some(end.to_owned());
    }
    (rid, node.with_domain("agent_memory", 1))
}

fn task(id: &str) -> (String, GraphRecord) {
    let rid = format!("project:v1:{id}");
    let node = GraphRecord::node(
        rid.clone(),
        NodeKind::Task,
        None,
        None,
        Some("task".to_owned()),
        format!("task {id}"),
    )
    .with_domain("project", 1);
    (rid, node)
}

fn code_symbol(name: &str, path: &str, start: usize, end: usize) -> (String, GraphRecord) {
    let id = stable_id(&["node", "symbol", path, name]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(span(start, end)),
        Some(name.to_owned()),
        format!("symbol {name}"),
    );
    (id, node)
}

fn symbol_snapshot(
    name: &str,
    path: &str,
    start: usize,
    end: usize,
    commit_sha: &str,
    valid_time: &str,
) -> (String, GraphRecord) {
    let id = stable_id(&["node", "symbol", path, name]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(span(start, end)),
        Some(name.to_owned()),
        format!("symbol {name}@{commit_sha}"),
    )
    .with_temporal(temporal(commit_sha, &[], valid_time));
    (id, node)
}

fn code_file(path: &str) -> (String, GraphRecord) {
    let id = stable_id(&["node", "file", path]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::File,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        format!("file {path}"),
    );
    (id, node)
}

fn defines(file_id: &str, symbol_id: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.to_owned(),
        symbol_id.to_owned(),
        None,
        "defines".to_owned(),
    )
}

fn observation(seed: &str, links: Vec<EvidenceLink>) -> (String, GraphRecord) {
    let id = format!("agent_memory:v1:obs_{seed}");
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::Observation,
        None,
        None,
        Some(format!("observation {seed}")),
        format!("observation {seed} SECRET_OBSERVATION_MARKER"),
    )
    .with_domain("agent_memory", 1)
    .with_evidence_links(links);
    (id, node)
}

fn observes_edge(obs_id: &str, symbol_id: &str) -> GraphRecord {
    GraphRecord::agent_memory_edge(
        EdgeLabel::Observes,
        obs_id.to_owned(),
        symbol_id.to_owned(),
        None,
        "observes".to_owned(),
    )
}

fn evidence_link(relation: &str, target_record_id: &str, target_domain: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target_record_id.to_owned()),
        target_domain: target_domain.to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

// ---------------------------------------------------------------------------
// Handle resolution.
// ---------------------------------------------------------------------------

#[test]
fn resolves_exact_signature_record_id() {
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 3, None);
    let records = vec![sig];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("exact record ID must resolve");
    assert_eq!(ctx.signature_ids, vec![sig_id.clone()]);
    assert_eq!(ctx.signatures.len(), 1);
    assert_eq!(ctx.signatures[0].record_id, sig_id);
}

#[test]
fn well_formed_absent_signature_id_is_no_match() {
    let (_sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 3, None);
    let (src_id, src) = log_source(ANCHOR, "app.log", "deadbeef");
    let records = vec![sig, src];
    // A well-formed but absent signature ID.
    let absent = log_stable_id(&["error_signature", ANCHOR, FINGERPRINT, "ghost", "error"]);
    match error_context(
        &records,
        &absent,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    ) {
        Err(ErrorContextError::NoMatch { handle }) => assert_eq!(handle, absent),
        other => panic!("expected NoMatch, got {other:?}"),
    }
    // A LogSource ID (well-formed log:v1: handle, wrong kind) is also no_match.
    match error_context(
        &records,
        &src_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    ) {
        Err(ErrorContextError::NoMatch { handle }) => assert_eq!(handle, src_id),
        other => panic!("expected NoMatch for LogSource ID, got {other:?}"),
    }
}

#[test]
fn resolves_unique_fingerprint_prefix() {
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 3, None);
    let records = vec![sig];
    let hex = sig_id.strip_prefix("log:v1:").unwrap();
    let prefix = &hex[..12]; // long enough to be unique
    let ctx = error_context(
        &records,
        prefix,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("unique fingerprint prefix must resolve");
    assert_eq!(ctx.signature_ids, vec![sig_id]);
}

/// Builds N signatures and returns the records, a single-hex-char prefix shared
/// by >=2 of them, and the sorted candidate IDs starting with that prefix.
fn ambiguous_prefix_fixture() -> (Vec<GraphRecord>, String, Vec<String>) {
    let mut records = Vec::new();
    let mut ids = Vec::new();
    // 20 signatures over 16 possible first hex digits guarantee a collision.
    for i in 0..20 {
        let seed = format!("amb-{i}");
        let (id, node) = error_signature(&seed, "error", SIG_FIRST, SIG_LAST, 1, None);
        ids.push(id);
        records.push(node);
    }
    let mut by_first: BTreeMap<char, Vec<String>> = BTreeMap::new();
    for id in &ids {
        let hex = id.strip_prefix("log:v1:").unwrap();
        let first = hex.chars().next().unwrap();
        by_first.entry(first).or_default().push(id.clone());
    }
    let (first, group) = by_first
        .iter()
        .find(|(_, g)| g.len() >= 2)
        .expect("20 signatures over 16 hex digits must collide on a first digit");
    let mut candidates: Vec<String> = ids
        .iter()
        .filter(|id| id.strip_prefix("log:v1:").unwrap().starts_with(*first))
        .cloned()
        .collect();
    candidates.sort();
    let _ = group;
    (records, first.to_string(), candidates)
}

#[test]
fn ambiguous_fingerprint_prefix_lists_candidates_exit_1() {
    let (records, prefix, expected) = ambiguous_prefix_fixture();
    match error_context(
        &records,
        &prefix,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    ) {
        Err(ErrorContextError::Ambiguous { mut candidates }) => {
            candidates.sort();
            assert!(candidates.len() >= 2);
            assert_eq!(candidates, expected);
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[test]
fn resolves_symbol_name_via_frame_targets() {
    let (sym_id, sym) = code_symbol("boom_handler", "src/lib.rs", 1, 10);
    let (sig_a, sig_a_node) = error_signature("a", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (sig_b, sig_b_node) = error_signature("b", "error", SIG_FIRST, SIG_LAST, 1, None);
    let records = vec![
        sym,
        sig_a_node,
        sig_b_node,
        frame_resolves(&sig_a, &sym_id, 0, FrameResolution::Resolved),
        frame_resolves(&sig_b, &sym_id, 0, FrameResolution::Resolved),
    ];
    let ctx = error_context(
        &records,
        "boom_handler",
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("symbol name naming two signatures resolves both, not ambiguous");
    let mut expected = vec![sig_a, sig_b];
    expected.sort();
    assert_eq!(ctx.signature_ids, expected);
}

#[test]
fn unknown_handle_is_no_match_exit_2() {
    let (_sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let records = vec![sig];
    match error_context(
        &records,
        "no_such_symbol_or_prefix_zzz",
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    ) {
        Err(ErrorContextError::NoMatch { handle }) => {
            assert_eq!(handle, "no_such_symbol_or_prefix_zzz");
        }
        other => panic!("expected NoMatch, got {other:?}"),
    }
}

#[test]
fn symbol_name_with_underscore_never_hits_fingerprint_mode() {
    // A non-hex name must skip prefix mode entirely and resolve via frames.
    let (sym_id, sym) = code_symbol("parse_input", "src/lib.rs", 1, 10);
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let records = vec![
        sym,
        sig,
        frame_resolves(&sig_id, &sym_id, 0, FrameResolution::Resolved),
    ];
    let ctx = error_context(
        &records,
        "parse_input",
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("underscore name resolves via frame targets");
    assert_eq!(ctx.signature_ids, vec![sig_id]);
}

// ---------------------------------------------------------------------------
// Section population & trust separation.
// ---------------------------------------------------------------------------

#[test]
fn signature_block_carries_severity_occurrence_template_excerpt() {
    let (sig_id, sig) = error_signature("boom", "fatal", SIG_FIRST, SIG_LAST, 42, None);
    let records = vec![sig];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    let block = &ctx.signatures[0];
    assert_eq!(block.severity, "fatal");
    assert_eq!(block.occurrence_count, 42);
    assert_eq!(block.fingerprint_algorithm, FINGERPRINT);
    assert_eq!(block.template_excerpt, "template boom");
    assert_eq!(block.trust_class, "runtime_observation");
    assert_eq!(block.first_seen, SIG_FIRST);
    assert_eq!(block.last_seen, SIG_LAST);
}

#[test]
fn buckets_populated_via_aggregates() {
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 8, None);
    let (b1n, b1e) = bucket_with_edge(&sig_id, "2026-01-02T13:00:00Z", 3);
    let (b2n, b2e) = bucket_with_edge(&sig_id, "2026-01-02T12:00:00Z", 5);
    let records = vec![sig, b1n, b1e, b2n, b2e];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    let buckets = &ctx.signatures[0].buckets;
    assert_eq!(buckets.len(), 2);
    // Sorted by (bucket_start, record_id).
    assert_eq!(buckets[0].bucket_start, "2026-01-02T12:00:00Z");
    assert_eq!(buckets[0].occurrence_count, 5);
    assert_eq!(buckets[1].bucket_start, "2026-01-02T13:00:00Z");
    assert_eq!(buckets[1].occurrence_count, 3);
}

#[test]
fn frames_populated_via_frame_resolves_to_with_labels() {
    let (sym_id, sym) = code_symbol("boom_handler", "src/lib.rs", 1, 10);
    let (file_id, file) = code_file("src/lib.rs");
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let diag_id = "log:v1:diagnostic_unresolved_frame".to_owned();
    let diag = GraphRecord::node(
        diag_id.clone(),
        NodeKind::Diagnostic,
        Some("src/gone.rs".to_owned()),
        None,
        Some("gone".to_owned()),
        "unresolved frame".to_owned(),
    )
    .with_domain("log", LOG_SCHEMA_VERSION);
    let records = vec![
        sym,
        file,
        defines(&file_id, &sym_id),
        sig,
        diag,
        frame_resolves(&sig_id, &sym_id, 0, FrameResolution::Resolved),
        frame_resolves(&sig_id, &file_id, 1, FrameResolution::PathOnly),
        frame_resolves(&sig_id, &diag_id, 2, FrameResolution::Unresolved),
    ];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    let frames = &ctx.signatures[0].frames;
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].frame_resolution, "resolved");
    assert_eq!(frames[1].frame_resolution, "path_only");
    assert_eq!(frames[2].frame_resolution, "unresolved");
    // The unresolved (Diagnostic) target never seeds source_facts.
    assert!(
        ctx.source_facts.iter().all(|r| r.record_id != diag_id),
        "unresolved Diagnostic target must not appear in source_facts"
    );
}

#[test]
fn frame_targets_seed_source_facts() {
    let (sym_id, sym) = code_symbol("boom_handler", "src/lib.rs", 1, 10);
    let (file_id, file) = code_file("src/lib.rs");
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let records = vec![
        sym,
        file,
        defines(&file_id, &sym_id),
        sig,
        frame_resolves(&sig_id, &sym_id, 0, FrameResolution::Resolved),
    ];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    assert!(
        ctx.source_facts.iter().any(|r| r.record_id == sym_id),
        "resolved frame target Symbol must land in source_facts"
    );
    assert!(
        ctx.source_facts.iter().any(|r| r.record_id == file_id),
        "co-located File must land in source_facts"
    );
    assert!(
        ctx.source_facts
            .iter()
            .all(|r| r.trust_class == "source_fact"),
        "every source_facts row carries the source_fact trust class"
    );
}

#[test]
fn observations_via_emitted_during_carry_basis() {
    let hash = "deadbeefhash";
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    let (run_a, run_a_node) = agent_run("run_a", "2026-01-02T11:00:00Z", "2026-01-02T13:00:00Z");
    let (run_b, run_b_node) = agent_run("run_b", "2026-01-02T11:30:00Z", "2026-01-02T12:30:00Z");
    let (cmd_id, cmd_node) = command_run("cmd1", hash);
    let records = vec![
        sig,
        src,
        captured_from(&sig_id, &src_id),
        run_a_node,
        run_b_node,
        cmd_node,
        emitted_during(&sig_id, &run_a, CorrelationBasis::TemporalCorrelation),
        emitted_during(&sig_id, &run_b, CorrelationBasis::TemporalCorrelation),
        emitted_during(&sig_id, &cmd_id, CorrelationBasis::ContentHashJoin),
    ];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    // Both overlapping agent runs get an observation row (no silent winner).
    let obs_a = ctx
        .observations
        .iter()
        .find(|r| r.record_id == run_a)
        .unwrap();
    let obs_b = ctx
        .observations
        .iter()
        .find(|r| r.record_id == run_b)
        .unwrap();
    assert_eq!(
        obs_a.correlation_basis.as_deref(),
        Some("temporal_correlation")
    );
    assert_eq!(
        obs_b.correlation_basis.as_deref(),
        Some("temporal_correlation")
    );
    // The content-hash-joined CommandRun lands in verification with its basis.
    let ver = ctx
        .verification_evidence
        .iter()
        .find(|r| r.record_id == cmd_id)
        .unwrap();
    assert_eq!(ver.correlation_basis.as_deref(), Some("content_hash_join"));
}

#[test]
fn project_state_via_references_task() {
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (task_id, task_node) = task("t1");
    let records = vec![sig, task_node, references_task(&sig_id, &task_id)];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    assert!(
        ctx.project_state.iter().any(|r| r.record_id == task_id),
        "REFERENCES_TASK target must land in project_state"
    );
}

#[test]
fn no_runtime_observation_rows_leak_into_source_facts() {
    let (sym_id, sym) = code_symbol("boom_handler", "src/lib.rs", 1, 10);
    let (file_id, file) = code_file("src/lib.rs");
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (run_id, run_node) = agent_run("run_a", "2026-01-02T11:00:00Z", "2026-01-02T13:00:00Z");
    let records = vec![
        sym,
        file,
        defines(&file_id, &sym_id),
        sig,
        run_node,
        frame_resolves(&sig_id, &sym_id, 0, FrameResolution::Resolved),
        emitted_during(&sig_id, &run_id, CorrelationBasis::TemporalCorrelation),
    ];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    assert!(
        ctx.source_facts
            .iter()
            .all(|r| r.trust_class == "source_fact"),
        "no runtime_observation/agent rows may leak into source_facts"
    );
    assert!(
        ctx.source_facts.iter().all(|r| r.record_id != sig_id),
        "the signature never appears in source_facts"
    );
    // The signature/buckets/frames live only in the signatures section.
    assert!(ctx.signatures.iter().any(|b| b.record_id == sig_id));
}

#[test]
fn unresolved_section_surfaces_absent_evidence_targets() {
    let (sym_id, sym) = code_symbol("boom_handler", "src/lib.rs", 1, 10);
    let (file_id, file) = code_file("src/lib.rs");
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let missing = "verification:v1:missing_run".to_owned();
    let (obs_id, obs) = observation(
        "o1",
        vec![
            evidence_link("OBSERVES", &sym_id, "codegraph"),
            evidence_link("VALIDATED_BY", &missing, "verification"),
        ],
    );
    let records = vec![
        sym,
        file,
        defines(&file_id, &sym_id),
        sig,
        obs,
        observes_edge(&obs_id, &sym_id),
        frame_resolves(&sig_id, &sym_id, 0, FrameResolution::Resolved),
    ];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    assert!(
        ctx.unresolved.iter().any(|u| u.target_handle == missing),
        "an evidence link to a missing record must surface in unresolved"
    );
}

// ---------------------------------------------------------------------------
// first_seen_range.
// ---------------------------------------------------------------------------

fn history_fixture() -> (Vec<GraphRecord>, String, String) {
    let (file_id, file) = code_file("src/lib.rs");
    let (sym_id, _) = symbol_snapshot("tweaked", "src/lib.rs", 1, 10, "c1sha0000", T1);
    let s1 = symbol_snapshot("tweaked", "src/lib.rs", 1, 10, "c1sha0000", T1).1;
    let s2 = symbol_snapshot("tweaked", "src/lib.rs", 1, 12, "c2sha0000", T2).1;
    let s3 = symbol_snapshot("tweaked", "src/lib.rs", 1, 14, "c3sha0000", T3).1;
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        file,
        s1,
        s2,
        s3,
        sig,
        frame_resolves(&sig_id, &sym_id, 0, FrameResolution::Resolved),
    ];
    let _ = file_id;
    (records, sig_id, sym_id)
}

#[test]
fn history_backed_graph_yields_narrowest_window() {
    let (records, sig_id, sym_id) = history_fixture();
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    match ctx.first_seen_range {
        FirstSeenRange::History(window) => {
            assert_eq!(window.base_commit.as_deref(), Some("c2sha0000"));
            assert_eq!(window.head_commit.as_deref(), Some("c3sha0000"));
            assert_eq!(window.window_start.as_deref(), Some(T2));
            assert_eq!(window.window_end.as_deref(), Some(T3));
            // `tweaked` is modified between c2 and c3 and is the frame target.
            assert_eq!(window.overlapping_symbol_deltas.len(), 1);
            assert_eq!(window.overlapping_symbol_deltas[0].record_id, sym_id);
            assert_eq!(
                window.overlapping_symbol_deltas[0].change_class,
                "modified_symbol"
            );
        }
        FirstSeenRange::Unavailable { .. } => panic!("expected a history-backed window"),
    }
}

#[test]
fn plain_scan_graph_reports_history_unavailable() {
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let records = vec![sig];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    match ctx.first_seen_range {
        FirstSeenRange::Unavailable { diagnostic } => assert_eq!(diagnostic, "history_unavailable"),
        FirstSeenRange::History(_) => panic!("a plain scan graph must never fabricate a window"),
    }
}

#[test]
fn first_seen_before_all_commits_gives_partial_window() {
    let (sig_id, sig) = error_signature("boom", "error", "2025-01-01T00:00:00Z", SIG_LAST, 1, None);
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        sig,
    ];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    match ctx.first_seen_range {
        FirstSeenRange::History(window) => {
            assert!(
                window.base_commit.is_none(),
                "first_seen precedes all commits"
            );
            assert_eq!(window.head_commit.as_deref(), Some("c1sha0000"));
        }
        FirstSeenRange::Unavailable { .. } => panic!("expected a partial history window"),
    }
}

// ---------------------------------------------------------------------------
// Protected store.
// ---------------------------------------------------------------------------

fn write_manifest(store: &Path, handles: &[ProtectedHandle]) {
    fs::create_dir_all(store).unwrap();
    let mut out = String::new();
    for h in handles {
        out.push_str(&serde_json::to_string(h).unwrap());
        out.push('\n');
    }
    fs::write(store.join("manifest.jsonl"), out).unwrap();
}

fn protected_handle(content_hash: &str, byte_len: u64) -> ProtectedHandle {
    ProtectedHandle {
        handle: ProtectedHandle::compute_handle(
            &ProtectedPayloadClass::LogPayload,
            content_hash,
            Some("app.log"),
        ),
        schema_version: PROTECTED_SCHEMA_VERSION,
        source_class: ProtectedPayloadClass::LogPayload,
        source_path: Some("app.log".to_owned()),
        content_hash: content_hash.to_owned(),
        byte_len,
        captured_at: "2026-01-02T00:00:00Z".to_owned(),
        producer_id: "op-1".to_owned(),
        producer_version: "test".to_owned(),
    }
}

#[test]
fn protected_off_lists_content_hashes_only() {
    let hash = "deadbeefhash";
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    let records = vec![sig, src, captured_from(&sig_id, &src_id)];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    assert!(ctx.protected_payloads.is_none());
    let handles = &ctx.signatures[0].source_handles;
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].source_artifact_hash, hash);
}

#[test]
fn protected_on_matches_source_artifact_hash_to_handle() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("protected");
    let hash = "deadbeefhash";
    write_manifest(&store, &[protected_handle(hash, 4096)]);

    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    let records = vec![sig, src, captured_from(&sig_id, &src_id)];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        Some(&store),
    )
    .expect("resolve");
    let payloads = ctx.protected_payloads.expect("flag on yields Some");
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].source_artifact_hash, hash);
    assert!(payloads[0].handle.starts_with(PROTECTED_HANDLE_PREFIX));
    assert_eq!(payloads[0].source_class, "log_payload");
    assert_eq!(payloads[0].byte_len, 4096);
    // Raw bytes are never read: no blobs directory is ever touched.
    assert!(
        !store.join("blobs").exists(),
        "protected read-time join must never touch blobs/"
    );
}

#[test]
fn protected_on_no_match_yields_empty_list() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("protected");
    write_manifest(&store, &[protected_handle("some_other_hash", 10)]);

    let hash = "deadbeefhash";
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    let records = vec![sig, src, captured_from(&sig_id, &src_id)];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        Some(&store),
    )
    .expect("resolve");
    let payloads = ctx.protected_payloads.expect("flag on yields Some");
    assert!(payloads.is_empty());
}

#[test]
fn graph_with_protected_handle_rejected_exit_1() {
    let hash = "deadbeefhash";
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    // A stray protected handle in the graph (an evidence link into the protected
    // domain) must be rejected when the flag is set.
    let (obs_id, obs) = observation(
        "leak",
        vec![EvidenceLink {
            target_record_id: Some(format!("{PROTECTED_HANDLE_PREFIX}abc123")),
            target_domain: "protected".to_owned(),
            relation: "HAS_EVIDENCE".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }],
    );
    let _ = obs_id;
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("protected");
    write_manifest(&store, &[protected_handle(hash, 10)]);
    let records = vec![sig, src, captured_from(&sig_id, &src_id), obs];
    match error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        Some(&store),
    ) {
        Err(ErrorContextError::ProtectedHandleInGraph) => {}
        other => panic!("expected ProtectedHandleInGraph, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Supersession.
// ---------------------------------------------------------------------------

fn supersession_fixture() -> (Vec<GraphRecord>, String, String) {
    let (sym_id, sym) = code_symbol("boom_handler", "src/lib.rs", 1, 10);
    let (file_id, file) = code_file("src/lib.rs");
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    // obs_old is superseded by obs_new.
    let (obs_new_id, obs_new) =
        observation("new", vec![evidence_link("OBSERVES", &sym_id, "codegraph")]);
    let (obs_old_id, obs_old) =
        observation("old", vec![evidence_link("OBSERVES", &sym_id, "codegraph")]);
    let supersedes = GraphRecord::agent_memory_edge(
        EdgeLabel::Supersedes,
        obs_new_id.clone(),
        obs_old_id.clone(),
        None,
        "supersedes".to_owned(),
    );
    let records = vec![
        sym,
        file,
        defines(&file_id, &sym_id),
        sig,
        obs_new,
        obs_old,
        observes_edge(&obs_new_id, &sym_id),
        observes_edge(&obs_old_id, &sym_id),
        supersedes,
        frame_resolves(&sig_id, &sym_id, 0, FrameResolution::Resolved),
    ];
    (records, sig_id, obs_old_id)
}

#[test]
fn supersession_exclude_drops_and_reports_in_excluded() {
    let (records, sig_id, obs_old_id) = supersession_fixture();
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    assert!(
        ctx.observations.iter().all(|r| r.record_id != obs_old_id),
        "a superseded observation is dropped under Exclude"
    );
    let excluded = ctx
        .excluded
        .iter()
        .find(|e| e.record_id == obs_old_id)
        .expect("the superseded row must appear in excluded");
    assert_eq!(excluded.reason, "superseded");
    assert!(!excluded.superseded_by.is_empty());
}

#[test]
fn supersession_include_but_flag_keeps_and_flags() {
    let (records, sig_id, obs_old_id) = supersession_fixture();
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::IncludeButFlag,
        None,
    )
    .expect("resolve");
    let row = ctx
        .observations
        .iter()
        .find(|r| r.record_id == obs_old_id)
        .expect("include-but-flag keeps the superseded row");
    assert_eq!(row.supersession_status.as_deref(), Some("superseded"));
    assert!(!row.superseded_by.is_empty());
    assert!(
        ctx.excluded.iter().all(|e| e.record_id != obs_old_id),
        "include-but-flag leaves excluded empty for the flagged row"
    );
}

// ---------------------------------------------------------------------------
// Temporal selectors.
// ---------------------------------------------------------------------------

#[test]
fn at_commit_reresolves_frames_against_commit_view() {
    // A frame at src/lib.rs:5. At c1 the symbol `alpha` occupies lines 1-10; at
    // c2 the symbol `beta` occupies the same span. `--at` selects which one the
    // frame resolves to.
    let frames = Some(vec![StackFrame {
        frame_index: 0,
        module_path: None,
        file_path: Some("src/lib.rs".to_owned()),
        line: Some(5),
    }]);
    let (alpha_id, alpha) = symbol_snapshot("alpha", "src/lib.rs", 1, 10, "c1sha0000", T1);
    let (beta_id, beta) = symbol_snapshot("beta", "src/lib.rs", 1, 10, "c2sha0000", T2);
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, frames);
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        alpha,
        beta,
        sig,
    ];

    let at_c1 = error_context(
        &records,
        &sig_id,
        None,
        Some("c1sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve at c1");
    assert_eq!(at_c1.signatures[0].frames.len(), 1);
    assert_eq!(at_c1.signatures[0].frames[0].target_record_id, alpha_id);

    let at_c2 = error_context(
        &records,
        &sig_id,
        None,
        Some("c2sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve at c2");
    assert_eq!(at_c2.signatures[0].frames[0].target_record_id, beta_id);
}

#[test]
fn as_of_bounds_occurrence_view() {
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 8, None);
    let (early_n, early_e) = bucket_with_edge(&sig_id, "2026-01-02T12:00:00Z", 5);
    let (late_n, late_e) = bucket_with_edge(&sig_id, "2026-01-02T18:00:00Z", 3);
    let records = vec![sig, early_n, early_e, late_n, late_e];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        Some("2026-01-02T13:00:00Z"),
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    let buckets = &ctx.signatures[0].buckets;
    assert_eq!(buckets.len(), 1, "buckets after --as-of are dropped");
    assert_eq!(buckets[0].bucket_start, "2026-01-02T12:00:00Z");
}

#[test]
fn at_and_as_of_together_unsupported_combination_exit_1() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("g.jsonl");
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    write_graph(&[commit("c1sha0000", &[], T1), sig], &graph);
    let assert = CargoCommand::cargo_bin("egregore")
        .unwrap()
        .args(["query", "error-context", &sig_id])
        .args(["--at", "c1sha0000", "--as-of", "2026-01-02T00:00:00Z"])
        .arg("--graph")
        .arg(&graph)
        .assert()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(body["error"]["code"], "unsupported_combination");
}

// ---------------------------------------------------------------------------
// Redaction & determinism.
// ---------------------------------------------------------------------------

fn full_fixture() -> (Vec<GraphRecord>, String) {
    let hash = "deadbeefhash";
    let (sym_id, sym) = symbol_snapshot("tweaked", "src/lib.rs", 1, 10, "c1sha0000", T1);
    let s2 = symbol_snapshot("tweaked", "src/lib.rs", 1, 12, "c2sha0000", T2).1;
    let s3 = symbol_snapshot("tweaked", "src/lib.rs", 1, 14, "c3sha0000", T3).1;
    let (file_id, file) = code_file("src/lib.rs");
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 5, None);
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    let (run_id, run_node) = agent_run("run_a", "2026-01-02T11:00:00Z", "2026-01-02T13:00:00Z");
    let (cmd_id, cmd_node) = command_run("cmd1", hash);
    let (task_id, task_node) = task("t1");
    let (bnode, bedge) = bucket_with_edge(&sig_id, "2026-01-02T12:00:00Z", 5);
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        file,
        sym,
        s2,
        s3,
        defines(&file_id, &sym_id),
        sig,
        src,
        captured_from(&sig_id, &src_id),
        run_node,
        cmd_node,
        task_node,
        bnode,
        bedge,
        frame_resolves(&sig_id, &sym_id, 0, FrameResolution::Resolved),
        emitted_during(&sig_id, &run_id, CorrelationBasis::TemporalCorrelation),
        emitted_during(&sig_id, &cmd_id, CorrelationBasis::ContentHashJoin),
        references_task(&sig_id, &task_id),
    ];
    (records, sig_id)
}

#[test]
fn no_raw_payload_text_in_envelope() {
    let (records, sig_id) = full_fixture();
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    let json = serde_json::to_string(&ctx).unwrap();
    for marker in [
        "SECRET_COMMAND_OUTPUT_MARKER",
        "SECRET_TRANSCRIPT_MARKER",
        "SECRET_OBSERVATION_MARKER",
        "\"summary\"",
    ] {
        assert!(
            !json.contains(marker),
            "envelope must not leak `{marker}`: {json}"
        );
    }
    // The only free text is the bounded template excerpt.
    assert!(json.contains("template boom"));
}

#[test]
fn disclaimer_always_present_and_fixed() {
    // Even an empty-section resolution carries the fixed disclaimer.
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let records = vec![sig];
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    assert_eq!(
        ctx.disclaimer,
        aletheia_egregore::query::ERROR_CONTEXT_DISCLAIMER
    );
    assert!(ctx.disclaimer.contains("CORRELATION LEADS"));
    assert!(ctx.disclaimer.contains("never proof of cause"));
}

#[test]
fn byte_identical_across_5_runs() {
    let (records, sig_id) = full_fixture();
    let baseline = serde_json::to_string(
        &error_context(
            &records,
            &sig_id,
            None,
            None,
            None,
            SupersessionMode::Exclude,
            None,
        )
        .expect("resolve"),
    )
    .unwrap();
    for _ in 0..4 {
        let again = serde_json::to_string(
            &error_context(
                &records,
                &sig_id,
                None,
                None,
                None,
                SupersessionMode::Exclude,
                None,
            )
            .expect("resolve"),
        )
        .unwrap();
        assert_eq!(
            baseline, again,
            "envelope must be byte-identical across runs"
        );
    }
}

#[test]
fn sections_sorted_by_record_id() {
    let (records, sig_id) = full_fixture();
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
    )
    .expect("resolve");
    for section in [
        &ctx.source_facts,
        &ctx.observations,
        &ctx.project_state,
        &ctx.artifacts,
        &ctx.verification_evidence,
    ] {
        let ids: Vec<&str> = section.iter().map(|r| r.record_id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "each section must be sorted by record_id");
    }
    let sig_ids = &ctx
        .signatures
        .iter()
        .map(|b| b.record_id.clone())
        .collect::<Vec<_>>();
    let mut sorted = sig_ids.clone();
    sorted.sort();
    assert_eq!(sig_ids, &sorted);
}

// ---------------------------------------------------------------------------
// Seeded end-to-end CLI byte-stability layer.
// ---------------------------------------------------------------------------

fn write_graph(records: &[GraphRecord], path: &Path) {
    let mut out = String::new();
    for r in records {
        out.push_str(&serde_json::to_string(r).unwrap());
        out.push('\n');
    }
    fs::write(path, out).unwrap();
}

#[test]
fn cli_byte_stability_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("seed.jsonl");
    let (records, sig_id) = full_fixture();
    write_graph(&records, &graph);

    // Five runs must be byte-identical and exit 0.
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let assert = CargoCommand::cargo_bin("egregore")
            .unwrap()
            .args(["query", "error-context", &sig_id])
            .arg("--graph")
            .arg(&graph)
            .assert()
            .success();
        outputs.push(String::from_utf8(assert.get_output().stdout.clone()).unwrap());
    }
    for output in &outputs[1..] {
        assert_eq!(&outputs[0], output, "CLI output must be byte-identical");
    }
    let body: serde_json::Value = serde_json::from_str(&outputs[0]).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["signature_ids"][0], sig_id);
    assert!(
        body["disclaimer"]
            .as_str()
            .unwrap()
            .contains("CORRELATION LEADS")
    );

    // Ambiguous prefix → exit 1.
    let (amb_records, prefix, _) = ambiguous_prefix_fixture();
    let amb_graph = temp.path().join("amb.jsonl");
    write_graph(&amb_records, &amb_graph);
    let assert = CargoCommand::cargo_bin("egregore")
        .unwrap()
        .args(["query", "error-context", &prefix])
        .arg("--graph")
        .arg(&amb_graph)
        .assert()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(body["error"]["code"], "ambiguous");

    // Unknown handle → exit 2 with a no_match envelope on stdout.
    let assert = CargoCommand::cargo_bin("egregore")
        .unwrap()
        .args(["query", "error-context", "no_such_handle_zzz"])
        .arg("--graph")
        .arg(&graph)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(body["error"]["code"], "no_match");
    assert_eq!(body["error"]["handle"], "no_such_handle_zzz");
}
