//! Record-budget acceptance tests for issue #211.
//!
//! `--max-records N` caps the composite evidence answers (`eg query context`,
//! `eg query memory`, `eg query failures`, `eg query change-impact`) to at
//! most `N` evidence records across all sections, and every omitted record is
//! accounted for by a per-section `returned`/`total` pair — zero silently
//! dropped records.
//!
//! The tests drive the real CLI end to end against seeded graph JSONL files
//! and assert the budget *properties* against each command's unbudgeted
//! baseline answer, so they stay valid as long as the domain joins keep
//! returning the seeded evidence.

#![allow(missing_docs)]

use std::path::{Path, PathBuf};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, Graph, PROJECT_SCHEMA_VERSION,
        VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id, artifact_stable_id, project_stable_id,
        stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;
use serde_json::Value;

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
}

fn write_graph(graph: &Graph, name: &str) -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join(name);
    std::fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write fixture");
    Fixture {
        _temp: temp,
        graph: path,
    }
}

fn repo_node(graph: &mut Graph) -> String {
    let id = stable_id(&["node", "Repository", "repo-budget"]);
    graph.push(GraphRecord::node(
        id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-budget".to_owned()),
        "Repository repo-budget".to_owned(),
    ));
    id
}

fn file_node(graph: &mut Graph, repo: &str, path: &str) -> String {
    let id = stable_id(&["node", "File", path]);
    graph.push(GraphRecord::syntax_node(
        id.clone(),
        NodeKind::File,
        path.to_owned(),
        span(1, 200),
        path.to_owned(),
        "rust",
        format!("Source file {path}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repo.to_owned(),
        id.clone(),
        None,
        "repo contains file".to_owned(),
    ));
    id
}

fn symbol_node(graph: &mut Graph, file: &str, path: &str, name: &str) -> String {
    let id = stable_id(&["node", "Symbol", path, name]);
    graph.push(GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Symbol,
        path.to_owned(),
        span(10, 20),
        name.to_owned(),
        "rust",
        format!("fn {name}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Defines,
        file.to_owned(),
        id.clone(),
        None,
        format!("file defines {name}"),
    ));
    id
}

/// Agent-memory node (observation / decision / failure / task / artifact /
/// verification) with standard provenance fields and evidence links.
fn agent_node(
    graph: &mut Graph,
    id: String,
    kind: NodeKind,
    schema_version: u32,
    summary: &str,
    links: Vec<EvidenceLink>,
) -> String {
    let mut node = GraphRecord::node(id.clone(), kind, None, None, None, summary.to_owned());
    if let GraphRecord::Node {
        text,
        agent_id,
        session_id,
        observed_at,
        confidence,
        source_handle,
        schema_version: sv,
        evidence_links,
        ..
    } = &mut node
    {
        *text = Some(summary.to_owned());
        *agent_id = Some("agent_1".to_owned());
        *session_id = Some("sess_1".to_owned());
        *observed_at = Some("2026-06-01T00:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *source_handle = Some("trajectories/run-1.traj".to_owned());
        *sv = schema_version;
        *evidence_links = Some(links);
    }
    graph.push(node);
    id
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Context fixture: a symbol with callers (topology), observations, tasks,
/// artifacts, and verification evidence linked to it.
fn seed_context() -> (Fixture, String) {
    let mut graph = Graph::new();
    let repo = repo_node(&mut graph);
    let file = file_node(&mut graph, &repo, "src/budget.rs");
    let probe = symbol_node(&mut graph, &file, "src/budget.rs", "budget_probe");

    for i in 0..5 {
        let caller = symbol_node(&mut graph, &file, "src/budget.rs", &format!("caller_{i}"));
        graph.push(GraphRecord::edge(
            EdgeLabel::Calls,
            caller,
            probe.clone(),
            None,
            format!("caller_{i} calls budget_probe"),
        ));
    }
    for i in 0..4 {
        agent_node(
            &mut graph,
            agent_memory_stable_id(&["budget", "ctx_obs", &i.to_string()]),
            NodeKind::Observation,
            AGENT_MEMORY_SCHEMA_VERSION,
            &format!("context observation {i}"),
            vec![link(&probe, "codegraph", "OBSERVES")],
        );
    }
    for i in 0..2 {
        let id = project_stable_id(&["budget", "ctx_task", &i.to_string()]);
        let mut task = GraphRecord::node(
            id.clone(),
            NodeKind::Task,
            None,
            None,
            Some(format!("budget task {i}")),
            format!("Task budget task {i}"),
        );
        if let GraphRecord::Node {
            title,
            schema_version,
            evidence_links,
            ..
        } = &mut task
        {
            *title = Some(format!("budget task {i}"));
            *schema_version = PROJECT_SCHEMA_VERSION;
            *evidence_links = Some(vec![link(&probe, "codegraph", "RELATES_TO")]);
        }
        graph.push(task);
    }
    for i in 0..2 {
        agent_node(
            &mut graph,
            artifact_stable_id(&["budget", "ctx_art", &i.to_string()]),
            NodeKind::Artifact,
            ARTIFACT_SCHEMA_VERSION,
            &format!("context artifact {i}"),
            vec![link(&probe, "codegraph", "RELATES_TO")],
        );
    }
    for i in 0..2 {
        let mut ver = GraphRecord::node(
            verification_stable_id(&["budget", "ctx_ver", &i.to_string()]),
            NodeKind::TestRun,
            None,
            None,
            None,
            format!("context verification {i}"),
        );
        if let GraphRecord::Node {
            status,
            schema_version,
            evidence_links,
            ..
        } = &mut ver
        {
            *status = Some("pass".to_owned());
            *schema_version = VERIFICATION_SCHEMA_VERSION;
            *evidence_links = Some(vec![link(&probe, "codegraph", "VALIDATED_BY")]);
        }
        graph.push(ver);
    }

    let fx = write_graph(&graph, "budget_context.jsonl");
    (fx, probe)
}

/// Memory-audit fixture: a claim with supporting, contradicting, superseding,
/// related-code, related-project, and verification evidence.
fn seed_memory() -> (Fixture, String) {
    let mut graph = Graph::new();
    let repo = repo_node(&mut graph);
    let file = file_node(&mut graph, &repo, "src/budget.rs");
    let probe = symbol_node(&mut graph, &file, "src/budget.rs", "budget_probe");

    let task_id = project_stable_id(&["budget", "mem_task"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("budget memory task".to_owned()),
        "Task budget memory task".to_owned(),
    );
    if let GraphRecord::Node {
        title,
        schema_version,
        ..
    } = &mut task
    {
        *title = Some("budget memory task".to_owned());
        *schema_version = PROJECT_SCHEMA_VERSION;
    }
    graph.push(task);

    let ver_id = verification_stable_id(&["budget", "mem_ver"]);
    let mut ver = GraphRecord::node(
        ver_id.clone(),
        NodeKind::TestRun,
        None,
        None,
        None,
        "memory verification".to_owned(),
    );
    if let GraphRecord::Node {
        status,
        schema_version,
        ..
    } = &mut ver
    {
        *status = Some("pass".to_owned());
        *schema_version = VERIFICATION_SCHEMA_VERSION;
    }
    graph.push(ver);

    let sup0 = agent_memory_stable_id(&["budget", "mem_sup", "0"]);
    let sup1 = agent_memory_stable_id(&["budget", "mem_sup", "1"]);
    for (id, i) in [(&sup0, 0), (&sup1, 1)] {
        agent_node(
            &mut graph,
            (*id).clone(),
            NodeKind::Observation,
            AGENT_MEMORY_SCHEMA_VERSION,
            &format!("supporting observation {i}"),
            vec![],
        );
    }

    // The claim carries denormalized evidence links to every section.
    let claim_id = agent_memory_stable_id(&["budget", "mem_claim"]);
    agent_node(
        &mut graph,
        claim_id.clone(),
        NodeKind::Observation,
        AGENT_MEMORY_SCHEMA_VERSION,
        "budget memory claim",
        vec![
            link(&sup0, "agent_memory", "HAS_EVIDENCE"),
            link(&sup1, "agent_memory", "HAS_EVIDENCE"),
            link(&probe, "codegraph", "OBSERVES"),
            link(&task_id, "project", "REFERENCES_TASK"),
            link(&ver_id, "verification", "VALIDATED_BY"),
        ],
    );

    // Contradicting record: denormalized CONTRADICTS link targeting the claim.
    agent_node(
        &mut graph,
        agent_memory_stable_id(&["budget", "mem_contra"]),
        NodeKind::Observation,
        AGENT_MEMORY_SCHEMA_VERSION,
        "contradicting observation",
        vec![link(&claim_id, "agent_memory", "CONTRADICTS")],
    );
    // Superseding record: denormalized SUPERSEDES link targeting the claim.
    agent_node(
        &mut graph,
        agent_memory_stable_id(&["budget", "mem_super"]),
        NodeKind::Decision,
        AGENT_MEMORY_SCHEMA_VERSION,
        "superseding decision",
        vec![link(&claim_id, "agent_memory", "SUPERSEDES")],
    );

    let fx = write_graph(&graph, "budget_memory.jsonl");
    (fx, claim_id)
}

/// Failures fixture: agent failures, runtime failures, a superseding success,
/// and a patch artifact produced by one failure.
#[allow(clippy::too_many_lines)]
fn seed_failures() -> (Fixture, String) {
    let mut graph = Graph::new();
    let repo = repo_node(&mut graph);
    let file = file_node(&mut graph, &repo, "src/budget.rs");
    let probe = symbol_node(&mut graph, &file, "src/budget.rs", "budget_probe");

    let mut fail_ids = Vec::new();
    for i in 0..3 {
        let mut fail = GraphRecord::node(
            agent_memory_stable_id(&["budget", "fail", &i.to_string()]),
            NodeKind::Failure,
            None,
            None,
            None,
            format!("agent failure {i}"),
        );
        if let GraphRecord::Node {
            text,
            failure_kind,
            agent_id,
            session_id,
            observed_at,
            source_handle,
            schema_version,
            evidence_links,
            ..
        } = &mut fail
        {
            *text = Some(format!("agent failure {i}"));
            *failure_kind = Some("command_failure".to_owned());
            *agent_id = Some("agent_1".to_owned());
            *session_id = Some("sess_1".to_owned());
            *observed_at = Some(format!("2026-06-0{}T00:00:00Z", i + 1));
            *source_handle = Some("trajectories/run-1.traj".to_owned());
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *evidence_links = Some(vec![link(&probe, "codegraph", "FAILED_ON")]);
        }
        fail_ids.push(fail.id().to_owned());
        graph.push(fail);
    }

    for i in 0..2 {
        let mut ver = GraphRecord::node(
            verification_stable_id(&["budget", "run_fail", &i.to_string()]),
            NodeKind::TestRun,
            None,
            None,
            None,
            format!("runtime failure {i}"),
        );
        if let GraphRecord::Node {
            status,
            verification_kind,
            executed_at,
            schema_version,
            evidence_links,
            ..
        } = &mut ver
        {
            *status = Some("fail".to_owned());
            *verification_kind = Some("test_run".to_owned());
            *executed_at = Some(format!("2026-06-1{}T00:00:00Z", i + 1));
            *schema_version = VERIFICATION_SCHEMA_VERSION;
            *evidence_links = Some(vec![link(&probe, "codegraph", "VALIDATED_BY")]);
        }
        graph.push(ver);
    }

    // Later passing verification on the same target (superseding success).
    let mut success = GraphRecord::node(
        verification_stable_id(&["budget", "success"]),
        NodeKind::TestRun,
        None,
        None,
        None,
        "later passing run".to_owned(),
    );
    if let GraphRecord::Node {
        status,
        verification_kind,
        executed_at,
        schema_version,
        evidence_links,
        ..
    } = &mut success
    {
        *status = Some("pass".to_owned());
        *verification_kind = Some("test_run".to_owned());
        *executed_at = Some("2026-06-20T00:00:00Z".to_owned());
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *evidence_links = Some(vec![link(&probe, "codegraph", "VALIDATED_BY")]);
    }
    graph.push(success);

    // Patch artifact produced by the first failure.
    let patch_id = artifact_stable_id(&["budget", "patch"]);
    let mut patch = GraphRecord::node(
        patch_id.clone(),
        NodeKind::PatchArtifact,
        None,
        None,
        None,
        "patch for budget_probe".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        patch_status,
        patch_bytes_hash,
        ..
    } = &mut patch
    {
        *schema_version = ARTIFACT_SCHEMA_VERSION;
        *patch_status = Some("rejected_validation".to_owned());
        *patch_bytes_hash = Some("blake3:budgetpatch".to_owned());
    }
    graph.push(patch);
    graph.push(GraphRecord::edge(
        EdgeLabel::ProducedPatch,
        fail_ids[0].clone(),
        patch_id,
        None,
        "failure produced patch".to_owned(),
    ));

    let fx = write_graph(&graph, "budget_failures.jsonl");
    (fx, probe)
}

/// Change-impact fixture: an anchor with callers, callees, a referencing
/// file, an implementor, and containing file context.
fn seed_impact() -> (Fixture, String) {
    let mut graph = Graph::new();
    let repo = repo_node(&mut graph);
    let file = file_node(&mut graph, &repo, "src/budget.rs");
    let anchor = symbol_node(&mut graph, &file, "src/budget.rs", "budget_anchor");

    for i in 0..5 {
        let caller = symbol_node(
            &mut graph,
            &file,
            "src/budget.rs",
            &format!("imp_caller_{i}"),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Calls,
            caller,
            anchor.clone(),
            None,
            format!("imp_caller_{i} calls budget_anchor"),
        ));
    }
    for i in 0..2 {
        let callee = symbol_node(
            &mut graph,
            &file,
            "src/budget.rs",
            &format!("imp_callee_{i}"),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Calls,
            anchor.clone(),
            callee,
            None,
            format!("budget_anchor calls imp_callee_{i}"),
        ));
    }
    // Referencing file: a symbol in another file with a REFERENCES edge.
    let ref_file = file_node(&mut graph, &repo, "src/refs.rs");
    let referrer = symbol_node(&mut graph, &ref_file, "src/refs.rs", "referrer");
    graph.push(GraphRecord::edge(
        EdgeLabel::References,
        referrer,
        anchor.clone(),
        None,
        "referrer references budget_anchor".to_owned(),
    ));
    // Implementation symbol.
    let implementor = symbol_node(&mut graph, &file, "src/budget.rs", "anchor_impl");
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        implementor,
        anchor.clone(),
        None,
        "anchor_impl implements budget_anchor".to_owned(),
    ));

    let fx = write_graph(&graph, "budget_impact.jsonl");
    (fx, anchor)
}

