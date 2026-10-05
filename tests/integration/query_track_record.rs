#![allow(missing_docs)]

//! End-to-end tests for `eg query track-record` (alias `eg query agents`) —
//! the per-agent track record over the downstream fate of agent-authored
//! observations (issue #262).
//!
//! The lane answers "which agents produce promotable, durable knowledge, and
//! which produce noise?" from already-recorded store facts: one row per
//! `agent_id`, carrying deterministic counts over (a) observations written,
//! (b) promotion outcomes split by terminal verdict for candidates whose
//! supporting evidence is that agent's observations, (c) observations later
//! superseded, and (d) linked verification outcomes attributable to the
//! agent's sessions. Every nonzero bucket cites resolvable `record_id`
//! handles; agent-authored counts are trust-separated from deterministic
//! facts; rows sort canonically by `agent_id`.
//!
//! These tests pin the contract end-to-end over the built binary: they seed a
//! deterministic JSONL fixture (fixed timestamps, stable IDs) with two agents
//! of divergent fates — agent A earns an approved promotion plus a passing
//! verification, agent B a rejected candidate plus a superseded observation —
//! and assert each bucket is attributed to the right agent with zero
//! cross-agent misattribution.
//!
//! RED against current trunk: the `track-record` subcommand does not exist
//! yet, so every invocation here fails before the fixture is even read.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, IdentitySource, NodeKind, RepositoryIdentityPayload,
    SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, USER_CONTEXT_SCHEMA_VERSION, UserContextFields,
        UserContextScope, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id, stable_id,
        user_context_stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Fixture
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
        start_column: None,
        end_column: None,
    }
}

/// Hostile `agent_id` used to prove text output cannot be line-forged: it
/// sorts after the honest agents and must be sanitized on the text transport.
const HOSTILE_AGENT_ID: &str = "zzz-agent\nINJECTED";

fn write_jsonl(records: &[GraphRecord], path: &Path) {
    let mut out = String::new();
    for record in records {
        out.push_str(&serde_json::to_string(record).expect("record should serialize"));
        out.push('\n');
    }
    fs::write(path, out).expect("write fixture jsonl");
}

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

/// Seeds `Repository -CONTAINS-> File -DEFINES-> Symbol`, returning
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

fn agent(builder: &mut Builder, label: &str, agent_id_value: &str, agent_kind: &str) -> String {
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
        agent_kind: kind_field,
        ..
    } = &mut record
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some(agent_id_value.to_owned());
        *kind_field = Some(agent_kind.to_owned());
    }
    builder.push(record);
    builder.label(label, &id);
    id
}

fn session(
    builder: &mut Builder,
    label: &str,
    key: &str,
    agent_id_value: &str,
    agent: &str,
) -> String {
    let id = agent_memory_stable_id(&["node", "agent_session", key]);
    let mut record = GraphRecord::node(
        id.clone(),
        NodeKind::AgentSession,
        None,
        None,
        None,
        format!("AgentSession {key}"),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        agent_kind,
        session_id,
        observed_at,
        ..
    } = &mut record
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some(agent_id_value.to_owned());
        *agent_kind = Some("codex".to_owned());
        *session_id = Some(key.to_owned());
        *observed_at = Some("2026-06-01T08:00:00Z".to_owned());
    }
    builder.push(record);
    builder.label(label, &id);
    builder.push(GraphRecord::agent_memory_edge(
        EdgeLabel::SessionOf,
        id.clone(),
        agent.to_owned(),
        Some("1.0".to_owned()),
        "session of agent".to_owned(),
    ));
    id
}

fn run(builder: &mut Builder, label: &str, key: &str, session_id: &str) -> String {
    let id = agent_memory_stable_id(&["node", "agent_run", key]);
    let mut record = GraphRecord::node(
        id.clone(),
        NodeKind::AgentRun,
        None,
        None,
        None,
        format!("AgentRun {key}"),
    );
    if let GraphRecord::Node {
        schema_version,
        session_id: session_field,
        observed_at,
        ..
    } = &mut record
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *session_field = Some(session_id.to_owned());
        *observed_at = Some("2026-06-01T08:05:00Z".to_owned());
    }
    builder.push(record);
    builder.label(label, &id);
    builder.push(GraphRecord::agent_memory_edge(
        EdgeLabel::SessionOf,
        id.clone(),
        session_id.to_owned(),
        Some("1.0".to_owned()),
        "run of session".to_owned(),
    ));
    id
}

