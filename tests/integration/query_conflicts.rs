#![allow(missing_docs)]

//! End-to-end tests for `eg query conflicts` — the contradicting-observations
//! query (issue #232). The lane surfaces recorded `CONTRADICTS` edges as
//! citable conflict pairs scoped to a symbol, file, or subsystem prefix,
//! keeping deterministic code facts out of the party list, marking
//! superseded conflicts `resolved`, and emitting byte-stable output.

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id,
        stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
        start_column: None,
        end_column: None,
    }
}

/// Sentinel payloads that must NEVER appear in conflicts output (AC7).
const RAW_TEXT_SENTINEL: &str = "RAW_OBSERVATION_TEXT_SHOULD_NOT_LEAK";
const RAW_RULE_SENTINEL: &str = "RAW_PREFERENCE_RULE_SHOULD_NOT_LEAK";

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

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    sym_login_id: String,
    obs_login_racy_id: String,
    dec_login_serial_id: String,
    fail_login_suite_id: String,
    test_login_pass_id: String,
    obs_refresh_lockfree_id: String,
    pref_explicit_lock_id: String,
    obs_token_old_id: String,
    obs_token_new_id: String,
}

/// Builds one agent-memory node with the provenance fields the conflicts lane
/// surfaces (`agent_id`, `observed_at`, `confidence`, `evidence_links`).
#[allow(clippy::too_many_arguments)]
fn memory_node(
    id: String,
    kind: NodeKind,
    summary: &str,
    agent_id: &str,
    observed_at: &str,
    confidence: &str,
    links: Vec<EvidenceLink>,
) -> GraphRecord {
    let mut node = GraphRecord::node(id, kind, None, None, None, summary.to_owned());
    if let GraphRecord::Node {
        agent_id: aid,
        observed_at: oa,
        confidence: conf,
        evidence_links,
        schema_version,
        ..
    } = &mut node
    {
        *aid = Some(agent_id.to_owned());
        *oa = Some(observed_at.to_owned());
        *conf = Some(confidence.to_owned());
        *evidence_links = Some(links);
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }
    node
}