// ---------------------------------------------------------------------------
// Harness: section readers + per-command specs
// ---------------------------------------------------------------------------

/// Runs `eg query <subcommand> <positional..> --graph <graph> [extra..]`.
fn run_query(
    subcommand: &str,
    positional: &[&str],
    graph: &Path,
    extra: &[&str],
) -> (i32, String, String) {
    let assert = egregore()
        .arg("query")
        .arg(subcommand)
        .args(positional)
        .arg("--graph")
        .arg(graph)
        .args(extra)
        .assert();
    let output = assert.get_output().clone();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8(output.stderr).expect("utf8 stderr"),
    )
}

/// One evidence section in either wire shape: the bare record array (nothing
/// omitted) or the truncated `{"returned","total","records"}` object.
struct Section {
    records: Vec<Value>,
    /// `(returned, total)` — present only when the section was truncated.
    counts: Option<(usize, usize)>,
}

fn read_section(env: &Value, name: &str) -> Section {
    match env.get(name) {
        None | Some(Value::Null) => Section {
            records: Vec::new(),
            counts: None,
        },
        Some(Value::Array(items)) => Section {
            records: items.clone(),
            counts: None,
        },
        Some(Value::Object(map)) => {
            let records = map
                .get("records")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let returned = map
                .get("returned")
                .and_then(Value::as_u64)
                .expect("truncated section reports returned");
            let total = map
                .get("total")
                .and_then(Value::as_u64)
                .expect("truncated section reports total");
            Section {
                records,
                counts: Some((
                    usize::try_from(returned).expect("count fits"),
                    usize::try_from(total).expect("count fits"),
                )),
            }
        }
        Some(other) => panic!("section {name} has unexpected shape: {other}"),
    }
}