#[allow(clippy::too_many_arguments)]
fn observation(
    builder: &mut Builder,
    label: &str,
    key: &str,
    agent_id_value: Option<&str>,
    agent_kind: &str,
    session: Option<&str>,
    superseded_by: Option<&str>,
) -> String {
    let id = agent_memory_stable_id(&["node", "observation", key]);
    let mut record = GraphRecord::node(
        id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        format!("Observation {key}"),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        agent_kind: kind_field,
        session_id,
        observed_at,
        text,
        ..
    } = &mut record
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = agent_id_value.map(str::to_owned);
        *kind_field = agent_id_value.map(|_| agent_kind.to_owned());
        *session_id = session.map(str::to_owned);
        *observed_at = Some("2026-06-01T09:00:00Z".to_owned());
        *text = Some(format!("observation body {key}"));
    }
    if let Some(target) = superseded_by {
        record = record.with_superseded_by(target);
    }
    builder.push(record);
    builder.label(label, &id);
    if let Some(sess) = session {
        builder.push(GraphRecord::agent_memory_edge(
            EdgeLabel::AuthoredBy,
            id.clone(),
            sess.to_owned(),
            Some("1.0".to_owned()),
            "observation authored by session".to_owned(),
        ));
    }
    id
}

fn cite_symbol(builder: &mut Builder, source: &str, symbol: &str) {
    builder.push(GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        source.to_owned(),
        symbol.to_owned(),
        Some("1.0".to_owned()),
        "cites symbol".to_owned(),
    ));
}

fn support_link(target: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: "agent_memory".to_owned(),
        relation: "PROPOSED_BY".to_owned(),
        confidence: "0.9".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

fn candidate(builder: &mut Builder, label: &str, key: &str, supporting: &[String]) -> String {
    let id = user_context_stable_id(&["candidate", key]);
    let mut record = GraphRecord::node(
        id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        format!("PromoteCandidate {key}"),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        user_context,
        confidence,
        valid_time,
        ..
    } = &mut record
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *valid_time = Some("2026-06-01T09:30:00Z".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(format!("candidate rule {key}")),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(supporting.iter().map(|t| support_link(t)).collect()),
            contradicting_evidence: Some(vec![]),
            ..UserContextFields::empty()
        };
    }
    builder.push(record);
    builder.label(label, &id);
    // Real importers write BOTH the on-node supporting-evidence links and the
    // `ProposedBy` edges; the lane must read both without double counting.
    for target in supporting {
        builder.push(GraphRecord::edge(
            EdgeLabel::ProposedBy,
            id.clone(),
            target.clone(),
            Some("0.9".to_owned()),
            "candidate proposed by observation".to_owned(),
        ));
    }
    id
}

fn decision(
    builder: &mut Builder,
    label: &str,
    key: &str,
    candidate_id: &str,
    outcome: Option<&str>,
    decided_at: &str,
) -> String {
    let id = user_context_stable_id(&["decision", key]);
    let mut record = GraphRecord::node(
        id.clone(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        format!("PromotionDecision {key}"),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        user_context,
        ..
    } = &mut record
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(candidate_id.to_owned()),
            outcome: outcome.map(str::to_owned),
            decided_at: Some(decided_at.to_owned()),
            decided_by: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }
    builder.push(record);
    builder.label(label, &id);
    builder.push(GraphRecord::edge(
        EdgeLabel::DecidedOn,
        id.clone(),
        candidate_id.to_owned(),
        None,
        "decision on candidate".to_owned(),
    ));
    id
}

fn verification(
    builder: &mut Builder,
    label: &str,
    key: &str,
    kind: NodeKind,
    status: Option<&str>,
    run: Option<&str>,
) -> String {
    let id = verification_stable_id(&["verification", key]);
    let mut record = GraphRecord::node(
        id.clone(),
        kind,
        None,
        None,
        None,
        format!("Verification {key}"),
    );
    if let GraphRecord::Node {
        schema_version,
        status: status_field,
        verification_kind,
        executed_at,
        ..
    } = &mut record
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status_field = status.map(str::to_owned);
        *verification_kind = Some("command_run".to_owned());
        *executed_at = Some("2026-06-01T08:06:00Z".to_owned());
    }
    builder.push(record);
    builder.label(label, &id);
    if let Some(r) = run {
        builder.push(GraphRecord::agent_memory_edge(
            EdgeLabel::AuthoredBy,
            id.clone(),
            r.to_owned(),
            Some("1.0".to_owned()),
            "verification authored by run".to_owned(),
        ));
    }
    id
}

