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

fn repo_node(seed: &str) -> (String, GraphRecord) {
    let id = stable_id(&["node", "Repository", seed]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some(seed.to_owned()),
        format!("Repository {seed}"),
    );
    (id, node)
}

fn commit_in(repo_seed: &str, sha: &str, valid_time: &str) -> (String, GraphRecord) {
    let id = stable_id(&["node", "commit", repo_seed, sha]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("Commit {sha}"),
    )
    .with_temporal(temporal(sha, &[], valid_time));
    (id, node)
}

fn contains(parent: &str, child: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Contains,
        parent.to_owned(),
        child.to_owned(),
        None,
        "contains".to_owned(),
    )
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
fn source_facts_preserve_all_temporal_versions() {
    // The frame-target symbol `tweaked` is modified across c1/c2/c3 under ONE
    // stable ID. `record_context` returns every temporal version; error-context
    // must not re-collapse them to a single (lexically-largest-git_commit) row.
    let (records, sig_id, sym_id) = history_fixture();
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve");
    let commits: Vec<&str> = ctx
        .source_facts
        .iter()
        .filter(|r| r.record_id == sym_id)
        .filter_map(|r| r.git_commit.as_deref())
        .collect();
    assert_eq!(
        commits.len(),
        3,
        "all three temporal versions of the frame target survive, got {commits:?}"
    );
    assert!(commits.contains(&"c1sha0000"));
    assert!(commits.contains(&"c2sha0000"));
    assert!(commits.contains(&"c3sha0000"));
}

// A frame-target symbol with the SAME stable ID modified across three commits,
// resolved by a StackFrame so `--at` re-resolution works at the commit view.
fn at_source_facts_fixture() -> (Vec<GraphRecord>, String, String) {
    let frames = Some(vec![StackFrame {
        frame_index: 0,
        module_path: None,
        file_path: Some("src/lib.rs".to_owned()),
        line: Some(5),
    }]);
    let (sym_id, _) = symbol_snapshot("tweaked", "src/lib.rs", 1, 10, "c1sha0000", T1);
    let s1 = symbol_snapshot("tweaked", "src/lib.rs", 1, 10, "c1sha0000", T1).1;
    let s2 = symbol_snapshot("tweaked", "src/lib.rs", 1, 12, "c2sha0000", T2).1;
    let s3 = symbol_snapshot("tweaked", "src/lib.rs", 1, 14, "c3sha0000", T3).1;
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, frames);
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        s1,
        s2,
        s3,
        sig,
    ];
    (records, sig_id, sym_id)
}