/// One composite evidence command under test: its subcommand name, the
/// positional argument resolving the fixture's evidence, and its evidence
/// sections in envelope order (the budget division order).
struct CommandSpec {
    subcommand: &'static str,
    sections: &'static [&'static str],
    /// Whether the success envelope carries a `page` object whose `returned`
    /// must equal the retained record count and whose `has_more` must be true
    /// exactly when the budget omitted anything.
    has_page: bool,
}

const CONTEXT_SPEC: CommandSpec = CommandSpec {
    subcommand: "context",
    has_page: false,
    sections: &[
        "source_facts",
        "topology_edges",
        "observations",
        "project_state",
        "artifacts",
        "verification_evidence",
        "drift_history",
        "unresolved",
    ],
};

const MEMORY_SPEC: CommandSpec = CommandSpec {
    subcommand: "memory",
    has_page: true,
    sections: &[
        "memory_claim",
        "supporting_evidence",
        "contradicting_evidence",
        "superseding_records",
        "related_code_handles",
        "related_project_handles",
        "verification_evidence",
    ],
};

const FAILURES_SPEC: CommandSpec = CommandSpec {
    subcommand: "failures",
    has_page: true,
    sections: &[
        "runtime_failures",
        "agent_failures",
        "superseding_successes",
        "patch_artifacts",
    ],
};

