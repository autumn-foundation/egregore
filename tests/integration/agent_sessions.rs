#![allow(missing_docs)]

//! End-to-end tests for `eg query sessions <REPO>` — the repo-scoped,
//! recency-ordered digest of recent agent sessions (issue #112).
//!
//! The lane answers "what did agents recently do in this repository?" from
//! already-recorded agent-memory and project-state facts: one row per
//! `AgentSession` scoped to the selected repository, ordered by last activity
//! descending, carrying agent/session handles, time bounds, run outcomes,
//! referenced tasks, and per-kind record counts. Every row is an agent claim —
//! never verification, never proof a task completed.
//!
//! These tests pin the contract end-to-end over the built binary: they seed a
//! deterministic JSONL fixture (fixed timestamps, stable IDs) and assert
//! scoping, ordering, membership, counts, task status, redaction, determinism,
//! and read-only behavior.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, IdentitySource, NodeKind, RepositoryIdentityPayload,
    SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, OutputHandle, PROJECT_SCHEMA_VERSION,
        PatchHandle, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id, artifact_stable_id,
        project_stable_id, stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;

// ---------------------------------------------------------------------------
// Pinned contract constants (issue #112)
// ---------------------------------------------------------------------------

/// The verbatim envelope disclaimer every `sessions` answer must carry.
const SESSIONS_DISCLAIMER: &str = "rows are recorded agent-authored memory and project-state facts; outcomes, observations, decisions, failures, and counts are agent claims, never verification, proof of task completion, or proof that code works; an absent citation is not evidence that no work happened; task status is a recorded project-domain fact, not a correctness claim";

/// Raw payload sentinels that must NEVER appear on stdout.
const RAW_OBS_TEXT: &str = "RAW_OBS_TEXT_SHOULD_NOT_LEAK";
const RAW_SESSION_SUMMARY: &str = "RAW_SESSION_SUMMARY_SHOULD_NOT_LEAK";
const RAW_TASK_TITLE: &str = "RAW_TASK_TITLE_SHOULD_NOT_LEAK";
const RAW_STDOUT: &str = "RAW_STDOUT_SHOULD_NOT_LEAK";
const RAW_PATCH: &str = "RAW_PATCH_SHOULD_NOT_LEAK";
/// A run's stored `observed_at` is importer-controlled free text until it
/// parses: forwarding it verbatim would let arbitrary bytes ride out on a field
/// documented as a timestamp.
const RAW_OBSERVED_AT: &str = "RAW_OBSERVED_AT_SHOULD_NOT_LEAK";

const SENTINELS: &[&str] = &[
    RAW_OBS_TEXT,
    RAW_SESSION_SUMMARY,
    RAW_TASK_TITLE,
    RAW_STDOUT,
    RAW_PATCH,
    RAW_OBSERVED_AT,
];

/// Closed set of `scope_basis` values.
const SCOPE_BASES: &[&str] = &["code_citation", "task_reference"];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should build")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: start_line * 10,
        end_byte: end_line * 10,
        start_line,
        end_line,
    }
}

fn link(target: &str, domain: &str, relation: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: domain.to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

/// Serializes records in INSERTION order (unlike [`Graph::to_jsonl`], which
/// sorts). Physical order matters here: the fixture deliberately writes `S1`
/// before `S2` even though `S2` must sort FIRST in the answer, so a lane that
/// leaks file order into row order is caught. Duplicate physical lines survive
/// verbatim, which is what the duplicate-write count test needs.
fn write_jsonl(records: &[GraphRecord], path: &Path) {
    let mut out = String::new();
    for record in records {
        out.push_str(&serde_json::to_string(record).expect("record should serialize"));
        out.push('\n');
    }
    fs::write(path, out).expect("write fixture jsonl");
}

/// Fixture handle: the graph path plus every stable record ID by fixture label.
struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    ids: BTreeMap<String, String>,
}

impl Fixture {
    fn id(&self, key: &str) -> &str {
        self.ids
            .get(key)
            .unwrap_or_else(|| panic!("fixture has no record labelled {key}"))
            .as_str()
    }
}

/// Accumulates fixture records in insertion order alongside their labels.
struct Builder {
    records: Vec<GraphRecord>,
    ids: BTreeMap<String, String>,
}

impl Builder {
    const fn new() -> Self {
        Self {
            records: Vec::new(),
            ids: BTreeMap::new(),
        }
    }

    fn push(&mut self, record: GraphRecord) {
        self.records.push(record);
    }

    fn label(&mut self, key: &str, id: &str) {
        self.ids.insert(key.to_owned(), id.to_owned());
    }

    fn finish(self, file_name: &str) -> Fixture {
        let temp = tempfile::tempdir().expect("temp dir");
        let graph = temp.path().join(file_name);
        write_jsonl(&self.records, &graph);
        Fixture {
            _temp: temp,
            graph,
            ids: self.ids,
        }
    }
}

// ── Record constructors ──────────────────────────────────────────────────────

fn repository(builder: &mut Builder, label: &str, display: &str, basename: &str) -> String {
    let id = stable_id(&["repository", "operator-override", display]);
    builder.push(
        GraphRecord::node(
            id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some(display.to_owned()),
            format!("Repository {display}"),
        )
        .with_repository_identity(RepositoryIdentityPayload {
            identity_source: IdentitySource::OperatorOverride,
            remote_url: None,
            root_commit_sha: None,
            canonical_path: None,
            basename: basename.to_owned(),
        }),
    );
    builder.label(label, &id);
    id
}

/// Seeds `Repository -CONTAINS-> File -DEFINES-> Symbol` and returns
/// `(file_id, symbol_id)`.
fn code_topology(
    builder: &mut Builder,
    repo_id: &str,
    file_label: &str,
    symbol_label: &str,
    path: &str,
    symbol_name: &str,
) -> (String, String) {
    let file_id = stable_id(&["node", "file", repo_id, path]);
    builder.push(GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        path.to_owned(),
        span(1, 100),
        path.to_owned(),
        "rust",
        format!("Source file {path}"),
    ));
    builder.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        file_id.clone(),
        Some("1.0".to_owned()),
        "repository contains file".to_owned(),
    ));

    let symbol_id = stable_id(&["node", "symbol", repo_id, path, symbol_name]);
    builder.push(GraphRecord::syntax_node(
        symbol_id.clone(),
        NodeKind::Symbol,
        path.to_owned(),
        span(10, 20),
        symbol_name.to_owned(),
        "rust",
        format!("Symbol {symbol_name}"),
    ));
    builder.push(GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        symbol_id.clone(),
        Some("1.0".to_owned()),
        "file defines symbol".to_owned(),
    ));

    builder.label(file_label, &file_id);
    builder.label(symbol_label, &symbol_id);
    (file_id, symbol_id)
}

fn agent(builder: &mut Builder, label: &str, agent_id_value: &str) -> String {
    let id = agent_memory_stable_id(&["node", "agent", agent_id_value]);
    let mut record = GraphRecord::node(
        id.clone(),
        NodeKind::Agent,
        None,
        None,
        Some(agent_id_value.to_owned()),
        format!("Agent {agent_id_value}"),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        agent_kind,
        ..
    } = &mut record
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some(agent_id_value.to_owned());
        *agent_kind = Some("claude-code".to_owned());
    }
    builder.push(record);
    builder.label(label, &id);
    id
}

/// Builds an agent-memory node with the shared provenance fields set.
fn memory_node(
    kind: NodeKind,
    id: String,
    agent_id_value: Option<&str>,
    session_id_value: Option<&str>,
    observed: Option<&str>,
    summary: &str,
    text_value: Option<&str>,
) -> GraphRecord {
    let mut record = GraphRecord::node(id, kind, None, None, None, summary.to_owned());
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        agent_kind,
        session_id,
        observed_at,
        ingested_at,
        confidence,
        text,
        ..
    } = &mut record
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = agent_id_value.map(str::to_owned);
        *agent_kind = agent_id_value.map(|_| "claude-code".to_owned());
        *session_id = session_id_value.map(str::to_owned);
        *observed_at = observed.map(str::to_owned);
        *ingested_at = observed.map(str::to_owned);
        *confidence = Some("1.0".to_owned());
        *text = text_value.map(str::to_owned);
    }
    record
}

fn session(
    builder: &mut Builder,
    label: &str,
    session_id_value: &str,
    agent_id_value: Option<&str>,
    observed: Option<&str>,
    summary: &str,
) -> String {
    let id = agent_memory_stable_id(&["node", "agent_session", session_id_value]);
    builder.push(memory_node(
        NodeKind::AgentSession,
        id.clone(),
        agent_id_value,
        Some(session_id_value),
        observed,
        summary,
        None,
    ));
    builder.label(label, &id);
    id
}

fn run(
    builder: &mut Builder,
    label: &str,
    key: &str,
    session_id_value: &str,
    observed: Option<&str>,
    summary: &str,
) -> String {
    let id = agent_memory_stable_id(&["node", "agent_run", key]);
    builder.push(memory_node(
        NodeKind::AgentRun,
        id.clone(),
        Some("agent-1"),
        Some(session_id_value),
        observed,
        summary,
        None,
    ));
    builder.label(label, &id);
    id
}

fn turn(builder: &mut Builder, label: &str, key: &str, session_id_value: &str) -> String {
    let id = agent_memory_stable_id(&["node", "agent_turn", key]);
    builder.push(memory_node(
        NodeKind::AgentTurn,
        id.clone(),
        Some("agent-1"),
        Some(session_id_value),
        None,
        "AgentTurn 0",
        None,
    ));
    builder.label(label, &id);
    id
}