/// Seeds a store with four in-subsystem (`src/auth/`) conflicts plus controls:
///
/// - C1: agent Observation vs agent Decision on symbol `login`
/// - C2: passing verification `TestRun` vs agent `Failure` belief on `login`
/// - C3: user-context Preference vs agent Observation on symbol `refresh`
/// - C4: superseded Observation vs its replacement on file `src/auth/token.rs`
///   (resolved — excluded by default)
/// - one CONTRADICTS edge to a code fact (must never surface)
/// - one out-of-subsystem conflict under `src/other/` (scope control)
#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("conflicts_seeded.jsonl");
    let mut graph = Graph::new();

    // ── Repository ──────────────────────────────────────────────────────────
    let repo_id = stable_id(&["node", "Repository", "repo-a"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-a".to_owned()),
        "Repository repo-a".to_owned(),
    ));

    // ── Code handles ────────────────────────────────────────────────────────
    let mut file = |p: &str| -> String {
        let id = stable_id(&["node", "File", p]);
        graph.push(GraphRecord::syntax_node(
            id.clone(),
            NodeKind::File,
            p.to_owned(),
            span(1, 200),
            p.to_owned(),
            "rust",
            format!("Source file {p}"),
        ));
        let contains = GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.clone(),
            id.clone(),
            None,
            "repo contains file".to_owned(),
        );
        graph.push(contains);
        id
    };
    let file_session_id = file("src/auth/session.rs");
    let file_token_id = file("src/auth/token.rs");
    let _file_clean_id = file("src/auth/clean.rs");
    let file_other_id = file("src/other/util.rs");

    let mut symbol = |file_id: &str, p: &str, name: &str| -> String {
        let id = stable_id(&["node", "Symbol", p, name]);
        graph.push(GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Symbol,
            p.to_owned(),
            span(10, 20),
            name.to_owned(),
            "rust",
            format!("Symbol {name}"),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Defines,
            file_id.to_owned(),
            id.clone(),
            None,
            "file defines symbol".to_owned(),
        ));
        id
    };
    let sym_login_id = symbol(&file_session_id, "src/auth/session.rs", "login");
    let sym_refresh_id = symbol(&file_token_id, "src/auth/token.rs", "refresh");
    let _sym_audit_id = symbol(&file_session_id, "src/auth/clean.rs", "audit");
    let sym_helper_id = symbol(&file_other_id, "src/other/util.rs", "helper");

    let contradicts = |source: &str, target: &str| -> GraphRecord {
        GraphRecord::agent_memory_edge(
            EdgeLabel::Contradicts,
            source.to_owned(),
            target.to_owned(),
            Some("0.9".to_owned()),
            "recorded disagreement".to_owned(),
        )
    };

    // ── C1: agent Observation vs agent Decision on `login` ──────────────────
    let obs_login_racy_id = agent_memory_stable_id(&["observation", "login", "racy"]);
    let mut obs_login_racy = memory_node(
        obs_login_racy_id.clone(),
        NodeKind::Observation,
        "login is racy",
        "agent_1",
        "2026-01-01T00:00:00Z",
        "0.8",
        vec![link(&sym_login_id, "codegraph", "OBSERVES")],
    );
    if let GraphRecord::Node { text, .. } = &mut obs_login_racy {
        *text = Some(format!("{RAW_TEXT_SENTINEL}: login races under refresh"));
    }
    graph.push(obs_login_racy);

    let dec_login_serial_id = agent_memory_stable_id(&["decision", "login", "serial"]);
    graph.push(memory_node(
        dec_login_serial_id.clone(),
        NodeKind::Decision,
        "login is single-threaded",
        "agent_2",
        "2026-01-02T00:00:00Z",
        "0.7",
        vec![link(&sym_login_id, "codegraph", "OBSERVES")],
    ));
    graph.push(contradicts(&obs_login_racy_id, &dec_login_serial_id));

    // ── C2: passing verification vs agent Failure belief on `login` ────────
    let fail_login_suite_id = agent_memory_stable_id(&["failure", "login", "suite"]);
    graph.push(memory_node(
        fail_login_suite_id.clone(),
        NodeKind::Failure,
        "login suite fails on main",
        "agent_1",
        "2026-01-03T00:00:00Z",
        "0.6",
        vec![link(&sym_login_id, "codegraph", "FAILED_ON")],
    ));

    let test_login_pass_id = verification_stable_id(&["verification", "login", "pass"]);
    let mut test_login_pass = GraphRecord::node(
        test_login_pass_id.clone(),
        NodeKind::TestRun,
        None,
        None,
        None,
        "cargo test login passed".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        status,
        verification_kind,
        executed_at,
        agent_id,
        observed_at,
        confidence,
        evidence_links,
        ..
    } = &mut test_login_pass
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
        *verification_kind = Some("test_run".to_owned());
        *executed_at = Some("2026-01-04T00:00:00Z".to_owned());
        *agent_id = Some("agent_3".to_owned());
        *observed_at = Some("2026-01-04T00:00:00Z".to_owned());
        *confidence = Some("1.0".to_owned());
        *evidence_links = Some(vec![link(&sym_login_id, "codegraph", "VALIDATED_BY")]);
    }
    graph.push(test_login_pass);
    graph.push(contradicts(&test_login_pass_id, &fail_login_suite_id));

    // ── C3: user-context Preference vs agent Observation on `refresh` ───────
    let obs_refresh_lockfree_id = agent_memory_stable_id(&["observation", "refresh", "lockfree"]);
    graph.push(memory_node(
        obs_refresh_lockfree_id.clone(),
        NodeKind::Observation,
        "refresh is lock-free by design",
        "agent_2",
        "2026-01-05T00:00:00Z",
        "0.75",
        vec![link(&sym_refresh_id, "codegraph", "OBSERVES")],
    ));

    let pref_explicit_lock_id = agent_memory_stable_id(&["preference", "explicit", "lock"]);
    let mut pref = GraphRecord::node(
        pref_explicit_lock_id.clone(),
        NodeKind::Preference,
        None,
        None,
        None,
        "prefer explicit locking in auth code".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        observed_at,
        confidence,
        evidence_links,
        user_context,
        ..
    } = &mut pref
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *observed_at = Some("2026-01-06T00:00:00Z".to_owned());
        *confidence = Some("0.95".to_owned());
        *evidence_links = Some(vec![link(&sym_refresh_id, "codegraph", "MENTIONS_SYMBOL")]);
        user_context.decided_by = Some("mark".to_owned());
        user_context.rule_text = Some(format!("{RAW_RULE_SENTINEL}: redacted rule body"));
    }
    graph.push(pref);
    graph.push(contradicts(
        &pref_explicit_lock_id,
        &obs_refresh_lockfree_id,
    ));

    // ── C4: superseded Observation vs replacement on `src/auth/token.rs` ────
    let obs_token_old_id = agent_memory_stable_id(&["observation", "token", "old"]);
    let obs_token_new_id = agent_memory_stable_id(&["observation", "token", "new"]);
    let mut obs_token_old = memory_node(
        obs_token_old_id.clone(),
        NodeKind::Observation,
        "token.rs hashes with MD5",
        "agent_1",
        "2026-01-01T00:00:00Z",
        "0.5",
        vec![link(&file_token_id, "codegraph", "OBSERVES")],
    );
    obs_token_old = obs_token_old.with_superseded_by(obs_token_new_id.clone());
    graph.push(obs_token_old);
    graph.push(memory_node(
        obs_token_new_id.clone(),
        NodeKind::Observation,
        "token.rs hashes with SHA-256",
        "agent_1",
        "2026-02-01T00:00:00Z",
        "0.9",
        vec![link(&file_token_id, "codegraph", "OBSERVES")],
    ));
    graph.push(contradicts(&obs_token_old_id, &obs_token_new_id));

    // ── Code-fact edge: must never surface a code fact as a party ───────────
    graph.push(contradicts(&obs_login_racy_id, &sym_login_id));

    // ── Out-of-scope control under `src/other/` ─────────────────────────────
    let obs_helper_id = agent_memory_stable_id(&["observation", "helper"]);
    let dec_helper_id = agent_memory_stable_id(&["decision", "helper"]);
    graph.push(memory_node(
        obs_helper_id.clone(),
        NodeKind::Observation,
        "helper is slow",
        "agent_9",
        "2026-01-07T00:00:00Z",
        "0.4",
        vec![link(&sym_helper_id, "codegraph", "OBSERVES")],
    ));
    graph.push(memory_node(
        dec_helper_id.clone(),
        NodeKind::Decision,
        "helper is fast enough",
        "agent_9",
        "2026-01-08T00:00:00Z",
        "0.4",
        vec![link(&sym_helper_id, "codegraph", "OBSERVES")],
    ));
    graph.push(contradicts(&obs_helper_id, &dec_helper_id));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    Fixture {
        _temp: temp,
        graph: path,
        sym_login_id,
        obs_login_racy_id,
        dec_login_serial_id,
        fail_login_suite_id,
        test_login_pass_id,
        obs_refresh_lockfree_id,
        pref_explicit_lock_id,
        obs_token_old_id,
        obs_token_new_id,
    }
}

