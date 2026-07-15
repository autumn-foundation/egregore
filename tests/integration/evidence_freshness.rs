#![allow(missing_docs)]

//! End-to-end + unit tests for `eg query evidence-freshness` — the evidence-link
//! freshness workflow (issue #85). The workflow flags each agent observation by
//! whether the code it cites has drifted since the observation was recorded,
//! returning `current` / `drifted` / `unresolved` / `untemporal` verdicts as a
//! freshness lead — never a truth claim — without touching any code fact.

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, MetricKind, NodeKind, SelectionBasis,
    SemanticDriftMetadata, SourceSpan, TemporalMetadata,
    evidence_freshness::{self as freshness, FreshnessVerdict},
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, SEMANTIC_SCHEMA_VERSION, agent_memory_stable_id,
        semantic_stable_id, stable_id,
    },
    scan_repository_history,
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

/// Builds a Rust `Module` version pinned to a commit (issue #206). The display
/// summary is name-only (`Rust module {name}`), so body drift is carried by the
/// additive `content_signature` handle rather than the summary: two versions
/// with the same name but different `signature` values hash differently, while
/// the same `signature` hashes identically.
fn module_version(
    mod_id: &str,
    path: &str,
    name: &str,
    mod_span: SourceSpan,
    signature: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    GraphRecord::node(
        mod_id.to_owned(),
        NodeKind::Module,
        Some(path.to_owned()),
        Some(mod_span),
        Some(name.to_owned()),
        format!("Rust module {name}"),
    )
    .with_content_signature(signature.to_owned())
    .with_temporal(temporal(commit, valid_time))
}

/// Builds a Rust `Import` version pinned to a commit (issue #206). Unlike
/// [`module_version`], imports carry NO `content_signature`: the import's stable
/// ID already encodes the full trimmed `use ...;` declaration (via
/// `import_name`, the whole path — not the bound leaf), so a body change mints a
/// DIFFERENT record ID rather than riding a content signature. A content
/// signature on an import could therefore never be the drift trigger.
fn import_version(
    import_id: &str,
    path: &str,
    name: &str,
    import_span: SourceSpan,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    GraphRecord::node(
        import_id.to_owned(),
        NodeKind::Import,
        Some(path.to_owned()),
        Some(import_span),
        Some(name.to_owned()),
        format!("Rust import {name}"),
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
    let mut untemporal_node = observation(
        &obs_untemporal,
        "stable_fn is fine, no anchor recorded",
        "0.5",
        Some(&stable_sym),
        Some(file_path),
        Some(span(10, 20)),
        "OBSERVES",
        None,
        None,
    );
    // Genuinely untemporal: no commit, no valid-time, and no recording time
    // (`observed_at`) either, so no anchor can be inferred.
    if let GraphRecord::Node { observed_at, .. } = &mut untemporal_node {
        *observed_at = None;
    }
    graph.push(untemporal_node);

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
    let mut args = vec![
        "query",
        "evidence-freshness",
        "--graph",
        path.to_str().unwrap(),
    ];
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

fn find_verdict<'a>(v: &'a serde_json::Value, obs_id: &str) -> Option<&'a serde_json::Value> {
    v["verdicts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["observation_id"] == obs_id)
}

// ── Retracted notes are not classified ───────────────────────────────────────

#[test]
fn tombstoned_observation_yields_no_verdict() {
    // A drifted observation that is later retracted (tombstoned) must not appear
    // in freshness output — it is no longer part of current memory.
    let mut graph = Graph::new();
    let path = "src/r.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    graph.push(symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "v1",
        "commit_a",
        "2026-01-01T00:00:00Z",
    ));
    graph.push(symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "v2",
        "commit_b",
        "2026-01-02T00:00:00Z",
    ));
    let obs = agent_memory_stable_id(&["obs", "retracted"]);
    graph.push(observation(
        &obs,
        "f does the thing",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));
    // Retract the observation.
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &obs]),
        schema_version: aletheia_egregore::SCHEMA_VERSION,
        deleted_id: obs.clone(),
        summary: "observation retracted".to_owned(),
        producer: None,
    });

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(
        find_verdict(&v, &obs).is_none(),
        "tombstoned observation must not be classified"
    );
}

// ── Non-handle codegraph links (Commit/Change) are not code handles ──────────