#[allow(clippy::too_many_arguments)]
fn memory(
    builder: &mut Builder,
    label: &str,
    kind: NodeKind,
    key: &str,
    session_id_value: Option<&str>,
    observed: Option<&str>,
    summary: &str,
    text_value: Option<&str>,
    links: Option<Vec<EvidenceLink>>,
) -> String {
    let id = agent_memory_stable_id(&["node", kind.as_str(), key]);
    let mut record = memory_node(
        kind,
        id.clone(),
        Some("agent-1"),
        session_id_value,
        observed,
        summary,
        text_value,
    );
    if let Some(links) = links {
        record = record.with_evidence_links(links);
    }
    builder.push(record);
    builder.label(label, &id);
    id
}

fn task(
    builder: &mut Builder,
    label: &str,
    key: &str,
    status_value: &str,
    title_text: &str,
) -> String {
    let id = project_stable_id(&["task", key]);
    let mut record = GraphRecord::node(
        id.clone(),
        NodeKind::Task,
        None,
        None,
        Some(key.to_owned()),
        format!("Task {key}"),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        entity_id,
        title,
        status,
        source_kind,
        valid_time,
        ..
    } = &mut record
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *domain = Some("project".to_owned());
        *entity_id = Some(id.clone());
        *title = Some(title_text.to_owned());
        *status = Some(status_value.to_owned());
        *source_kind = Some("local_task".to_owned());
        *valid_time = Some("2026-03-01T00:00:00Z".to_owned());
    }
    builder.push(record);
    builder.label(label, &id);
    id
}

fn am_edge(builder: &mut Builder, label: EdgeLabel, source: &str, target: &str) {
    builder.push(GraphRecord::agent_memory_edge(
        label,
        source.to_owned(),
        target.to_owned(),
        Some("1.0".to_owned()),
        format!("{} edge", label.as_str()),
    ));
}

fn proj_edge(builder: &mut Builder, label: EdgeLabel, source: &str, target: &str) {
    builder.push(GraphRecord::project_edge(
        label,
        source.to_owned(),
        target.to_owned(),
        Some("1.0".to_owned()),
        format!("{} edge", label.as_str()),
    ));
}

/// Writes an agent-memory-domain deletion tombstone for `deleted`.
fn tombstone(builder: &mut Builder, deleted: &str) {
    builder.push(GraphRecord::Tombstone {
        id: agent_memory_stable_id(&["tombstone", deleted]),
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        deleted_id: deleted.to_owned(),
        summary: "record deleted".to_owned(),
        producer: None,
    });
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Core scope fixture: two repositories, four sessions.
///
/// * `S1` — repo A, direct code citations + a task reference, one templated run.
/// * `S2` — repo A via a TASK path only, 3-hop membership, non-template run.
/// * `S3` — repo B ONLY, and the NEWEST session overall (a scope bug puts it
///   first in repo A's digest).
/// * `S4` — no citations at all → `unresolved_repository_scope`.
///
/// `S1` is written BEFORE `S2` even though `S2` must sort first.
#[allow(clippy::too_many_lines)]
fn seed_scope_into(builder: &mut Builder) {
    let repo_a = repository(builder, "RepoA", "repo-a", "repo-a");
    let (file_a, sym_a) = code_topology(builder, &repo_a, "FileA", "SymA", "src/a.rs", "alpha");
    let repo_b = repository(builder, "RepoB", "repo-b", "repo-b");
    let (file_b, _sym_b) = code_topology(builder, &repo_b, "FileB", "SymB", "src/b.rs", "beta");

    let ag1 = agent(builder, "Ag1", "agent-1");
    // Both sessions carry a real `Agent` record and a canonical
    // `AgentSession -SESSION_OF-> Agent` edge, matching what the importers
    // actually write. (`SESSION_OF` runs session→agent, so this adds no member:
    // membership walks PREDECESSORS of the session.)
    let ag2 = agent(builder, "Ag2", "agent-2");

    // ── Tasks referenced by S1 / S2 ─────────────────────────────────────────
    let task_a = task(
        builder,
        "TaskA",
        "task-a",
        "in_progress",
        &format!("{RAW_TASK_TITLE}: finish alpha"),
    );
    proj_edge(builder, EdgeLabel::MentionsSymbol, &task_a, &sym_a);

    let task_a2 = task(
        builder,
        "TaskA2",
        "task-a2",
        "closed_completed",
        "close out alpha",
    );
    proj_edge(builder, EdgeLabel::TouchesFile, &task_a2, &file_a);

    // ── S1 (written FIRST, sorts SECOND) ────────────────────────────────────
    let s1 = session(
        builder,
        "S1",
        "sess-1",
        Some("agent-1"),
        Some("2026-03-01T10:00:00Z"),
        &format!("AgentSession {RAW_SESSION_SUMMARY}"),
    );
    am_edge(builder, EdgeLabel::SessionOf, &s1, &ag1);

    let r1 = run(
        builder,
        "R1",
        "run-1",
        "sess-1",
        Some("2026-03-01T10:05:00Z"),
        "AgentRun outcome=success exit_reason=completed",
    );
    am_edge(builder, EdgeLabel::SessionOf, &r1, &s1);

    let o1 = memory(
        builder,
        "O1",
        NodeKind::Observation,
        "obs-1",
        Some("sess-1"),
        Some("2026-03-01T11:00:00Z"),
        "Observation on alpha",
        Some(&format!("{RAW_OBS_TEXT}: alpha looks wrong")),
        None,
    );
    am_edge(builder, EdgeLabel::AuthoredBy, &o1, &s1);
    am_edge(builder, EdgeLabel::MentionsSymbol, &o1, &sym_a);
    am_edge(builder, EdgeLabel::ReferencesTask, &o1, &task_a);

    // D1 carries its code citation ONLY as an on-node evidence link — no Edge
    // record — so the lane must read both representations.
    let d1 = memory(
        builder,
        "D1",
        NodeKind::Decision,
        "dec-1",
        Some("sess-1"),
        Some("2026-03-01T12:30:00Z"),
        "Decision about alpha",
        None,
        Some(vec![link(&file_a, "codegraph", "TOUCHED_FILE")]),
    );
    am_edge(builder, EdgeLabel::AuthoredBy, &d1, &s1);

    let f1 = memory(
        builder,
        "F1",
        NodeKind::Failure,
        "fail-1",
        Some("sess-1"),
        Some("2026-03-01T09:00:00Z"),
        "Failure on alpha",
        None,
        None,
    );
    am_edge(builder, EdgeLabel::AuthoredBy, &f1, &s1);
    am_edge(builder, EdgeLabel::FailedOn, &f1, &sym_a);

    // Raw-payload carriers hanging off the run: their bytes must never surface.
    let command_run_id = verification_stable_id(&["verification", "command-run-1"]);
    let mut command_run = GraphRecord::node(
        command_run_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "cargo test".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        status,
        verification_kind,
        executed_at,
        stdout_handle,
        ..
    } = &mut command_run
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
        *verification_kind = Some("command_run".to_owned());
        *executed_at = Some("2026-03-01T10:06:00Z".to_owned());
        *stdout_handle = Some(Box::new(OutputHandle {
            inline: Some(RAW_STDOUT.to_owned()),
            hash: "blake3:stdouthash".to_owned(),
            bytes: 64,
        }));
    }
    builder.push(command_run);
    builder.label("CommandRun", &command_run_id);
    am_edge(builder, EdgeLabel::AuthoredBy, &command_run_id, &r1);

    let patch_id = artifact_stable_id(&["patch", "patch-1"]);
    let mut patch = GraphRecord::node(
        patch_id.clone(),
        NodeKind::PatchArtifact,
        None,
        None,
        None,
        "Patch for alpha".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        patch_status,
        patch_bytes_hash,
        patch_handle,
        ..
    } = &mut patch
    {
        *schema_version = ARTIFACT_SCHEMA_VERSION;
        *patch_status = Some("applied".to_owned());
        *patch_bytes_hash = Some("blake3:patchhash".to_owned());
        *patch_handle = Some(Box::new(PatchHandle {
            path: "protected/patch-1.patch".to_owned(),
            inline: Some(RAW_PATCH.to_owned()),
        }));
    }
    builder.push(patch);
    builder.label("PatchArtifact", &patch_id);
    am_edge(builder, EdgeLabel::AuthoredBy, &patch_id, &r1);

    // ── S2 (written SECOND, sorts FIRST) ────────────────────────────────────
    let s2 = session(
        builder,
        "S2",
        "sess-2",
        Some("agent-2"),
        Some("2026-03-02T07:00:00Z"),
        "AgentSession sess-2",
    );
    am_edge(builder, EdgeLabel::SessionOf, &s2, &ag2);
    let r2 = run(
        builder,
        "R2",
        "run-2",
        "sess-2",
        Some("2026-03-02T07:10:00Z"),
        "AgentRun claude-code",
    );
    am_edge(builder, EdgeLabel::SessionOf, &r2, &s2);

    let u2 = turn(builder, "U2", "turn-2", "sess-2");
    am_edge(builder, EdgeLabel::AuthoredBy, &u2, &r2);

    let o2 = memory(
        builder,
        "O2",
        NodeKind::Observation,
        "obs-2",
        Some("sess-2"),
        Some("2026-03-02T08:00:00Z"),
        "Observation from run 2",
        None,
        None,
    );
    am_edge(builder, EdgeLabel::AuthoredBy, &o2, &r2);
    am_edge(builder, EdgeLabel::ReferencesTask, &o2, &task_a2);

    // 3-hop membership: D2 -AUTHORED_BY-> U2 -AUTHORED_BY-> R2 -SESSION_OF-> S2.
    let d2 = memory(
        builder,
        "D2",
        NodeKind::Decision,
        "dec-2",
        Some("sess-2"),
        Some("2026-03-02T07:30:00Z"),
        "Decision from turn 2",
        None,
        None,
    );
    am_edge(builder, EdgeLabel::AuthoredBy, &d2, &u2);

    // ── S3: repo B only, newest of all ──────────────────────────────────────
    let s3 = session(
        builder,
        "S3",
        "sess-3",
        Some("agent-1"),
        Some("2026-03-05T00:00:00Z"),
        "AgentSession sess-3",
    );
    let o3 = memory(
        builder,
        "O3",
        NodeKind::Observation,
        "obs-3",
        Some("sess-3"),
        Some("2026-03-05T00:00:00Z"),
        "Observation on beta",
        None,
        None,
    );
    am_edge(builder, EdgeLabel::AuthoredBy, &o3, &s3);
    am_edge(builder, EdgeLabel::TouchedFile, &o3, &file_b);

    // ── S4: no citations anywhere → unresolvable repository scope ───────────
    let s4 = session(
        builder,
        "S4",
        "sess-4",
        Some("agent-1"),
        Some("2026-03-04T00:00:00Z"),
        "AgentSession sess-4",
    );
    let o4 = memory(
        builder,
        "O4",
        NodeKind::Observation,
        "obs-4",
        Some("sess-4"),
        Some("2026-03-04T00:00:00Z"),
        "Observation with no citation",
        None,
        None,
    );
    am_edge(builder, EdgeLabel::AuthoredBy, &o4, &s4);
}