fn run(fx: &Fixture, scope: &str, extra: &[&str]) -> (i32, String, String) {
    let assert = egregore()
        .args(["query", "conflicts", scope, "--graph"])
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

fn parse_ok(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout.trim()).expect("valid JSON response")
}

fn conflicts_of(v: &serde_json::Value) -> &Vec<serde_json::Value> {
    v["conflicts"].as_array().expect("conflicts array")
}

fn parties_of(conflict: &serde_json::Value) -> &Vec<serde_json::Value> {
    conflict["parties"].as_array().expect("parties array")
}

/// All party record IDs across every conflict, sorted.
fn party_ids(v: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = conflicts_of(v)
        .iter()
        .flat_map(|c| parties_of(c).iter())
        .filter_map(|p| p["record_id"].as_str())
        .map(str::to_owned)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

#[test]
fn subsystem_scope_returns_all_unresolved_pairs() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx, "src/auth/", &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v = parse_ok(&stdout);
    assert_eq!(v["ok"], true);
    assert_eq!(v["scope_kind"], "subsystem");

    let conflicts = conflicts_of(&v);
    // C1, C2, C3 live; C4 resolved (excluded); code-fact edge skipped.
    assert_eq!(conflicts.len(), 3, "stdout={stdout}");

    for conflict in conflicts {
        let parties = parties_of(conflict);
        assert_eq!(parties.len(), 2, "every conflict is a citable pair");
        assert_eq!(conflict["resolved"], false);
        for party in parties {
            assert!(
                party["record_id"].as_str().is_some_and(|s| !s.is_empty()),
                "party carries record_id"
            );
            let handles = party["citation_handles"]
                .as_array()
                .expect("citation handles");
            assert!(!handles.is_empty(), "party carries ≥1 citation handle");
            assert!(
                party["author"].as_str().is_some_and(|s| !s.is_empty()),
                "party carries author"
            );
            assert!(
                party["observed_at"].as_str().is_some(),
                "party carries observed_at"
            );
            assert!(
                party["confidence"].as_str().is_some(),
                "party carries confidence"
            );
            assert!(
                party["target_handle"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
                "party carries the shared target handle"
            );
            let trust = party["trust_class"].as_str().expect("trust class");
            assert!(
                trust != "source_derived",
                "code facts never appear as a party"
            );
            let kind = party["kind"].as_str().expect("node kind");
            assert!(
                kind != "Symbol" && kind != "File",
                "no code-graph node as party"
            );
        }
    }

    let ids = party_ids(&v);
    for expected in [
        &fx.obs_login_racy_id,
        &fx.dec_login_serial_id,
        &fx.fail_login_suite_id,
        &fx.test_login_pass_id,
        &fx.obs_refresh_lockfree_id,
        &fx.pref_explicit_lock_id,
    ] {
        assert!(ids.contains(expected), "missing party {expected}");
    }
    // Resolved + out-of-scope parties are absent by default.
    assert!(!ids.contains(&fx.obs_token_old_id));
    assert!(!ids.contains(&fx.obs_token_new_id));
    assert!(!ids.contains(&fx.sym_login_id), "code fact never a party");

    // No raw payloads leak.
    assert!(!stdout.contains(RAW_TEXT_SENTINEL), "raw text leaked");
    assert!(!stdout.contains(RAW_RULE_SENTINEL), "raw rule leaked");
}

#[test]
fn verification_party_is_marked_verification_trust() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx, "src/auth/", &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v = parse_ok(&stdout);

    let c2 = conflicts_of(&v)
        .iter()
        .find(|c| {
            parties_of(c)
                .iter()
                .any(|p| p["record_id"] == fx.fail_login_suite_id)
        })
        .expect("verification-vs-failure conflict present");
    let parties = parties_of(c2);
    let verifier = parties
        .iter()
        .find(|p| p["record_id"] == fx.test_login_pass_id)
        .expect("verification party present");
    assert_eq!(verifier["party_class"], "verification");
    assert_eq!(verifier["trust_class"], "verification_evidence");
    let believer = parties
        .iter()
        .find(|p| p["record_id"] == fx.fail_login_suite_id)
        .expect("agent belief party present");
    assert_eq!(believer["party_class"], "agent_authored");
}