/// Two agents with divergent fates (issue #262 acceptance fixture):
///
/// * agent A (`agent-a`, kind `codex`): two observations; one candidate
///   approved (a second, earlier `rejected` decision on the same candidate
///   must NOT leak into any bucket — latest decision wins); one
///   `edited_then_approved`; one `deferred`; one candidate co-supported by
///   both agents' observations (each agent credited exactly once); one
///   passing verification linked through run → session.
/// * agent B (`agent-b`, kind `claude-code`): three observations, one later
///   superseded; one candidate rejected; one expired; the shared approved
///   candidate; one failing and one inconclusive verification.
/// * `zzz-agent\nINJECTED`: one observation with a hostile id — proves text
///   output sanitizes importer-controlled free text.
/// * `agent-idle`: an `Agent` record with no qualifying records — proves the
///   unscoped query emits an explicit all-zero row rather than silence.
/// * Diagnostics: one observation with no `agent_id`, one candidate with no
///   terminal decision, one verification linked to no session.
// Long fixture builder: allow the pedantic lints the rest of the suite
// already allows on fixtures (see agent_sessions.rs).
#[allow(clippy::too_many_lines, clippy::similar_names)]
fn seed_track_record() -> Fixture {
    let mut builder = Builder::new();

    let repo_a = repository(&mut builder, "RepoA", "repo-a", "repo-a");
    let (_file_a, sym_a) =
        code_topology(&mut builder, &repo_a, "FileA", "SymA", "src/a.rs", "alpha");
    let _repo_b = repository(&mut builder, "RepoB", "repo-b", "repo-b");

    // ── Agent A ────────────────────────────────────────────────────────────
    let ag_a = agent(&mut builder, "AgA", "agent-a", "codex");
    let sess_a = session(&mut builder, "SessA", "sess-a", "agent-a", &ag_a);
    let run_a = run(&mut builder, "RunA", "run-a", &sess_a);

    let obs_a1 = observation(
        &mut builder,
        "ObsA1",
        "obs-a1",
        Some("agent-a"),
        "codex",
        Some(&sess_a),
        None,
    );
    cite_symbol(&mut builder, &obs_a1, &sym_a);
    let obs_a2 = observation(
        &mut builder,
        "ObsA2",
        "obs-a2",
        Some("agent-a"),
        "codex",
        Some(&sess_a),
        None,
    );
    cite_symbol(&mut builder, &obs_a2, &sym_a);

    // Candidate A: an EARLIER rejected decision is superseded by the LATEST
    // approved decision — only the latest may land in a bucket.
    let cand_a = candidate(
        &mut builder,
        "CandA",
        "cand-a",
        std::slice::from_ref(&obs_a1),
    );
    decision(
        &mut builder,
        "DecAEarly",
        "dec-a-early",
        &cand_a,
        Some("rejected"),
        "2026-06-01T09:00:00Z",
    );
    let _dec_a = decision(
        &mut builder,
        "DecA",
        "dec-a",
        &cand_a,
        Some("approved"),
        "2026-06-01T10:05:00Z",
    );

    let cand_a2 = candidate(
        &mut builder,
        "CandA2",
        "cand-a2",
        std::slice::from_ref(&obs_a2),
    );
    let _dec_a2 = decision(
        &mut builder,
        "DecA2",
        "dec-a2",
        &cand_a2,
        Some("edited_then_approved"),
        "2026-06-01T10:06:00Z",
    );

    let cand_a3 = candidate(
        &mut builder,
        "CandA3",
        "cand-a3",
        std::slice::from_ref(&obs_a1),
    );
    let _dec_a3 = decision(
        &mut builder,
        "DecA3",
        "dec-a3",
        &cand_a3,
        Some("deferred"),
        "2026-06-01T10:07:00Z",
    );

    let ver_a = verification(
        &mut builder,
        "VerA",
        "ver-a",
        NodeKind::CommandRun,
        Some("pass"),
        Some(&run_a),
    );
    cite_symbol(&mut builder, &ver_a, &sym_a);

    // ── Agent B ────────────────────────────────────────────────────────────
    let ag_b = agent(&mut builder, "AgB", "agent-b", "claude-code");
    let sess_b = session(&mut builder, "SessB", "sess-b", "agent-b", &ag_b);
    let run_b = run(&mut builder, "RunB", "run-b", &sess_b);

    let obs_b1 = observation(
        &mut builder,
        "ObsB1",
        "obs-b1",
        Some("agent-b"),
        "claude-code",
        Some(&sess_b),
        None,
    );
    cite_symbol(&mut builder, &obs_b1, &sym_a);
    let obs_b3 = observation(
        &mut builder,
        "ObsB3",
        "obs-b3",
        Some("agent-b"),
        "claude-code",
        Some(&sess_b),
        None,
    );
    let obs_b2 = observation(
        &mut builder,
        "ObsB2",
        "obs-b2",
        Some("agent-b"),
        "claude-code",
        Some(&sess_b),
        Some(&obs_b3),
    );

    let cand_b = candidate(
        &mut builder,
        "CandB",
        "cand-b",
        std::slice::from_ref(&obs_b1),
    );
    let _dec_b = decision(
        &mut builder,
        "DecB",
        "dec-b",
        &cand_b,
        Some("rejected"),
        "2026-06-01T10:08:00Z",
    );

    let cand_b2 = candidate(
        &mut builder,
        "CandB2",
        "cand-b2",
        std::slice::from_ref(&obs_b2),
    );
    let _dec_b2 = decision(
        &mut builder,
        "DecB2",
        "dec-b2",
        &cand_b2,
        Some("expired"),
        "2026-06-01T10:09:00Z",
    );

    let ver_b = verification(
        &mut builder,
        "VerB",
        "ver-b",
        NodeKind::CommandRun,
        Some("fail"),
        Some(&run_b),
    );
    cite_symbol(&mut builder, &ver_b, &sym_a);
    let _ver_b2 = verification(
        &mut builder,
        "VerB2",
        "ver-b2",
        NodeKind::TestRun,
        Some("cancelled"),
        Some(&run_b),
    );

    // ── Shared candidate: supported by BOTH agents' observations. Each agent
    // is credited exactly once — no double counting within an agent, no
    // leakage across agents.
    let cand_x = candidate(
        &mut builder,
        "CandX",
        "cand-x",
        &[obs_a1.clone(), obs_b1.clone()],
    );
    let _dec_x = decision(
        &mut builder,
        "DecX",
        "dec-x",
        &cand_x,
        Some("approved"),
        "2026-06-01T10:10:00Z",
    );

    // ── Hostile agent id: text output must sanitize it. ────────────────────
    observation(
        &mut builder,
        "ObsC1",
        "obs-c1",
        Some(HOSTILE_AGENT_ID),
        "codex",
        None,
        None,
    );

    // ── Idle agent: an Agent record with no qualifying records at all. The
    // unscoped query must still emit an explicit all-zero row for it.
    agent(&mut builder, "AgIdle", "agent-idle", "codex");

    // ── Diagnostics ─────────────────────────────────────────────────────────
    // Observation with no agent_id: unattributable, counted in diagnostics.
    observation(
        &mut builder,
        "ObsOrphan",
        "obs-orphan",
        None,
        "codex",
        None,
        None,
    );
    // Candidate with no decision: no terminal verdict, counted in diagnostics.
    candidate(
        &mut builder,
        "CandPending",
        "cand-pending",
        std::slice::from_ref(&obs_a2),
    );
    // Verification linked to no session: unattributable, counted in diagnostics.
    verification(
        &mut builder,
        "VerOrphan",
        "ver-orphan",
        NodeKind::CommandRun,
        Some("pass"),
        None,
    );

    builder.finish("track_record.jsonl")
}