#[test]
fn non_handle_codegraph_link_is_not_flagged() {
    // An observation that EXPLAINS_CHANGE a codegraph `Commit` cites a valid link,
    // not a stale code handle, so it must not surface as `unresolved`.
    let mut graph = Graph::new();
    let commit_id = stable_id(&["node", "Commit", "repo-a", "commit_a"]);
    graph.push(GraphRecord::node(
        commit_id.clone(),
        NodeKind::Commit,
        None,
        None,
        Some("commit_a".to_owned()),
        "Commit commit_a".to_owned(),
    ));
    let obs = agent_memory_stable_id(&["obs", "explains_change"]);
    graph.push(observation(
        &obs,
        "this commit introduced the bug",
        "0.9",
        Some(&commit_id),
        None,
        None,
        "EXPLAINS_CHANGE",
        Some("commit_a"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(
        find_verdict(&v, &obs).is_none(),
        "a codegraph Commit link is not a code handle and must not be classified"
    );
}

// ── Drift recovered from the DRIFTS_PRIOR edge when metadata prior is stale ───

#[test]
fn drift_edge_triggers_when_metadata_prior_is_stale() {
    // Body is identical across commits, so only a drift record can flag this
    // handle. The drift metadata's prior_record_id is stale, but its DRIFTS_PRIOR
    // edge points at the cited symbol — freshness must still report `drifted`.
    let mut graph = Graph::new();
    let path = "src/e.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    for (commit, vt) in [
        ("commit_a", "2026-01-01T00:00:00Z"),
        ("commit_b", "2026-01-02T00:00:00Z"),
    ] {
        graph.push(symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "identical_body",
            commit,
            vt,
        ));
    }

    // Drift node whose metadata prior is a stale/unrelated ID.
    let drift_id = semantic_stable_id(&["drift", "edge_only"]);
    let drift = SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "p".to_owned(),
            name: "m".to_owned(),
            version: "v".to_owned(),
            dim: 8,
            content_hash: "h".to_owned(),
        },
        target_record_id: sym.clone(),
        prior_record_id: "stale:does-not-resolve".to_owned(),
        before_git_commit: "commit_a".to_owned(),
        after_git_commit: "commit_b".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.7,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    };
    graph.push(
        GraphRecord::node(
            drift_id.clone(),
            NodeKind::SemanticDrift,
            None,
            None,
            None,
            "Drift".to_owned(),
        )
        .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
        .with_semantic_drift(drift),
    );
    // The stable recovery path: a DRIFTS_PRIOR edge to the cited symbol.
    graph.push(GraphRecord::edge(
        EdgeLabel::DriftsPrior,
        drift_id,
        sym.clone(),
        None,
        "drifts prior".to_owned(),
    ));

    let obs = agent_memory_stable_id(&["obs", "edge_drift"]);
    graph.push(observation(
        &obs,
        "f behaves a certain way",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let entry = verdict_for(&v, &obs);
    assert_eq!(entry["verdict"], "drifted");
    assert_eq!(entry["triggering_handle"]["kind"], "drift_record");
}

// ── observed_at is a usable anchor when no commit / valid-time is present ─────

#[test]
fn observed_at_anchors_drift_when_no_valid_time() {
    // The observation carries only a recording time (observed_at), no commit and
    // no valid_time. Code drifts after that recording time → `drifted`, not the
    // `untemporal` that a valid-time-only anchor would yield.
    let mut graph = Graph::new();
    let path = "src/o.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    // observation() stamps observed_at = 2026-02-01. v1 precedes it; v2 follows.
    graph.push(symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "body_v1",
        "commit_a",
        "2026-01-01T00:00:00Z",
    ));
    graph.push(symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "body_v2",
        "commit_c",
        "2026-03-01T00:00:00Z",
    ));
    let obs = agent_memory_stable_id(&["obs", "observed_at"]);
    graph.push(observation(
        &obs,
        "f returns body_v1",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        None, // no commit anchor
        None, // no valid_time
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let entry = verdict_for(&v, &obs);
    assert_eq!(entry["verdict"], "drifted");
    assert_eq!(
        entry["cited_handle"]["anchor_valid_time"], "2026-02-01T00:00:00Z",
        "the recording time should be used as the anchor"
    );
}

// ── Commit-anchored drift across a committer-timestamp tie still drifts ───────

#[test]
fn commit_tie_drift_is_detected() {
    // Two commits share the same committer timestamp. A drift whose before-commit
    // is exactly the anchor commit must register even though the later valid-time
    // is not strictly greater.
    let mut graph = Graph::new();
    let path = "src/t.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let tie = "2026-01-01T00:00:00Z";
    // Identical body so only the drift record (not a content change) can flag it.
    graph.push(symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "same",
        "commit_a",
        tie,
    ));
    graph.push(symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "same",
        "commit_b",
        tie,
    ));
    for r in drift_record(
        &semantic_stable_id(&["drift", "tie"]),
        &sym,
        &sym,
        "commit_a",
        "commit_b",
        tie,
        tie,
    ) {
        graph.push(r);
    }
    let obs = agent_memory_stable_id(&["obs", "tie"]);
    graph.push(observation(
        &obs,
        "f at commit_a",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(verdict_for(&v, &obs)["verdict"], "drifted");
}

// ── Triple resolved at the anchor commit, not the live path/span occupant ─────

#[test]
fn triple_resolves_at_commit_not_reused_path_span() {
    // A note cites a symbol by triple at commit_a. That symbol is later removed
    // and a DIFFERENT live symbol reuses the same path/span. Honoring the anchor
    // commit binds the citation to the original identity → `unresolved`, not a
    // silent re-point at the new occupant.
    let mut graph = Graph::new();
    let path = "src/x.rs";
    let old_sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "old", "0"]);
    let new_sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "new", "0"]);

    // OLD existed at commit_a, then was tombstoned.
    graph.push(symbol_version(
        &old_sym,
        path,
        "old",
        span(10, 20),
        "old_body",
        "commit_a",
        "2026-01-01T00:00:00Z",
    ));
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &old_sym]),
        schema_version: aletheia_egregore::SCHEMA_VERSION,
        deleted_id: old_sym.clone(),
        summary: "old removed".to_owned(),
        producer: None,
    });
    // NEW reused the same path/span at commit_b and is live.
    graph.push(symbol_version(
        &new_sym,
        path,
        "new",
        span(10, 20),
        "new_body",
        "commit_b",
        "2026-01-02T00:00:00Z",
    ));

    // Cite by triple (no record ID) anchored at commit_a.
    let obs = agent_memory_stable_id(&["obs", "triple_commit"]);
    graph.push(observation(
        &obs,
        "the symbol here did X",
        "0.9",
        None,
        Some(path),
        Some(span(10, 20)),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let entry = verdict_for(&v, &obs);
    assert_eq!(entry["verdict"], "unresolved");
    assert_eq!(entry["triggering_handle"]["kind"], "handle_removed");
}

// ── A retracted (tombstoned) drift record is not a trigger ───────────────────

#[test]
fn tombstoned_drift_record_is_not_a_trigger() {
    // The cited symbol has an identical body across commits, so only a drift
    // record could flag it. That drift record is itself tombstoned (a retracted /
    // recomputed measurement), so the verdict must be `current`, not `drifted`.
    let mut graph = Graph::new();
    let path = "src/td.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    for (commit, vt) in [
        ("commit_a", "2026-01-01T00:00:00Z"),
        ("commit_b", "2026-01-02T00:00:00Z"),
    ] {
        graph.push(symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "same_body",
            commit,
            vt,
        ));
    }
    let drift_id = semantic_stable_id(&["drift", "retracted"]);
    for r in drift_record(
        &drift_id,
        &sym,
        &sym,
        "commit_a",
        "commit_b",
        "2026-01-01T00:00:00Z",
        "2026-01-02T00:00:00Z",
    ) {
        graph.push(r);
    }
    // Retract the drift record.
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &drift_id]),
        schema_version: aletheia_egregore::SCHEMA_VERSION,
        deleted_id: drift_id.clone(),
        summary: "drift retracted".to_owned(),
        producer: None,
    });
    let obs = agent_memory_stable_id(&["obs", "td"]);
    graph.push(observation(
        &obs,
        "f at commit_a",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(verdict_for(&v, &obs)["verdict"], "current");
}

// ── Content change across a committer-timestamp tie still drifts ─────────────

#[test]
fn content_change_across_committer_timestamp_tie_drifts() {
    // Two commits share the same committer timestamp, with no drift record. The
    // later version is a direct child of the anchor commit and its body changed,
    // so the content-hash path must report `drifted` despite the tied timestamps.
    let mut graph = Graph::new();
    let path = "src/cc.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let tie = "2026-01-01T00:00:00Z";
    graph.push(symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "body_v1",
        "commit_a",
        tie,
    ));
    // commit_b shares the timestamp but is a child of commit_a and changed the body.
    let mut child = symbol_version(&sym, path, "f", span(1, 5), "body_v2", "commit_b", tie);
    if let GraphRecord::Node {
        temporal: Some(t), ..
    } = &mut child
    {
        t.git_parent_commits = vec!["commit_a".to_owned()];
    }
    graph.push(child);
    let obs = agent_memory_stable_id(&["obs", "cc"]);
    graph.push(observation(
        &obs,
        "f returns body_v1",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let entry = verdict_for(&v, &obs);
    assert_eq!(entry["verdict"], "drifted");
    assert_eq!(entry["triggering_handle"]["kind"], "content_change");
}

// ── History-only removal (no tombstone) is unresolved, not current ───────────

#[test]
fn historical_only_handle_without_tombstone_is_unresolved() {
    // `scan-history` leaves no tombstone when a symbol is removed; the ghost
    // symbol exists only at commit_a while the graph frontier advanced to
    // commit_b. A note citing the ghost must be `unresolved`, never `current`.
    let mut graph = Graph::new();
    let path = "src/h.rs";
    let ghost = stable_id(&["node", "symbol", "fn", "repo-a", path, "ghost", "0"]);
    let keeper = stable_id(&["node", "symbol", "fn", "repo-a", path, "keeper", "0"]);
    // ghost present only at commit_a (no tombstone).
    graph.push(symbol_version(
        &ghost,
        path,
        "ghost",
        span(10, 20),
        "ghost_body",
        "commit_a",
        "2026-01-01T00:00:00Z",
    ));
    // keeper is HEAD (child of commit_a) → commit_a is interior, not a tip, so
    // the ghost present only at commit_a is treated as removed.
    let mut keeper_rec = symbol_version(
        &keeper,
        path,
        "keeper",
        span(30, 40),
        "keeper_body",
        "commit_b",
        "2026-01-02T00:00:00Z",
    );
    if let GraphRecord::Node {
        temporal: Some(t), ..
    } = &mut keeper_rec
    {
        t.git_parent_commits = vec!["commit_a".to_owned()];
    }
    graph.push(keeper_rec);
    let obs = agent_memory_stable_id(&["obs", "ghost"]);
    graph.push(observation(
        &obs,
        "ghost did the thing",
        "0.9",
        Some(&ghost),
        Some(path),
        Some(span(10, 20)),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let entry = verdict_for(&v, &obs);
    assert_eq!(entry["verdict"], "unresolved");
    assert_eq!(entry["triggering_handle"]["kind"], "handle_absent");
}

// ── HEAD code is live even when an ancestor commit is future-dated ───────────

#[test]
fn head_handle_live_despite_future_dated_ancestor() {
    // Clock skew / rebase: an ancestor commit carries a LATER committer timestamp
    // than HEAD. A max-timestamp frontier would treat the ancestor as the frontier
    // and wrongly drop the HEAD symbol; the tip-commit frontier keeps HEAD live.
    let mut graph = Graph::new();
    let path = "src/skew.rs";
    let head_sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "head", "0"]);
    // head is HEAD (child of commit_old) but committed earlier than its ancestor.
    let mut head_rec = symbol_version(
        &head_sym,
        path,
        "head",
        span(10, 20),
        "head_body",
        "commit_head",
        "2026-01-01T00:00:00Z",
    );
    if let GraphRecord::Node {
        temporal: Some(t), ..
    } = &mut head_rec
    {
        t.git_parent_commits = vec!["commit_old".to_owned()];
    }
    graph.push(head_rec);
    // An ancestor symbol on the future-dated commit_old (no longer at HEAD).
    graph.push(symbol_version(
        &stable_id(&["node", "symbol", "fn", "repo-a", path, "old", "0"]),
        path,
        "old",
        span(30, 40),
        "old_body",
        "commit_old",
        "2026-03-01T00:00:00Z",
    ));

    let obs = agent_memory_stable_id(&["obs", "head"]);
    graph.push(observation(
        &obs,
        "head does the thing",
        "0.9",
        Some(&head_sym),
        Some(path),
        Some(span(10, 20)),
        "OBSERVES",
        Some("commit_head"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        verdict_for(&v, &obs)["verdict"],
        "current",
        "HEAD code must stay live even when an ancestor commit is future-dated"
    );
}

// ── A triple cites a Module handle, not the whole file ───────────────────────

#[test]
fn triple_resolves_module_handle_not_file_fallback() {
    // A Module and a File share a path. The module's body changed across commits;
    // the file body did not. A triple citing the module's span must resolve to the
    // module (→ `drifted`), not fall back to the unchanged file (→ `current`).
    let mut graph = Graph::new();
    let path = "src/m.rs";
    let module = stable_id(&["node", "module", "repo-a", path, "m"]);
    let file = stable_id(&["node", "File", "repo-a", path]);
    let module_span = span(5, 10);
    for (commit, vt, body) in [
        ("commit_a", "2026-01-01T00:00:00Z", "mod_v1"),
        ("commit_b", "2026-01-02T00:00:00Z", "mod_v2"),
    ] {
        graph.push(
            GraphRecord::node(
                module.clone(),
                NodeKind::Module,
                Some(path.to_owned()),
                Some(module_span),
                Some("m".to_owned()),
                format!("Rust mod m\nSource:\n{body}"),
            )
            .with_temporal(temporal(commit, vt)),
        );
        // File body is identical across both commits.
        graph.push(file_version(&file, path, "file_body", commit, vt));
    }
    let obs = agent_memory_stable_id(&["obs", "module"]);
    graph.push(observation(
        &obs,
        "module m sets things up",
        "0.9",
        None, // no record id → triple resolution
        Some(path),
        Some(module_span),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let entry = verdict_for(&v, &obs);
    assert_eq!(
        entry["verdict"], "drifted",
        "triple should resolve the changed module, not the unchanged file"
    );
    assert_eq!(entry["triggering_handle"]["kind"], "content_change");
}

/// Builds a `Commit` node carrying temporal metadata and parent commits.
fn commit_node(sha: &str, parents: &[&str], valid_time: &str) -> GraphRecord {
    GraphRecord::node(
        stable_id(&["node", "commit", "repo-a", sha]),
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("Git commit {sha}"),
    )
    .with_temporal(TemporalMetadata {
        git_commit: sha.to_owned(),
        git_parent_commits: parents.iter().map(|p| (*p).to_owned()).collect(),
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    })
}

// ── A tip commit that deletes the last code file is still the frontier ───────

#[test]
fn deletion_commit_advances_frontier_to_unresolved() {
    // commit_b is HEAD and deletes the last `.rs` file, so it emits a `Commit`
    // node but no code-handle version. The frontier must still advance to
    // commit_b (via that node), making the symbol that lived only at commit_a
    // `unresolved` rather than a false `current`.
    let mut graph = Graph::new();
    let path = "src/d.rs";
    let gone = stable_id(&["node", "symbol", "fn", "repo-a", path, "gone", "0"]);
    graph.push(symbol_version(
        &gone,
        path,
        "gone",
        span(10, 20),
        "gone_body",
        "commit_a",
        "2026-01-01T00:00:00Z",
    ));
    // commit_a exists as a commit; commit_b is its child and the tip, with no code.
    graph.push(commit_node("commit_a", &[], "2026-01-01T00:00:00Z"));
    graph.push(commit_node(
        "commit_b",
        &["commit_a"],
        "2026-01-02T00:00:00Z",
    ));

    let obs = agent_memory_stable_id(&["obs", "gone"]);
    graph.push(observation(
        &obs,
        "gone did the thing",
        "0.9",
        Some(&gone),
        Some(path),
        Some(span(10, 20)),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("g.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let entry = verdict_for(&v, &obs);
    assert_eq!(
        entry["verdict"], "unresolved",
        "code absent from the deleting HEAD commit must be unresolved"
    );
}

// ── Superseded and duplicate memory rows collapse to the current view ────────

#[test]
fn superseded_observation_is_excluded_from_current_view() {
    // The history-inclusive read can surface a superseded note alongside its
    // successor. Only the current (non-superseded) note should be classified.
    let path = "src/s.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let obs_old = agent_memory_stable_id(&["obs", "old"]);
    let obs_new = agent_memory_stable_id(&["obs", "new"]);
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v1",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v2",
            "commit_b",
            "2026-01-02T00:00:00Z",
        ),
        observation(
            &obs_old,
            "f returns body_v1 (old)",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        )
        .with_superseded_by(&obs_new),
        observation(
            &obs_new,
            "f returns body_v1 (current)",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    assert!(
        verdicts.iter().all(|e| e.observation_id != obs_old),
        "a superseded note must not be classified"
    );
    let current: Vec<_> = verdicts
        .iter()
        .filter(|e| e.observation_id == obs_new)
        .collect();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].verdict, FreshnessVerdict::Drifted);
}

#[test]
fn supersession_marker_in_a_later_row_is_honored() {
    // The same observation ID is emitted twice: first the original row (no
    // marker), then the updated row carrying `superseded_by`. Order must not
    // matter — the note is superseded and must not be classified.
    let path = "src/sm.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let obs_id = agent_memory_stable_id(&["obs", "marker"]);
    let successor = agent_memory_stable_id(&["obs", "successor"]);
    let original = observation(
        &obs_id,
        "f returns body_v1",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    );
    let superseded = observation(
        &obs_id,
        "f returns body_v1",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    )
    .with_superseded_by(&successor);
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v1",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v2",
            "commit_b",
            "2026-01-02T00:00:00Z",
        ),
        // Original row first, supersession marker second.
        original,
        superseded,
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    assert!(
        verdicts.iter().all(|e| e.observation_id != obs_id),
        "supersession must be honored even when the marker is on a later row"
    );
}

#[test]
fn duplicate_observation_rows_are_deduped() {
    // Two byte-identical physical rows of the same observation (a re-ingest) must
    // produce exactly one verdict, not two.
    let path = "src/dup.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let obs_id = agent_memory_stable_id(&["obs", "dup"]);
    let obs = observation(
        &obs_id,
        "f exists",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    );
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        obs.clone(),
        obs,
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let count = verdicts
        .iter()
        .filter(|e| e.observation_id == obs_id)
        .count();
    assert_eq!(count, 1, "re-ingested observation must be classified once");
}

// ── A backdated child commit is still the later code state ───────────────────

#[test]
fn backdated_child_commit_content_change_drifts() {
    // commit_b is a child of the anchor commit_a but carries an *earlier*
    // committer timestamp (rebase / clock skew). It is still the later code
    // state, so a content change there must register as `drifted`.
    let path = "src/bd.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    // Anchor version at commit_a, dated later than its own child.
    let anchor = symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "body_v1",
        "commit_a",
        "2026-02-01T00:00:00Z",
    );
    // Child of commit_a, backdated, with changed content.
    let mut child = symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "body_v2",
        "commit_b",
        "2026-01-01T00:00:00Z",
    );
    if let GraphRecord::Node {
        temporal: Some(t), ..
    } = &mut child
    {
        t.git_parent_commits = vec!["commit_a".to_owned()];
    }
    let obs = agent_memory_stable_id(&["obs", "bd"]);
    let records = vec![
        anchor,
        child,
        observation(
            &obs,
            "f returns body_v1",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs)
        .expect("verdict for obs");
    assert_eq!(entry.verdict, FreshnessVerdict::Drifted);
    assert!(matches!(
        entry.triggering_handle,
        Some(freshness::TriggeringHandle::ContentChange { .. })
    ));
}

// ── Module / Import body drift via content_signature (issue #206) ────────────

#[test]
fn module_body_change_is_detected_as_drift() {
    // A Rust `Module` whose inline body changed while its name/path stayed
    // fixed. The display summary is name-only, so before #206 both versions
    // hashed identically and freshness falsely reported `current`. The additive
    // `content_signature` now differs between the versions, so the body edit
    // must surface as a `drifted` ContentChange.
    let path = "src/m.rs";
    let module = stable_id(&["node", "module", "repo-a", path, "my_mod"]);
    let anchor = module_version(
        &module,
        path,
        "my_mod",
        span(1, 20),
        "blake3:module-body-v1",
        "commit_a",
        "2026-01-01T00:00:00Z",
    );
    let frontier = module_version(
        &module,
        path,
        "my_mod",
        span(1, 22),
        "blake3:module-body-v2",
        "commit_b",
        "2026-03-01T00:00:00Z",
    );
    let obs = agent_memory_stable_id(&["obs", "module_body"]);
    let records = vec![
        anchor,
        frontier,
        observation(
            &obs,
            "my_mod gates the parser behind cfg(test)",
            "0.9",
            Some(&module),
            Some(path),
            Some(span(1, 20)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs)
        .expect("verdict for obs");
    assert_eq!(entry.verdict, FreshnessVerdict::Drifted);
    assert!(matches!(
        entry.triggering_handle,
        Some(freshness::TriggeringHandle::ContentChange { .. })
    ));
}

#[test]
fn import_body_change_is_a_handle_identity_change_end_to_end() {
    // HONEST end-to-end proof (Codex finding, issue #206): an import body change
    // is NEVER reported as `drifted`/ContentChange, because the import's stable
    // ID encodes the whole trimmed `use ...;` declaration (`import_name`). When
    // the declaration changes (`use std::fmt::Debug;` -> `use std::fmt::Debug as
    // Dbg;`) the real extractor mints a DIFFERENT record ID, so the two versions
    // never share an ID for a content signature to be compared within. A
    // citation to the OLD import handle resolves `unresolved` (the handle is
    // absent from the frontier), NOT `drifted`. This is why imports carry no
    // `content_signature` — it could never fire.
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path();
    init_git_repo(repo);

    write_repo_file(
        repo,
        "src/lib.rs",
        "use std::fmt::Debug;\npub fn a() -> u32 { 1 }\n",
    );
    let first = commit_repo(repo, "initial import", "2026-01-01T00:00:00Z");

    // Change ONLY the import declaration (add an `as` alias). The old import
    // handle disappears and a new one is minted at the frontier.
    write_repo_file(
        repo,
        "src/lib.rs",
        "use std::fmt::Debug as Dbg;\npub fn a() -> u32 { 1 }\n",
    );
    let _second = commit_repo(repo, "alias the import", "2026-01-02T00:00:00Z");

    let graph = scan_repository_history(repo).expect("history scan should succeed");
    // The OLD declaration keyed the id on `std::fmt::Debug`; the NEW one on
    // `std::fmt::Debug as Dbg` — proving the body change moved the identity.
    let old_import_id = find_node_id(graph.records(), NodeKind::Import, "std::fmt::Debug")
        .expect("old import node present at the first commit");
    assert!(
        find_node_id(graph.records(), NodeKind::Import, "std::fmt::Debug as Dbg").is_some(),
        "the changed declaration must mint a distinct import handle"
    );

    let obs = agent_memory_stable_id(&["obs", "e2e_import_identity_change"]);
    let mut records = graph.into_records();
    records.push(observation(
        &obs,
        "imports std::fmt::Debug",
        "0.9",
        Some(&old_import_id),
        Some("src/lib.rs"),
        None,
        "OBSERVES",
        Some(&first),
        None,
    ));

    let verdicts = freshness::evidence_link_freshness(&records);
    let entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs)
        .expect("verdict for obs");
    assert_eq!(
        entry.verdict,
        FreshnessVerdict::Unresolved,
        "an import body change mints a new ID, so a citation to the old import \
         handle must resolve `unresolved`, never `drifted`"
    );
    // No tombstone is emitted for a renamed import; the handle is simply absent
    // from the frontier.
    assert!(matches!(
        entry.triggering_handle,
        Some(freshness::TriggeringHandle::HandleAbsent)
    ));
}

#[test]
fn unchanged_module_import_bodies_stay_current() {
    // Regression / no-false-positive guard: a `Module` whose body is UNCHANGED
    // between anchor and frontier (same `content_signature`) and an `Import`
    // whose UNCHANGED declaration keeps the same stable ID must both stay
    // `current`. Folding the module `content_signature` into the content hash
    // must not flip an unchanged body to `drifted`, and an unchanged import
    // (which carries no `content_signature`) must not drift either.
    let mpath = "src/mu.rs";
    let module = stable_id(&["node", "module", "repo-a", mpath, "stable_mod"]);
    let ipath = "src/iu.rs";
    let import = stable_id(&["node", "import", "repo-a", ipath, "HashMap"]);

    let obs_mod = agent_memory_stable_id(&["obs", "module_unchanged"]);
    let obs_import = agent_memory_stable_id(&["obs", "import_unchanged"]);
    let records = vec![
        module_version(
            &module,
            mpath,
            "stable_mod",
            span(1, 20),
            "blake3:same-module-body",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        module_version(
            &module,
            mpath,
            "stable_mod",
            span(1, 20),
            "blake3:same-module-body",
            "commit_b",
            "2026-03-01T00:00:00Z",
        ),
        import_version(
            &import,
            ipath,
            "HashMap",
            span(1, 1),
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        import_version(
            &import,
            ipath,
            "HashMap",
            span(1, 1),
            "commit_b",
            "2026-03-01T00:00:00Z",
        ),
        observation(
            &obs_mod,
            "stable_mod groups the adapters",
            "0.9",
            Some(&module),
            Some(mpath),
            Some(span(1, 20)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
        observation(
            &obs_import,
            "imports HashMap",
            "0.9",
            Some(&import),
            Some(ipath),
            Some(span(1, 1)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    for obs in [&obs_mod, &obs_import] {
        let entry = verdicts
            .iter()
            .find(|e| &e.observation_id == obs)
            .expect("verdict for obs");
        assert_eq!(entry.verdict, FreshnessVerdict::Current);
        assert!(entry.triggering_handle.is_none());
    }
}

#[test]
fn symbol_hashing_is_unchanged_by_content_signature() {
    // Symbols embed their normalized body in the summary and carry no
    // `content_signature`, so their content hash — and thus their freshness
    // verdicts — must be exactly what it was before #206: a body edit drifts, an
    // unchanged body stays current.
    let dpath = "src/sd.rs";
    let drifted_sym = stable_id(&["node", "symbol", "fn", "repo-a", dpath, "f", "0"]);
    let upath = "src/su.rs";
    let stable_sym = stable_id(&["node", "symbol", "fn", "repo-a", upath, "g", "0"]);

    let obs_drift = agent_memory_stable_id(&["obs", "sym_drift"]);
    let obs_stable = agent_memory_stable_id(&["obs", "sym_stable"]);
    let records = vec![
        symbol_version(
            &drifted_sym,
            dpath,
            "f",
            span(1, 5),
            "body_v1",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &drifted_sym,
            dpath,
            "f",
            span(1, 5),
            "body_v2",
            "commit_b",
            "2026-03-01T00:00:00Z",
        ),
        symbol_version(
            &stable_sym,
            upath,
            "g",
            span(1, 5),
            "body_same",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &stable_sym,
            upath,
            "g",
            span(1, 5),
            "body_same",
            "commit_b",
            "2026-03-01T00:00:00Z",
        ),
        observation(
            &obs_drift,
            "f returns body_v1",
            "0.9",
            Some(&drifted_sym),
            Some(dpath),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
        observation(
            &obs_stable,
            "g returns body_same",
            "0.9",
            Some(&stable_sym),
            Some(upath),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let drift_entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs_drift)
        .expect("verdict for drift obs");
    assert_eq!(drift_entry.verdict, FreshnessVerdict::Drifted);
    let stable_entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs_stable)
        .expect("verdict for stable obs");
    assert_eq!(stable_entry.verdict, FreshnessVerdict::Current);
}

// ── ingested_at anchors drift for legacy notes lacking observed_at ───────────

#[test]
fn ingested_at_anchors_drift_when_no_observed_at() {
    // A legacy/imported note carries only `ingested_at` — no valid_time, no
    // commit, and no observed_at. The recording-time fallback must still anchor
    // the comparison so later drift surfaces, instead of reporting `untemporal`.
    let path = "src/ia.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let obs = agent_memory_stable_id(&["obs", "ingested"]);
    let mut note = observation(
        &obs,
        "f returns body_v1",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        None, // no commit
        None, // no valid_time
    );
    // Strip observed_at; keep only ingested_at as the recording time.
    if let GraphRecord::Node {
        observed_at,
        ingested_at,
        ..
    } = &mut note
    {
        *observed_at = None;
        *ingested_at = Some("2026-02-01T00:00:00Z".to_owned());
    }
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v1",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v2",
            "commit_c",
            "2026-03-01T00:00:00Z",
        ),
        note,
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs)
        .expect("verdict for obs");
    assert_eq!(entry.verdict, FreshnessVerdict::Drifted);
    assert_eq!(
        entry.cited_handle.anchor_valid_time.as_deref(),
        Some("2026-02-01T00:00:00Z"),
        "ingested_at should anchor the comparison"
    );
}

// ── A tombstoned DRIFTS_PRIOR edge is not a trigger ──────────────────────────

#[test]
fn tombstoned_drift_prior_edge_is_not_a_trigger() {
    // Identical body across commits, so only a drift can flag it. The drift's
    // metadata prior is stale and its DRIFTS_PRIOR edge — the only recovery path —
    // is tombstoned. The retracted edge must not produce a `drifted` verdict.
    let path = "src/te.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let drift_id = semantic_stable_id(&["drift", "edge_retracted"]);
    let drift = SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "p".to_owned(),
            name: "m".to_owned(),
            version: "v".to_owned(),
            dim: 8,
            content_hash: "h".to_owned(),
        },
        target_record_id: sym.clone(),
        prior_record_id: "stale:nope".to_owned(),
        before_git_commit: "commit_a".to_owned(),
        after_git_commit: "commit_b".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.7,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    };
    let edge = GraphRecord::edge(
        EdgeLabel::DriftsPrior,
        drift_id.clone(),
        sym.clone(),
        None,
        "drifts prior".to_owned(),
    );
    let edge_id = edge.id().to_owned();
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "same",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "same",
            "commit_b",
            "2026-01-02T00:00:00Z",
        ),
        GraphRecord::node(
            drift_id,
            NodeKind::SemanticDrift,
            None,
            None,
            None,
            "Drift".to_owned(),
        )
        .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
        .with_semantic_drift(drift),
        edge,
        GraphRecord::Tombstone {
            id: stable_id(&["tombstone", &edge_id]),
            schema_version: aletheia_egregore::SCHEMA_VERSION,
            deleted_id: edge_id,
            summary: "drift-prior edge retracted".to_owned(),
            producer: None,
        },
        observation(
            &agent_memory_stable_id(&["obs", "te"]),
            "f at commit_a",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "te"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Current);
}

// ── Ambiguous multi-repo triple citation is left unresolved ──────────────────

#[test]
fn ambiguous_multi_repo_triple_is_unresolved() {
    // Two repositories share the same path + span. A triple-only citation carries
    // no repository identity, so it must not be resolved to an arbitrary repo —
    // the verdict is `unresolved`.
    let path = "src/x.rs";
    let sym_a = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let sym_b = stable_id(&["node", "symbol", "fn", "repo-b", path, "f", "0"]);
    let records = vec![
        symbol_version(
            &sym_a,
            path,
            "f",
            span(10, 20),
            "body_a",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &sym_b,
            path,
            "f",
            span(10, 20),
            "body_b",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        observation(
            &agent_memory_stable_id(&["obs", "amb"]),
            "the f at this span",
            "0.9",
            None, // triple-only, no record id
            Some(path),
            Some(span(10, 20)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "amb"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Unresolved);
}

// ── The evidence link's recorded span is preserved in the report ─────────────

#[test]
fn recorded_link_span_is_preserved() {
    // The link records a specific span; the live node carries a different one.
    // The report must surface the recorded citation span, not an arbitrary live
    // version's span, so auditors are pointed at the cited lines.
    let path = "src/rs.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let recorded = span(99, 120);
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        observation(
            &agent_memory_stable_id(&["obs", "rs"]),
            "f at the recorded span",
            "0.9",
            Some(&sym),
            Some(path),
            Some(recorded),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "rs"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(
        entry.cited_handle.span.map(|s| s.start_line),
        Some(99),
        "the recorded citation span must be preserved"
    );
}

// ── A restored handle (delete then re-ingest) is live, not unresolved ────────

#[test]
fn restored_handle_after_tombstone_is_live() {
    // Append-only order: original node, a tombstone, then a re-ingested node with
    // the same stable ID. The tombstone is superseded by the restore, so the
    // handle is live and a citation to it is `current`, not `unresolved`.
    let path = "src/restore.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "same",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        GraphRecord::Tombstone {
            id: stable_id(&["tombstone", &sym]),
            schema_version: aletheia_egregore::SCHEMA_VERSION,
            deleted_id: sym.clone(),
            summary: "deleted".to_owned(),
            producer: None,
        },
        // Restored after the delete.
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "same",
            "commit_b",
            "2026-01-02T00:00:00Z",
        ),
        observation(
            &agent_memory_stable_id(&["obs", "restore"]),
            "f exists",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "restore"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Current);
}

// ── A backdated grandchild commit is a later code state (full ancestry) ──────

#[test]
fn descendant_grandchild_content_change_drifts() {
    // Anchor at commit_a; commit_c is a grandchild (a→b→c) whose committer
    // timestamp is backdated before the anchor. Commit ancestry must still treat
    // it as later code, so its content change registers as `drifted`.
    let path = "src/anc.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let mk = |body: &str, commit: &str, vt: &str, parent: Option<&str>| {
        let mut v = symbol_version(&sym, path, "f", span(1, 5), body, commit, vt);
        if let GraphRecord::Node {
            temporal: Some(t), ..
        } = &mut v
        {
            t.git_parent_commits = parent.map(|p| vec![p.to_owned()]).unwrap_or_default();
        }
        v
    };
    let records = vec![
        mk("body_v1", "commit_a", "2026-02-01T00:00:00Z", None),
        // unchanged at b, child of a
        mk(
            "body_v1",
            "commit_b",
            "2026-02-02T00:00:00Z",
            Some("commit_a"),
        ),
        // changed at c, grandchild of a, backdated before the anchor
        mk(
            "body_v2",
            "commit_c",
            "2026-01-01T00:00:00Z",
            Some("commit_b"),
        ),
        observation(
            &agent_memory_stable_id(&["obs", "anc"]),
            "f returns body_v1",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "anc"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Drifted);
}

// ── Anchor selection orders mixed UTC offsets by parsed instant ──────────────

#[test]
fn mixed_offset_anchor_selection_uses_parsed_instants() {
    // Two versions precede the anchor instant (10:30Z): P at 09:00Z written as
    // +02:00 (so it sorts *after* Q as a raw string) and Q at 10:00Z. The correct
    // anchor is Q. A later version R matches Q's content, so the verdict is
    // `current`; string ordering would wrongly anchor on P and report `drifted`.
    let path = "src/tz.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "other",
            "commit_p",
            "2026-01-01T11:00:00+02:00", // == 09:00Z
        ),
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "anchor_body",
            "commit_q",
            "2026-01-01T10:00:00Z",
        ),
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "anchor_body", // same as Q ⇒ no drift from the correct anchor
            "commit_r",
            "2026-01-01T12:00:00Z",
        ),
        observation(
            &agent_memory_stable_id(&["obs", "tz"]),
            "f at 10:30Z",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            None,                         // no commit anchor
            Some("2026-01-01T10:30:00Z"), // valid-time anchor
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "tz"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Current);
}

// ── A SUPERSEDES edge excludes the replaced note (no superseded_by field) ────

#[test]
fn supersedes_edge_excludes_old_observation() {
    // The old note carries no `superseded_by` field; supersession is expressed
    // only via a SUPERSEDES edge (newer → older). The replaced note must not be
    // classified, even though its cited code drifted.
    let path = "src/sup.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let obs_old = agent_memory_stable_id(&["obs", "sup_old"]);
    let obs_new = agent_memory_stable_id(&["obs", "sup_new"]);
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v1",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v2",
            "commit_b",
            "2026-01-02T00:00:00Z",
        ),
        observation(
            &obs_old,
            "f returns body_v1 (old)",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
        // newer SUPERSEDES older (source supersedes target).
        GraphRecord::edge(
            EdgeLabel::Supersedes,
            obs_new,
            obs_old.clone(),
            None,
            "supersedes".to_owned(),
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    assert!(
        verdicts.iter().all(|e| e.observation_id != obs_old),
        "a note superseded via a SUPERSEDES edge must not be classified"
    );
}

/// Symbol version with explicit parent commits, for building commit DAGs.
fn symbol_version_p(
    sym_id: &str,
    path: &str,
    body: &str,
    commit: &str,
    parents: &[&str],
    valid_time: &str,
) -> GraphRecord {
    let mut v = symbol_version(sym_id, path, "f", span(1, 5), body, commit, valid_time);
    if let GraphRecord::Node {
        temporal: Some(t), ..
    } = &mut v
    {
        t.git_parent_commits = parents.iter().map(|p| (*p).to_owned()).collect();
    }
    v
}

// ── Drift beginning at a descendant commit is post-anchor (full ancestry) ─────

#[test]
fn descendant_drift_is_post_anchor_via_ancestry() {
    // Body identical across commits, so only a drift record can flag it. The drift
    // is recorded for B→C where B is a *descendant* of the anchor commit A, and is
    // backdated (after_valid_time not later than the anchor). Commit reachability,
    // not timestamps, must recognize it as post-anchor → `drifted`.
    let path = "src/da.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let t_anchor = "2026-02-01T00:00:00Z";
    let t_back = "2026-01-01T00:00:00Z"; // backdated descendants
    let records = vec![
        symbol_version_p(&sym, path, "same", "commit_a", &[], t_anchor),
        symbol_version_p(&sym, path, "same", "commit_b", &["commit_a"], t_back),
        symbol_version_p(&sym, path, "same", "commit_c", &["commit_b"], t_back),
        GraphRecord::node(
            semantic_stable_id(&["drift", "desc"]),
            NodeKind::SemanticDrift,
            None,
            None,
            None,
            "Drift".to_owned(),
        )
        .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
        .with_semantic_drift(SemanticDriftMetadata {
            embedding_model: EmbeddingModel {
                provider: "p".to_owned(),
                name: "m".to_owned(),
                version: "v".to_owned(),
                dim: 8,
                content_hash: "h".to_owned(),
            },
            target_record_id: sym.clone(),
            prior_record_id: sym.clone(),
            before_git_commit: "commit_b".to_owned(),
            after_git_commit: "commit_c".to_owned(),
            before_valid_time: t_back.to_owned(),
            after_valid_time: t_back.to_owned(),
            metric_kind: MetricKind::CosineDistance,
            score: 0.7,
            selection_threshold: 0.2,
            selection_basis: SelectionBasis::ThresholdOnly,
        }),
        observation(
            &agent_memory_stable_id(&["obs", "da"]),
            "f at commit_a",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "da"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Drifted);
    assert!(matches!(
        entry.triggering_handle,
        Some(freshness::TriggeringHandle::DriftRecord { .. })
    ));
}

// ── A side-branch version is not a later state of the anchored branch ─────────

#[test]
fn side_branch_version_does_not_falsely_drift() {
    // Commit A (anchored) and S are siblings — both children of P. S carries a
    // later timestamp and different content, but it is not a descendant of A, so a
    // note anchored at A must be `current`, not falsely `drifted`.
    let path = "src/sb.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let records = vec![
        symbol_version_p(
            &sym,
            path,
            "body_a",
            "commit_a",
            &["commit_p"],
            "2026-01-01T00:00:00Z",
        ),
        symbol_version_p(
            &sym,
            path,
            "body_s",
            "commit_s",
            &["commit_p"],
            "2026-02-01T00:00:00Z",
        ),
        observation(
            &agent_memory_stable_id(&["obs", "sb"]),
            "f on branch A",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "sb"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(
        entry.verdict,
        FreshnessVerdict::Current,
        "a sibling side-branch change is not drift of the anchored branch"
    );
}

// ── A superseded (renamed/replaced) code handle resolves unresolved ──────────

#[test]
fn superseded_code_handle_is_unresolved() {
    // A symbol was renamed/replaced: its old handle carries `superseded_by` and the
    // replacement is present. An observation citing the old handle must be
    // `unresolved` (the cited identity is no longer current), not `current`.
    let path = "src/sc.rs";
    let old_sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "old", "0"]);
    let new_sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "new", "0"]);
    let old_handle = symbol_version(
        &old_sym,
        path,
        "old",
        span(10, 20),
        "old_body",
        "commit_a",
        "2026-01-01T00:00:00Z",
    )
    .with_superseded_by(&new_sym);
    let records = vec![
        old_handle,
        symbol_version(
            &new_sym,
            path,
            "new",
            span(10, 20),
            "new_body",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        observation(
            &agent_memory_stable_id(&["obs", "sc"]),
            "old did the thing",
            "0.9",
            Some(&old_sym),
            Some(path),
            Some(span(10, 20)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "sc"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Unresolved);
}

// ── Node supersession is cleared by a later restored row (#285) ──────────────

#[test]
fn restored_row_clears_node_supersession() {
    // An older physical row carries `superseded_by`; a later row for the same ID
    // is restored without it. The current view exposes the restored note, so
    // freshness must classify it, not skip it forever.
    let path = "src/rr.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let obs_id = agent_memory_stable_id(&["obs", "rr"]);
    let superseded_row = observation(
        &obs_id,
        "f",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    )
    .with_superseded_by("some:replacement");
    let restored_row = observation(
        &obs_id,
        "f",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    );
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "b",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        superseded_row,
        restored_row,
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    assert!(
        verdicts.iter().any(|e| e.observation_id == obs_id),
        "a restored note must be classified, not skipped"
    );
}

// ── A retracted superseding note does not hide the old note (#304) ───────────

#[test]
fn tombstoned_superseding_note_does_not_hide_old() {
    // A newer note N supersedes O via a SUPERSEDES evidence link, but N is then
    // tombstoned (retracted). With the only supersession evidence gone, O is still
    // current and must be classified.
    let path = "src/ts.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let old = agent_memory_stable_id(&["obs", "old"]);
    let new = agent_memory_stable_id(&["obs", "new"]);
    let mut superseding = observation(
        &new,
        "f (new)",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    );
    if let GraphRecord::Node {
        evidence_links: Some(links),
        ..
    } = &mut superseding
    {
        links.push(EvidenceLink {
            target_record_id: Some(old.clone()),
            target_domain: "agent_memory".to_owned(),
            relation: "SUPERSEDES".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        });
    }
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "b",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        observation(
            &old,
            "f (old)",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
        superseding,
        GraphRecord::Tombstone {
            id: stable_id(&["tombstone", &new]),
            schema_version: aletheia_egregore::SCHEMA_VERSION,
            deleted_id: new.clone(),
            summary: "superseding note retracted".to_owned(),
            producer: None,
        },
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    assert!(
        verdicts.iter().any(|e| e.observation_id == old),
        "a retracted supersession must not hide the old note"
    );
}

// ── A spanned triple to a removed symbol is unresolved, not the file (#499) ──

#[test]
fn spanned_triple_does_not_fall_back_to_file() {
    let path = "src/sp.rs";
    let file = stable_id(&["node", "File", "repo-a", path]);
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "gone", "0"]);
    let records = vec![
        file_version(&file, path, "fb", "commit_a", "2026-01-01T00:00:00Z"),
        symbol_version(
            &sym,
            path,
            "gone",
            span(10, 20),
            "g",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        GraphRecord::Tombstone {
            id: stable_id(&["tombstone", &sym]),
            schema_version: aletheia_egregore::SCHEMA_VERSION,
            deleted_id: sym.clone(),
            summary: "gone removed".to_owned(),
            producer: None,
        },
        observation(
            &agent_memory_stable_id(&["obs", "sp"]),
            "the symbol",
            "0.9",
            None,
            Some(path),
            Some(span(10, 20)),
            "OBSERVES",
            None,
            None,
        ),
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "sp"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Unresolved);
}

// ── A drift that lands on a sibling branch is not drift of the anchor (#955) ──

#[test]
fn drift_landing_on_sibling_branch_is_not_post_anchor() {
    let path = "src/dl.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let records = vec![
        symbol_version_p(
            &sym,
            path,
            "same",
            "commit_a",
            &["commit_p"],
            "2026-01-01T00:00:00Z",
        ),
        symbol_version_p(
            &sym,
            path,
            "same",
            "commit_s",
            &["commit_p"],
            "2026-02-01T00:00:00Z",
        ),
        GraphRecord::node(
            semantic_stable_id(&["drift", "sib"]),
            NodeKind::SemanticDrift,
            None,
            None,
            None,
            "Drift".to_owned(),
        )
        .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
        .with_semantic_drift(SemanticDriftMetadata {
            embedding_model: EmbeddingModel {
                provider: "p".to_owned(),
                name: "m".to_owned(),
                version: "v".to_owned(),
                dim: 8,
                content_hash: "h".to_owned(),
            },
            target_record_id: sym.clone(),
            prior_record_id: sym.clone(),
            before_git_commit: "commit_a".to_owned(),
            after_git_commit: "commit_s".to_owned(),
            before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
            after_valid_time: "2026-02-01T00:00:00Z".to_owned(),
            metric_kind: MetricKind::CosineDistance,
            score: 0.7,
            selection_threshold: 0.2,
            selection_basis: SelectionBasis::ThresholdOnly,
        }),
        observation(
            &agent_memory_stable_id(&["obs", "dl"]),
            "f on A",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "dl"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(
        entry.verdict,
        FreshnessVerdict::Current,
        "a drift onto a sibling branch is not a later state of the anchored branch"
    );
}

// ── Triple identity resolves at target_git_commit, anchor stays as_of (#772) ──

#[test]
fn triple_identity_uses_target_git_commit() {
    // OLD occupied (path, span) at c1 and was removed; NEW reused the same
    // (path, span) at c2. The link records target_git_commit=c1 (identity) and
    // as_of_commit=c2 (freshness anchor). Identity must bind to OLD at c1 →
    // unresolved, not silently re-point at NEW.
    let path = "src/ti.rs";
    let old = stable_id(&["node", "symbol", "fn", "repo-a", path, "old", "0"]);
    let new = stable_id(&["node", "symbol", "fn", "repo-a", path, "new", "0"]);
    let mut obs = observation(
        &agent_memory_stable_id(&["obs", "ti"]),
        "the symbol",
        "0.9",
        None,
        Some(path),
        Some(span(10, 20)),
        "OBSERVES",
        Some("commit_c2"),
        None,
    );
    if let GraphRecord::Node {
        evidence_links: Some(links),
        ..
    } = &mut obs
    {
        links[0].target_git_commit = Some("commit_c1".to_owned());
    }
    let records = vec![
        symbol_version(
            &old,
            path,
            "old",
            span(10, 20),
            "ob",
            "commit_c1",
            "2026-01-01T00:00:00Z",
        ),
        GraphRecord::Tombstone {
            id: stable_id(&["tombstone", &old]),
            schema_version: aletheia_egregore::SCHEMA_VERSION,
            deleted_id: old.clone(),
            summary: "old removed".to_owned(),
            producer: None,
        },
        symbol_version(
            &new,
            path,
            "new",
            span(10, 20),
            "nb",
            "commit_c2",
            "2026-01-02T00:00:00Z",
        ),
        obs,
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs_id = agent_memory_stable_id(&["obs", "ti"]);
    let entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs_id)
        .unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Unresolved);
}

// ── Unanchored triples resolve against frontier spans only (#468) ─────────────

#[test]
fn unanchored_triple_matches_frontier_span_only() {
    // The symbol is still live but its span moved (S1 at c1 → S2 at the tip c2). A
    // note recorded with the old span S1 and no anchor commit must be `unresolved`
    // against the frontier, not resolved through the historical version.
    let path = "src/uf.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let mut v1 = symbol_version(
        &sym,
        path,
        "f",
        span(10, 20),
        "b",
        "commit_a",
        "2026-01-01T00:00:00Z",
    );
    let mut v2 = symbol_version(
        &sym,
        path,
        "f",
        span(30, 40),
        "b",
        "commit_b",
        "2026-01-02T00:00:00Z",
    );
    if let GraphRecord::Node {
        temporal: Some(t), ..
    } = &mut v2
    {
        t.git_parent_commits = vec!["commit_a".to_owned()];
    }
    // (v1 keeps empty parents; commit_a is the root.)
    let _ = &mut v1;
    let records = vec![
        v1,
        v2,
        observation(
            &agent_memory_stable_id(&["obs", "uf"]),
            "old span",
            "0.9",
            None,
            Some(path),
            Some(span(10, 20)),
            "OBSERVES",
            None,
            None,
        ),
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "uf"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(
        entry.verdict,
        FreshnessVerdict::Unresolved,
        "old span must not resolve through a historical version"
    );
}

// ── A SUPERSEDES edge from a tombstoned source does not hide the old (#319) ───

#[test]
fn tombstoned_supersedes_edge_source_does_not_hide_old() {
    let path = "src/se.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let old = agent_memory_stable_id(&["obs", "old"]);
    let new = agent_memory_stable_id(&["obs", "new"]);
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "b",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        observation(
            &old,
            "old",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
        observation(
            &new,
            "new",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
        GraphRecord::edge(
            EdgeLabel::Supersedes,
            new.clone(),
            old.clone(),
            None,
            "supersedes".to_owned(),
        ),
        GraphRecord::Tombstone {
            id: stable_id(&["tombstone", &new]),
            schema_version: aletheia_egregore::SCHEMA_VERSION,
            deleted_id: new.clone(),
            summary: "superseding note retracted".to_owned(),
            producer: None,
        },
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    assert!(
        verdicts.iter().any(|e| e.observation_id == old),
        "a SUPERSEDES edge from a retracted source must not hide the old note"
    );
}

// ── Ancestry is scoped to the cited anchor, not the whole store (#444) ───────

#[test]
fn ancestry_is_scoped_to_the_cited_anchor() {
    // The cited handle's commits carry no parent metadata, but an UNRELATED history
    // in the same store does. Freshness must fall back to timestamps for the cited
    // anchor (drifted), not apply descendant-only logic and report `current`.
    let path = "src/sa.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let records = vec![
        // Cited handle: no parent edges between its versions.
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v1",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v2",
            "commit_b",
            "2026-01-02T00:00:00Z",
        ),
        // Unrelated history that DOES carry ancestry edges.
        commit_node("commit_x", &[], "2026-01-01T00:00:00Z"),
        commit_node("commit_y", &["commit_x"], "2026-01-02T00:00:00Z"),
        observation(
            &agent_memory_stable_id(&["obs", "sa"]),
            "f returns body_v1",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "sa"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(
        entry.verdict,
        FreshnessVerdict::Drifted,
        "an anchor with no ancestry of its own must use the timestamp fallback"
    );
}

// ── EXPLAINS_CHANGE to an absent Commit is skipped by relation (#729) ────────

#[test]
fn explains_change_to_absent_target_is_skipped() {
    // The cited Commit/Change target is not in the slice; the link must be skipped
    // by relation, not reported as a false `unresolved` handle.
    let records = vec![observation(
        &agent_memory_stable_id(&["obs", "ec"]),
        "this commit introduced the bug",
        "0.9",
        Some("codegraph:v1:commit:absent"),
        None,
        None,
        "EXPLAINS_CHANGE",
        Some("commit_a"),
        None,
    )];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "ec"]);
    assert!(
        verdicts.iter().all(|e| e.observation_id != obs),
        "a non-handle relation must be skipped even when the target is absent"
    );
}

// ── Ambiguous commit-anchored triple does not fall back to the live one (#828) ─

#[test]
fn ambiguous_anchored_triple_does_not_fall_back_to_live() {
    // Two repos occupied the same (path, span) at the target commit; only repo-a
    // remains live. The ambiguous anchored lookup must not silently bind to repo-a.
    let path = "src/at.rs";
    let sym_a = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let sym_b = stable_id(&["node", "symbol", "fn", "repo-b", path, "f", "0"]);
    let mut obs = observation(
        &agent_memory_stable_id(&["obs", "at"]),
        "the symbol",
        "0.9",
        None,
        Some(path),
        Some(span(10, 20)),
        "OBSERVES",
        Some("commit_c"),
        None,
    );
    if let GraphRecord::Node {
        evidence_links: Some(links),
        ..
    } = &mut obs
    {
        links[0].target_git_commit = Some("commit_c".to_owned());
    }
    let records = vec![
        symbol_version(
            &sym_a,
            path,
            "f",
            span(10, 20),
            "a",
            "commit_c",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &sym_b,
            path,
            "f",
            span(10, 20),
            "b",
            "commit_c",
            "2026-01-01T00:00:00Z",
        ),
        // repo-b's symbol is removed (so a live fallback would bind only to repo-a).
        GraphRecord::Tombstone {
            id: stable_id(&["tombstone", &sym_b]),
            schema_version: aletheia_egregore::SCHEMA_VERSION,
            deleted_id: sym_b.clone(),
            summary: "repo-b symbol removed".to_owned(),
            producer: None,
        },
        obs,
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs_id = agent_memory_stable_id(&["obs", "at"]);
    let entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs_id)
        .unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Unresolved);
}

// ── A sibling→merge drift is not drift of the anchored lineage (#1067) ───────

#[test]
fn sibling_to_merge_drift_is_not_post_anchor() {
    // Merge history: M merges anchor branch A and sibling branch B. A semantic
    // drift compares B → M. M is a descendant of A, but B is not, so the B→M
    // change is the sibling branch being merged in, not drift of A's lineage. A
    // note anchored at A whose content equals M must be `current`.
    let path = "src/mg.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let records = vec![
        symbol_version_p(&sym, path, "same", "commit_a", &[], "2026-01-01T00:00:00Z"),
        // Merge commit M has both A and B as parents.
        symbol_version_p(
            &sym,
            path,
            "same",
            "commit_m",
            &["commit_a", "commit_b"],
            "2026-01-03T00:00:00Z",
        ),
        GraphRecord::node(
            semantic_stable_id(&["drift", "merge"]),
            NodeKind::SemanticDrift,
            None,
            None,
            None,
            "Drift".to_owned(),
        )
        .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
        .with_semantic_drift(SemanticDriftMetadata {
            embedding_model: EmbeddingModel {
                provider: "p".to_owned(),
                name: "m".to_owned(),
                version: "v".to_owned(),
                dim: 8,
                content_hash: "h".to_owned(),
            },
            target_record_id: sym.clone(),
            prior_record_id: sym.clone(),
            before_git_commit: "commit_b".to_owned(),
            after_git_commit: "commit_m".to_owned(),
            before_valid_time: "2026-01-02T00:00:00Z".to_owned(),
            after_valid_time: "2026-01-03T00:00:00Z".to_owned(),
            metric_kind: MetricKind::CosineDistance,
            score: 0.7,
            selection_threshold: 0.2,
            selection_basis: SelectionBasis::ThresholdOnly,
        }),
        observation(
            &agent_memory_stable_id(&["obs", "mg"]),
            "f on A",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "mg"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(
        entry.verdict,
        FreshnessVerdict::Current,
        "a sibling-branch change merged in is not drift of the anchored lineage"
    );
}

// ── Liveness is scoped to the anchored lineage, not sibling tips (#414) ──────

#[test]
fn anchored_liveness_ignores_sibling_branch_tips() {
    // The cited symbol exists at the anchor A and on a sibling branch tip S, but
    // the anchored lineage's own frontier D (a descendant of A) deleted it. A note
    // anchored at A must be `unresolved` — the sibling tip does not keep it current.
    let path = "src/al.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let records = vec![
        // Symbol at A (anchor, child of P) and S (sibling, child of P); absent at D.
        symbol_version_p(
            &sym,
            path,
            "b",
            "commit_a",
            &["commit_p"],
            "2026-01-02T00:00:00Z",
        ),
        symbol_version_p(
            &sym,
            path,
            "b",
            "commit_s",
            &["commit_p"],
            "2026-01-03T00:00:00Z",
        ),
        // D is a descendant of A (the anchored lineage frontier) where the symbol
        // is gone; represented by a Commit node so D is in the DAG as a tip.
        commit_node("commit_d", &["commit_a"], "2026-01-04T00:00:00Z"),
        observation(
            &agent_memory_stable_id(&["obs", "al"]),
            "f on A",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "al"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(
        entry.verdict,
        FreshnessVerdict::Unresolved,
        "a symbol deleted on the anchored lineage is unresolved despite a sibling tip"
    );
}

// ── A file-level triple citing the file's own span resolves (#605) ───────────

#[test]
fn file_triple_with_recorded_span_resolves() {
    // A file-level citation stored as a triple carries `target_span` (the evidence
    // triple resolver requires one). When that span equals the File node's own
    // span, it must resolve to the file (→ drifted on body change), not be reported
    // `unresolved` for lack of a symbol match.
    let path = "src/fs.rs";
    let file = stable_id(&["node", "File", "repo-a", path]);
    let records = vec![
        file_version(&file, path, "v1", "commit_a", "2026-01-01T00:00:00Z"),
        file_version(&file, path, "v2", "commit_b", "2026-01-02T00:00:00Z"),
        observation(
            &agent_memory_stable_id(&["obs", "fs"]),
            "the file does X",
            "0.9",
            None, // triple, no record id
            Some(path),
            Some(span(1, 100)), // exactly the File node's span
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs = agent_memory_stable_id(&["obs", "fs"]);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(
        entry.verdict,
        FreshnessVerdict::Drifted,
        "a file-level triple matching the file's own span must resolve to the file"
    );
}

// ── Current-tree content change keeps its valid_time in the trigger (#1165) ──

#[test]
fn current_tree_content_change_preserves_valid_time() {
    // Two repeated current-tree (non-temporal) versions of one symbol carry only a
    // node-level `valid_time`; the later one changes content. The trigger must
    // report that `valid_time` (the commit stays empty), not an empty string.
    let path = "src/ct.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let mk = |body: &str, vt: &str| {
        let mut n = GraphRecord::node(
            sym.clone(),
            NodeKind::Symbol,
            Some(path.to_owned()),
            Some(span(1, 5)),
            Some("f".to_owned()),
            format!("Rust fn f\nSource:\n{body}"),
        );
        if let GraphRecord::Node { valid_time, .. } = &mut n {
            *valid_time = Some(vt.to_owned());
        }
        n
    };
    let mut obs = observation(
        &agent_memory_stable_id(&["obs", "ct"]),
        "f returns v1",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        None,
        Some("2026-01-01T00:00:00Z"), // valid_time anchor, no commit
    );
    // Clear observed_at so the anchor is the supplied valid_time.
    if let GraphRecord::Node { observed_at, .. } = &mut obs {
        *observed_at = None;
    }
    let records = vec![
        mk("body_v1", "2026-01-01T00:00:00Z"),
        mk("body_v2", "2026-02-01T00:00:00Z"),
        obs,
    ];
    let verdicts = freshness::evidence_link_freshness(&records);
    let obs_id = agent_memory_stable_id(&["obs", "ct"]);
    let entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs_id)
        .unwrap();
    assert_eq!(entry.verdict, FreshnessVerdict::Drifted);
    match &entry.triggering_handle {
        Some(freshness::TriggeringHandle::ContentChange {
            after_git_commit,
            after_valid_time,
            ..
        }) => {
            assert_eq!(after_valid_time, "2026-02-01T00:00:00Z");
            assert!(
                after_git_commit.is_empty(),
                "non-temporal version has no commit"
            );
        }
        other => panic!("expected content_change trigger, got {other:?}"),
    }
}

// ── `--data-dir` evidence-freshness is strictly read-only (#4030) ────────────

#[cfg(feature = "embedded-aletheiadb")]
fn dir_fingerprint(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
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

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_evidence_freshness_is_read_only() {
    // Ingest a minimal graph into an embedded store, then run evidence-freshness
    // against `--data-dir`. The store must be byte-for-byte unchanged afterwards:
    // the command reads a throwaway copy, never the live engine.
    let mut graph = Graph::new();
    let path = "src/ro.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    graph.push(symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "v1",
        "commit_a",
        "2026-01-01T00:00:00Z",
    ));
    graph.push(symbol_version(
        &sym,
        path,
        "f",
        span(1, 5),
        "v2",
        "commit_b",
        "2026-01-02T00:00:00Z",
    ));
    graph.push(observation(
        &agent_memory_stable_id(&["obs", "ro"]),
        "f returns v1",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));

    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("ro.jsonl");
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();
    let data_dir = temp.path().join("store");

    egregore()
        .args(["ingest"])
        .arg(&graph_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let before = dir_fingerprint(&data_dir);
    let assert = egregore()
        .args(["query", "evidence-freshness", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    // Sanity: it produced a verdict report.
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["ok"], true);

    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "evidence-freshness must not modify any store file when reading --data-dir"
    );
}

// ── Issue #205: `--data-dir` honors tombstone write order like `--graph` ─────

/// A current-tree (non-temporal) symbol, the shape the incremental cache and
/// `scan` produce; issue #205 is specific to non-temporal stable IDs.
#[cfg(feature = "embedded-aletheiadb")]
fn current_tree_symbol(sym_id: &str, path: &str, name: &str, body: &str) -> GraphRecord {
    GraphRecord::node(
        sym_id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(span(1, 5)),
        Some(name.to_owned()),
        format!("Rust fn {name}\nSource:\n{body}"),
    )
}

#[cfg(feature = "embedded-aletheiadb")]
fn tombstone_of(deleted_id: &str, marker: &str) -> GraphRecord {
    GraphRecord::Tombstone {
        id: stable_id(&["tombstone", deleted_id, marker]),
        schema_version: aletheia_egregore::SCHEMA_VERSION,
        deleted_id: deleted_id.to_owned(),
        summary: "deleted".to_owned(),
        producer: None,
    }
}

#[cfg(feature = "embedded-aletheiadb")]
fn ingest_into(
    data_dir: &std::path::Path,
    scratch: &std::path::Path,
    name: &str,
    records: &[GraphRecord],
) {
    let mut graph = Graph::new();
    for record in records {
        graph.push(record.clone());
    }
    let path = scratch.join(name);
    fs::write(&path, graph.to_jsonl().unwrap()).unwrap();
    egregore()
        .args(["ingest"])
        .arg(&path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
fn freshness_from_data_dir(data_dir: &std::path::Path) -> serde_json::Value {
    let assert = egregore()
        .args(["query", "evidence-freshness", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    serde_json::from_str(stdout.trim()).expect("valid JSON")
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_tombstoned_then_not_restored_records_match_graph_semantics() {
    // Issue #205: the embedded history read used to emit active tombstones in
    // its current-state prefix while the deleted non-temporal node versions
    // were re-emitted in a write-order suffix *after* them. Freshness infers
    // restoration from slice order, so a genuine deletion looked restored: a
    // retracted observation was still classified and a deleted current-tree
    // handle reported live. The `--graph` append-only path is the contract.
    let path = "src/t205.rs";
    let kept_sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "kept", "0"]);
    let gone_sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "gone", "0"]);
    let obs_retracted = agent_memory_stable_id(&["obs", "retracted-205"]);
    let obs_deleted_handle = agent_memory_stable_id(&["obs", "deleted-handle-205"]);

    // First write batch: two current-tree symbols, one observation citing each.
    let first = vec![
        current_tree_symbol(&kept_sym, path, "kept", "kept_body"),
        current_tree_symbol(&gone_sym, path, "gone", "gone_body"),
        observation(
            &obs_retracted,
            RAW_OBS_TEXT_SENTINEL,
            "0.9",
            Some(&kept_sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            None,
            None,
        ),
        observation(
            &obs_deleted_handle,
            "gone does Y",
            "0.9",
            Some(&gone_sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            None,
            None,
        ),
    ];
    // Second write batch: retract the first observation, delete the second's
    // cited handle. Neither stable ID is ever re-emitted afterwards.
    let second = vec![
        tombstone_of(&obs_retracted, "retract"),
        tombstone_of(&gone_sym, "delete"),
    ];

    let assert_tombstone_semantics = |v: &serde_json::Value, source: &str| {
        let verdicts = v["verdicts"].as_array().unwrap();
        assert!(
            !verdicts
                .iter()
                .any(|e| e["observation_id"] == obs_retracted.as_str()),
            "{source}: a retracted (tombstoned, never restored) observation must be \
             omitted, got {verdicts:?}"
        );
        let entry = verdict_for(v, &obs_deleted_handle);
        assert_eq!(
            entry["verdict"], "unresolved",
            "{source}: a citation to a deleted (tombstoned, never restored) handle \
             must be unresolved"
        );
        assert_eq!(entry["triggering_handle"]["kind"], "handle_removed");
    };

    // Contract: the append-only `--graph` path over the same write order.
    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("combined.jsonl");
    let mut combined = Graph::new();
    for record in first.iter().chain(second.iter()) {
        combined.push(record.clone());
    }
    fs::write(&graph_path, combined.to_jsonl().unwrap()).unwrap();
    let (code, stdout, stderr) = run(&graph_path, &[]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_tombstone_semantics(&v, "--graph");

    // Same write order through the embedded store must behave identically.
    let data_dir = temp.path().join("store");
    ingest_into(&data_dir, temp.path(), "first.jsonl", &first);
    ingest_into(&data_dir, temp.path(), "second.jsonl", &second);
    let v = freshness_from_data_dir(&data_dir);
    assert_tombstone_semantics(&v, "--data-dir");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_tombstoned_then_restored_observation_stays_live() {
    // Issue #205 acceptance: fixing the deletion case must not break
    // restoration — a stable ID re-ingested AFTER its tombstone is live again
    // and its observation is classified.
    let path = "src/t205r.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let obs = agent_memory_stable_id(&["obs", "restored-205"]);

    let obs_record = |text: &str| {
        observation(
            &obs,
            text,
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            None,
            None,
        )
    };

    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("store");
    ingest_into(
        &data_dir,
        temp.path(),
        "first.jsonl",
        &[
            current_tree_symbol(&sym, path, "f", "body"),
            obs_record("f does X"),
        ],
    );
    ingest_into(
        &data_dir,
        temp.path(),
        "second.jsonl",
        &[tombstone_of(&obs, "retract")],
    );
    // Restoration: the same stable observation ID is re-ingested after the
    // tombstone (revised text so the write is not an idempotent no-op).
    ingest_into(
        &data_dir,
        temp.path(),
        "third.jsonl",
        &[obs_record("f does X (re-verified)")],
    );

    let v = freshness_from_data_dir(&data_dir);
    let entry = verdict_for(&v, &obs);
    assert!(
        entry["verdict"].is_string(),
        "a tombstoned-then-restored observation must be classified again"
    );
}

// ── A superseded (not tombstoned) drift record is not a trigger ──────────────

#[test]
fn superseded_drift_record_is_not_a_trigger() {
    // The cited symbol body is identical across commits, so only a drift record
    // could flag it. That drift record was superseded (replaced/recomputed) rather
    // than tombstoned, via a SUPERSEDES edge. The current-state view excludes it,
    // so the verdict must be `current`, not `drifted`.
    let path = "src/sd.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let mut records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "same_body",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "same_body",
            "commit_b",
            "2026-01-02T00:00:00Z",
        ),
    ];
    let drift_id = semantic_stable_id(&["drift", "superseded"]);
    records.extend(drift_record(
        &drift_id,
        &sym,
        &sym,
        "commit_a",
        "commit_b",
        "2026-01-01T00:00:00Z",
        "2026-01-02T00:00:00Z",
    ));
    // A newer (recomputed) drift record SUPERSEDES the old one.
    let new_drift_id = semantic_stable_id(&["drift", "superseded_replacement"]);
    records.push(GraphRecord::edge(
        EdgeLabel::Supersedes,
        new_drift_id,
        drift_id,
        None,
        "supersedes".to_owned(),
    ));
    let obs = agent_memory_stable_id(&["obs", "sd"]);
    records.push(observation(
        &obs,
        "f at commit_a",
        "0.9",
        Some(&sym),
        Some(path),
        Some(span(1, 5)),
        "OBSERVES",
        Some("commit_a"),
        None,
    ));

    let verdicts = freshness::evidence_link_freshness(&records);
    let entry = verdicts.iter().find(|e| e.observation_id == obs).unwrap();
    assert_eq!(
        entry.verdict,
        FreshnessVerdict::Current,
        "a superseded drift record must not trigger a drifted verdict"
    );
}

// ── Edge-backed citations (graph-edge form, no inline evidence link) ──────────

/// Builds an agent `Observation` with no inline evidence links — it cites code
/// only through graph edges (`OBSERVES`, `MENTIONS_SYMBOL`, `TOUCHED_FILE`, …).
fn bare_observation(obs_id: &str, text: &str) -> GraphRecord {
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
        agent_id,
        session_id,
        observed_at,
        confidence,
        text: txt,
        domain,
        ..
    } = &mut node
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent_1".to_owned());
        *session_id = Some("sess_1".to_owned());
        *observed_at = Some("2026-02-01T00:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *txt = Some(text.to_owned());
        *domain = Some("agent_memory".to_owned());
    }
    node
}

/// A code-citation edge (observation → code handle) anchored at `anchor_commit`.
fn citation_edge(
    obs_id: &str,
    target_id: &str,
    label: EdgeLabel,
    anchor_commit: &str,
) -> GraphRecord {
    GraphRecord::edge(
        label,
        obs_id.to_owned(),
        target_id.to_owned(),
        Some("0.9".to_owned()),
        "edge citation".to_owned(),
    )
    .with_temporal(temporal(anchor_commit, "2026-01-01T00:00:00Z"))
}

#[test]
fn edge_backed_citation_without_inline_link_is_classified() {
    // The note cites the symbol only through an OBSERVES edge — it has no inline
    // evidence_links. The cited symbol's content changed after the anchor commit,
    // so the edge-only note must still be reported `drifted`, not silently dropped.
    let path = "src/edge.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let obs = agent_memory_stable_id(&["obs", "edge_only"]);
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v1",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v2",
            "commit_b",
            "2026-01-02T00:00:00Z",
        ),
        bare_observation(&obs, "f returns body_v1"),
        citation_edge(&obs, &sym, EdgeLabel::Observes, "commit_a"),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs)
        .expect("an edge-only code citation must be classified, not dropped");
    assert_eq!(entry.verdict, FreshnessVerdict::Drifted);
    assert_eq!(
        entry.cited_handle.target_record_id.as_deref(),
        Some(sym.as_str())
    );
    assert_eq!(entry.cited_handle.relation, "OBSERVES");
}

#[test]
fn inline_and_duplicate_edge_citation_classified_once() {
    // The note carries an inline OBSERVES link AND a redundant OBSERVES edge to the
    // same symbol at the same anchor. The citation must be classified exactly once.
    let path = "src/dup.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let obs = agent_memory_stable_id(&["obs", "dup_edge"]);
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body_v1",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        observation(
            &obs,
            "f",
            "0.9",
            Some(&sym),
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
        citation_edge(&obs, &sym, EdgeLabel::Observes, "commit_a"),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let count = verdicts.iter().filter(|e| e.observation_id == obs).count();
    assert_eq!(
        count, 1,
        "an inline link and a duplicate edge citing one handle must classify once"
    );
}

#[test]
fn edge_to_non_handle_target_is_not_synthesized() {
    // An EXPLAINS_CHANGE edge from a note to a Commit record is a valid citation,
    // not a code handle. It must not be synthesized into a freshness input (which
    // would surface as a false `unresolved`), even with no inline links.
    let commit_id = stable_id(&["node", "commit", "repo-a", "commit_a"]);
    let obs = agent_memory_stable_id(&["obs", "explains"]);
    let records = vec![
        commit_node("commit_a", &[], "2026-01-01T00:00:00Z"),
        bare_observation(&obs, "this change explains the refactor"),
        citation_edge(&obs, &commit_id, EdgeLabel::ExplainsChange, "commit_a"),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    assert!(
        verdicts.iter().all(|e| e.observation_id != obs),
        "an EXPLAINS_CHANGE edge to a Commit is not a code handle and must not be classified"
    );
}

// ── Distinct inline spans to one handle each get a verdict (no over-dedupe) ───

#[test]
fn distinct_inline_spans_to_same_handle_each_classified() {
    // One observation cites the same symbol via two inline links with the same
    // record id/relation/anchor but DIFFERENT recorded spans (two snippets). Each
    // is a distinct citation and must receive its own verdict — they must not be
    // collapsed by the edge-dedupe path.
    let path = "src/two.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let mk_link = |start, end| EvidenceLink {
        target_record_id: Some(sym.clone()),
        target_domain: "codegraph".to_owned(),
        relation: "OBSERVES".to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: Some("commit_a".to_owned()),
        target_repo_relative_path: Some(path.to_owned()),
        target_span: Some(span(start, end)),
        target_git_commit: None,
    };
    let obs = agent_memory_stable_id(&["obs", "two_spans"]);
    let mut note = GraphRecord::node(
        obs.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "two snippets in f".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        evidence_links,
        agent_id,
        domain,
        ..
    } = &mut note
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![mk_link(1, 5), mk_link(10, 15)]);
        *agent_id = Some("agent_1".to_owned());
        *domain = Some("agent_memory".to_owned());
    }
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        note,
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let count = verdicts.iter().filter(|e| e.observation_id == obs).count();
    assert_eq!(
        count, 2,
        "two inline links with distinct spans are distinct citations"
    );
}

#[test]
fn edge_only_citation_to_absent_handle_is_unresolved() {
    // The note cites a symbol only through a MENTIONS_SYMBOL edge, and the cited
    // handle is absent from the slice (deleted/pruned, no node, no kind). The
    // citation must still be classified `unresolved` — the same as an inline link
    // to an absent handle — not silently dropped.
    let absent_sym = stable_id(&["node", "symbol", "fn", "repo-a", "src/gone.rs", "g", "0"]);
    let obs = agent_memory_stable_id(&["obs", "edge_absent"]);
    let records = vec![
        bare_observation(&obs, "g did something"),
        citation_edge(&obs, &absent_sym, EdgeLabel::MentionsSymbol, "commit_a"),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs)
        .expect("an edge-only citation to an absent handle must be classified");
    assert_eq!(entry.verdict, FreshnessVerdict::Unresolved);
}

#[test]
fn triple_inline_and_materialized_edge_classified_once() {
    // A triple-only inline link (path/span, no record id) and the daemon-
    // materialized standalone edge to the resolved record id are the SAME citation.
    // The triple must resolve to the record id for the covered key so the edge is
    // recognized as a duplicate and the citation is classified exactly once.
    let path = "src/tri.rs";
    let sym = stable_id(&["node", "symbol", "fn", "repo-a", path, "f", "0"]);
    let obs = agent_memory_stable_id(&["obs", "tri_dup"]);
    let records = vec![
        symbol_version(
            &sym,
            path,
            "f",
            span(1, 5),
            "body",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        observation(
            &obs,
            "f does x",
            "0.9",
            None, // triple-only inline link
            Some(path),
            Some(span(1, 5)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
        citation_edge(&obs, &sym, EdgeLabel::Observes, "commit_a"),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    let count = verdicts.iter().filter(|e| e.observation_id == obs).count();
    assert_eq!(
        count, 1,
        "a triple inline link and its materialized edge are one citation"
    );
}

// ── End-to-end scan-history Module/Import drift (issue #206 / Codex finding A) ─

/// Initializes a deterministic, isolated Git repo (no remote, GPG signing off,
/// autocrlf off) so history replay yields byte-stable IDs.
fn init_git_repo(repo: &std::path::Path) {
    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.email", "codegraph@example.invalid"]);
    run_git(repo, &["config", "user.name", "Codegraph Test"]);
    run_git(repo, &["config", "core.autocrlf", "false"]);
    run_git(repo, &["config", "commit.gpgsign", "false"]);
}

fn write_repo_file(repo: &std::path::Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

/// Commits everything staged at a pinned author/committer date and returns the
/// new HEAD SHA.
fn commit_repo(repo: &std::path::Path, message: &str, date: &str) -> String {
    run_git(repo, &["add", "."]);
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        status.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    git_stdout(repo, &["rev-parse", "HEAD"])
}

fn run_git(repo: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(repo: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("git should execute");
    assert!(output.status.success(), "git command failed");
    String::from_utf8(output.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}

/// Finds the stable record ID of the first node of `kind` whose recorded name is
/// `name`, from a scanned graph's records.
fn find_node_id(records: &[GraphRecord], kind: NodeKind, name: &str) -> Option<String> {
    records.iter().find_map(|record| match record {
        GraphRecord::Node {
            id,
            kind: node_kind,
            name: Some(node_name),
            ..
        } if *node_kind == kind && node_name == name => Some(id.clone()),
        _ => None,
    })
}

#[test]
fn scan_history_detects_inline_module_body_drift_end_to_end() {
    // End-to-end proof (Codex finding A) that #206 content_signature drift
    // detection is NOT inert in scan-history. The reviewer read
    // `is_temporal_change_target` as gating temporal-history membership; it only
    // gates the `CHANGED_IN` edges. Real `scan_repository_history` extraction
    // pushes Module (and Import) records with temporal provenance at every commit,
    // so an inline `mod foo { .. }` whose BODY changed between two commits — while
    // its name/path stayed fixed — must surface as `drifted`, and an UNCHANGED
    // `use` import must stay `current` (proving the Import record reaches the
    // freshness comparison rather than being discarded).
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path();
    init_git_repo(repo);

    write_repo_file(
        repo,
        "src/lib.rs",
        "use std::fmt::Debug;\npub mod foo {\n    pub fn a() -> u32 { 1 }\n}\n",
    );
    let first = commit_repo(repo, "initial module and import", "2026-01-01T00:00:00Z");

    // Change only the inline module body; the module NAME/path and the import are
    // untouched, so the module's stable ID is unchanged while its body signature
    // flips.
    write_repo_file(
        repo,
        "src/lib.rs",
        "use std::fmt::Debug;\npub mod foo {\n    pub fn a() -> u32 { 999 }\n}\n",
    );
    let _second = commit_repo(repo, "change module body only", "2026-01-02T00:00:00Z");

    let graph = scan_repository_history(repo).expect("history scan should succeed");
    let module_id =
        find_node_id(graph.records(), NodeKind::Module, "foo").expect("module foo node present");
    let import_id = find_node_id(graph.records(), NodeKind::Import, "std::fmt::Debug")
        .expect("import node present");

    let obs_mod = agent_memory_stable_id(&["obs", "e2e_module_drift"]);
    let obs_import = agent_memory_stable_id(&["obs", "e2e_import_current"]);
    let mut records = graph.into_records();
    records.push(observation(
        &obs_mod,
        "foo gates the parser body",
        "0.9",
        Some(&module_id),
        Some("src/lib.rs"),
        None,
        "OBSERVES",
        Some(&first),
        None,
    ));
    records.push(observation(
        &obs_import,
        "imports std::fmt::Debug",
        "0.9",
        Some(&import_id),
        Some("src/lib.rs"),
        None,
        "OBSERVES",
        Some(&first),
        None,
    ));

    let verdicts = freshness::evidence_link_freshness(&records);

    let module_entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs_mod)
        .expect("verdict for module observation");
    assert_eq!(
        module_entry.verdict,
        FreshnessVerdict::Drifted,
        "inline module body drift must be detected end-to-end via scan-history"
    );
    assert!(matches!(
        module_entry.triggering_handle,
        Some(freshness::TriggeringHandle::ContentChange { .. })
    ));

    let import_entry = verdicts
        .iter()
        .find(|e| e.observation_id == obs_import)
        .expect("verdict for import observation");
    assert_eq!(
        import_entry.verdict,
        FreshnessVerdict::Current,
        "an unchanged import must resolve to a live version — proving the Import \
         record is pushed into temporal history, not discarded"
    );
    assert!(import_entry.triggering_handle.is_none());
}

// ── One-sided-missing content_signature back-compat (Codex finding B) ─────────

/// Builds a temporal Module/Import version carrying NO `content_signature`,
/// simulating a legacy record hashed before the #206 field existed.
#[allow(clippy::too_many_arguments)]
fn signatureless_version(
    id: &str,
    kind: NodeKind,
    path: &str,
    name: &str,
    node_span: SourceSpan,
    summary: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        kind,
        Some(path.to_owned()),
        Some(node_span),
        Some(name.to_owned()),
        summary.to_owned(),
    )
    .with_temporal(temporal(commit, valid_time))
}

#[test]
fn one_sided_missing_content_signature_is_not_drift() {
    // Back-compat regression (Codex finding B): a store upgraded ACROSS #206 holds
    // a legacy `Module` version with `content_signature = None` (hashed
    // summary-only) alongside a post-upgrade rescan of the SAME body carrying
    // `Some(sig)`. The bodies are byte-identical, so freshness must report
    // `current`. Before the fix, folding the signature into the hash on only the
    // upgraded side flipped a byte-identical body to a false `drifted`. The
    // `Import` here never carries a `content_signature` on either side (imports
    // encode their declaration in the ID), so its unchanged declaration must
    // likewise stay `current`.
    let mpath = "src/legacy_mod.rs";
    let module = stable_id(&["node", "module", "repo-a", mpath, "legacy_mod"]);
    let ipath = "src/legacy_import.rs";
    let import = stable_id(&["node", "import", "repo-a", ipath, "BTreeSet"]);

    let obs_mod = agent_memory_stable_id(&["obs", "module_one_sided"]);
    let obs_import = agent_memory_stable_id(&["obs", "import_one_sided"]);
    let records = vec![
        // Legacy anchors: no content_signature (pre-#206 records).
        signatureless_version(
            &module,
            NodeKind::Module,
            mpath,
            "legacy_mod",
            span(1, 20),
            "Rust module legacy_mod",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        signatureless_version(
            &import,
            NodeKind::Import,
            ipath,
            "BTreeSet",
            span(1, 1),
            "Rust import BTreeSet",
            "commit_a",
            "2026-01-01T00:00:00Z",
        ),
        // Post-upgrade frontiers: the module carries Some(sig) over the SAME
        // (unchanged) body; the import stays signatureless (imports never carry
        // one) over its unchanged declaration.
        module_version(
            &module,
            mpath,
            "legacy_mod",
            span(1, 20),
            "blake3:legacy-mod-body",
            "commit_b",
            "2026-03-01T00:00:00Z",
        ),
        import_version(
            &import,
            ipath,
            "BTreeSet",
            span(1, 1),
            "commit_b",
            "2026-03-01T00:00:00Z",
        ),
        observation(
            &obs_mod,
            "legacy_mod groups the adapters",
            "0.9",
            Some(&module),
            Some(mpath),
            Some(span(1, 20)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
        observation(
            &obs_import,
            "imports BTreeSet",
            "0.9",
            Some(&import),
            Some(ipath),
            Some(span(1, 1)),
            "OBSERVES",
            Some("commit_a"),
            None,
        ),
    ];

    let verdicts = freshness::evidence_link_freshness(&records);
    for obs in [&obs_mod, &obs_import] {
        let entry = verdicts
            .iter()
            .find(|e| &e.observation_id == obs)
            .expect("verdict for one-sided observation");
        assert_eq!(
            entry.verdict,
            FreshnessVerdict::Current,
            "a one-sided-missing content_signature (upgrade across #206) with an \
             unchanged body must not be reported as drift"
        );
        assert!(entry.triggering_handle.is_none());
    }
}
