#![allow(missing_docs)]

//! End-to-end + unit tests for `eg query freshness` — the evidence-link
//! freshness workflow (issue #85). The workflow flags each agent observation by
//! whether the code it cites has drifted since the observation was recorded,
//! returning `current` / `drifted` / `unresolved` / `untemporal` verdicts as a
//! freshness lead — never a truth claim — without touching any code fact.

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, MetricKind, NodeKind, SelectionBasis,
    SemanticDriftMetadata, SourceSpan, TemporalMetadata,
    freshness::{self, FreshnessVerdict},
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, SEMANTIC_SCHEMA_VERSION, agent_memory_stable_id,
        semantic_stable_id, stable_id,
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
    }
}

fn temporal(commit: &str, valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

/// Sentinel that must NEVER appear in freshness output (AC9).
const RAW_OBS_TEXT_SENTINEL: &str = "RAW_OBSERVATION_TEXT_SHOULD_NOT_LEAK";

/// Builds a code-graph symbol version pinned to a commit. The summary embeds a
/// `body` marker so two versions with different bodies hash differently.
fn symbol_version(
    sym_id: &str,
    path: &str,
    name: &str,
    sym_span: SourceSpan,
    body: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    GraphRecord::node(
        sym_id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(sym_span),
        Some(name.to_owned()),
        format!("Rust fn {name}\nSource:\n{body}"),
    )
    .with_temporal(temporal(commit, valid_time))
}

fn file_version(
    file_id: &str,
    path: &str,
    body: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    GraphRecord::node(
        file_id.to_owned(),
        NodeKind::File,
        Some(path.to_owned()),
        Some(span(1, 100)),
        Some(path.to_owned()),
        format!("Source file {path}\n{body}"),
    )
    .with_temporal(temporal(commit, valid_time))
}

/// Builds an agent `Observation` citing `target_id` at `anchor_commit`.
#[allow(clippy::too_many_arguments)]
fn observation(
    obs_id: &str,
    text: &str,
    confidence: &str,
    target_id: Option<&str>,
    target_path: Option<&str>,
    target_span: Option<SourceSpan>,
    relation: &str,
    anchor_commit: Option<&str>,
    obs_valid_time: Option<&str>,
) -> GraphRecord {
    let link = EvidenceLink {
        target_record_id: target_id.map(ToOwned::to_owned),
        target_domain: "codegraph".to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: anchor_commit.map(ToOwned::to_owned),
        target_repo_relative_path: target_path.map(ToOwned::to_owned),
        target_span,
        target_git_commit: None,
    };
    let mut node = GraphRecord::node(
        obs_id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        format!("Observation by agent_1:sess_1: {text}"),
    );
    if let GraphRecord::Node {
        schema_version,
        evidence_links,
        agent_id,
        session_id,
        observed_at,
        confidence: conf,
        text: txt,
        valid_time,
        domain,
        ..
    } = &mut node
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![link]);
        *agent_id = Some("agent_1".to_owned());
        *session_id = Some("sess_1".to_owned());
        *observed_at = Some("2026-02-01T00:00:00Z".to_owned());
        *conf = Some(confidence.to_owned());
        *txt = Some(text.to_owned());
        *valid_time = obs_valid_time.map(ToOwned::to_owned);
        *domain = Some("agent_memory".to_owned());
    }
    node
}

fn drift_record(
    drift_id: &str,
    prior_id: &str,
    target_id: &str,
    before_commit: &str,
    after_commit: &str,
    before_vt: &str,
    after_vt: &str,
) -> Vec<GraphRecord> {
    let drift = SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "p".to_owned(),
            name: "m".to_owned(),
            version: "v".to_owned(),
            dim: 8,
            content_hash: "h".to_owned(),
        },
        target_record_id: target_id.to_owned(),
        prior_record_id: prior_id.to_owned(),
        before_git_commit: before_commit.to_owned(),
        after_git_commit: after_commit.to_owned(),
        before_valid_time: before_vt.to_owned(),
        after_valid_time: after_vt.to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.7,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    };
    let node = GraphRecord::node(
        drift_id.to_owned(),
        NodeKind::SemanticDrift,
        None,
        None,
        None,
        "Drift".to_owned(),
    )
    .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
    .with_semantic_drift(drift);
    let edge = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_id.to_owned(),
        target_id.to_owned(),
        None,
        "drifts from".to_owned(),
    );
    vec![node, edge]
}

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    obs_current: String,
    obs_drifted_record: String,
    obs_drifted_content: String,
    obs_unresolved: String,
    obs_untemporal: String,
    obs_neighbor: String,
}