/// Runs the lane and parses the JSON envelope.
fn query_json(fixture: &Fixture, extra: &[&str]) -> Value {
    let mut cmd = egregore();
    cmd.arg("query")
        .arg("track-record")
        .arg("--graph")
        .arg(&fixture.graph);
    for arg in extra {
        cmd.arg(arg);
    }
    let output = cmd.output().expect("lane should run");
    assert!(
        output.status.success(),
        "track-record should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout should be JSON")
}

fn agents_of(envelope: &Value) -> Vec<Value> {
    envelope["agents"]
        .as_array()
        .expect("envelope should carry agents")
        .clone()
}

fn agent_row<'a>(agents: &'a [Value], agent_id: &str) -> &'a Value {
    agents
        .iter()
        .find(|a| a["agent_id"] == agent_id)
        .unwrap_or_else(|| panic!("no row for agent {agent_id}"))
}

fn bucket<'a>(row: &'a Value, section: &str, name: Option<&str>) -> &'a Value {
    name.map_or(&row[section], |n| &row[section][n])
}

fn id_set(value: &Value, field: &str) -> Vec<String> {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} should be an array"))
        .iter()
        .map(|v| v.as_str().expect("id should be a string").to_owned())
        .collect()
}

fn assert_bucket(
    fixture: &Fixture,
    row: &Value,
    section: &str,
    name: Option<&str>,
    expected_count: usize,
    expected_ids: &[&str],
    expected_decisions: &[&str],
) {
    let b = bucket(row, section, name);
    let count = usize::try_from(b["count"].as_u64().expect("count should be a number"))
        .expect("count should fit in usize");
    let label = name.unwrap_or(section);
    assert_eq!(
        count, expected_count,
        "{section}.{label} count for agent {}",
        row["agent_id"]
    );
    let id_field = if section == "promotion_outcomes" {
        "candidate_ids"
    } else {
        "record_ids"
    };
    let mut got = id_set(b, id_field);
    got.sort();
    let mut expected: Vec<String> = expected_ids
        .iter()
        .map(|k| fixture.id(k).to_owned())
        .collect();
    expected.sort();
    assert_eq!(
        got, expected,
        "{section}.{label} {id_field} for agent {}",
        row["agent_id"]
    );
    if section == "promotion_outcomes" {
        let mut got_dec = id_set(b, "decision_ids");
        got_dec.sort();
        let mut expected_dec: Vec<String> = expected_decisions
            .iter()
            .map(|k| fixture.id(k).to_owned())
            .collect();
        expected_dec.sort();
        assert_eq!(
            got_dec, expected_dec,
            "promotion_outcomes.{label} decision_ids for agent {}",
            row["agent_id"]
        );
    }
    // Every nonzero bucket is backed by citable handles — never bare prose —
    // and every zero bucket carries empty handle lists.
    if expected_count == 0 {
        assert!(
            got.is_empty(),
            "zero bucket {section}.{label} must cite no ids"
        );
    } else {
        assert_eq!(got.len(), expected_count, "cited ids must back every count");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The acceptance fixture: agent A earns an approved promotion plus a passing
/// verification; agent B a rejected candidate plus a superseded observation —
/// every bucket attributed to the right agent with zero cross-agent
/// misattribution. Plus an idle agent (`agent-idle`) with no qualifying
/// records, which must still get an explicit all-zero row.
#[allow(clippy::too_many_lines)]
#[test]
fn divergent_fates_are_attributed_per_agent() {
    let fixture = seed_track_record();
    let envelope = query_json(&fixture, &[]);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["lane"], "track-record");

    let agents = agents_of(&envelope);
    let ids: Vec<&str> = agents
        .iter()
        .map(|a| a["agent_id"].as_str().expect("agent_id should be a string"))
        .collect();
    // Canonical byte-sort by agent_id across runs and platforms.
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "agent rows must be canonically sorted");
    assert_eq!(
        ids,
        vec!["agent-a", "agent-b", "agent-idle", HOSTILE_AGENT_ID]
    );

    // ── Agent A ──────────────────────────────────────────────────────────
    let row_a = agent_row(&agents, "agent-a");
    assert_eq!(row_a["agent_kind"], "codex");
    assert_bucket(
        &fixture,
        row_a,
        "observations_written",
        None,
        2,
        &["ObsA1", "ObsA2"],
        &[],
    );
    // cand-a (latest decision approved — the earlier rejected decision must
    // not leak) + cand-x (co-supported by both agents).
    assert_bucket(
        &fixture,
        row_a,
        "promotion_outcomes",
        Some("approved"),
        2,
        &["CandA", "CandX"],
        &["DecA", "DecX"],
    );
    assert_bucket(
        &fixture,
        row_a,
        "promotion_outcomes",
        Some("edited_then_approved"),
        1,
        &["CandA2"],
        &["DecA2"],
    );
    assert_bucket(
        &fixture,
        row_a,
        "promotion_outcomes",
        Some("rejected"),
        0,
        &[],
        &[],
    );
    assert_bucket(
        &fixture,
        row_a,
        "promotion_outcomes",
        Some("deferred"),
        1,
        &["CandA3"],
        &["DecA3"],
    );
    assert_bucket(
        &fixture,
        row_a,
        "promotion_outcomes",
        Some("expired"),
        0,
        &[],
        &[],
    );
    assert_bucket(
        &fixture,
        row_a,
        "superseded_observations",
        None,
        0,
        &[],
        &[],
    );
    assert_bucket(
        &fixture,
        row_a,
        "verification_outcomes",
        Some("passed"),
        1,
        &["VerA"],
        &[],
    );
    assert_bucket(
        &fixture,
        row_a,
        "verification_outcomes",
        Some("failed"),
        0,
        &[],
        &[],
    );
    assert_bucket(
        &fixture,
        row_a,
        "verification_outcomes",
        Some("inconclusive"),
        0,
        &[],
        &[],
    );

    // ── Agent B ──────────────────────────────────────────────────────────
    let row_b = agent_row(&agents, "agent-b");
    assert_eq!(row_b["agent_kind"], "claude-code");
    assert_bucket(
        &fixture,
        row_b,
        "observations_written",
        None,
        3,
        &["ObsB1", "ObsB2", "ObsB3"],
        &[],
    );
    // The shared candidate credits B exactly once — no cross-agent leakage
    // from A's private candidates, no double count from two supporting obs.
    assert_bucket(
        &fixture,
        row_b,
        "promotion_outcomes",
        Some("approved"),
        1,
        &["CandX"],
        &["DecX"],
    );
    assert_bucket(
        &fixture,
        row_b,
        "promotion_outcomes",
        Some("edited_then_approved"),
        0,
        &[],
        &[],
    );
    assert_bucket(
        &fixture,
        row_b,
        "promotion_outcomes",
        Some("rejected"),
        1,
        &["CandB"],
        &["DecB"],
    );
    assert_bucket(
        &fixture,
        row_b,
        "promotion_outcomes",
        Some("deferred"),
        0,
        &[],
        &[],
    );
    assert_bucket(
        &fixture,
        row_b,
        "promotion_outcomes",
        Some("expired"),
        1,
        &["CandB2"],
        &["DecB2"],
    );
    assert_bucket(
        &fixture,
        row_b,
        "superseded_observations",
        None,
        1,
        &["ObsB2"],
        &[],
    );
    assert_bucket(
        &fixture,
        row_b,
        "verification_outcomes",
        Some("passed"),
        0,
        &[],
        &[],
    );
    assert_bucket(
        &fixture,
        row_b,
        "verification_outcomes",
        Some("failed"),
        1,
        &["VerB"],
        &[],
    );
    assert_bucket(
        &fixture,
        row_b,
        "verification_outcomes",
        Some("inconclusive"),
        1,
        &["VerB2"],
        &[],
    );

    // ── Hostile agent id ─────────────────────────────────────────────────
    let row_c = agent_row(&agents, HOSTILE_AGENT_ID);
    assert_bucket(
        &fixture,
        row_c,
        "observations_written",
        None,
        1,
        &["ObsC1"],
        &[],
    );
    assert_bucket(
        &fixture,
        row_c,
        "superseded_observations",
        None,
        0,
        &[],
        &[],
    );

    // ── Trust separation ─────────────────────────────────────────────────
    let disclaimer = envelope["disclaimer"]
        .as_str()
        .expect("disclaimer should be a string");
    assert!(!disclaimer.is_empty(), "a disclaimer must be present");
    for row in &agents {
        // Agent-authored counts are never co-mingled with deterministic facts:
        // each section names its own trust basis.
        assert_eq!(
            row["observations_written"]["trust_basis"],
            "agent_authored_claims"
        );
        assert_eq!(
            row["superseded_observations"]["trust_basis"],
            "agent_authored_claims"
        );
        assert_eq!(
            row["promotion_outcomes"]["trust_basis"],
            "recorded_operator_decisions"
        );
        assert_eq!(
            row["verification_outcomes"]["trust_basis"],
            "verification_evidence"
        );
    }

    // ── Diagnostics ──────────────────────────────────────────────────────
    let diagnostics = envelope["diagnostics"]
        .as_array()
        .expect("diagnostics should be an array");
    let diag = |code: &str| {
        diagnostics
            .iter()
            .find(|d| d["code"] == code)
            .unwrap_or_else(|| panic!("missing diagnostic {code}"))
    };
    assert_eq!(diag("observations_without_agent_id")["count"], 1);
    assert_eq!(
        id_set(diag("observations_without_agent_id"), "record_ids"),
        vec![fixture.id("ObsOrphan").to_owned()]
    );
    assert_eq!(diag("candidates_without_terminal_decision")["count"], 1);
    assert_eq!(
        id_set(diag("candidates_without_terminal_decision"), "record_ids"),
        vec![fixture.id("CandPending").to_owned()]
    );
    assert_eq!(diag("verifications_without_session")["count"], 1);
    assert_eq!(
        id_set(diag("verifications_without_session"), "record_ids"),
        vec![fixture.id("VerOrphan").to_owned()]
    );
}