const IMPACT_SPEC: CommandSpec = CommandSpec {
    subcommand: "change-impact",
    has_page: true,
    sections: &[
        "direct_callers",
        "direct_callees",
        "referencing_files",
        "implementation_symbols",
        "containing_context",
        "construction_sites",
    ],
};

fn section_totals(env: &Value, spec: &CommandSpec) -> Vec<usize> {
    spec.sections
        .iter()
        .map(|s| read_section(env, s).records.len())
        .collect()
}

/// Sequential-fill expectation: each section keeps `min(total, remaining)`,
/// the remainder flows to the next section.
fn expected_kept(totals: &[usize], budget: usize) -> Vec<usize> {
    let mut remaining = budget;
    totals
        .iter()
        .map(|&t| {
            let kept = t.min(remaining);
            remaining -= kept;
            kept
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Generic budget-property checks, run once per command
// ---------------------------------------------------------------------------

fn check_budget_caps_total(spec: &CommandSpec, fx: &Fixture, handle: &str) {
    let positional = [handle];
    let (code, base_out, stderr) = run_query(spec.subcommand, &positional, &fx.graph, &[]);
    assert_eq!(code, 0, "unbudgeted run failed: {stderr}");
    let base: Value = serde_json::from_str(&base_out).expect("baseline is JSON");
    assert_eq!(base["ok"], true);

    let totals = section_totals(&base, spec);
    let total: usize = totals.iter().sum();
    assert!(
        total > 4,
        "fixture for `{}` must hold enough evidence to budget (got {total})",
        spec.subcommand
    );

    // Budget strictly below the natural size: truncation must occur.
    let budget = total / 2;
    assert!(budget >= 1 && budget < total);
    let (code, out, stderr) = run_query(
        spec.subcommand,
        &positional,
        &fx.graph,
        &["--max-records", &budget.to_string()],
    );
    assert_eq!(code, 0, "budgeted run failed: {stderr}");
    let v: Value = serde_json::from_str(&out).expect("budgeted answer is JSON");
    assert_eq!(v["ok"], true);

    let kept = expected_kept(&totals, budget);
    let mut got_total = 0;
    for (i, name) in spec.sections.iter().enumerate() {
        let section = read_section(&v, name);
        let base_section = read_section(&base, name);
        assert_eq!(
            section.records.len(),
            kept[i],
            "section `{name}` kept {} records, want {}",
            section.records.len(),
            kept[i]
        );
        got_total += section.records.len();
        if kept[i] < totals[i] {
            // Truncated: the section object must report returned/total, and
            // the kept subset must be the top-ranked prefix of the baseline.
            let (returned, total_i) = section
                .counts
                .unwrap_or_else(|| panic!("truncated section `{name}` lacks counts"));
            assert_eq!(returned, kept[i], "section `{name}` returned count");
            assert_eq!(total_i, totals[i], "section `{name}` total count");
            assert_eq!(
                section.records,
                base_section.records[..kept[i]].to_vec(),
                "section `{name}` must keep the top-ranked prefix"
            );
        } else {
            // Untruncated: bare array, byte-identical to the baseline section.
            assert!(
                section.counts.is_none(),
                "untruncated section `{name}` must stay a bare array"
            );
            assert_eq!(
                section.records, base_section.records,
                "untruncated section `{name}` must match the baseline"
            );
        }
    }
    assert_eq!(got_total, budget.min(total), "total records under budget");

    // The `page` envelope (where present) must report the retained count and
    // flag truncation: `has_more` is true exactly when the budget omitted
    // anything.
    if spec.has_page {
        let page = &v["page"];
        assert_eq!(
            page["returned"], got_total,
            "`page.returned` must equal the retained record count"
        );
        let any_truncated = kept.iter().zip(totals.iter()).any(|(k, t)| k < t);
        assert_eq!(
            page["has_more"], any_truncated,
            "`page.has_more` must be true exactly when the budget omitted records"
        );
    }
}

fn check_budget_deterministic(spec: &CommandSpec, fx: &Fixture, handle: &str) {
    let positional = [handle];
    let (code, base_out, stderr) = run_query(spec.subcommand, &positional, &fx.graph, &[]);
    assert_eq!(code, 0, "unbudgeted run failed: {stderr}");
    let base: Value = serde_json::from_str(&base_out).expect("baseline is JSON");
    let total: usize = section_totals(&base, spec).iter().sum();
    let budget = (total / 2).max(1);

    let (code_a, out_a, _) = run_query(
        spec.subcommand,
        &positional,
        &fx.graph,
        &["--max-records", &budget.to_string()],
    );
    let (code_b, out_b, _) = run_query(
        spec.subcommand,
        &positional,
        &fx.graph,
        &["--max-records", &budget.to_string()],
    );
    assert_eq!(code_a, 0);
    assert_eq!(code_b, 0);
    assert_eq!(out_a, out_b, "same query + same N must be byte-identical");
}

fn check_generous_budget_matches_unbudgeted(spec: &CommandSpec, fx: &Fixture, handle: &str) {
    let positional = [handle];
    let (code, base_out, stderr) = run_query(spec.subcommand, &positional, &fx.graph, &[]);
    assert_eq!(code, 0, "unbudgeted run failed: {stderr}");
    let base: Value = serde_json::from_str(&base_out).expect("baseline is JSON");
    let total: usize = section_totals(&base, spec).iter().sum();

    // A budget at or above the natural size reports zero omissions and is
    // byte-identical to the un-budgeted answer (AC4).
    for budget in [total, total + 100] {
        let (code, out, stderr) = run_query(
            spec.subcommand,
            &positional,
            &fx.graph,
            &["--max-records", &budget.to_string()],
        );
        assert_eq!(code, 0, "budget={budget} run failed: {stderr}");
        assert_eq!(
            out, base_out,
            "budget {budget} >= natural size {total} must be byte-identical to the un-budgeted answer"
        );
        let v: Value = serde_json::from_str(&out).expect("JSON");
        for name in spec.sections {
            let section = read_section(&v, name);
            assert!(
                section.counts.is_none(),
                "no section may report truncation under a generous budget"
            );
        }
    }
}

fn check_zero_budget_reports_truncated_to_zero(spec: &CommandSpec, fx: &Fixture, handle: &str) {
    let positional = [handle];
    let (code, base_out, stderr) = run_query(spec.subcommand, &positional, &fx.graph, &[]);
    assert_eq!(code, 0, "unbudgeted run failed: {stderr}");
    let base: Value = serde_json::from_str(&base_out).expect("baseline is JSON");
    let totals = section_totals(&base, spec);
    assert!(totals.iter().sum::<usize>() > 0);

    let (code, out, stderr) = run_query(
        spec.subcommand,
        &positional,
        &fx.graph,
        &["--max-records", "0"],
    );
    // AC6: a found answer is never converted into a no-match.
    assert_eq!(code, 0, "zero-budget run failed: {stderr}");
    let v: Value = serde_json::from_str(&out).expect("JSON");
    assert_eq!(v["ok"], true);

    for (i, name) in spec.sections.iter().enumerate() {
        let section = read_section(&v, name);
        assert!(
            section.records.is_empty(),
            "section `{name}` must keep zero records under --max-records 0"
        );
        if totals[i] > 0 {
            // Truncated-to-zero: reported with returned/total, distinguishable
            // from a genuinely empty section (AC5).
            let (returned, total_i) = section.counts.unwrap_or_else(|| {
                panic!(
                    "section `{name}` with total {} must report truncated-to-zero",
                    totals[i]
                )
            });
            assert_eq!(returned, 0);
            assert_eq!(total_i, totals[i]);
        } else {
            assert!(
                section.counts.is_none(),
                "genuinely empty section `{name}` must not claim truncation"
            );
        }
    }
}

fn check_budget_one_returns_top_record(spec: &CommandSpec, fx: &Fixture, handle: &str) {
    let positional = [handle];
    let (code, base_out, stderr) = run_query(spec.subcommand, &positional, &fx.graph, &[]);
    assert_eq!(code, 0, "unbudgeted run failed: {stderr}");
    let base: Value = serde_json::from_str(&base_out).expect("baseline is JSON");
    let totals = section_totals(&base, spec);
    assert!(totals.iter().sum::<usize>() > 0);

    let (code, out, stderr) = run_query(
        spec.subcommand,
        &positional,
        &fx.graph,
        &["--max-records", "1"],
    );
    assert_eq!(code, 0, "budget-1 run failed: {stderr}");
    let v: Value = serde_json::from_str(&out).expect("JSON");
    assert_eq!(v["ok"], true);

    // AC6: N >= 1 with any evidence returns at least the single
    // highest-ranked record — the first record of the first non-empty
    // baseline section (sequential fill).
    let first_nonempty = spec
        .sections
        .iter()
        .zip(totals.iter())
        .find(|(_, t)| **t > 0)
        .map(|(s, _)| *s)
        .expect("some evidence exists");
    let want = read_section(&base, first_nonempty).records[0].clone();
    let got_sec = read_section(&v, first_nonempty);
    assert_eq!(
        got_sec.records.len(),
        1,
        "budget 1 keeps exactly one record"
    );
    assert_eq!(
        got_sec.records[0], want,
        "budget 1 keeps the top-ranked record"
    );
    let total_kept: usize = spec
        .sections
        .iter()
        .map(|s| read_section(&v, s).records.len())
        .sum();
    assert_eq!(total_kept, 1);
}

fn check_no_match_exit_codes_unchanged(spec: &CommandSpec, fx: &Fixture) {
    // A missing entity still exits 2 with the flag present (AC6): the budget
    // never converts a no-match into a found answer either.
    let missing = "no_such_entity_211";
    let (code_plain, _, _) = run_query(spec.subcommand, &[missing], &fx.graph, &[]);
    let (code_budget, _, _) = run_query(
        spec.subcommand,
        &[missing],
        &fx.graph,
        &["--max-records", "5"],
    );
    assert_eq!(code_plain, 2, "unbudgeted no-match must exit 2");
    assert_eq!(
        code_budget, 2,
        "budgeted no-match must exit 2, got {code_budget}"
    );
}

// ---------------------------------------------------------------------------
// Per-command tests
// ---------------------------------------------------------------------------

#[test]
fn context_budget_caps_total_records() {
    let (fx, _) = seed_context();
    // `eg query context` takes a symbol NAME.
    check_budget_caps_total(&CONTEXT_SPEC, &fx, "budget_probe");
}

#[test]
fn context_budget_is_deterministic() {
    let (fx, _) = seed_context();
    check_budget_deterministic(&CONTEXT_SPEC, &fx, "budget_probe");
}

#[test]
fn context_generous_budget_matches_unbudgeted() {
    let (fx, _) = seed_context();
    check_generous_budget_matches_unbudgeted(&CONTEXT_SPEC, &fx, "budget_probe");
}

#[test]
fn context_zero_budget_reports_truncated_to_zero() {
    let (fx, _) = seed_context();
    check_zero_budget_reports_truncated_to_zero(&CONTEXT_SPEC, &fx, "budget_probe");
}

#[test]
fn context_budget_one_returns_top_record() {
    let (fx, _) = seed_context();
    check_budget_one_returns_top_record(&CONTEXT_SPEC, &fx, "budget_probe");
}

#[test]
fn context_no_match_exit_code_unchanged() {
    let (fx, _) = seed_context();
    check_no_match_exit_codes_unchanged(&CONTEXT_SPEC, &fx);
}

#[test]
fn memory_budget_caps_total_records() {
    let (fx, claim_id) = seed_memory();
    check_budget_caps_total(&MEMORY_SPEC, &fx, &claim_id);
}

#[test]
fn memory_budget_is_deterministic() {
    let (fx, claim_id) = seed_memory();
    check_budget_deterministic(&MEMORY_SPEC, &fx, &claim_id);
}

#[test]
fn memory_generous_budget_matches_unbudgeted() {
    let (fx, claim_id) = seed_memory();
    check_generous_budget_matches_unbudgeted(&MEMORY_SPEC, &fx, &claim_id);
}

#[test]
fn memory_zero_budget_reports_truncated_to_zero() {
    let (fx, claim_id) = seed_memory();
    check_zero_budget_reports_truncated_to_zero(&MEMORY_SPEC, &fx, &claim_id);
}

#[test]
fn memory_budget_one_returns_top_record() {
    let (fx, claim_id) = seed_memory();
    check_budget_one_returns_top_record(&MEMORY_SPEC, &fx, &claim_id);
}

#[test]
fn memory_no_match_exit_code_unchanged() {
    let (fx, _) = seed_memory();
    check_no_match_exit_codes_unchanged(&MEMORY_SPEC, &fx);
}

#[test]
fn failures_budget_caps_total_records() {
    let (fx, probe) = seed_failures();
    check_budget_caps_total(&FAILURES_SPEC, &fx, &probe);
}

#[test]
fn failures_budget_is_deterministic() {
    let (fx, probe) = seed_failures();
    check_budget_deterministic(&FAILURES_SPEC, &fx, &probe);
}

#[test]
fn failures_generous_budget_matches_unbudgeted() {
    let (fx, probe) = seed_failures();
    check_generous_budget_matches_unbudgeted(&FAILURES_SPEC, &fx, &probe);
}

#[test]
fn failures_zero_budget_reports_truncated_to_zero() {
    let (fx, probe) = seed_failures();
    check_zero_budget_reports_truncated_to_zero(&FAILURES_SPEC, &fx, &probe);
}

#[test]
fn failures_budget_one_returns_top_record() {
    let (fx, probe) = seed_failures();
    check_budget_one_returns_top_record(&FAILURES_SPEC, &fx, &probe);
}

#[test]
fn failures_no_match_exit_code_unchanged() {
    let (fx, _) = seed_failures();
    check_no_match_exit_codes_unchanged(&FAILURES_SPEC, &fx);
}

#[test]
fn impact_budget_caps_total_records() {
    let (fx, anchor) = seed_impact();
    check_budget_caps_total(&IMPACT_SPEC, &fx, &anchor);
}

#[test]
fn impact_budget_is_deterministic() {
    let (fx, anchor) = seed_impact();
    check_budget_deterministic(&IMPACT_SPEC, &fx, &anchor);
}

#[test]
fn impact_generous_budget_matches_unbudgeted() {
    let (fx, anchor) = seed_impact();
    check_generous_budget_matches_unbudgeted(&IMPACT_SPEC, &fx, &anchor);
}

#[test]
fn impact_zero_budget_reports_truncated_to_zero() {
    let (fx, anchor) = seed_impact();
    check_zero_budget_reports_truncated_to_zero(&IMPACT_SPEC, &fx, &anchor);
}

#[test]
fn impact_budget_one_returns_top_record() {
    let (fx, anchor) = seed_impact();
    check_budget_one_returns_top_record(&IMPACT_SPEC, &fx, &anchor);
}

#[test]
fn impact_no_match_exit_code_unchanged() {
    let (fx, _) = seed_impact();
    check_no_match_exit_codes_unchanged(&IMPACT_SPEC, &fx);
}

#[test]
fn max_records_flag_is_documented_in_help() {
    // AC7: `eg query --help` surfaces the budget flag on every composite
    // evidence subcommand.
    for sub in ["context", "memory", "failures", "change-impact"] {
        let output = egregore()
            .args(["query", sub, "--help"])
            .assert()
            .get_output()
            .clone();
        assert!(output.status.success(), "{sub} --help failed");
        let help = String::from_utf8(output.stdout).expect("utf8 help");
        assert!(
            help.contains("--max-records"),
            "`eg query {sub} --help` must document --max-records"
        );
    }
}