/// Seeds a graph where K observations cite code that provably drifted/became
/// unresolvable after their anchor, and M cite unchanged code. Covers all four
/// verdicts plus the neighbor false-positive guard.
#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let mut graph = Graph::new();

    // ── Repository + two symbols under one file ───────────────────────────────
    let repo_id = stable_id(&["node", "Repository", "repo-a"]);
    graph.push(GraphRecord::node(
        repo_id,
        NodeKind::Repository,
        None,
        None,
        Some("repo-a".to_owned()),
        "Repository repo-a".to_owned(),
    ));

    let file_path = "src/query.rs";
    let stable_sym = stable_id(&[
        "node",
        "symbol",
        "fn",
        "repo-a",
        file_path,
        "stable_fn",
        "0",
    ]);
    let drifting_sym = stable_id(&[
        "node",
        "symbol",
        "fn",
        "repo-a",
        file_path,
        "handle_query",
        "0",
    ]);
    let content_sym = stable_id(&[
        "node",
        "symbol",
        "fn",
        "repo-a",
        file_path,
        "content_fn",
        "0",
    ]);
    let sibling_sym = stable_id(&[
        "node",
        "symbol",
        "fn",
        "repo-a",
        file_path,
        "sibling_fn",
        "0",
    ]);
    let removed_sym = stable_id(&[
        "node",
        "symbol",
        "fn",
        "repo-a",
        file_path,
        "removed_fn",
        "0",
    ]);

    // commit_a = anchor for all citations; commit_b = later state.
    let (ca, cb) = ("commit_a", "commit_b");
    let (vt_a, vt_b) = ("2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z");

    // stable_fn: identical body at both commits → never drifts.
    graph.push(symbol_version(
        &stable_sym,
        file_path,
        "stable_fn",
        span(10, 20),
        "stable_body",
        ca,
        vt_a,
    ));
    graph.push(symbol_version(
        &stable_sym,
        file_path,
        "stable_fn",
        span(10, 20),
        "stable_body",
        cb,
        vt_b,
    ));

    // handle_query: drift record measures change after the anchor.
    graph.push(symbol_version(
        &drifting_sym,
        file_path,
        "handle_query",
        span(30, 40),
        "validate_token",
        ca,
        vt_a,
    ));
    graph.push(symbol_version(
        &drifting_sym,
        file_path,
        "handle_query",
        span(30, 40),
        "validate_token",
        cb,
        vt_b,
    ));

    // content_fn: no drift record, but body changes between commit_a and commit_b.
    graph.push(symbol_version(
        &content_sym,
        file_path,
        "content_fn",
        span(50, 60),
        "body_v1",
        ca,
        vt_a,
    ));
    graph.push(symbol_version(
        &content_sym,
        file_path,
        "content_fn",
        span(50, 60),
        "body_v2",
        cb,
        vt_b,
    ));

    // sibling_fn: body changes after anchor, but nobody cites it. Used to prove a
    // citation to stable_fn in the SAME file is not flagged by sibling drift.
    graph.push(symbol_version(
        &sibling_sym,
        file_path,
        "sibling_fn",
        span(70, 80),
        "sib_v1",
        ca,
        vt_a,
    ));
    graph.push(symbol_version(
        &sibling_sym,
        file_path,
        "sibling_fn",
        span(70, 80),
        "sib_v2",
        cb,
        vt_b,
    ));

    // removed_fn: present at commit_a, then tombstoned → unresolved.
    graph.push(symbol_version(
        &removed_sym,
        file_path,
        "removed_fn",
        span(90, 95),
        "removed_body",
        ca,
        vt_a,
    ));
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &removed_sym]),
        schema_version: aletheia_egregore::SCHEMA_VERSION,
        deleted_id: removed_sym.clone(),
        summary: "removed_fn deleted".to_owned(),
        producer: None,
    });

    // ── Drift record for handle_query (prior == cited symbol) ─────────────────
    for r in drift_record(
        &semantic_stable_id(&["drift", "handle_query"]),
        &drifting_sym,
        &drifting_sym,
        ca,
        cb,
        vt_a,
        vt_b,
    ) {
        graph.push(r);
    }
    // A neighbor drift on sibling_fn — must NOT taint a stable_fn citation (AC5).
    for r in drift_record(
        &semantic_stable_id(&["drift", "sibling_fn"]),
        &sibling_sym,
        &sibling_sym,
        ca,
        cb,
        vt_a,
        vt_b,
    ) {
        graph.push(r);
    }

    // ── Agent + session provenance ────────────────────────────────────────────
    let agent_id = agent_memory_stable_id(&["node", "agent", "agent_1"]);
    let mut agent = GraphRecord::node(
        agent_id,
        NodeKind::Agent,
        None,
        None,
        Some("agent_1".to_owned()),
        "Agent agent_1".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id: aid,
        ..
    } = &mut agent
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *aid = Some("agent_1".to_owned());
    }
    graph.push(agent);

    // ── Observations (one per verdict) ────────────────────────────────────────
    let obs_current = agent_memory_stable_id(&["obs", "current"]);
    graph.push(observation(
        &obs_current,
        "stable_fn computes the key here",
        "0.8",
        Some(&stable_sym),
        Some(file_path),
        Some(span(10, 20)),
        "OBSERVES",
        Some(ca),
        None,
    ));

    let obs_drifted_record = agent_memory_stable_id(&["obs", "drifted_record"]);
    graph.push(observation(
        &obs_drifted_record,
        &format!("handle_query validates the token here {RAW_OBS_TEXT_SENTINEL}"),
        "0.9",
        Some(&drifting_sym),
        Some(file_path),
        Some(span(30, 40)),
        "OBSERVES",
        Some(ca),
        None,
    ));

    let obs_drifted_content = agent_memory_stable_id(&["obs", "drifted_content"]);
    graph.push(observation(
        &obs_drifted_content,
        "content_fn returns body_v1",
        "0.7",
        Some(&content_sym),
        Some(file_path),
        Some(span(50, 60)),
        "OBSERVES",
        Some(ca),
        None,
    ));

    let obs_unresolved = agent_memory_stable_id(&["obs", "unresolved"]);
    graph.push(observation(
        &obs_unresolved,
        "removed_fn does the thing",
        "0.6",
        Some(&removed_sym),
        Some(file_path),
        Some(span(90, 95)),
        "OBSERVES",
        Some(ca),
        None,
    ));

    let obs_untemporal = agent_memory_stable_id(&["obs", "untemporal"]);
    graph.push(observation(
        &obs_untemporal,
        "stable_fn is fine, no anchor recorded",
        "0.5",
        Some(&stable_sym),
        Some(file_path),
        Some(span(10, 20)),
        "OBSERVES",
        None,
        None,
    ));

    // Neighbor guard: cite stable_fn (unchanged) in a file where sibling_fn drifted.
    let obs_neighbor = agent_memory_stable_id(&["obs", "neighbor"]);
    graph.push(observation(
        &obs_neighbor,
        "stable_fn unaffected by sibling churn",
        "0.8",
        Some(&stable_sym),
        Some(file_path),
        Some(span(10, 20)),
        "MENTIONS_SYMBOL",
        Some(ca),
        None,
    ));

    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("freshness_seeded.jsonl");
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    Fixture {
        _temp: temp,
        graph: path,
        obs_current,
        obs_drifted_record,
        obs_drifted_content,
        obs_unresolved,
        obs_untemporal,
        obs_neighbor,
    }
}