#[test]
fn source_facts_under_at_include_historical_version() {
    // Headline case: `--at c1` re-resolves the frame to `tweaked`'s stable ID at
    // c1, but the cited source fact must INCLUDE the c1 version (the reason `--at`
    // exists), not an arbitrary later-SHA version.
    let (records, sig_id, sym_id) = at_source_facts_fixture();
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        Some("c1sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve at c1");
    assert!(
        ctx.source_facts
            .iter()
            .any(|r| r.record_id == sym_id && r.git_commit.as_deref() == Some("c1sha0000")),
        "the --at c1 historical version must be present in source_facts"
    );

    // Byte-stable across repeated runs.
    let ctx2 = error_context(
        &records,
        &sig_id,
        None,
        Some("c1sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve at c1");
    assert_eq!(
        serde_json::to_string(&ctx).unwrap(),
        serde_json::to_string(&ctx2).unwrap(),
        "source_facts under --at is byte-identical across runs"
    );
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
        false,
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
        false,
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

#[test]
fn first_seen_window_is_scoped_to_repo() {
    // Repo A owns commits at T1 and T3, bracketing SIG_FIRST widely; repo B owns
    // commits at 06:00 and 18:00 the same day, bracketing SIG_FIRST more tightly.
    // Unscoped, the tighter repo-B commits win the window; scoped to repo A the
    // window must cite ONLY repo A's commits (the cross-repo bleed the fix
    // closes — commits are attributable via `owner_of`, so the timeline used for
    // bracketing must be repo-scoped, not built from every Commit node).
    let (repo_a, repo_a_node) = repo_node("repo-a");
    let (repo_b, repo_b_node) = repo_node("repo-b");
    let (ca1_id, ca1) = commit_in("repo-a", "a1sha00000", T1);
    let (ca2_id, ca2) = commit_in("repo-a", "a2sha00000", T3);
    let (cb1_id, cb1) = commit_in("repo-b", "b1sha00000", "2026-01-02T06:00:00Z");
    let (cb2_id, cb2) = commit_in("repo-b", "b2sha00000", "2026-01-02T18:00:00Z");
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let records = vec![
        repo_a_node,
        repo_b_node,
        contains(&repo_a, &ca1_id),
        contains(&repo_a, &ca2_id),
        contains(&repo_b, &cb1_id),
        contains(&repo_b, &cb2_id),
        ca1,
        ca2,
        cb1,
        cb2,
        sig,
    ];

    // Unscoped baseline: the tighter repo-B window wins (documents the bug).
    let unscoped = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve");
    match unscoped.first_seen_range {
        FirstSeenRange::History(window) => {
            assert_eq!(window.base_commit.as_deref(), Some("b1sha00000"));
            assert_eq!(window.head_commit.as_deref(), Some("b2sha00000"));
        }
        FirstSeenRange::Unavailable { .. } => panic!("expected a history-backed window"),
    }

    // Scoped to repo A: the window must bracket with repo A's commits only.
    let scoped = error_context(
        &records,
        &sig_id,
        Some(&repo_a),
        None,
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve");
    match scoped.first_seen_range {
        FirstSeenRange::History(window) => {
            assert_eq!(
                window.base_commit.as_deref(),
                Some("a1sha00000"),
                "base must be repo A's commit, never repo B's"
            );
            assert_eq!(
                window.head_commit.as_deref(),
                Some("a2sha00000"),
                "head must be repo A's commit, never repo B's"
            );
            assert_eq!(window.window_start.as_deref(), Some(T1));
            assert_eq!(window.window_end.as_deref(), Some(T3));
        }
        FirstSeenRange::Unavailable { .. } => panic!("expected a history-backed window"),
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

fn protected_handle_with_class(
    content_hash: &str,
    byte_len: u64,
    class: ProtectedPayloadClass,
    source_path: Option<&str>,
) -> ProtectedHandle {
    ProtectedHandle {
        handle: ProtectedHandle::compute_handle(&class, content_hash, source_path),
        schema_version: PROTECTED_SCHEMA_VERSION,
        source_class: class,
        source_path: source_path.map(str::to_owned),
        content_hash: content_hash.to_owned(),
        byte_len,
        captured_at: "2026-01-02T00:00:00Z".to_owned(),
        producer_id: "op-1".to_owned(),
        producer_version: "test".to_owned(),
    }
}

#[test]
fn protected_on_emits_all_handles_sharing_one_content_hash() {
    // Handle identity is (source_class, content_hash, source_path), so ONE
    // captured artifact hash can back multiple distinct handles. The read-time
    // join must emit ALL of them — a last-write-wins map would drop the actual
    // `log_payload` or surface the wrong class.
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("protected");
    let hash = "deadbeefhash";
    let log_h = protected_handle_with_class(
        hash,
        4096,
        ProtectedPayloadClass::LogPayload,
        Some("app.log"),
    );
    let report_h =
        protected_handle_with_class(hash, 128, ProtectedPayloadClass::Report, Some("app.log"));
    assert_ne!(
        log_h.handle, report_h.handle,
        "distinct handles for one hash"
    );
    write_manifest(&store, &[log_h, report_h]);

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
        false,
    )
    .expect("resolve");
    let payloads = ctx.protected_payloads.clone().expect("flag on yields Some");
    // BOTH handles matching the source hash appear, with their distinct classes.
    assert_eq!(payloads.len(), 2);
    let classes: Vec<&str> = payloads.iter().map(|p| p.source_class.as_str()).collect();
    assert!(
        classes.contains(&"log_payload"),
        "log_payload handle present"
    );
    assert!(classes.contains(&"report"), "report handle present");
    assert!(
        payloads.iter().all(|p| p.source_artifact_hash == hash),
        "every row cites the shared source hash"
    );
    assert_ne!(payloads[0].handle, payloads[1].handle, "distinct handles");

    // Byte-stable across repeated runs.
    let ctx2 = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        Some(&store),
        false,
    )
    .expect("resolve");
    assert_eq!(
        serde_json::to_string(&ctx).unwrap(),
        serde_json::to_string(&ctx2).unwrap(),
        "protected join is byte-identical across runs"
    );
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
    )
    .expect("resolve at c2");
    assert_eq!(at_c2.signatures[0].frames[0].target_record_id, beta_id);
}

// A frame at src/lib.rs:5 that resolves to a DIFFERENT symbol at the old commit
// (`alpha` at c1) than at HEAD (`beta` at c2), plus a pre-existing HEAD-view
// FRAME_RESOLVES_TO edge to `beta` — the normal combined-graph input a prior
// `eg resolve-frames` / `link-logs` run produces.
fn at_reresolution_fixture() -> (Vec<GraphRecord>, String, String, String) {
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
        // Pre-existing HEAD-view resolution to `beta`, carried through as input.
        frame_resolves(&sig_id, &beta_id, 0, FrameResolution::Resolved),
    ];
    (records, sig_id, alpha_id, beta_id)
}

#[test]
fn at_reresolution_replaces_stale_frame_edge_with_commit_view() {
    // `--at c1` must REPLACE the frame view, not merge with it: the pre-existing
    // HEAD edge to `beta` is dropped and the frame re-resolves to `alpha` at c1.
    // Before the fix the carried-through `beta` edge was reported ALONGSIDE the
    // fresh `alpha` target, silently mixing the two views.
    let (records, sig_id, alpha_id, beta_id) = at_reresolution_fixture();
    let at_c1 = error_context(
        &records,
        &sig_id,
        None,
        Some("c1sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve at c1");
    let targets: Vec<&str> = at_c1.signatures[0]
        .frames
        .iter()
        .map(|f| f.target_record_id.as_str())
        .collect();
    assert_eq!(
        targets,
        vec![alpha_id.as_str()],
        "`--at c1` must report ONLY the c1 re-resolution (alpha), not the stale HEAD target (beta)"
    );
    assert!(
        !targets.contains(&beta_id.as_str()),
        "the pre-existing HEAD frame edge must not bleed into the --at view"
    );
}

#[test]
fn non_at_path_preserves_preexisting_frame_edge() {
    // Regression guard for the fix: the NON-`--at` path relies on the input
    // graph's existing FRAME_RESOLVES_TO edges and must report the pre-existing
    // HEAD target (beta) unchanged — the strip is confined to the `--at` arm.
    let (records, sig_id, _alpha_id, beta_id) = at_reresolution_fixture();
    let head = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve at HEAD");
    let targets: Vec<&str> = head.signatures[0]
        .frames
        .iter()
        .map(|f| f.target_record_id.as_str())
        .collect();
    assert_eq!(
        targets,
        vec![beta_id.as_str()],
        "the non-`--at` path must report the pre-existing HEAD frame edge (beta) unchanged"
    );
}

#[test]
fn symbol_mode_resolves_against_at_commit_reresolved_frames() {
    // A frame at src/lib.rs:5. At c1 the symbol `alpha` occupies lines 1-10; at
    // c2 `beta` occupies the same span. The graph carries NO pre-existing
    // FRAME_RESOLVES_TO edge, so symbol-mode handle resolution MUST re-resolve
    // frames against the `--at` commit view — resolving from the current graph
    // (as before the fix) would find no frame target and return no_match.
    let frames = Some(vec![StackFrame {
        frame_index: 0,
        module_path: None,
        file_path: Some("src/lib.rs".to_owned()),
        line: Some(5),
    }]);
    let (_alpha_id, alpha) = symbol_snapshot("alpha", "src/lib.rs", 1, 10, "c1sha0000", T1);
    let (_beta_id, beta) = symbol_snapshot("beta", "src/lib.rs", 1, 10, "c2sha0000", T2);
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, frames);
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        alpha,
        beta,
        sig,
    ];

    // `error-context alpha --at c1`: the frame re-resolves to alpha at c1, so the
    // signature is returned (exit 0).
    let alpha_at_c1 = error_context(
        &records,
        "alpha",
        None,
        Some("c1sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("symbol-mode under --at c1 must resolve the frame to alpha");
    assert_eq!(alpha_at_c1.signature_ids, vec![sig_id.clone()]);

    // `error-context beta --at c1`: beta is NOT the c1 frame target, so no match.
    match error_context(
        &records,
        "beta",
        None,
        Some("c1sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        false,
    ) {
        Err(ErrorContextError::NoMatch { handle }) => assert_eq!(handle, "beta"),
        other => panic!("beta must not spuriously match at c1, got {other:?}"),
    }

    // Symmetric at c2: beta matches, alpha does not.
    let beta_at_c2 = error_context(
        &records,
        "beta",
        None,
        Some("c2sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("symbol-mode under --at c2 must resolve the frame to beta");
    assert_eq!(beta_at_c2.signature_ids, vec![sig_id]);
    match error_context(
        &records,
        "alpha",
        None,
        Some("c2sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        false,
    ) {
        Err(ErrorContextError::NoMatch { handle }) => assert_eq!(handle, "alpha"),
        other => panic!("alpha must not spuriously match at c2, got {other:?}"),
    }
}

#[test]
fn at_scopes_name_only_frame_to_commit_view() {
    // Issue #377: a name-only (module-path, no file/line) frame `app::handler`.
    // The simple name `handler` names DIFFERENT symbol ids across commits
    // (`src/a.rs` at c1, `src/b.rs` at c2). `error-context --at` re-runs
    // `resolve_frames` against the commit view, so the frame must scope to the
    // id that existed at that commit — never the union of both (which the
    // pre-fix, non-`at`-scoped name map produced as a spurious `ambiguous`).
    let frames = Some(vec![StackFrame {
        frame_index: 0,
        module_path: Some("app::handler".to_owned()),
        file_path: None,
        line: None,
    }]);
    let (id_a, sym_a) = symbol_snapshot("handler", "src/a.rs", 1, 10, "c1sha0000", T1);
    let (id_b, sym_b) = symbol_snapshot("handler", "src/b.rs", 1, 10, "c2sha0000", T2);
    let (sig_id, sig) = error_signature("handler-boom", "error", SIG_FIRST, SIG_LAST, 1, frames);
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        sym_a,
        sym_b,
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
        false,
    )
    .expect("resolve at c1");
    assert_eq!(
        at_c1.signatures[0].frames.len(),
        1,
        "the name-only frame scopes to a single c1 target, not the cross-commit union"
    );
    assert_eq!(
        at_c1.signatures[0].frames[0].target_record_id, id_a,
        "`--at c1` must scope the name-only frame to the c1 symbol id"
    );

    let at_c2 = error_context(
        &records,
        &sig_id,
        None,
        Some("c2sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve at c2");
    assert_eq!(
        at_c2.signatures[0].frames[0].target_record_id, id_b,
        "`--at c2` must scope the name-only frame to the c2 symbol id"
    );
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
        false,
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
    // An Observation that OBSERVES the frame-target symbol; its summary carries a
    // secret marker the redaction projection must strip (FIX 5 coverage).
    let (obs_id, obs) = observation(
        "redact",
        vec![evidence_link("OBSERVES", &sym_id, "codegraph")],
    );
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
        obs,
        observes_edge(&obs_id, &sym_id),
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
        false,
    )
    .expect("resolve");
    // The Observation carrying SECRET_OBSERVATION_MARKER must actually reach the
    // observations section, so the marker-absence assertion below is not vacuous.
    assert!(
        ctx.observations
            .iter()
            .any(|r| r.record_id == "agent_memory:v1:obs_redact"),
        "the OBSERVES-linked Observation must land in observations"
    );
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
        false,
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
            false,
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
                false,
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
        false,
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
// FIX 3: `content_hash_join` always wins over `temporal_correlation` on the
// SAME run, regardless of record order.
// ---------------------------------------------------------------------------

#[test]
fn stronger_basis_wins_over_weaker_for_same_run_regardless_of_order() {
    // Two signatures both frame-resolve to one symbol (so both are anchors) and
    // both link the SAME agent run. The weaker `temporal_correlation` edge is
    // listed FIRST; the stronger `content_hash_join` must still win (no
    // file-order downgrade).
    let (sym_id, sym) = code_symbol("boom_handler", "src/lib.rs", 1, 10);
    let (file_id, file) = code_file("src/lib.rs");
    let (sig_a, sig_a_node) = error_signature("a", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (sig_b, sig_b_node) = error_signature("b", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (run_id, run_node) = agent_run("shared", "2026-01-02T11:00:00Z", "2026-01-02T13:00:00Z");
    let records = vec![
        sym,
        file,
        defines(&file_id, &sym_id),
        sig_a_node,
        sig_b_node,
        run_node,
        frame_resolves(&sig_a, &sym_id, 0, FrameResolution::Resolved),
        frame_resolves(&sig_b, &sym_id, 0, FrameResolution::Resolved),
        // Weaker basis first — old `.or_insert` would keep it.
        emitted_during(&sig_a, &run_id, CorrelationBasis::TemporalCorrelation),
        emitted_during(&sig_b, &run_id, CorrelationBasis::ContentHashJoin),
    ];
    let ctx = error_context(
        &records,
        "boom_handler",
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve");
    let row = ctx
        .observations
        .iter()
        .find(|r| r.record_id == run_id)
        .expect("the shared run must appear once in observations");
    assert_eq!(
        row.correlation_basis.as_deref(),
        Some("content_hash_join"),
        "content_hash_join must win over temporal_correlation regardless of edge order"
    );
}

// ---------------------------------------------------------------------------
// FIX 4: `--repo` scope caveat (honest disclosure).
// ---------------------------------------------------------------------------

#[test]
fn repo_scope_caveat_present_only_when_repo_set() {
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let records = vec![sig];
    // Unscoped: field is absent (None).
    let unscoped = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve");
    assert!(unscoped.repo_scope_caveat.is_none());
    // Scoped: field discloses that the runtime sections are NOT filtered.
    let scoped = error_context(
        &records,
        &sig_id,
        Some("acme/widget"),
        None,
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve");
    assert_eq!(
        scoped.repo_scope_caveat,
        Some(aletheia_egregore::query::REPO_SCOPE_CAVEAT)
    );
    assert!(
        scoped
            .repo_scope_caveat
            .unwrap()
            .contains("NOT repository-filtered"),
        "the caveat must disclose the runtime sections are unfiltered"
    );
}

// ---------------------------------------------------------------------------
// Embedded-store log-retention caveat (issue #363 disclosure), mirroring the
// sibling `eg query log-deltas` disclosure. When error-context runs over an
// embedded (`--data-dir`) store the log-retained read surface surfaces every
// superseded non-temporal log version, so multi-scan coalescing IS reconstructed
// there for differing-content scans; the caveat now discloses only the residual
// byte-identical-re-ingest divergence. The `--graph` path preserves every
// ingested line and must NOT carry the caveat.
// ---------------------------------------------------------------------------

#[test]
fn embedded_source_discloses_log_retention_caveat() {
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
        true, // embedded_source
    )
    .expect("resolve");

    let caveat = ctx
        .embedded_log_retention_caveat
        .as_ref()
        .expect("embedded path with an ErrorSignature must disclose the retention caveat");
    // Reuses the lane-agnostic `log-deltas` (#326) disclosure verbatim.
    assert_eq!(
        caveat.message,
        aletheia_egregore::query::LOG_EMBEDDED_RETENTION_CAVEAT
    );
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
        caveat.message.contains("byte-identical re-ingests"),
        "the caveat must disclose the residual byte-identical-re-ingest divergence"
    );
    assert!(
        caveat.message.contains("#363"),
        "the caveat must reference the issue tracking the fix"
    );

    // Disclosure-only: resolution is unchanged by the flag.
    assert_eq!(ctx.signature_ids, vec![sig_id]);
}

#[test]
fn graph_source_never_carries_log_retention_caveat() {
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
        false, // embedded_source: the --graph path
    )
    .expect("resolve");
    assert!(
        ctx.embedded_log_retention_caveat.is_none(),
        "the --graph path preserves every ingested line and must not carry the retention caveat"
    );
}

/// Issue #363: a `--data-dir` store ingested with two differing-content
/// `scan-logs` outputs for the SAME signature ID must coalesce the signature
/// block (earliest `first_seen`, latest `last_seen`, summed occurrence, both
/// buckets) exactly as the `--graph` path does — the log-retained read surfaces
/// both physical versions. Distinct buckets avoid the byte-identical re-ingest
/// dedup, so occurrence data matches too.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_data_dir_coalesces_signature_block_like_graph() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};

    // Two scans of one fingerprint: scan 1 earlier/lower count, scan 2 later.
    let (sig_id, sig1) = error_signature(
        "boom",
        "error",
        "2026-01-02T10:00:00Z",
        "2026-01-02T11:00:00Z",
        3,
        None,
    );
    let (sig_id2, sig2) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 5, None);
    assert_eq!(sig_id, sig_id2, "same seed → same stable signature ID");
    let (b1n, b1e) = bucket_with_edge(&sig_id, "2026-01-02T10:00:00Z", 3);
    let (b2n, b2e) = bucket_with_edge(&sig_id, "2026-01-02T12:00:00Z", 5);
    let records = vec![sig1, sig2, b1n, b1e, b2n, b2e];

    // Build the embedded store, then read it back with log retention.
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
    for record in &records {
        sink.write_record(record).expect("record should write");
    }
    let embedded_records = sink
        .read_all_records_log_retained()
        .expect("log-retained read should surface both signature versions");
    drop(sink);

    let graph = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("graph resolve");
    let embedded = error_context(
        &embedded_records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        true,
    )
    .expect("embedded resolve");

    let g = &graph.signatures[0];
    let e = &embedded.signatures[0];
    // Coalesced identity + occurrence data match the --graph path.
    assert_eq!(e.first_seen, g.first_seen);
    assert_eq!(e.last_seen, g.last_seen);
    assert_eq!(e.occurrence_count, g.occurrence_count);
    // And concretely the coalesced values (earliest, latest, summed).
    assert_eq!(e.first_seen, "2026-01-02T10:00:00Z");
    assert_eq!(e.last_seen, SIG_LAST);
    assert_eq!(e.occurrence_count, 8);
    // Both buckets survive on the embedded path, matching --graph.
    let g_buckets: Vec<(String, u64)> = g
        .buckets
        .iter()
        .map(|b| (b.bucket_start.clone(), b.occurrence_count))
        .collect();
    let e_buckets: Vec<(String, u64)> = e
        .buckets
        .iter()
        .map(|b| (b.bucket_start.clone(), b.occurrence_count))
        .collect();
    assert_eq!(e_buckets, g_buckets);
    assert_eq!(
        e_buckets,
        vec![
            ("2026-01-02T10:00:00Z".to_owned(), 3),
            ("2026-01-02T12:00:00Z".to_owned(), 5),
        ]
    );

    // The embedded path discloses the residual retention caveat; --graph does not.
    assert!(embedded.embedded_log_retention_caveat.is_some());
    assert!(graph.embedded_log_retention_caveat.is_none());
}

/// Issue #363 (Codex P2): a `forget`-retracted log observation is NOT resurrected
/// by a later re-scan on the `--data-dir` retained read. A signature is written
/// (occurrence 3), then tombstoned (as `eg forget` does — a log ID is
/// non-`codegraph:`, non-temporal, so retraction is allowed), then re-observed by
/// a later scan with a DISTINCT payload (occurrence 5). The current-state read
/// exposes only the post-forget version; the log-retained read must too — the
/// pre-retraction observation is suppressed by the retraction boundary, so the
/// coalesced `error-context` occurrence is 5, never 3 + 5 = 8.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_data_dir_honors_forget_retraction_boundary_in_error_context() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};
    use aletheia_egregore::forget::retraction_tombstone_id;

    // v1 (occurrence 3) → forget/tombstone → v2 (occurrence 5, distinct window).
    let (sig_id, sig1) = error_signature(
        "boom",
        "error",
        "2026-01-02T10:00:00Z",
        "2026-01-02T11:00:00Z",
        3,
        None,
    );
    let (sig_id2, sig2) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 5, None);
    assert_eq!(sig_id, sig_id2, "same seed → same stable signature ID");
    // The tombstone mirrors exactly what `eg forget` mints for a log ID.
    let (tombstone_id, tombstone_version) = retraction_tombstone_id(&sig_id);
    let tombstone = GraphRecord::Tombstone {
        id: tombstone_id,
        schema_version: tombstone_version,
        deleted_id: sig_id.clone(),
        summary: "retracted: leaked value".to_owned(),
        producer: None,
    };

    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("forget-boundary-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
    sink.write_record(&sig1).expect("v1 should write");
    sink.write_record(&tombstone)
        .expect("tombstone should write");
    sink.write_record(&sig2)
        .expect("post-forget re-scan should write");
    let embedded_records = sink
        .read_all_records_log_retained()
        .expect("log-retained read");
    drop(sink);

    let embedded = error_context(
        &embedded_records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        true,
    )
    .expect("embedded resolve");

    assert_eq!(
        embedded.signatures.len(),
        1,
        "the retracted-then-re-observed signature coalesces to one row"
    );
    let e = &embedded.signatures[0];
    // Only the post-forget observation survives — never 3 + 5 = 8.
    assert_eq!(
        e.occurrence_count, 5,
        "the pre-forget observation must not be resurrected into the coalesced sum"
    );
    assert_eq!(e.first_seen, SIG_FIRST);
    assert_eq!(e.last_seen, SIG_LAST);
}

/// Issue #363 (Codex P2): the same-hour/same-count `LogOccurrenceBucket` sub-case.
/// When two DIFFERING scans of one signature share a bucket for the same hour with
/// the SAME count, that bucket record is BYTE-IDENTICAL (same ID AND content), so
/// its second write is an idempotent no-op → only ONE physical bucket node. The
/// two `ErrorSignature` records still differ, so both are retained and coalesced.
/// Consequence: the coalesced signature's occurrence-bucket block holds the shared
/// bucket ONCE on `--data-dir`, whereas `--graph` (which iterates bucket nodes over
/// the concatenated JSONL, never deduping by bucket record ID) holds it TWICE. This
/// case LEGITIMATELY diverges from `--graph`; the real fix is source-aware bucket
/// identity, tracked in issue #361. We assert the OBSERVED behavior and do NOT
/// assert `--data-dir == --graph` for the bucket block.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_data_dir_dedups_shared_same_count_bucket_block() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};

    // Two differing scans of one fingerprint (distinct last_seen + count).
    let (sig_id, sig1) = error_signature(
        "boom",
        "error",
        "2026-01-02T10:00:00Z",
        "2026-01-02T11:00:00Z",
        3,
        None,
    );
    let (sig_id2, sig2) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 5, None);
    assert_eq!(sig_id, sig_id2, "same seed → same stable signature ID");
    // A SHARED 12:00 bucket with an IDENTICAL count (4) is emitted by BOTH scans →
    // byte-identical record → deduped on the embedded path. Plus one DISTINCT-hour
    // bucket per scan so the block has non-shared occurrence data too.
    let (shared_n, shared_e) = bucket_with_edge(&sig_id, "2026-01-02T12:00:00Z", 4);
    let (d1n, d1e) = bucket_with_edge(&sig_id, "2026-01-02T10:00:00Z", 3);
    let (d2n, d2e) = bucket_with_edge(&sig_id, "2026-01-02T13:00:00Z", 6);
    let records = vec![
        sig1,
        sig2,
        shared_n.clone(),
        shared_e.clone(),
        d1n,
        d1e,
        shared_n,
        shared_e,
        d2n,
        d2e,
    ];

    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("shared-bucket-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
    for record in &records {
        sink.write_record(record).expect("record should write");
    }
    let embedded_records = sink
        .read_all_records_log_retained()
        .expect("log-retained read should surface both signature versions");
    drop(sink);

    let graph = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("graph resolve");
    let embedded = error_context(
        &embedded_records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        true,
    )
    .expect("embedded resolve");

    let shared_rows = |ctx: &aletheia_egregore::query::ErrorContext| {
        ctx.signatures[0]
            .buckets
            .iter()
            .filter(|b| b.bucket_start == "2026-01-02T12:00:00Z")
            .count()
    };
    let total = |ctx: &aletheia_egregore::query::ErrorContext| -> u64 {
        ctx.signatures[0]
            .buckets
            .iter()
            .map(|b| b.occurrence_count)
            .sum()
    };

    // --graph: the shared 12:00 bucket appears TWICE (both byte-identical copies),
    // so the bucket-block total sums it twice: 3 + 4 + 4 + 6 = 17.
    assert_eq!(
        shared_rows(&graph),
        2,
        "--graph carries both shared-bucket copies"
    );
    assert_eq!(total(&graph), 17);

    // --data-dir: the byte-identical shared bucket is deduped to one physical node,
    // so it appears ONCE and its count is reflected once: 3 + 4 + 6 = 13.
    assert_eq!(
        shared_rows(&embedded),
        1,
        "the byte-identical shared bucket is deduped to one physical record on --data-dir"
    );
    assert_eq!(total(&embedded), 13);

    // The two paths LEGITIMATELY diverge here by exactly the once-deduped shared
    // count (4) — inherent to non-source-aware bucket identity (issue #361), NOT a
    // bug in the embedded read path. Deliberately NOT asserting block equality.
    // (Coalesced signature IDENTITY parity — first_seen/last_seen/aggregate — is
    // covered by `embedded_data_dir_coalesces_signature_block_like_graph`; this
    // sub-case affects only duplicate buckets.)
    assert_eq!(total(&graph) - total(&embedded), 4);

    // The embedded path still discloses the retention caveat, which now names this
    // shared-hour/shared-count bucket sub-case and cites #361.
    let caveat = embedded
        .embedded_log_retention_caveat
        .as_ref()
        .expect("embedded path with an ErrorSignature must disclose the retention caveat");
    assert!(caveat.message.contains("#361"));
    assert!(graph.embedded_log_retention_caveat.is_none());
}

/// Issue #363 (Codex P2): the `error-context --data-dir --at`/`--as-of`
/// TEMPORAL lane must NOT double-count an enrichment-only `ErrorSignature`
/// rewrite. The standard pipeline `scan-logs -> resolve-frames -> link-logs`
/// writes a signature bare, then rewrites it with `FRAME_RESOLVES_TO` /
/// `EMITTED_DURING` evidence links while leaving the log payload untouched —
/// producing two physical versions of one scan observation. The unfiltered
/// history-inclusive read (`read_all_records_including_superseded`, the pre-fix
/// temporal lane) re-emits BOTH, so the coalescer SUMS `occurrence_count` twice.
/// The log-retained history read collapses the enrichment rewrite to its single
/// latest (enriched) version, counting the observation once, while leaving every
/// non-log temporal/superseded record intact for `--at` reconstruction.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_temporal_lane_collapses_enrichment_rewrite_occurrence_count() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};

    // A signature whose frame resolves to a symbol present at c1, plus commit
    // history so `--at c1` resolves — mirrors `at_source_facts_fixture`.
    let frames = Some(vec![StackFrame {
        frame_index: 0,
        module_path: None,
        file_path: Some("src/lib.rs".to_owned()),
        line: Some(5),
    }]);
    let (sym_id, s1) = symbol_snapshot("tweaked", "src/lib.rs", 1, 10, "c1sha0000", T1);
    let (sig_id, bare_sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 7, frames);
    let (bn, be) = bucket_with_edge(&sig_id, SIG_FIRST, 7);
    // The enrichment rewrite: SAME log payload (occurrence 7), an evidence link
    // added by resolve-frames/link-logs (mirrors FRAME_RESOLVES_TO on the node).
    let enriched_sig = bare_sig.clone().with_evidence_links(vec![evidence_link(
        "FRAME_RESOLVES_TO",
        &sym_id,
        "codegraph",
    )]);
    // resolve-frames also emits the FRAME_RESOLVES_TO edge itself.
    let fr_edge = frame_resolves(&sig_id, &sym_id, 0, FrameResolution::Resolved);
    // Pipeline write order: scan-logs (bare + bucket), then resolve-frames
    // (enriched signature rewrite + edge).
    let records = vec![
        commit("c1sha0000", &[], T1),
        s1,
        bare_sig,
        bn,
        be,
        enriched_sig,
        fr_edge,
    ];

    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("enrichment-temporal-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
    for record in &records {
        sink.write_record(record).expect("record should write");
    }

    // HEAD behaviour (regression guard for the hole): the UNFILTERED
    // history-inclusive read re-emits BOTH physical signature versions, so the
    // coalescer SUMS 7 + 7 = 14 — the observed double-count on trunk.
    let unfiltered = sink
        .read_all_records_including_superseded()
        .expect("history read");
    let bug = error_context(
        &unfiltered,
        &sig_id,
        None,
        Some("c1sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        true,
    )
    .expect("resolve at c1 (unfiltered)");
    assert_eq!(
        bug.signatures[0].occurrence_count, 14,
        "the unfiltered temporal read double-counts the enrichment rewrite (documented HEAD hole)"
    );

    // FIXED behaviour: the log-retained history read collapses the enrichment
    // rewrite to exactly ONE physical version — the latest (enriched) one.
    let retained = sink
        .read_all_records_including_superseded_log_retained()
        .expect("log-retained history read");
    let sig_versions: Vec<&GraphRecord> = retained
        .iter()
        .filter(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
        .collect();
    assert_eq!(
        sig_versions.len(),
        1,
        "the enrichment rewrite collapses to one physical signature version"
    );
    assert!(
        sig_versions[0]
            .evidence_links()
            .is_some_and(|links| !links.is_empty()),
        "the retained version is the ENRICHED one (carries the evidence links)"
    );
    let fixed = error_context(
        &retained,
        &sig_id,
        None,
        Some("c1sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        true,
    )
    .expect("resolve at c1 (retained)");
    assert_eq!(
        fixed.signatures[0].occurrence_count, 7,
        "the enrichment rewrite is counted ONCE on the fixed temporal lane"
    );
    // The FRAME_RESOLVES_TO edge still joins after the enrichment dedup.
    assert!(
        !fixed.signatures[0].frames.is_empty(),
        "the resolved frame still joins after the enrichment dedup"
    );
    // Non-log temporal reconstruction is unaffected: the c1 frame-target symbol
    // is still resolved under `--at c1`.
    assert!(
        fixed.source_facts.iter().any(|r| r.record_id == sym_id),
        "the frame-target symbol is still resolved under --at c1"
    );
}

/// Issue #363: the log-retained temporal read must still coalesce two DISTINCT
/// scan observations (differing `first_seen`/`last_seen`/`occurrence_count`) —
/// the enrichment-dedup collapses only same-observation rewrites, never distinct
/// observations — so a genuine multi-scan case reconstructs earliest/latest/summed
/// exactly as the unfiltered read does under `--at`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_temporal_lane_retains_distinct_scan_observations() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};

    let (sig_id, sig1) = error_signature(
        "boom",
        "error",
        "2026-01-02T10:00:00Z",
        "2026-01-02T11:00:00Z",
        3,
        None,
    );
    let (sig_id2, sig2) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 5, None);
    assert_eq!(sig_id, sig_id2, "same seed → same stable signature ID");
    let records = vec![commit("c1sha0000", &[], T1), sig1, sig2];

    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("distinct-obs-temporal-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
    for record in &records {
        sink.write_record(record).expect("record should write");
    }
    let retained = sink
        .read_all_records_including_superseded_log_retained()
        .expect("log-retained history read");
    let versions = retained
        .iter()
        .filter(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
        .count();
    assert_eq!(
        versions, 2,
        "two DISTINCT scan observations both survive the log-retained temporal read"
    );
    let ctx = error_context(
        &retained,
        &sig_id,
        None,
        Some("c1sha0000"),
        None,
        SupersessionMode::Exclude,
        None,
        true,
    )
    .expect("resolve at c1");
    assert_eq!(ctx.signatures[0].first_seen, "2026-01-02T10:00:00Z");
    assert_eq!(ctx.signatures[0].last_seen, SIG_LAST);
    assert_eq!(
        ctx.signatures[0].occurrence_count, 8,
        "distinct observations coalesce to the summed count (3 + 5)"
    );
}

#[test]
fn embedded_source_over_pure_scan_graph_carries_no_caveat_envelope() {
    // The caveat is gated on at least one `ErrorSignature` node being present.
    // A pure `scan` graph (code symbols only, zero log records) can never
    // resolve a handle — handle resolution requires an `ErrorSignature` id — so
    // an embedded query over such a store returns `no_match` and emits NO
    // envelope at all, hence never the caveat. This exercises the guard's
    // rationale: the disclosure is content-gated, never emitted merely because
    // the read path was embedded.
    let (_id, sym) = code_symbol("only_code", "src/lib.rs", 1, 10);
    let records = vec![sym];
    let bogus = log_stable_id(&["error_signature", ANCHOR, FINGERPRINT, "ghost", "error"]);
    match error_context(
        &records,
        &bogus,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        true, // embedded_source, yet zero ErrorSignature records
    ) {
        Err(ErrorContextError::NoMatch { .. }) => {}
        other => panic!("expected NoMatch over a pure scan graph, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// FIX 2: a corrupt/unreadable protected manifest must fail loudly, never
// silently degrade to an empty list.
// ---------------------------------------------------------------------------

#[test]
fn corrupt_protected_manifest_fails_loudly() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("protected");
    fs::create_dir_all(&store).unwrap();
    // A malformed manifest line makes `ProtectedStore::list()` return an error.
    fs::write(store.join("manifest.jsonl"), "{ this is not valid json }\n").unwrap();

    let hash = "deadbeefhash";
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, None);
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    let records = vec![sig, src, captured_from(&sig_id, &src_id)];
    match error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        Some(&store),
        false,
    ) {
        Err(ErrorContextError::ProtectedStoreUnreadable { message }) => {
            assert!(
                message.contains("failed to read protected store"),
                "message must name the failure: {message}"
            );
        }
        other => panic!("expected ProtectedStoreUnreadable, got {other:?}"),
    }
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

// ---------------------------------------------------------------------------
// FIX 1 (MAJOR, AC): `--as-of` bounds the occurrence view on the valid axis
// WITHOUT re-resolving frames and WITHOUT requiring a Commit timeline. This is
// the CLI-level coverage that the missing test let slip: `--as-of` on a plain
// (commit-less) log graph — the natural bucket-bearing `scan-logs` input — must
// succeed (exit 0) instead of dying with `empty_history`.
// ---------------------------------------------------------------------------

/// A commit-less log graph: a signature with NO `StackFrame`s but an existing
/// `FRAME_RESOLVES_TO` edge onto a code symbol, plus an early and a late bucket.
/// Because the signature carries no raw frames, any frame re-resolution would
/// yield an EMPTY frames list — so a non-empty `frames` proves frames were NOT
/// re-resolved.
fn commitless_log_fixture() -> (Vec<GraphRecord>, String, String) {
    let (sym_id, sym) = code_symbol("boom_handler", "src/lib.rs", 1, 10);
    let (file_id, file) = code_file("src/lib.rs");
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 8, None);
    let (early_n, early_e) = bucket_with_edge(&sig_id, "2026-01-02T12:00:00Z", 5);
    let (late_n, late_e) = bucket_with_edge(&sig_id, "2026-01-02T18:00:00Z", 3);
    let records = vec![
        sym,
        file,
        defines(&file_id, &sym_id),
        sig,
        early_n,
        early_e,
        late_n,
        late_e,
        frame_resolves(&sig_id, &sym_id, 0, FrameResolution::Resolved),
    ];
    (records, sig_id, sym_id)
}

#[test]
fn cli_as_of_on_commitless_log_graph_bounds_buckets_no_reresolve() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("log.jsonl");
    let (records, sig_id, sym_id) = commitless_log_fixture();
    write_graph(&records, &graph);

    // `--as-of` between the two buckets: exit 0 (NOT exit-2 empty_history), the
    // late bucket dropped, and the existing frame preserved (not re-resolved).
    let assert = CargoCommand::cargo_bin("egregore")
        .unwrap()
        .args(["query", "error-context", &sig_id])
        .args(["--as-of", "2026-01-02T13:00:00Z"])
        .arg("--graph")
        .arg(&graph)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(body["ok"], true);
    let block = &body["signatures"][0];
    let buckets = block["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 1, "the post-cutoff bucket must be dropped");
    assert_eq!(buckets[0]["bucket_start"], "2026-01-02T12:00:00Z");
    // Frames are read from the existing edge, never re-resolved.
    let frames = block["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1, "the existing frame edge is preserved");
    assert_eq!(frames[0]["target_record_id"], sym_id);
    // No Commit timeline → history_unavailable, never a fabricated window.
    assert_eq!(body["first_seen_range"]["status"], "unavailable");
}

#[test]
fn cli_malformed_as_of_is_rejected_not_silently_ignored() {
    // A malformed `--as-of` (not full RFC 3339) must fail with a machine-readable
    // `invalid_as_of_timestamp` error (exit 1), NEVER exit 0 with an unbounded
    // (all-bucket) view. Before the fix a bad `--as-of` silently produced no
    // cutoff because the core's `parse_instant` returned `None`.
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("log.jsonl");
    let (records, sig_id, _sym_id) = commitless_log_fixture();
    write_graph(&records, &graph);

    for bad in ["2026-01-02", "not-a-timestamp"] {
        let assert = CargoCommand::cargo_bin("egregore")
            .unwrap()
            .args(["query", "error-context", &sig_id])
            .args(["--as-of", bad])
            .arg("--graph")
            .arg(&graph)
            .assert()
            .code(1);
        let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        let body: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(
            body["error"]["code"], "invalid_as_of_timestamp",
            "a malformed --as-of must be rejected, not silently ignored"
        );
    }
}

#[test]
fn cli_as_of_on_history_graph_bounds_buckets_no_reresolve() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("hist.jsonl");
    let (mut records, sig_id, sym_id) = history_fixture();
    // history_fixture's signature carries no StackFrames but an existing frame
    // edge; add an early and a late bucket to bound.
    let (early_n, early_e) = bucket_with_edge(&sig_id, "2026-01-02T12:00:00Z", 5);
    let (late_n, late_e) = bucket_with_edge(&sig_id, "2026-01-02T18:00:00Z", 3);
    records.extend([early_n, early_e, late_n, late_e]);
    write_graph(&records, &graph);

    let assert = CargoCommand::cargo_bin("egregore")
        .unwrap()
        .args(["query", "error-context", &sig_id])
        .args(["--as-of", "2026-01-02T13:00:00Z"])
        .arg("--graph")
        .arg(&graph)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).unwrap();
    let block = &body["signatures"][0];
    let buckets = block["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 1, "late bucket dropped by --as-of");
    assert_eq!(buckets[0]["bucket_start"], "2026-01-02T12:00:00Z");
    // Not re-resolved: the existing frame edge survives (no StackFrames exist to
    // re-resolve from, so re-resolution would have emptied frames).
    let frames = block["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1, "existing frame preserved under --as-of");
    assert_eq!(frames[0]["target_record_id"], sym_id);
    // The valid history window is still derived from the Commit timeline.
    assert_eq!(body["first_seen_range"]["status"], "history");
}

#[test]
fn cli_at_reresolves_frames_against_commit_view() {
    // `--at` (unlike `--as-of`) re-resolves frames: the signature carries a
    // StackFrame at src/lib.rs:5; at c1 `alpha` occupies that span, at c2 `beta`.
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("at.jsonl");
    let frames = Some(vec![StackFrame {
        frame_index: 0,
        module_path: None,
        file_path: Some("src/lib.rs".to_owned()),
        line: Some(5),
    }]);
    let (_alpha_id, alpha) = symbol_snapshot("alpha", "src/lib.rs", 1, 10, "c1sha0000", T1);
    let (beta_id, beta) = symbol_snapshot("beta", "src/lib.rs", 1, 10, "c2sha0000", T2);
    let (sig_id, sig) = error_signature("boom", "error", SIG_FIRST, SIG_LAST, 1, frames);
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        alpha,
        beta,
        sig,
    ];
    write_graph(&records, &graph);

    let assert = CargoCommand::cargo_bin("egregore")
        .unwrap()
        .args(["query", "error-context", &sig_id])
        .args(["--at", "c2sha0000"])
        .arg("--graph")
        .arg(&graph)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).unwrap();
    let frames = body["signatures"][0]["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0]["target_record_id"], beta_id,
        "`--at c2` re-resolves the frame to the symbol at that commit view"
    );
}