fn seed_scope() -> Fixture {
    let mut builder = Builder::new();
    seed_scope_into(&mut builder);
    builder.finish("sessions_scope.jsonl")
}

/// Repo-A-only edge cases layered on top of [`seed_scope_into`].
#[allow(clippy::too_many_lines)]
fn seed_edge_cases() -> Fixture {
    let mut builder = Builder::new();
    seed_scope_into(&mut builder);
    let sym_a = builder.ids["SymA"].clone();

    // ── S5: no run at all, plus a task with an invalid status ───────────────
    let task_bad = task(
        &mut builder,
        "TaskBad",
        "task-bad",
        "frobnicated",
        "invalid status task",
    );
    let s5 = session(
        &mut builder,
        "S5",
        "sess-5",
        Some("agent-1"),
        Some("2026-03-03T00:00:00Z"),
        "AgentSession sess-5",
    );
    let o5 = memory(
        &mut builder,
        "O5",
        NodeKind::Observation,
        "obs-5",
        Some("sess-5"),
        Some("2026-03-03T00:00:00Z"),
        "Observation for session 5",
        None,
        None,
    );
    am_edge(&mut builder, EdgeLabel::AuthoredBy, &o5, &s5);
    am_edge(&mut builder, EdgeLabel::MentionsSymbol, &o5, &sym_a);
    am_edge(&mut builder, EdgeLabel::ReferencesTask, &o5, &task_bad);

    // ── S6 / S7: identical last_activity → tie-break on record ID ───────────
    for (label, session_key, obs_label, obs_key) in [
        ("S6", "sess-6", "O6", "obs-6"),
        ("S7", "sess-7", "O7", "obs-7"),
    ] {
        let s = session(
            &mut builder,
            label,
            session_key,
            Some("agent-1"),
            Some("2026-04-01T00:00:00Z"),
            &format!("AgentSession {session_key}"),
        );
        let o = memory(
            &mut builder,
            obs_label,
            NodeKind::Observation,
            obs_key,
            Some(session_key),
            Some("2026-04-01T00:00:00Z"),
            "Observation at the tie instant",
            None,
            None,
        );
        am_edge(&mut builder, EdgeLabel::AuthoredBy, &o, &s);
        am_edge(&mut builder, EdgeLabel::MentionsSymbol, &o, &sym_a);
    }

    // ── S8: no timestamps anywhere ──────────────────────────────────────────
    let s8 = session(
        &mut builder,
        "S8",
        "sess-8",
        Some("agent-1"),
        None,
        "AgentSession sess-8",
    );
    let o8 = memory(
        &mut builder,
        "O8",
        NodeKind::Observation,
        "obs-8",
        Some("sess-8"),
        None,
        "Observation with no observed_at",
        None,
        None,
    );
    am_edge(&mut builder, EdgeLabel::AuthoredBy, &o8, &s8);
    am_edge(&mut builder, EdgeLabel::MentionsSymbol, &o8, &sym_a);

    // ── S9: two runs → multiple_runs, ordered by observed_at ────────────────
    let s9 = session(
        &mut builder,
        "S9",
        "sess-9",
        Some("agent-1"),
        Some("2026-03-10T00:00:00Z"),
        "AgentSession sess-9",
    );
    let r9a = run(
        &mut builder,
        "R9a",
        "run-9a",
        "sess-9",
        Some("2026-03-10T01:00:00Z"),
        "AgentRun outcome=success exit_reason=completed",
    );
    am_edge(&mut builder, EdgeLabel::SessionOf, &r9a, &s9);
    let r9b = run(
        &mut builder,
        "R9b",
        "run-9b",
        "sess-9",
        Some("2026-03-10T02:00:00Z"),
        "AgentRun outcome=failure exit_reason=timeout",
    );
    am_edge(&mut builder, EdgeLabel::SessionOf, &r9b, &s9);
    let o9 = memory(
        &mut builder,
        "O9",
        NodeKind::Observation,
        "obs-9",
        Some("sess-9"),
        Some("2026-03-10T01:30:00Z"),
        "Observation for session 9",
        None,
        None,
    );
    am_edge(&mut builder, EdgeLabel::AuthoredBy, &o9, &s9);
    am_edge(&mut builder, EdgeLabel::MentionsSymbol, &o9, &sym_a);

    // ── S10: malformed outcome template ─────────────────────────────────────
    let s10 = session(
        &mut builder,
        "S10",
        "sess-10",
        Some("agent-1"),
        Some("2026-03-11T00:00:00Z"),
        "AgentSession sess-10",
    );
    // R10's `observed_at` is not a timestamp at all: the digest must parse and
    // re-render the field, so an unparseable value becomes `null` rather than
    // riding out verbatim.
    let r10 = run(
        &mut builder,
        "R10",
        "run-10",
        "sess-10",
        Some(RAW_OBSERVED_AT),
        "AgentRun outcome=succ ess\nLEAK exit_reason=x",
    );
    am_edge(&mut builder, EdgeLabel::SessionOf, &r10, &s10);
    let o10 = memory(
        &mut builder,
        "O10",
        NodeKind::Observation,
        "obs-10",
        Some("sess-10"),
        Some("2026-03-11T00:20:00Z"),
        "Observation for session 10",
        None,
        None,
    );
    am_edge(&mut builder, EdgeLabel::AuthoredBy, &o10, &s10);
    am_edge(&mut builder, EdgeLabel::MentionsSymbol, &o10, &sym_a);

    // ── Ox: session_id-stamped but NOT edge-linked to S1 ────────────────────
    let ox = memory(
        &mut builder,
        "Ox",
        NodeKind::Observation,
        "obs-x-unlinked",
        Some("sess-1"),
        Some("2026-03-01T13:00:00Z"),
        "Observation stamped sess-1 with no AUTHORED_BY edge",
        None,
        None,
    );
    am_edge(&mut builder, EdgeLabel::MentionsSymbol, &ox, &sym_a);

    // ── S11: created, then tombstoned ───────────────────────────────────────
    let s11 = session(
        &mut builder,
        "S11",
        "sess-11",
        Some("agent-1"),
        Some("2026-05-01T00:00:00Z"),
        "AgentSession sess-11",
    );
    let o11 = memory(
        &mut builder,
        "O11",
        NodeKind::Observation,
        "obs-11",
        Some("sess-11"),
        Some("2026-05-01T00:00:00Z"),
        "Observation for session 11",
        None,
        None,
    );
    am_edge(&mut builder, EdgeLabel::AuthoredBy, &o11, &s11);
    am_edge(&mut builder, EdgeLabel::MentionsSymbol, &o11, &sym_a);
    tombstone(&mut builder, &s11);

    // ── S12: duplicate physical writes of the session AND its member ────────
    let s12 = session(
        &mut builder,
        "S12",
        "sess-12",
        Some("agent-1"),
        Some("2026-03-12T00:00:00Z"),
        "AgentSession sess-12",
    );
    let o12 = memory(
        &mut builder,
        "O12",
        NodeKind::Observation,
        "obs-12",
        Some("sess-12"),
        Some("2026-03-12T01:00:00Z"),
        "Observation for session 12",
        None,
        None,
    );
    am_edge(&mut builder, EdgeLabel::AuthoredBy, &o12, &s12);
    am_edge(&mut builder, EdgeLabel::MentionsSymbol, &o12, &sym_a);
    // Byte-identical second writes of the same two nodes.
    let s12_dup = builder
        .records
        .iter()
        .find(|r| r.id() == s12)
        .expect("S12 node present")
        .clone();
    let o12_dup = builder
        .records
        .iter()
        .find(|r| r.id() == o12)
        .expect("O12 node present")
        .clone();
    builder.push(s12_dup);
    builder.push(o12_dup);

    // ── S13: two live SESSION_OF edges to distinct Agent nodes ──────────────
    // Ingest validates SESSION_OF endpoint kinds but not the documented
    // many-to-one cardinality, so this is reachable; the digest must refuse
    // to guess an `agent_record_id` and instead raise
    // `ambiguous_agent_provenance` naming both candidates — on BOTH the JSON
    // and `--format text` surfaces.
    let ag1 = builder.ids["Ag1"].clone();
    let ag2 = builder.ids["Ag2"].clone();
    let s13 = session(
        &mut builder,
        "S13",
        "sess-13",
        Some("agent-1"),
        Some("2026-03-13T00:00:00Z"),
        "AgentSession sess-13",
    );
    let o13 = memory(
        &mut builder,
        "O13",
        NodeKind::Observation,
        "obs-13",
        Some("sess-13"),
        Some("2026-03-13T00:00:00Z"),
        "Observation for session 13",
        None,
        None,
    );
    am_edge(&mut builder, EdgeLabel::AuthoredBy, &o13, &s13);
    am_edge(&mut builder, EdgeLabel::MentionsSymbol, &o13, &sym_a);
    am_edge(&mut builder, EdgeLabel::SessionOf, &s13, &ag1);
    am_edge(&mut builder, EdgeLabel::SessionOf, &s13, &ag2);

    builder.finish("sessions_edge_cases.jsonl")
}