#[test]
fn user_context_party_is_marked_user_context() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx, "src/auth/", &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v = parse_ok(&stdout);
    let c3 = conflicts_of(&v)
        .iter()
        .find(|c| {
            parties_of(c)
                .iter()
                .any(|p| p["record_id"] == fx.pref_explicit_lock_id)
        })
        .expect("user-context conflict present");
    let pref = parties_of(c3)
        .iter()
        .find(|p| p["record_id"] == fx.pref_explicit_lock_id)
        .expect("preference party present");
    assert_eq!(pref["party_class"], "user_context");
}

#[test]
fn resolved_conflicts_excluded_by_default_and_shown_with_flag() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx, "src/auth/", &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v = parse_ok(&stdout);
    assert_eq!(conflicts_of(&v).len(), 3);

    let (code, stdout, stderr) = run(&fx, "src/auth/", &["--include-resolved"]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v = parse_ok(&stdout);
    let conflicts = conflicts_of(&v);
    assert_eq!(conflicts.len(), 4, "stdout={stdout}");
    let c4 = conflicts
        .iter()
        .find(|c| {
            parties_of(c)
                .iter()
                .any(|p| p["record_id"] == fx.obs_token_old_id)
        })
        .expect("resolved conflict shown with --include-resolved");
    assert_eq!(c4["resolved"], true);
    let resolved_by = c4["resolved_by"].as_array().expect("resolved_by");
    assert!(
        resolved_by.iter().any(|id| id == &fx.obs_token_new_id),
        "resolved_by names the superseding record"
    );
}