/// An agent with no qualifying records still gets an explicit all-zero row
/// in the unscoped query — never silence — while a repo-scoped query does
/// not leak it into a repository it never touched.
#[test]
fn idle_agent_gets_explicit_empty_row() {
    let fixture = seed_track_record();
    let envelope = query_json(&fixture, &[]);
    let agents = agents_of(&envelope);
    let row = agent_row(&agents, "agent-idle");
    assert_eq!(row["agent_kind"], "codex");
    for (section, sub) in [
        ("observations_written", None),
        ("promotion_outcomes", Some("approved")),
        ("promotion_outcomes", Some("edited_then_approved")),
        ("promotion_outcomes", Some("rejected")),
        ("promotion_outcomes", Some("deferred")),
        ("promotion_outcomes", Some("expired")),
        ("superseded_observations", None),
        ("verification_outcomes", Some("passed")),
        ("verification_outcomes", Some("failed")),
        ("verification_outcomes", Some("inconclusive")),
    ] {
        assert_bucket(&fixture, row, section, sub, 0, &[], &[]);
    }
    assert_eq!(
        row["promotion_outcomes"]["trust_basis"],
        "recorded_operator_decisions"
    );
    assert_eq!(
        row["observations_written"]["trust_basis"],
        "agent_authored_claims"
    );
    assert_eq!(
        row["verification_outcomes"]["trust_basis"],
        "verification_evidence"
    );

    // Repo-scoped: the idle agent never cited repo-a, so it stays out.
    let scoped = query_json(&fixture, &["--repo", "repo-a"]);
    let scoped_agents = agents_of(&scoped);
    let idle_leaks = scoped_agents
        .iter()
        .any(|a| a["agent_id"].as_str() == Some("agent-idle"));
    assert!(!idle_leaks, "idle agent must not leak into --repo scope");
}