/// One session scoped to BOTH repositories by two DIFFERENT bases: a direct
/// code citation into repo A, and a task reference that only reaches repo B.
fn seed_multi_repo() -> Fixture {
    let mut builder = Builder::new();
    let repo_a = repository(&mut builder, "RepoA", "repo-a", "repo-a");
    let (_file_a, sym_a) =
        code_topology(&mut builder, &repo_a, "FileA", "SymA", "src/a.rs", "alpha");
    let repo_b = repository(&mut builder, "RepoB", "repo-b", "repo-b");
    let (file_b, _sym_b) =
        code_topology(&mut builder, &repo_b, "FileB", "SymB", "src/b.rs", "beta");

    let task_b = task(&mut builder, "TaskB", "task-b", "open", "beta follow-up");
    proj_edge(&mut builder, EdgeLabel::TouchesFile, &task_b, &file_b);

    let s = session(
        &mut builder,
        "S",
        "sess-multi",
        Some("agent-1"),
        Some("2026-03-01T10:00:00Z"),
        "AgentSession sess-multi",
    );
    let o = memory(
        &mut builder,
        "O",
        NodeKind::Observation,
        "obs-multi",
        Some("sess-multi"),
        Some("2026-03-01T11:00:00Z"),
        "Observation spanning two repositories",
        None,
        None,
    );
    am_edge(&mut builder, EdgeLabel::AuthoredBy, &o, &s);
    am_edge(&mut builder, EdgeLabel::MentionsSymbol, &o, &sym_a);
    am_edge(&mut builder, EdgeLabel::ReferencesTask, &o, &task_b);
    builder.finish("sessions_multi_repo.jsonl")
}

/// One session carrying 21 runs and 21 referenced tasks — one over each
/// per-row cap.
fn seed_over_row_caps() -> Fixture {
    let mut builder = Builder::new();
    let repo = repository(&mut builder, "RepoA", "repo-a", "repo-a");
    let (_file, sym) = code_topology(&mut builder, &repo, "FileA", "SymA", "src/a.rs", "alpha");

    let s = session(
        &mut builder,
        "S",
        "sess-cap",
        Some("agent-1"),
        Some("2026-03-01T00:00:00Z"),
        "AgentSession sess-cap",
    );
    let o = memory(
        &mut builder,
        "O",
        NodeKind::Observation,
        "obs-cap",
        Some("sess-cap"),
        Some("2026-03-01T00:30:00Z"),
        "Observation for the capped session",
        None,
        None,
    );
    am_edge(&mut builder, EdgeLabel::AuthoredBy, &o, &s);
    am_edge(&mut builder, EdgeLabel::MentionsSymbol, &o, &sym);

    for index in 0..21 {
        let r = run(
            &mut builder,
            &format!("R{index:02}"),
            &format!("run-{index:02}"),
            "sess-cap",
            Some(&format!("2026-03-01T{index:02}:00:00Z")),
            "AgentRun claude-code",
        );
        am_edge(&mut builder, EdgeLabel::SessionOf, &r, &s);

        let t = task(
            &mut builder,
            &format!("T{index:02}"),
            &format!("task-{index:02}"),
            "open",
            "capped task",
        );
        am_edge(&mut builder, EdgeLabel::ReferencesTask, &o, &t);
    }
    builder.finish("sessions_over_row_caps.jsonl")
}

/// A repository with code but ZERO agent-memory records.
fn seed_empty_repo() -> Fixture {
    let mut builder = Builder::new();
    let repo_c = repository(&mut builder, "RepoC", "repo-c", "repo-c");
    code_topology(&mut builder, &repo_c, "FileC", "SymC", "src/c.rs", "gamma");
    builder.finish("sessions_empty_repo.jsonl")
}

/// One repository whose display name (the node's own `name`, distinct from
/// the safe record-ID selector this test resolves by) carries control
/// characters, so a naive `--format text` renderer would forge output lines.
fn seed_hostile_repository_display() -> Fixture {
    let mut builder = Builder::new();
    let repo_id = repository(
        &mut builder,
        "RepoHostile",
        "REPO_DISPLAY_LEAK\u{7}\nFORGED_LINE",
        "repo-hostile",
    );
    code_topology(
        &mut builder,
        &repo_id,
        "FileHostile",
        "SymHostile",
        "src/hostile.rs",
        "hostile",
    );
    builder.finish("sessions_hostile_repo_display.jsonl")
}

/// Two repositories sharing the selector `widget` so a bare basename is
/// ambiguous.
fn seed_ambiguous_repos() -> Fixture {
    let mut builder = Builder::new();
    for display in ["acme/widget-a", "acme/widget-b"] {
        let id = stable_id(&["repository", "operator-override", display]);
        builder.push(
            GraphRecord::node(
                id.clone(),
                NodeKind::Repository,
                None,
                None,
                Some(display.to_owned()),
                format!("Repository {display}"),
            )
            .with_repository_identity(RepositoryIdentityPayload {
                identity_source: IdentitySource::OperatorOverride,
                remote_url: None,
                root_commit_sha: None,
                canonical_path: None,
                basename: "widget".to_owned(),
            }),
        );
        builder.label(display, &id);
    }
    builder.finish("sessions_ambiguous.jsonl")
}

// ---------------------------------------------------------------------------
// Invocation helpers
// ---------------------------------------------------------------------------

fn run_sessions(fx: &Fixture, selector: &str, extra: &[&str]) -> (i32, String, String) {
    let assert = egregore()
        .args(["query", "sessions", selector, "--graph"])
        .arg(&fx.graph)
        .args(extra)
        .assert();
    let output = assert.get_output().clone();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8(output.stderr).expect("utf8 stderr"),
    )
}

fn digest(fx: &Fixture, selector: &str, extra: &[&str]) -> serde_json::Value {
    let (code, stdout, stderr) = run_sessions(fx, selector, extra);
    assert_eq!(
        code, 0,
        "`eg query sessions {selector}` must exit 0; stdout={stdout} stderr={stderr}"
    );
    serde_json::from_str(stdout.trim()).expect("stdout must be one JSON envelope")
}

fn rows(value: &serde_json::Value) -> Vec<serde_json::Value> {
    value["sessions"]
        .as_array()
        .expect("envelope must carry a sessions array")
        .clone()
}

fn row_ids(value: &serde_json::Value) -> Vec<String> {
    rows(value)
        .iter()
        .map(|r| {
            r["session_record_id"]
                .as_str()
                .expect("row must carry session_record_id")
                .to_owned()
        })
        .collect()
}

fn row_for(value: &serde_json::Value, id: &str) -> serde_json::Value {
    value["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .find(|r| r["session_record_id"] == id)
        .unwrap_or_else(|| panic!("digest must contain a row for {id}: {value}"))
        .clone()
}

fn diagnostic(value: &serde_json::Value, code: &str) -> Option<serde_json::Value> {
    value["diagnostics"]
        .as_array()
        .expect("envelope must carry a diagnostics array")
        .iter()
        .find(|d| d["code"] == code)
        .cloned()
}

// ---------------------------------------------------------------------------
// AC: repository scoping
// ---------------------------------------------------------------------------

#[test]
fn digest_returns_exactly_s1_and_s2_for_repo_a() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);

    assert_eq!(value["ok"], true);
    assert_eq!(value["lane"], "sessions");
    assert_eq!(value["repository_id"], fx.id("RepoA"));

    let mut ids = row_ids(&value);
    ids.sort();
    let mut expected = vec![fx.id("S1").to_owned(), fx.id("S2").to_owned()];
    expected.sort();
    assert_eq!(
        ids, expected,
        "repo A's digest must be exactly S1 and S2: {value}"
    );
}

#[test]
fn s3_never_appears_in_repo_a_digest() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);
    assert!(
        !row_ids(&value).contains(&fx.id("S3").to_owned()),
        "S3 is scoped to repo B only and must never appear in repo A: {value}"
    );

    // …and it is the ONLY session in repo B's digest.
    let repo_b = digest(&fx, "repo-b", &[]);
    assert_eq!(row_ids(&repo_b), vec![fx.id("S3").to_owned()]);
}

#[test]
fn s4_reported_under_unresolved_repository_scope_diagnostic() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);

    assert!(
        !row_ids(&value).contains(&fx.id("S4").to_owned()),
        "a session with no resolvable repository scope must not be a row: {value}"
    );
    let diag = diagnostic(&value, "unresolved_repository_scope")
        .unwrap_or_else(|| panic!("unresolved_repository_scope diagnostic required: {value}"));
    let listed: Vec<&str> = diag["session_record_ids"]
        .as_array()
        .expect("session_record_ids array")
        .iter()
        .map(|v| v.as_str().expect("record id string"))
        .collect();
    assert!(
        listed.contains(&fx.id("S4")),
        "S4 must be named in the unresolved_repository_scope diagnostic: {diag}"
    );
    assert_eq!(
        diag["count"],
        serde_json::json!(listed.len()),
        "count must equal the listed set size: {diag}"
    );
    let mut sorted = listed.clone();
    sorted.sort_unstable();
    assert_eq!(
        listed, sorted,
        "session_record_ids must be sorted ascending"
    );
}

// ---------------------------------------------------------------------------
// AC: ordering
// ---------------------------------------------------------------------------

#[test]
fn sessions_ordered_by_last_activity_descending() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);
    assert_eq!(
        row_ids(&value),
        vec![fx.id("S2").to_owned(), fx.id("S1").to_owned()],
        "S2 (last activity 2026-03-02T08:00:00Z) must precede S1 (2026-03-01T12:30:00Z) \
         even though S1 was written first: {value}"
    );
}