fn run(path: &std::path::Path, extra: &[&str]) -> (i32, String, String) {
    let mut args = vec!["query", "freshness", "--graph", path.to_str().unwrap()];
    args.extend_from_slice(extra);
    let assert = egregore().args(&args).assert();
    let out = assert.get_output();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn verdict_for<'a>(v: &'a serde_json::Value, obs_id: &str) -> &'a serde_json::Value {
    v["verdicts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["observation_id"] == obs_id)
        .unwrap_or_else(|| panic!("no verdict for {obs_id}"))
}

// ── AC1 + AC2: per-observation verdict covering all four classes ─────────────

#[test]
fn classifies_each_verdict_class() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx.graph, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(verdict_for(&v, &fx.obs_current)["verdict"], "current");
    assert_eq!(
        verdict_for(&v, &fx.obs_drifted_record)["verdict"],
        "drifted"
    );
    assert_eq!(
        verdict_for(&v, &fx.obs_drifted_content)["verdict"],
        "drifted"
    );
    assert_eq!(verdict_for(&v, &fx.obs_unresolved)["verdict"], "unresolved");
    assert_eq!(verdict_for(&v, &fx.obs_untemporal)["verdict"], "untemporal");
}

// ── AC3: freshness lead, never a truth claim ─────────────────────────────────