/// `eg query agents` is the documented alias: byte-identical output.
#[test]
fn agents_alias_matches_track_record() {
    let fixture = seed_track_record();
    let via_primary = egregore()
        .arg("query")
        .arg("track-record")
        .arg("--graph")
        .arg(&fixture.graph)
        .output()
        .expect("track-record should run");
    let via_alias = egregore()
        .arg("query")
        .arg("agents")
        .arg("--graph")
        .arg(&fixture.graph)
        .output()
        .expect("agents alias should run");
    assert!(via_alias.status.success(), "agents alias should exit 0");
    assert_eq!(
        via_primary.stdout, via_alias.stdout,
        "alias output must be byte-identical"
    );
}

/// Byte-stable ordering across repeated runs.
#[test]
fn output_is_deterministic_across_runs() {
    let fixture = seed_track_record();
    let first = egregore()
        .arg("query")
        .arg("track-record")
        .arg("--graph")
        .arg(&fixture.graph)
        .output()
        .expect("first run should run");
    let second = egregore()
        .arg("query")
        .arg("track-record")
        .arg("--graph")
        .arg(&fixture.graph)
        .output()
        .expect("second run should run");
    assert_eq!(
        first.stdout, second.stdout,
        "repeated runs must be byte-identical"
    );
}

/// `--format text` gives a skimmable table and sanitizes importer-controlled
/// free text; nonzero buckets still cite their record ids.
#[test]
fn text_format_is_a_sanitized_table() {
    let fixture = seed_track_record();
    let output = egregore()
        .arg("query")
        .arg("track-record")
        .arg("--graph")
        .arg(&fixture.graph)
        .arg("--format")
        .arg("text")
        .output()
        .expect("text run should run");
    assert!(output.status.success(), "text format should exit 0");
    let text = String::from_utf8(output.stdout).expect("text should be UTF-8");

    assert!(text.contains("agent-a"), "table should name agent-a");
    assert!(text.contains("agent-b"), "table should name agent-b");
    // The hostile id's newline is sanitized, never a forged line.
    assert!(
        !text.lines().any(|line| line.starts_with("INJECTED")),
        "raw hostile agent_id must not start a line"
    );
    assert!(
        text.contains("zzz-agent·INJECTED"),
        "sanitized id should appear"
    );
    // Nonzero buckets cite handles in text mode too.
    assert!(
        text.contains(fixture.id("CandA")),
        "text should cite candidate ids"
    );
    assert!(
        text.contains(fixture.id("DecA")),
        "text should cite decision ids"
    );
    assert!(
        text.contains(fixture.id("VerA")),
        "text should cite verification ids"
    );
}