#[test]
fn identical_last_activity_ties_break_on_session_record_id() {
    let fx = seed_edge_cases();
    let value = digest(&fx, "repo-a", &[]);
    let ids = row_ids(&value);

    let (s6, s7) = (fx.id("S6").to_owned(), fx.id("S7").to_owned());
    let i6 = ids.iter().position(|id| *id == s6).expect("S6 row");
    let i7 = ids.iter().position(|id| *id == s7).expect("S7 row");
    let mut tied = [s6.clone(), s7.clone()];
    tied.sort();
    let observed = if i6 < i7 { [s6, s7] } else { [s7, s6] };
    assert_eq!(
        observed, tied,
        "sessions tied on last_activity must be ordered by session_record_id ascending: {value}"
    );
    assert_eq!(
        i7.abs_diff(i6),
        1,
        "the two tied sessions must be adjacent: {value}"
    );
    // They share the maximum last_activity in the fixture, so they lead.
    assert_eq!(i6.min(i7), 0, "the tied pair carries the newest activity");
}

#[test]
fn session_without_timestamps_sorts_last_with_absent_time_basis() {
    let fx = seed_edge_cases();
    let value = digest(&fx, "repo-a", &[]);
    let ids = row_ids(&value);
    assert_eq!(
        ids.last().map(String::as_str),
        Some(fx.id("S8")),
        "a session with no parseable timestamp anywhere sorts LAST: {value}"
    );

    let row = row_for(&value, fx.id("S8"));
    assert_eq!(row["time_basis"], "absent");
    assert_eq!(row["time_source_count"], 0);
    for field in [
        "first_activity",
        "last_activity",
        "first_ingested_at",
        "last_ingested_at",
    ] {
        assert!(
            row[field].is_null(),
            "{field} must be null when no timestamp is recorded: {row}"
        );
    }
}

// ---------------------------------------------------------------------------
// AC: row shape — handles, time bounds, runs
// ---------------------------------------------------------------------------

#[test]
fn row_carries_agent_and_session_handles_and_time_bounds() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S1"));

    assert_eq!(row["session_record_id"], fx.id("S1"));
    assert_eq!(row["agent_record_id"], fx.id("Ag1"));
    assert_eq!(row["agent_id"], "agent-1");
    assert_eq!(row["session_id"], "sess-1");
    assert_eq!(
        row["first_activity"], "2026-03-01T09:00:00Z",
        "first activity is F1's observed_at: {row}"
    );
    assert_eq!(
        row["last_activity"], "2026-03-01T12:30:00Z",
        "last activity is D1's observed_at: {row}"
    );
    assert_eq!(row["time_basis"], "derived_from_member_observed_at");
    assert_eq!(
        row["time_source_count"], 5,
        "S1, R1, O1, D1, F1 each contribute one parseable observed_at: {row}"
    );
    assert_eq!(
        row["repository_scope"],
        serde_json::json!([fx.id("RepoA")]),
        "S1 resolves to repo A only: {row}"
    );
    // The summary is a SAFE label plus a hash — never the stored summary.
    assert!(
        row["summary_label"]
            .as_str()
            .is_some_and(|l| !l.is_empty() && !l.contains(RAW_SESSION_SUMMARY)),
        "summary_label must be a redaction-safe label: {row}"
    );
    assert!(
        row["summary_hash"]
            .as_str()
            .is_some_and(|h| h.starts_with("blake3:")),
        "summary_hash must be a blake3 handle: {row}"
    );

    // S2 carries its own SESSION_OF edge to a DIFFERENT agent: the handle is
    // resolved per session, never inherited from another row.
    let s2 = row_for(&value, fx.id("S2"));
    assert_eq!(
        s2["agent_record_id"],
        fx.id("Ag2"),
        "each session's agent handle comes from its OWN SESSION_OF edge: {s2}"
    );
    assert_eq!(s2["agent_id"], "agent-2");
    assert_eq!(s2["session_id"], "sess-2");
    assert_eq!(s2["first_activity"], "2026-03-02T07:00:00Z");
    assert_eq!(s2["last_activity"], "2026-03-02T08:00:00Z");
}

#[test]
fn run_outcome_parsed_from_producer_template() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S1"));

    assert_eq!(row["run_status"], "outcome_recorded");
    let runs = row["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1, "S1 has exactly one run: {row}");
    assert_eq!(runs[0]["run_record_id"], fx.id("R1"));
    assert_eq!(runs[0]["outcome"], "success");
    assert_eq!(runs[0]["exit_reason"], "completed");
    assert_eq!(runs[0]["observed_at"], "2026-03-01T10:05:00Z");
}

#[test]
fn non_template_run_summary_reports_outcome_unrecorded() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S2"));

    assert_eq!(row["run_status"], "outcome_unrecorded");
    let runs = row["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["run_record_id"], fx.id("R2"));
    assert!(
        runs[0]["outcome"].is_null() && runs[0]["exit_reason"].is_null(),
        "a non-template run summary must never be guessed into an outcome: {row}"
    );
}

#[test]
fn session_without_session_of_edge_has_null_agent_record_id() {
    let fx = seed_edge_cases();
    let value = digest(&fx, "repo-a", &[]);
    // S5 has no `SESSION_OF` edge at all. A stamped `agent_id` string is not a
    // citable agent handle, so the handle must be null — never invented from
    // the string, and never borrowed from another session's agent.
    let row = row_for(&value, fx.id("S5"));
    assert_eq!(
        row["agent_id"], "agent-1",
        "the stamped string is still reported: {row}"
    );
    assert!(
        row["agent_record_id"].is_null(),
        "a session with no SESSION_OF edge has no agent record handle: {row}"
    );
}

#[test]
fn session_without_run_reports_run_absent() {
    let fx = seed_edge_cases();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S5"));

    assert_eq!(row["run_status"], "run_absent");
    assert_eq!(
        row["runs"],
        serde_json::json!([]),
        "run_absent must carry an EMPTY runs array, never a fabricated entry: {row}"
    );
}

#[test]
fn multiple_runs_reported_as_array_in_deterministic_order() {
    let fx = seed_edge_cases();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S9"));

    assert_eq!(row["run_status"], "multiple_runs");
    let runs = row["runs"].as_array().expect("runs array");
    assert_eq!(
        runs.len(),
        2,
        "both runs are reported, none silently won: {row}"
    );
    assert_eq!(runs[0]["run_record_id"], fx.id("R9a"));
    assert_eq!(runs[0]["outcome"], "success");
    assert_eq!(runs[0]["exit_reason"], "completed");
    assert_eq!(runs[1]["run_record_id"], fx.id("R9b"));
    assert_eq!(runs[1]["outcome"], "failure");
    assert_eq!(runs[1]["exit_reason"], "timeout");
}

#[test]
fn malformed_outcome_is_not_emitted() {
    let fx = seed_edge_cases();
    let (code, stdout, stderr) = run_sessions(&fx, "repo-a", &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON envelope");
    let row = row_for(&value, fx.id("S10"));

    assert_eq!(row["run_status"], "outcome_unrecorded");
    let runs = row["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    assert!(
        runs[0]["outcome"].is_null() && runs[0]["exit_reason"].is_null(),
        "a malformed outcome template must never yield an outcome: {row}"
    );

    let diag = diagnostic(&value, "outcome_not_enum_shaped")
        .unwrap_or_else(|| panic!("outcome_not_enum_shaped diagnostic required: {value}"));
    assert_eq!(
        diag["run_record_id"],
        fx.id("R10"),
        "the diagnostic names the run, not its bytes: {diag}"
    );

    // The malformed bytes themselves must never reach stdout.
    assert!(
        !stdout.contains("succ ess"),
        "the malformed outcome bytes must not appear on stdout"
    );
    assert!(
        !stdout.contains("LEAK"),
        "no raw run-summary bytes may appear on stdout"
    );
}

// ---------------------------------------------------------------------------
// AC: counts
// ---------------------------------------------------------------------------

#[test]
fn per_kind_counts_match_authored_members() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);

    let s1 = row_for(&value, fx.id("S1"));
    assert_eq!(s1["record_counts"]["observation"], 1);
    assert_eq!(s1["record_counts"]["decision"], 1);
    assert_eq!(s1["record_counts"]["failure"], 1);

    let s2 = row_for(&value, fx.id("S2"));
    assert_eq!(s2["record_counts"]["observation"], 1);
    assert_eq!(s2["record_counts"]["decision"], 1);
    assert_eq!(
        s2["record_counts"]["failure"], 0,
        "an absent kind counts ZERO, never null: {s2}"
    );
}

#[test]
fn lesson_count_is_null_and_listed_unsupported() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);
    assert_eq!(
        value["unsupported_count_kinds"],
        serde_json::json!(["lesson"]),
        "the envelope must disclose that `lesson` has no backing node kind: {value}"
    );
    for row in rows(&value) {
        assert!(
            row["record_counts"]["lesson"].is_null(),
            "lesson must be JSON null (unsupported), never 0: {row}"
        );
    }
}

#[test]
fn duplicate_record_writes_do_not_inflate_counts() {
    let fx = seed_edge_cases();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S12"));
    assert_eq!(
        row["record_counts"]["observation"], 1,
        "a member written twice counts ONCE (distinct record IDs): {row}"
    );
    assert_eq!(
        row_ids(&value)
            .iter()
            .filter(|id| *id == fx.id("S12"))
            .count(),
        1,
        "a session written twice yields ONE row: {value}"
    );
}

#[test]
fn session_stamped_records_without_authored_by_are_not_counted() {
    let fx = seed_edge_cases();
    let value = digest(&fx, "repo-a", &[]);

    // Ox is stamped session_id=sess-1 but has no AUTHORED_BY edge: it is not a
    // member of S1 and must not inflate S1's counts.
    let s1 = row_for(&value, fx.id("S1"));
    assert_eq!(
        s1["record_counts"]["observation"], 1,
        "membership is edge-derived only; a session_id string match is not membership: {s1}"
    );

    let diag = diagnostic(&value, "unlinked_session_stamped_records")
        .unwrap_or_else(|| panic!("unlinked_session_stamped_records diagnostic required: {value}"));
    assert_eq!(
        diag["count"], 1,
        "exactly one unlinked stamped record: {diag}"
    );
}