#[test]
fn stale_verdicts_are_freshness_leads_not_truth_claims() {
    let fx = seed();
    let (_c, stdout, _e) = run(&fx.graph, &[]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    let drifted = verdict_for(&v, &fx.obs_drifted_record);
    let lead = drifted["freshness_lead"].as_str().expect("lead present");
    assert!(lead.contains("re-verify"), "lead={lead}");
    // Must never assert the note is false/superseded/correct.
    let blob = stdout.to_lowercase();
    assert!(!blob.contains("\"false\""));
    assert!(!blob.contains("superseded"));
    assert!(!blob.contains("now incorrect"));
    // current/untemporal carry no lead.
    assert!(
        verdict_for(&v, &fx.obs_current)
            .get("freshness_lead")
            .is_none()
    );
    assert!(
        verdict_for(&v, &fx.obs_untemporal)
            .get("freshness_lead")
            .is_none()
    );
}

// ── AC4: trust separation — code facts are never mutated ─────────────────────

#[test]
fn workflow_never_mutates_code_facts() {
    let fx = seed();
    let before = fs::read_to_string(&fx.graph).unwrap();
    let (code, _o, _e) = run(&fx.graph, &[]);
    assert_eq!(code, 0);
    let after = fs::read_to_string(&fx.graph).unwrap();
    assert_eq!(before, after, "freshness query must not modify the store");
}

// ── AC5: no false drift from neighbors ───────────────────────────────────────

#[test]
fn neighbor_drift_does_not_flag_unrelated_symbol() {
    let fx = seed();
    let (_c, stdout, _e) = run(&fx.graph, &[]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    // obs_neighbor cites stable_fn (unchanged) in a file where sibling_fn drifted.
    assert_eq!(verdict_for(&v, &fx.obs_neighbor)["verdict"], "current");
}

// ── AC6: every flagged verdict carries provenance + cited + triggering handle ─

#[test]
fn flagged_verdicts_carry_full_provenance_and_triggers() {
    let fx = seed();
    let (_c, stdout, _e) = run(&fx.graph, &[]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    let drifted = verdict_for(&v, &fx.obs_drifted_record);
    assert_eq!(drifted["provenance"]["agent_id"], "agent_1");
    assert_eq!(drifted["provenance"]["session_id"], "sess_1");
    assert_eq!(drifted["provenance"]["observed_at"], "2026-02-01T00:00:00Z");
    assert_eq!(drifted["provenance"]["confidence"], "0.9");
    assert!(drifted["cited_handle"]["target_record_id"].is_string());
    assert_eq!(
        drifted["cited_handle"]["repo_relative_path"],
        "src/query.rs"
    );
    assert_eq!(drifted["cited_handle"]["anchor_commit"], "commit_a");
    // Drift-record trigger names the drift record + later commit.
    let trig = &drifted["triggering_handle"];
    assert_eq!(trig["kind"], "drift_record");
    assert!(
        trig["drift_record_id"]
            .as_str()
            .unwrap()
            .starts_with("semantic:v1:")
    );
    assert_eq!(trig["after_git_commit"], "commit_b");

    // Content-change trigger names the later commit + a content hash.
    let content = verdict_for(&v, &fx.obs_drifted_content);
    let ctrig = &content["triggering_handle"];
    assert_eq!(ctrig["kind"], "content_change");
    assert_eq!(ctrig["after_git_commit"], "commit_b");
    assert!(
        ctrig["content_hash"]
            .as_str()
            .unwrap()
            .starts_with("blake3:")
    );

    // Unresolved trigger names the tombstone.
    let unresolved = verdict_for(&v, &fx.obs_unresolved);
    assert_eq!(unresolved["triggering_handle"]["kind"], "handle_removed");
    assert!(unresolved["triggering_handle"]["tombstone_id"].is_string());
}

// ── AC7: stale-only mode + non-silent empty result ───────────────────────────

#[test]
fn stale_only_returns_only_drifted_and_unresolved() {
    let fx = seed();
    let (code, stdout, _e) = run(&fx.graph, &["--stale-only"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["stale_only"], true);
    assert_eq!(v["diagnostic"], "stale_observations_present");
    let verdicts = v["verdicts"].as_array().unwrap();
    assert!(!verdicts.is_empty());
    for entry in verdicts {
        let verdict = entry["verdict"].as_str().unwrap();
        assert!(
            verdict == "drifted" || verdict == "unresolved",
            "unexpected verdict in stale-only: {verdict}"
        );
    }
}

#[test]
fn stale_only_empty_is_reported_not_silent() {
    // A graph with only a `current` observation yields no stale rows.
    let mut graph = Graph::new();
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", "src/a.rs", "f", "0"]);
    graph.push(symbol_version(
        &sym,
        "src/a.rs",
        "f",
        span(1, 5),
        "b",
        "c1",
        "2026-01-01T00:00:00Z",
    ));
    let obs = agent_memory_stable_id(&["obs", "only"]);
    graph.push(observation(
        &obs,
        "f exists",
        "0.9",
        Some(&sym),
        Some("src/a.rs"),
        Some(span(1, 5)),
        "OBSERVES",
        Some("c1"),
        None,
    ));
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("g.jsonl");
    fs::write(&path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, _e) = run(&path, &["--stale-only"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["diagnostic"], "no_stale_observations");
    assert_eq!(v["verdicts"].as_array().unwrap().len(), 0);
    assert_eq!(v["ok"], true);
}

// ── AC8: read-only + byte-identical across 5 runs ────────────────────────────

#[test]
fn deterministic_byte_identical_across_runs() {
    let fx = seed();
    let first = run(&fx.graph, &[]).1;
    for _ in 0..4 {
        let again = run(&fx.graph, &[]).1;
        assert_eq!(first, again, "output must be byte-identical across runs");
    }
}

// ── AC9: no raw observation text leaks ───────────────────────────────────────

#[test]
fn output_never_leaks_raw_observation_text() {
    let fx = seed();
    let (_c, stdout, _e) = run(&fx.graph, &[]);
    assert!(
        !stdout.contains(RAW_OBS_TEXT_SENTINEL),
        "raw observation text leaked into freshness output"
    );
}

// ── Library-level success metric: 100% of K stale, 0 false positives ─────────

#[test]
fn library_classifies_all_stale_and_no_false_positives() {
    let fx = seed();
    let jsonl = fs::read_to_string(&fx.graph).unwrap();
    let records = aletheia_egregore::adapters::records_from_jsonl(&jsonl).unwrap();
    let verdicts = freshness::evidence_link_freshness(&records);

    let stale = verdicts.iter().filter(|e| e.verdict.is_stale()).count();
    // K = 3 stale: drift-record, content-change, unresolved.
    assert_eq!(stale, 3, "expected exactly 3 stale verdicts");
    // M observations citing unchanged code must never be flagged.
    for e in &verdicts {
        if e.observation_id == fx.obs_current || e.observation_id == fx.obs_neighbor {
            assert_eq!(e.verdict, FreshnessVerdict::Current);
        }
    }
}

// ── File-level citation: content change on the whole file drifts ─────────────

#[test]
fn file_level_citation_drifts_on_content_change() {
    let mut graph = Graph::new();
    let file_path = "src/main.rs";
    let file_id = stable_id(&["node", "File", "repo-a", file_path]);
    // File body changes between commit c1 and c2.
    graph.push(file_version(
        &file_id,
        file_path,
        "v1",
        "c1",
        "2026-01-01T00:00:00Z",
    ));
    graph.push(file_version(
        &file_id,
        file_path,
        "v2",
        "c2",
        "2026-01-02T00:00:00Z",
    ));

    // Cite the file by triple (path only, no record ID, no span).
    let obs = agent_memory_stable_id(&["obs", "file"]);
    graph.push(observation(
        &obs,
        "main.rs sets up the CLI",
        "0.8",
        None,
        Some(file_path),
        None,
        "OBSERVES",
        Some("c1"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.jsonl");
    fs::write(&path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let entry = verdict_for(&v, &obs);
    assert_eq!(entry["verdict"], "drifted");
    assert_eq!(entry["triggering_handle"]["kind"], "content_change");
    assert_eq!(entry["cited_handle"]["repo_relative_path"], file_path);
}