/// An empty store is an explicit empty verdict, not silence.
#[test]
fn empty_store_returns_explicit_empty_verdict() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("empty.jsonl");
    fs::write(&graph, "").expect("write empty jsonl");
    let fixture = Fixture {
        _temp: temp,
        graph,
        ids: BTreeMap::new(),
    };
    let envelope = query_json(&fixture, &[]);
    assert_eq!(envelope["ok"], true);
    assert_eq!(
        envelope["agents"].as_array().expect("agents array").len(),
        0
    );
    let diagnostics = envelope["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert!(
        diagnostics.iter().any(|d| d["code"] == "no_agents"),
        "empty store must carry an explicit no_agents verdict"
    );
}

/// `--repo` scoping: only agents with in-scope evidence are reported; a
/// repository nobody cites yields the explicit empty verdict; an unknown
/// selector is a machine-readable exit-1.
#[test]
fn repo_scoping_filters_agents() {
    let fixture = seed_track_record();

    let scoped = query_json(&fixture, &["--repo", "repo-a"]);
    assert_eq!(scoped["repository_id"], fixture.id("RepoA"));
    let scoped_agents = agents_of(&scoped);
    let ids: Vec<&str> = scoped_agents
        .iter()
        .map(|a| a["agent_id"].as_str().expect("agent_id"))
        .collect();
    assert_eq!(
        ids,
        vec!["agent-a", "agent-b"],
        "only citing agents stay in scope"
    );

    let unscoped = query_json(&fixture, &["--repo", "repo-b"]);
    assert_eq!(
        unscoped["agents"].as_array().expect("agents array").len(),
        0,
        "a repository nobody cites yields no agents"
    );
    assert!(
        unscoped["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|d| d["code"] == "no_agents"),
        "out-of-scope store must carry the explicit empty verdict"
    );

    let output = egregore()
        .arg("query")
        .arg("track-record")
        .arg("--graph")
        .arg(&fixture.graph)
        .arg("--repo")
        .arg("no-such-repo")
        .output()
        .expect("unknown repo run should run");
    assert!(!output.status.success(), "unknown repo must exit nonzero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown_repository_selector"),
        "unknown repo must be machine-readable, got: {stderr}"
    );
}