#[test]
fn three_hop_membership_via_turn_and_run_is_counted() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S2"));

    // D2 -AUTHORED_BY-> U2(AgentTurn) -AUTHORED_BY-> R2(AgentRun) -SESSION_OF-> S2
    assert_eq!(
        row["record_counts"]["decision"], 1,
        "a decision three hops from the session is a member: {row}"
    );
    assert_eq!(
        row["last_activity"], "2026-03-02T08:00:00Z",
        "run/turn members contribute to the time bounds: {row}"
    );
}

// ---------------------------------------------------------------------------
// AC: repository scope derivation
// ---------------------------------------------------------------------------

#[test]
fn evidence_link_only_citation_confers_repo_scope() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S1"));

    let bases: Vec<&str> = row["scope_basis"]
        .as_array()
        .expect("scope_basis array")
        .iter()
        .map(|v| v.as_str().expect("basis string"))
        .collect();
    // D1's ONLY citation is an on-node evidence_link (TOUCHED_FILE → FileA);
    // together with O1/F1's edge citations it makes S1 code-cited into repo A.
    assert!(
        bases.contains(&"code_citation"),
        "an on-node evidence_link citation must confer code_citation scope: {row}"
    );
    let mut sorted = bases.clone();
    sorted.sort_unstable();
    assert_eq!(bases, sorted, "scope_basis must be sorted ascending: {row}");
    for basis in &bases {
        assert!(
            SCOPE_BASES.contains(basis),
            "scope_basis must come from the closed set {SCOPE_BASES:?}: {row}"
        );
    }
}

#[test]
fn task_reference_path_confers_scope_with_task_reference_basis() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S2"));

    // S2 has NO direct code citation. Its only path to repo A is
    // O2 -REFERENCES_TASK-> TaskA2 -TOUCHES_FILE-> FileA.
    assert_eq!(
        row["scope_basis"],
        serde_json::json!(["task_reference"]),
        "a task-mediated scope must be labelled task_reference only: {row}"
    );
    assert_eq!(row["repository_scope"], serde_json::json!([fx.id("RepoA")]));
}

// ---------------------------------------------------------------------------
// AC: task references
// ---------------------------------------------------------------------------