#[test]
fn symbol_scope_resolves_and_filters() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx, "login", &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v = parse_ok(&stdout);
    assert_eq!(v["scope_kind"], "symbol");
    // C1 + C2 cite `login`; C3 cites `refresh` only.
    assert_eq!(conflicts_of(&v).len(), 2, "stdout={stdout}");
    let ids = party_ids(&v);
    assert!(ids.contains(&fx.obs_login_racy_id));
    assert!(!ids.contains(&fx.obs_refresh_lockfree_id));
}

#[test]
fn file_scope_resolves() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx, "src/auth/token.rs", &["--include-resolved"]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v = parse_ok(&stdout);
    assert_eq!(v["scope_kind"], "file");
    // C3 (refresh symbol) + C4 (token.rs file, resolved).
    assert_eq!(conflicts_of(&v).len(), 2, "stdout={stdout}");
}

#[test]
fn no_match_scope_exits_2_with_stable_envelope() {
    let fx = seed();
    let (code, stdout, _stderr) = run(&fx, "src/void/", &[]);
    assert_eq!(code, 2);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_match");
}

#[test]
fn empty_but_valid_scope_is_exit_0_with_empty_conflicts() {
    let fx = seed();
    // `src/auth/clean.rs` is scanned but has no recorded conflicts.
    let (code, stdout, stderr) = run(&fx, "src/auth/clean.rs", &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v = parse_ok(&stdout);
    assert_eq!(v["ok"], true);
    assert!(conflicts_of(&v).is_empty());
    assert_ne!(v["error"]["code"], serde_json::json!("no_match"));
}

#[test]
fn empty_handle_is_unsupported_exit_1() {
    let fx = seed();
    let (code, _stdout, stderr) = run(&fx, "", &[]);
    assert_eq!(code, 1, "stderr={stderr}");
}

#[test]
fn output_is_byte_stable_across_runs() {
    let fx = seed();
    let mut runs = Vec::new();
    for _ in 0..3 {
        let (code, stdout, stderr) = run(&fx, "src/auth/", &["--include-resolved"]);
        assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
        runs.push(stdout);
    }
    assert_eq!(runs[0], runs[1], "run 1 != run 2");
    assert_eq!(runs[1], runs[2], "run 2 != run 3");
}

#[test]
fn text_format_is_human_readable() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx, "src/auth/", &["--format", "text"]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    assert!(stdout.contains("conflict"), "text output names conflicts");
    assert!(!stdout.contains(RAW_TEXT_SENTINEL), "raw text leaked");
}