/// Success metric: an operator ranks agents by observation-acceptance rate
/// (approved + `edited_then_approved` promotions ÷ `promotion-decided`
/// observations) from one command's citable output.
// Rate arithmetic on small integer counts: the u64->f64 cast is exact for
// these magnitudes; allow the pedantic precision lint.
#[allow(clippy::cast_precision_loss)]
#[test]
fn acceptance_rate_is_computable_from_citable_output() {
    let fixture = seed_track_record();
    let envelope = query_json(&fixture, &[]);
    let agents = agents_of(&envelope);
    let mut rates: Vec<(&str, f64)> = agents
        .iter()
        .map(|row| {
            let id = row["agent_id"].as_str().expect("agent_id");
            let promo = &row["promotion_outcomes"];
            let decided: u64 = [
                "approved",
                "edited_then_approved",
                "rejected",
                "deferred",
                "expired",
            ]
            .iter()
            .map(|v| promo[v]["count"].as_u64().expect("count"))
            .sum();
            let accepted: u64 = ["approved", "edited_then_approved"]
                .iter()
                .map(|v| promo[v]["count"].as_u64().expect("count"))
                .sum();
            let rate = if decided == 0 {
                f64::NAN
            } else {
                accepted as f64 / decided as f64
            };
            (id, rate)
        })
        .collect();
    // NaN (no decided promotions) sorts last; finite rates rank descending.
    rates.sort_by(|a, b| match (a.1.is_nan(), b.1.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => b.1.partial_cmp(&a.1).expect("rates are finite"),
    });
    assert_eq!(rates[0].0, "agent-a");
    // agent-a: (2 approved + 1 edited) / 4 decided = 0.75
    assert!((rates[0].1 - 0.75).abs() < f64::EPSILON, "agent-a rate");
    assert_eq!(rates[1].0, "agent-b");
    // agent-b: (1 approved) / 3 decided = 1/3
    assert!(
        (rates[1].1 - (1.0 / 3.0)).abs() < f64::EPSILON,
        "agent-b rate"
    );
    assert!(rates[2].1.is_nan(), "agent with no decisions has no rate");
}