#[test]
fn task_refs_carry_status_and_project_state_trust_class() {
    let fx = seed_scope();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S1"));

    let tasks = row["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 1, "S1 references exactly TaskA: {row}");
    assert_eq!(tasks[0]["record_id"], fx.id("TaskA"));
    assert_eq!(tasks[0]["status"], "in_progress");
    assert_eq!(tasks[0]["status_recorded"], true);
    assert_eq!(
        tasks[0]["trust_class"], "project_state",
        "task status is a project-domain fact, not an agent claim: {row}"
    );
}

#[test]
fn invalid_task_status_reported_as_unknown_not_recorded() {
    let fx = seed_edge_cases();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S5"));

    let tasks = row["tasks"].as_array().expect("tasks array");
    let bad = tasks
        .iter()
        .find(|t| t["record_id"] == fx.id("TaskBad"))
        .unwrap_or_else(|| panic!("S5 must reference TaskBad: {row}"));
    assert_eq!(
        bad["status"], "unknown",
        "an out-of-vocabulary status must normalize to unknown, never pass through: {row}"
    );
    assert_eq!(
        bad["status_recorded"], false,
        "status_recorded must be false when the recorded status is invalid: {row}"
    );
}

// ---------------------------------------------------------------------------
// AC: liveness and empty results
// ---------------------------------------------------------------------------

#[test]
fn tombstoned_session_is_excluded() {
    let fx = seed_edge_cases();
    let (code, stdout, stderr) = run_sessions(&fx, "repo-a", &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON envelope");

    assert!(
        !row_ids(&value).contains(&fx.id("S11").to_owned()),
        "a tombstoned session must not be a row: {value}"
    );
    assert!(
        !stdout.contains(fx.id("S11")),
        "a tombstoned session's handle must not appear anywhere in the answer"
    );
}

#[test]
fn zero_session_repo_returns_no_sessions_signal_exit_zero() {
    let fx = seed_empty_repo();
    let (code, stdout, stderr) = run_sessions(&fx, "repo-c", &[]);
    assert_eq!(
        code, 0,
        "an empty digest is an explicit success, not an error; stderr={stderr}"
    );
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON envelope");
    assert_eq!(value["sessions"], serde_json::json!([]));
    assert!(
        diagnostic(&value, "no_sessions").is_some(),
        "an empty digest must carry the no_sessions diagnostic: {value}"
    );
    assert_eq!(value["repository_id"], fx.id("RepoC"));
}

// ---------------------------------------------------------------------------
// AC: selector errors and limit validation
// ---------------------------------------------------------------------------

#[test]
fn unknown_selector_exits_one_with_stable_code() {
    let fx = seed_scope();
    let (code, stdout, stderr) = run_sessions(&fx, "no-such-repo", &[]);
    assert_eq!(code, 1, "an unknown selector exits 1; stdout={stdout}");
    let diag: serde_json::Value = stderr
        .lines()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .unwrap_or_else(|| panic!("stderr must carry a JSON diagnostic, got {stderr}"));
    assert_eq!(diag["code"], "unknown_repository_selector");
    assert_eq!(diag["selector"], "no-such-repo");
    assert!(
        stdout.trim().is_empty(),
        "a selector failure must not print a digest: {stdout}"
    );
}

#[test]
fn ambiguous_selector_exits_one_with_candidates() {
    let fx = seed_ambiguous_repos();
    let (code, stdout, stderr) = run_sessions(&fx, "widget", &[]);
    assert_eq!(code, 1, "an ambiguous selector exits 1; stdout={stdout}");
    let diag: serde_json::Value = stderr
        .lines()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .unwrap_or_else(|| panic!("stderr must carry a JSON diagnostic, got {stderr}"));
    assert_eq!(diag["code"], "ambiguous_repository_selector");
    assert_eq!(diag["selector"], "widget");
    let candidates: Vec<&str> = diag["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .map(|v| v.as_str().expect("candidate string"))
        .collect();
    let mut expected = vec![fx.id("acme/widget-a"), fx.id("acme/widget-b")];
    expected.sort_unstable();
    assert_eq!(
        candidates, expected,
        "every candidate repository must be listed, sorted ascending: {diag}"
    );
}

#[test]
fn limit_truncates_and_emits_results_truncated_with_true_total() {
    let fx = seed_edge_cases();
    let full = digest(&fx, "repo-a", &[]);
    let matched = rows(&full).len();
    assert!(
        matched > 1,
        "the edge-case fixture must hold several repo-A sessions: {full}"
    );
    assert!(
        diagnostic(&full, "results_truncated").is_none(),
        "an untruncated answer carries no results_truncated diagnostic: {full}"
    );

    let capped = digest(&fx, "repo-a", &["--limit", "1"]);
    assert_eq!(rows(&capped).len(), 1);
    assert_eq!(
        row_ids(&capped)[0],
        row_ids(&full)[0],
        "truncation happens AFTER ordering, so the newest session survives"
    );
    let diag = diagnostic(&capped, "results_truncated")
        .unwrap_or_else(|| panic!("results_truncated diagnostic required: {capped}"));
    assert_eq!(
        diag["matched"],
        serde_json::json!(matched),
        "the diagnostic must carry the TRUE total, not the returned count: {diag}"
    );
    assert_eq!(diag["returned"], 1);
    assert_eq!(diag["limit"], 1);
}

#[test]
fn limit_zero_and_over_max_rejected_with_invalid_limit() {
    let fx = seed_scope();
    for bad in ["0", "201"] {
        let (code, stdout, stderr) = run_sessions(&fx, "repo-a", &["--limit", bad]);
        assert_eq!(code, 1, "--limit {bad} must exit 1; stdout={stdout}");
        let diag: serde_json::Value = stderr
            .lines()
            .find_map(|l| serde_json::from_str(l.trim()).ok())
            .unwrap_or_else(|| panic!("stderr must carry a JSON diagnostic, got {stderr}"));
        assert_eq!(diag["ok"], false);
        assert_eq!(diag["error"]["code"], "invalid_limit");
        assert_eq!(
            diag["error"]["limit"],
            serde_json::json!(bad.parse::<u64>().expect("numeric limit"))
        );
        assert_eq!(diag["error"]["min"], 1);
        assert_eq!(diag["error"]["max"], 200);
        assert_eq!(
            diag["error"]["message"],
            "--limit must be between 1 and 200 (default 20)"
        );
        assert!(
            stdout.trim().is_empty(),
            "a rejected limit must not print a digest: {stdout}"
        );
    }
}

#[test]
fn negative_limit_is_invalid_limit_not_a_clap_parse_error() {
    // `--limit -1` is a WELL-FORMED (if out-of-range) integer: it must reach
    // this lane's own `invalid_limit` diagnostic, never clap's built-in parse
    // error (which would exit 2 with a generic usage message, bypassing the
    // documented machine-readable envelope).
    let fx = seed_scope();
    let (code, stdout, stderr) = run_sessions(&fx, "repo-a", &["--limit", "-1"]);
    assert_eq!(
        code, 1,
        "--limit -1 must exit 1 (not clap's exit 2); stderr={stderr}"
    );
    let diag: serde_json::Value = stderr
        .lines()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .unwrap_or_else(|| panic!("stderr must carry a JSON diagnostic, got {stderr}"));
    assert_eq!(diag["ok"], false);
    assert_eq!(diag["error"]["code"], "invalid_limit");
    assert_eq!(diag["error"]["limit"], serde_json::json!(-1));
    assert!(
        stdout.trim().is_empty(),
        "a rejected limit must not print a digest: {stdout}"
    );
}

#[test]
fn limit_beyond_i128_range_is_invalid_limit_not_a_clap_parse_error() {
    // Issue #112 review round 22: `--limit` is now a raw `String`, so a
    // decimal literal wider than EVEN i128 (one past i128::MAX) reaches this
    // lane's own `invalid_limit` diagnostic instead of clap's built-in parse
    // error -- proving there is no fixed-width ceiling left to overflow.
    let fx = seed_scope();
    let (code, stdout, stderr) = run_sessions(
        &fx,
        "repo-a",
        &["--limit", "170141183460469231731687303715884105728"], // i128::MAX + 1
    );
    assert_eq!(
        code, 1,
        "an i128-overflowing --limit must exit 1 (not clap's exit 2); stderr={stderr}"
    );
    let diag: serde_json::Value = stderr
        .lines()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .unwrap_or_else(|| panic!("stderr must carry a JSON diagnostic, got {stderr}"));
    assert_eq!(diag["ok"], false);
    assert_eq!(diag["error"]["code"], "invalid_limit");
    assert_eq!(
        diag["error"]["limit"],
        serde_json::json!("170141183460469231731687303715884105728"),
        "a value that never even parses as i128 renders as the raw decimal \
         token verbatim: {diag}"
    );
    assert!(
        stdout.trim().is_empty(),
        "a rejected limit must not print a digest: {stdout}"
    );
}

#[test]
fn limit_non_numeric_is_invalid_limit_not_a_clap_parse_error() {
    // The non-numeric sibling of the above: with `--limit` now a raw
    // `String`, clap no longer rejects a non-integer token at all -- it must
    // reach this lane's own diagnostic just like an out-of-range integer,
    // never a bare clap usage error.
    let fx = seed_scope();
    let (code, stdout, stderr) = run_sessions(&fx, "repo-a", &["--limit", "not-a-number"]);
    assert_eq!(
        code, 1,
        "a non-numeric --limit must exit 1 (not clap's exit 2); stderr={stderr}"
    );
    let diag: serde_json::Value = stderr
        .lines()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .unwrap_or_else(|| panic!("stderr must carry a JSON diagnostic, got {stderr}"));
    assert_eq!(diag["ok"], false);
    assert_eq!(diag["error"]["code"], "invalid_limit");
    assert_eq!(diag["error"]["limit"], serde_json::json!("not-a-number"));
    assert!(
        stdout.trim().is_empty(),
        "a rejected limit must not print a digest: {stdout}"
    );
}

#[test]
fn limit_wider_than_i64_is_invalid_limit_not_a_clap_parse_error() {
    // One past `i64::MAX`: still a well-formed positive integer, and clap's
    // built-in parser for a narrower integer type would reject it before
    // this lane's own range check ever runs.
    let fx = seed_scope();
    let (code, stdout, stderr) = run_sessions(&fx, "repo-a", &["--limit", "9223372036854775808"]);
    assert_eq!(
        code, 1,
        "an i64-overflowing --limit must exit 1 (not clap's exit 2); stderr={stderr}"
    );
    let diag: serde_json::Value = stderr
        .lines()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .unwrap_or_else(|| panic!("stderr must carry a JSON diagnostic, got {stderr}"));
    assert_eq!(diag["ok"], false);
    assert_eq!(diag["error"]["code"], "invalid_limit");
    assert!(
        stdout.trim().is_empty(),
        "a rejected limit must not print a digest: {stdout}"
    );
}

#[test]
fn limit_beyond_u64_range_is_invalid_limit_not_a_panic() {
    // Issue #112 review round 19: `--limit` accepts the FULL `i128` range,
    // so a value beyond BOTH `i64` and `u64` (unlike the sibling
    // i64-overflow test above, whose regression value still fits `u64`)
    // reaches the `invalid_limit` diagnostic builder. `serde_json::json!`
    // embeds the raw `i128` via `Serialize`, which PANICS for such a value
    // without the `arbitrary_precision` feature this crate does not enable
    // -- this must render as the exit-1 `invalid_limit` envelope, never
    // crash the process.
    let fx = seed_scope();
    let (code, stdout, stderr) = run_sessions(
        &fx,
        "repo-a",
        &["--limit", "18446744073709551616"], // u64::MAX + 1
    );
    assert_eq!(
        code, 1,
        "a u64-overflowing --limit must exit 1, not panic; stderr={stderr}"
    );
    let diag: serde_json::Value = stderr
        .lines()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .unwrap_or_else(|| panic!("stderr must carry a JSON diagnostic, got {stderr}"));
    assert_eq!(diag["ok"], false);
    assert_eq!(diag["error"]["code"], "invalid_limit");
    assert_eq!(
        diag["error"]["limit"],
        serde_json::json!("18446744073709551616"),
        "a value unrepresentable as a JSON number renders as its exact \
         decimal string, never silently truncated: {diag}"
    );
    assert!(
        stdout.trim().is_empty(),
        "a rejected limit must not print a digest: {stdout}"
    );
}

#[test]
fn limit_below_i64_min_is_invalid_limit_not_a_panic() {
    // The negative mirror of the beyond-u64 case: one past `i64::MIN`,
    // still within `i128`, unrepresentable as a `serde_json::Number`.
    let fx = seed_scope();
    let (code, stdout, stderr) = run_sessions(
        &fx,
        "repo-a",
        &["--limit", "-9223372036854775809"], // i64::MIN - 1
    );
    assert_eq!(
        code, 1,
        "an i64::MIN-underflowing --limit must exit 1, not panic; stderr={stderr}"
    );
    let diag: serde_json::Value = stderr
        .lines()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .unwrap_or_else(|| panic!("stderr must carry a JSON diagnostic, got {stderr}"));
    assert_eq!(diag["ok"], false);
    assert_eq!(diag["error"]["code"], "invalid_limit");
    assert_eq!(
        diag["error"]["limit"],
        serde_json::json!("-9223372036854775809"),
        "a value unrepresentable as a JSON number renders as its exact \
         decimal string, never silently truncated: {diag}"
    );
    assert!(
        stdout.trim().is_empty(),
        "a rejected limit must not print a digest: {stdout}"
    );
}

#[test]
fn text_format_prints_agent_record_id_and_ingested_bounds() {
    let fx = seed_edge_cases();
    let (code, stdout, stderr) = run_sessions(&fx, "repo-a", &["--format", "text"]);
    assert_eq!(code, 0, "stderr={stderr}");

    // S1 has a live SESSION_OF -> Agent edge (Ag1); the citable handle must be
    // printed, not just the uncitable stamped `agent_id` string.
    let value = digest(&fx, "repo-a", &[]);
    let s1_row = row_for(&value, fx.id("S1"));
    let s1_agent_record_id = s1_row["agent_record_id"]
        .as_str()
        .expect("S1 has an agent_record_id");
    assert!(
        stdout.contains(&format!("agent_record_id {s1_agent_record_id} ")),
        "the text renderer must print the edge-derived agent handle, got:\n{stdout}"
    );

    // S5 has no SESSION_OF edge at all: the text output must say so
    // explicitly, not merely omit the field.
    let s5_id = fx.id("S5");
    assert!(
        stdout.contains(&format!(
            "session {s5_id} [agent_authored] agent_record_id <absent>"
        )),
        "a session with no SESSION_OF edge must render agent_record_id <absent>, got:\n{stdout}"
    );

    // The ingested-at transaction-time bounds are a distinct pair from
    // first/last_activity and must not be silently dropped from text output.
    assert!(
        stdout.contains("  ingested "),
        "the text renderer must print the ingested-at bounds, got:\n{stdout}"
    );
}

#[test]
fn ambiguous_agent_provenance_diagnostic_names_candidates_in_both_formats() {
    // S13 has two live SESSION_OF edges to distinct Agent nodes. The digest
    // must refuse to guess agent_record_id and raise
    // ambiguous_agent_provenance naming both candidates on the JSON envelope
    // AND the --format text renderer — a text-only renderer bug would
    // silently report "ambiguous" without identifying who the candidates are.
    let fx = seed_edge_cases();
    let mut expected_candidates = [fx.id("Ag1").to_owned(), fx.id("Ag2").to_owned()];
    expected_candidates.sort();

    let value = digest(&fx, "repo-a", &[]);
    let s13_row = row_for(&value, fx.id("S13"));
    assert!(
        s13_row["agent_record_id"].is_null(),
        "an ambiguous agent must never be guessed: {s13_row}"
    );
    let diag = diagnostic(&value, "ambiguous_agent_provenance")
        .expect("ambiguous_agent_provenance diagnostic present in JSON");
    assert_eq!(diag["session_record_id"], fx.id("S13"));
    let candidates: Vec<String> = diag["candidate_ids"]
        .as_array()
        .expect("candidate_ids array")
        .iter()
        .map(|v| v.as_str().expect("candidate id is a string").to_owned())
        .collect();
    assert_eq!(candidates, expected_candidates);

    let (code, stdout, stderr) = run_sessions(&fx, "repo-a", &["--format", "text"]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(
        stdout.contains("ambiguous_agent_provenance"),
        "text output must surface the diagnostic code, got:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "candidates [{}, {}]",
            expected_candidates[0], expected_candidates[1]
        )),
        "text output must name both candidates, not just say 'ambiguous', got:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// AC: determinism, redaction, trust labelling
// ---------------------------------------------------------------------------

#[test]
fn output_is_byte_identical_across_five_runs() {
    let fx = seed_edge_cases();
    let (code, baseline, stderr) = run_sessions(&fx, "repo-a", &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    for attempt in 1..5 {
        let (code, again, _stderr) = run_sessions(&fx, "repo-a", &[]);
        assert_eq!(code, 0);
        assert_eq!(
            baseline, again,
            "sessions output must be byte-identical across runs (attempt {attempt})"
        );
    }
}

#[test]
fn no_raw_transcript_bytes_in_output() {
    let fx = seed_edge_cases();
    // BOTH transports: the text renderer prints fields the JSON envelope also
    // carries, so a leak must be checked on each surface, not just the default.
    for format in [&["--format", "json"][..], &["--format", "text"][..]] {
        let (code, stdout, stderr) = run_sessions(&fx, "repo-a", format);
        assert_eq!(code, 0, "format={format:?} stderr={stderr}");
        for sentinel in SENTINELS {
            assert!(
                !stdout.contains(sentinel),
                "raw payload sentinel {sentinel} must never reach stdout (format={format:?})"
            );
            assert!(
                !stderr.contains(sentinel),
                "raw payload sentinel {sentinel} must never reach stderr (format={format:?})"
            );
        }
    }
}

#[test]
fn multi_repo_session_appears_in_both_digests_with_per_repo_basis() {
    let fx = seed_multi_repo();
    let (repo_a, repo_b) = (fx.id("RepoA").to_owned(), fx.id("RepoB").to_owned());

    for selector in ["repo-a", "repo-b"] {
        let value = digest(&fx, selector, &[]);
        let row = row_for(&value, fx.id("S"));

        let mut scope: Vec<&str> = row["repository_scope"]
            .as_array()
            .expect("repository_scope array")
            .iter()
            .map(|v| v.as_str().expect("repository id"))
            .collect();
        scope.sort_unstable();
        let mut expected = vec![repo_a.as_str(), repo_b.as_str()];
        expected.sort_unstable();
        assert_eq!(
            scope, expected,
            "a cross-repo session is reported truthfully in EACH digest: {row}"
        );

        // The flat union says only that the session reached code somehow; the
        // per-repository map says HOW it reached each one.
        assert_eq!(
            row["scope_basis"],
            serde_json::json!(["code_citation", "task_reference"]),
            "scope_basis is the sorted union: {row}"
        );
        assert_eq!(
            row["scope_basis_by_repository"],
            serde_json::json!({
                repo_a.clone(): ["code_citation"],
                repo_b.clone(): ["task_reference"],
            }),
            "each repository must carry only the basis that put IT in scope: {row}"
        );
        assert_eq!(
            row["aggregation_scope"], "whole_session",
            "the row must disclose that its counts span the WHOLE session, not \
             the queried repository's slice: {row}"
        );
    }
}

#[test]
fn runs_truncated_and_tasks_truncated_fire_at_caps() {
    let fx = seed_over_row_caps();
    let value = digest(&fx, "repo-a", &[]);
    let row = row_for(&value, fx.id("S"));

    assert_eq!(
        row["runs"].as_array().expect("runs array").len(),
        20,
        "runs are capped at 20: {row}"
    );
    assert_eq!(
        row["tasks"].as_array().expect("tasks array").len(),
        20,
        "tasks are capped at 20: {row}"
    );

    for (code, session_scoped) in [("runs_truncated", true), ("tasks_truncated", true)] {
        let diag = diagnostic(&value, code)
            .unwrap_or_else(|| panic!("{code} diagnostic required: {value}"));
        assert_eq!(diag["matched"], 21, "the TRUE total is reported: {diag}");
        assert_eq!(diag["returned"], 20);
        assert_eq!(diag["limit"], 20);
        assert!(session_scoped);
        assert_eq!(
            diag["session_record_id"],
            fx.id("S"),
            "a per-row cap names the row it fired on: {diag}"
        );
    }
}

#[test]
fn truncated_sessions_leave_no_orphan_diagnostics() {
    // Diagnostics that name a session or run are attached to their row and must
    // vanish with it: an answer must never cite a record it does not contain.
    const ENVELOPE_SCOPED: &[&str] = &[
        "unresolved_repository_scope",
        "unlinked_session_stamped_records",
        "results_truncated",
        "no_sessions",
    ];

    let fx = seed_edge_cases();
    let value = digest(&fx, "repo-a", &["--limit", "1"]);
    let returned_sessions = row_ids(&value);
    assert_eq!(returned_sessions.len(), 1);
    let returned_runs: Vec<String> = rows(&value)
        .iter()
        .flat_map(|row| {
            row["runs"]
                .as_array()
                .expect("runs array")
                .iter()
                .map(|run| {
                    run["run_record_id"]
                        .as_str()
                        .expect("run_record_id")
                        .to_owned()
                })
                .collect::<Vec<_>>()
        })
        .collect();

    // The full answer DOES carry a run-scoped diagnostic, so this test would be
    // vacuous if truncation simply never produced one.
    let full = digest(&fx, "repo-a", &[]);
    assert!(
        diagnostic(&full, "outcome_not_enum_shaped").is_some(),
        "the untruncated answer must carry the run-scoped diagnostic: {full}"
    );

    for diag in value["diagnostics"].as_array().expect("diagnostics array") {
        let code = diag["code"].as_str().expect("diagnostic code");
        if ENVELOPE_SCOPED.contains(&code) {
            continue;
        }
        if let Some(session) = diag["session_record_id"].as_str() {
            assert!(
                returned_sessions.iter().any(|id| id == session),
                "diagnostic {code} names session {session}, which truncation dropped: {value}"
            );
        }
        if let Some(run) = diag["run_record_id"].as_str() {
            assert!(
                returned_runs.iter().any(|id| id == run),
                "diagnostic {code} names run {run}, which truncation dropped: {value}"
            );
        }
    }
}

#[test]
fn text_format_is_deterministic_and_payload_complete() {
    let fx = seed_edge_cases();
    let (code, first, stderr) = run_sessions(&fx, "repo-a", &["--format", "text", "--limit", "1"]);
    assert_eq!(code, 0, "stderr={stderr}");
    let (code, second, _stderr) =
        run_sessions(&fx, "repo-a", &["--format", "text", "--limit", "1"]);
    assert_eq!(code, 0);
    assert_eq!(
        first, second,
        "the text rendering must be byte-identical across runs"
    );

    // The truncation numbers must be readable WITHOUT switching to JSON.
    let json = digest(&fx, "repo-a", &["--limit", "1"]);
    let diag = diagnostic(&json, "results_truncated")
        .unwrap_or_else(|| panic!("results_truncated diagnostic required: {json}"));
    let matched = diag["matched"].as_u64().expect("matched count");
    assert!(
        first.contains(&format!(
            "diagnostic results_truncated matched {matched} returned 1 limit 1"
        )),
        "the text renderer must print the truncation payload, got:\n{first}"
    );
}

#[test]
fn text_format_sanitizes_hostile_repository_display_name() {
    let fx = seed_hostile_repository_display();
    // Select by the SAFE record-ID handle; the repository's own display name
    // (rendered in the header line) is the hostile string under test.
    let repo_id = fx.id("RepoHostile").to_owned();
    let (code, stdout, stderr) = run_sessions(&fx, &repo_id, &["--format", "text"]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(
        !stdout.contains("REPO_DISPLAY_LEAK\u{7}"),
        "the control byte must never reach the terminal raw, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("FORGED_LINE\n") && !stdout.lines().any(|l| l == "FORGED_LINE"),
        "an embedded newline in the display name must never forge a \
         standalone output line, got:\n{stdout}"
    );
    assert!(
        stdout.contains("REPO_DISPLAY_LEAK") && stdout.contains("FORGED_LINE"),
        "the sanitized (not deleted) text must still be present, got:\n{stdout}"
    );
}

#[test]
fn every_row_labels_agent_authored_trust_class_and_envelope_disclaimer() {
    let fx = seed_edge_cases();
    let value = digest(&fx, "repo-a", &[]);
    assert_eq!(
        value["disclaimer"], SESSIONS_DISCLAIMER,
        "the envelope must carry the verbatim disclaimer: {value}"
    );
    for row in rows(&value) {
        assert_eq!(
            row["trust_class"], "agent_authored",
            "every session row is an agent claim: {row}"
        );
    }
}

// ---------------------------------------------------------------------------
// AC: read-only, transport parity, latency
// ---------------------------------------------------------------------------

#[test]
fn graph_query_is_read_only() {
    let fx = seed_edge_cases();
    let before = fs::read(&fx.graph).expect("read graph before");
    let (code, _stdout, _stderr) = run_sessions(&fx, "repo-a", &[]);
    assert_eq!(code, 0);
    let after = fs::read(&fx.graph).expect("read graph after");
    assert_eq!(
        before, after,
        "sessions must never mutate its --graph input"
    );
}

/// Sorted `(relative path, bytes)` fingerprint of every file under `root`.
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

/// Ingests the fixture into a fresh embedded store and returns its path plus
/// the owning temp dir.
#[cfg(feature = "embedded-aletheiadb")]
fn ingest_fixture(fx: &Fixture) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    egregore()
        .arg("ingest")
        .arg(&fx.graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    (temp, data_dir)
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_query_is_strictly_read_only() {
    let fx = seed_edge_cases();
    let (_temp, data_dir) = ingest_fixture(&fx);

    let before = dir_fingerprint(&data_dir);
    egregore()
        .args(["query", "sessions", "repo-a", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "sessions must not modify any store file when reading --data-dir"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn graph_and_data_dir_transports_agree() {
    let fx = seed_edge_cases();
    let (_temp, data_dir) = ingest_fixture(&fx);

    let from_graph = digest(&fx, "repo-a", &[]);
    let output = egregore()
        .args(["query", "sessions", "repo-a", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let from_store: serde_json::Value =
        serde_json::from_slice(&output).expect("--data-dir stdout must be one JSON envelope");
    assert_eq!(
        from_graph, from_store,
        "--graph and --data-dir must answer identically"
    );
}

#[test]
fn digest_completes_under_two_seconds_on_fixture() {
    let fx = seed_edge_cases();
    // Take the fastest of three runs: the bound proves the query's own cost,
    // not scheduler contention from unrelated tests sharing the machine.
    let mut best = std::time::Duration::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let (code, stdout, stderr) = run_sessions(&fx, "repo-a", &[]);
        let elapsed = started.elapsed();
        assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
        best = best.min(elapsed);
    }
    assert!(
        best.as_secs_f64() < 2.0,
        "the digest must complete under two seconds, took {best:?} (best of 3)"
    );
}
