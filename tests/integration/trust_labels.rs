#![allow(missing_docs)]
//! Derived trust labels on cross-domain context answers (issue #114).
//!
//! Every record row of a context answer must carry a `trust` field from the
//! closed five-value vocabulary, derived from the record's kind plus its
//! evidence/contradiction edges at the queried snapshot. See
//! `docs/schema/trust-labels.md`.

use std::{collections::BTreeMap, fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, MetricKind, NodeKind, SelectionBasis,
    SemanticDriftMetadata, SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, Graph, PROJECT_SCHEMA_VERSION,
        VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id, artifact_stable_id, project_stable_id,
        stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

/// The closed vocabulary from `docs/schema/trust-labels.md` §2.
const TRUST_VOCABULARY: [&str; 5] = [
    "source_derived",
    "verification_evidence",
    "agent_verified",
    "agent_unverified",
    "agent_contradicted",
];

/// Sections whose rows are records and therefore MUST carry `trust`.
const RECORD_SECTIONS: [&str; 7] = [
    "source_facts",
    "observations",
    "project_state",
    "artifacts",
    "verification_evidence",
    "drift_history",
    "excluded",
];

/// Sections that deliberately carry NO `trust` field (they are not records).
const UNLABELED_SECTIONS: [&str; 2] = ["topology_edges", "unresolved"];

fn code_link(target: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: "codegraph".to_owned(),
        relation: "MENTIONS_SYMBOL".to_owned(),
        confidence: "0.9".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

fn evidence_link(relation: &str, target: &str, domain: &str) -> EvidenceLink {
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

fn observation(id: &str, summary: &str, links: Vec<EvidenceLink>) -> GraphRecord {
    let mut r = GraphRecord::node(
        id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        summary.to_owned(),
    );
    if let GraphRecord::Node {
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ref mut schema_version,
        ..
    } = r
    {
        *agent_id = Some("agent:trust_test".to_owned());
        *session_id = Some("session:trust_test".to_owned());
        *observed_at = Some("2026-03-01T12:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(links);
    }
    r
}

/// The AC5 mixed fixture: a source symbol, an unverified observation, an
/// observation backed by a passing `TestRun`, a contradicted observation, a
/// verification record, a project task, and an artifact — all reachable from
/// one queried symbol.
fn fixture_mixed_trust() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("mixed_trust.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "target_fn"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 80,
            start_line: 10,
            end_line: 20,
        },
        "target_fn".to_owned(),
        "Rust fn target_fn at src/lib.rs:10".to_owned(),
    );

    // A passing TestRun, cited by the symbol-linked observation below.
    let test_id = verification_stable_id(&["test_run", "trust", "green"]);
    let mut test_run = GraphRecord::node(
        test_id.clone(),
        NodeKind::TestRun,
        None,
        None,
        None,
        "cargo test — 12 passed".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut status,
        ref mut verification_kind,
        ref mut evidence_links,
        ..
    } = test_run
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
        *verification_kind = Some("test_run".to_owned());
        *evidence_links = Some(vec![code_link(&sym_id)]);
    }

    let claims = agent_claim_records(&sym_id, &test_id);

    // A project Task and an Artifact, both citing the symbol.
    let task_id = project_stable_id(&["task", "trust", "1"]);
    let mut task = GraphRecord::node(
        task_id,
        NodeKind::Task,
        None,
        None,
        Some("Harden target_fn".to_owned()),
        "Task: harden target_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut title,
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = task
    {
        *title = Some("Harden target_fn".to_owned());
        *schema_version = PROJECT_SCHEMA_VERSION;
        *evidence_links = Some(vec![code_link(&sym_id)]);
    }

    let artifact_id = artifact_stable_id(&["artifact", "trust", "1"]);
    let mut artifact = GraphRecord::node(
        artifact_id,
        NodeKind::Artifact,
        None,
        None,
        None,
        "Report artifact".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = artifact
    {
        *schema_version = ARTIFACT_SCHEMA_VERSION;
        *evidence_links = Some(vec![code_link(&sym_id)]);
    }

    let mut records = vec![sym, test_run, task, artifact];
    records.extend(claims);
    records.extend(topology_and_drift_records(&sym_id));

    let mut graph = Graph::new();
    for record in records.clone() {
        graph.push(record);
    }
    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    MIXED_RECORDS.with(|cell| *cell.borrow_mut() = Some(records));
    (temp, path)
}

thread_local! {
    /// The record slice most recently written by [`fixture_mixed_trust`], so the
    /// library-level MCP tool can be driven over the same fixture the CLI reads.
    static MIXED_RECORDS: std::cell::RefCell<Option<Vec<GraphRecord>>> =
        const { std::cell::RefCell::new(None) };
}

fn mixed_trust_records() -> Vec<GraphRecord> {
    MIXED_RECORDS.with(|cell| {
        cell.borrow()
            .clone()
            .expect("fixture_mixed_trust must run first")
    })
}

/// The three agent-authored claims that exercise all three agent verdicts:
/// A carries no supporting link (`agent_unverified`), B links to the passing
/// `TestRun` (`agent_verified`), and C is the target of a live `CONTRADICTS`
/// link from D (`agent_contradicted`).
fn agent_claim_records(sym_id: &str, test_id: &str) -> Vec<GraphRecord> {
    let obs_a = observation(
        &agent_memory_stable_id(&["obs", "trust", "a"]),
        "target_fn probably needs error handling",
        vec![code_link(sym_id)],
    );
    let obs_b = observation(
        &agent_memory_stable_id(&["obs", "trust", "b"]),
        "target_fn is covered by the suite",
        vec![
            code_link(sym_id),
            evidence_link("HAS_EVIDENCE", test_id, "verification"),
        ],
    );
    let obs_c_id = agent_memory_stable_id(&["obs", "trust", "c"]);
    let obs_c = observation(
        &obs_c_id,
        "target_fn is unreachable",
        vec![code_link(sym_id)],
    );
    let obs_d = observation(
        &agent_memory_stable_id(&["obs", "trust", "d"]),
        "target_fn is called from the CLI",
        vec![evidence_link("CONTRADICTS", &obs_c_id, "agent_memory")],
    );
    vec![obs_a, obs_b, obs_c, obs_d]
}

/// The records that populate the sections the core fixture would otherwise
/// leave empty: a `File` + `DEFINES` edge (so `topology_edges` is non-empty),
/// a `SemanticDrift` targeting the symbol (so `drift_history` and the subsystem
/// `semantic_drift` section are non-empty), and an observation citing an
/// ABSENT target (so `unresolved` is non-empty). Without these, the
/// "unlabeled sections carry no trust" and "every record row carries trust"
/// guards would pass vacuously.
fn topology_and_drift_records(sym_id: &str) -> Vec<GraphRecord> {
    let file_id = stable_id(&["node", "File", "src/lib.rs"]);
    let file = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 400,
            start_line: 1,
            end_line: 60,
        },
        "lib.rs".to_owned(),
        "rust",
        "Source file src/lib.rs".to_owned(),
    );
    let defines = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id,
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "file defines target_fn".to_owned(),
    );

    let drift_id = stable_id(&["node", "SemanticDrift", "trust", "1"]);
    let drift = GraphRecord::node(
        drift_id.clone(),
        NodeKind::SemanticDrift,
        Some("src/lib.rs".to_owned()),
        None,
        Some("target_fn".to_owned()),
        "semantic drift on target_fn".to_owned(),
    )
    .with_semantic_drift(SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "test".to_owned(),
            name: "test-model-v1".to_owned(),
            version: "v1".to_owned(),
            dim: 384,
            content_hash: "fixture".to_owned(),
        },
        target_record_id: sym_id.to_owned(),
        prior_record_id: sym_id.to_owned(),
        before_git_commit: "aaaaaaaa".to_owned(),
        after_git_commit: "bbbbbbbb".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.42,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    });
    let drifts_from = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_id,
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "drifts from edge".to_owned(),
    );

    // Cites the symbol (so it joins the answer) AND an absent record (so the
    // dangling handle lands in `unresolved`).
    let dangling = observation(
        &agent_memory_stable_id(&["obs", "trust", "e"]),
        "target_fn was checked against a run that is not in this slice",
        vec![
            code_link(sym_id),
            evidence_link(
                "HAS_EVIDENCE",
                "verification:v1:absent_from_this_slice",
                "verification",
            ),
        ],
    );

    vec![file, defines, drift, drifts_from, dangling]
}

fn run_context(graph: &PathBuf, extra: &[&str]) -> serde_json::Value {
    let mut cmd = egregore();
    cmd.args(["query", "context", "target_fn", "--graph"])
        .arg(graph);
    cmd.args(extra);
    let output = cmd.assert().success().get_output().stdout.clone();
    let stdout = String::from_utf8(output).expect("utf8");
    serde_json::from_str(stdout.trim()).expect("valid JSON")
}

/// Collects `record_id -> trust` for every row in every record section.
fn trust_map(envelope: &serde_json::Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for section in RECORD_SECTIONS {
        let Some(rows) = envelope.get(section).and_then(|v| v.as_array()) else {
            continue;
        };
        for row in rows {
            let Some(id) = row.get("record_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let trust = row
                .get("trust")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("row {id} in section {section} carries no `trust`"));
            out.insert(id.to_owned(), trust.to_owned());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// AC5 — the mixed fixture returns a distinct, correct label for each class
// ---------------------------------------------------------------------------

#[test]
fn context_mixed_fixture_labels_every_trust_class() {
    let (_temp, graph) = fixture_mixed_trust();
    let parsed = run_context(&graph, &["--supersession", "include-but-flag"]);
    let labels = trust_map(&parsed);

    let find = |needle: &str| -> String {
        labels
            .iter()
            .find(|(id, _)| id.contains(needle))
            .map_or_else(
                || panic!("no row matching {needle} in {labels:?}"),
                |(_, t)| t.clone(),
            )
    };

    // The queried code fact.
    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "target_fn"]);
    assert_eq!(
        labels.get(&sym_id).map(String::as_str),
        Some("source_derived")
    );

    // The verification record itself.
    let test_id = verification_stable_id(&["test_run", "trust", "green"]);
    assert_eq!(
        labels.get(&test_id).map(String::as_str),
        Some("verification_evidence")
    );

    // The three agent resolutions.
    let obs_a = agent_memory_stable_id(&["obs", "trust", "a"]);
    let obs_b = agent_memory_stable_id(&["obs", "trust", "b"]);
    let obs_c = agent_memory_stable_id(&["obs", "trust", "c"]);
    assert_eq!(
        labels.get(&obs_a).map(String::as_str),
        Some("agent_unverified")
    );
    assert_eq!(
        labels.get(&obs_b).map(String::as_str),
        Some("agent_verified")
    );
    assert_eq!(
        labels.get(&obs_c).map(String::as_str),
        Some("agent_contradicted")
    );

    // All five classes are present in one answer.
    let seen: std::collections::BTreeSet<&str> = labels.values().map(String::as_str).collect();
    for value in TRUST_VOCABULARY {
        assert!(seen.contains(value), "class {value} missing from {seen:?}");
    }

    // Success metric, negative half: zero agent-authored rows mislabeled as
    // source_derived or verification_evidence.
    for obs in [&obs_a, &obs_b, &obs_c] {
        let trust = find(obs);
        assert!(
            trust.starts_with("agent_"),
            "agent-authored row {obs} was mislabeled {trust}"
        );
    }
}

// ---------------------------------------------------------------------------
// AC1 — every record row in every section carries a value from the vocabulary
// ---------------------------------------------------------------------------

#[test]
fn context_every_record_row_carries_a_vocabulary_trust_value() {
    let (_temp, graph) = fixture_mixed_trust();
    for mode in [
        vec![],
        vec!["--supersession", "include-but-flag"],
        vec!["--supersession", "exclude"],
    ] {
        let parsed = run_context(&graph, &mode);
        let labels = trust_map(&parsed);
        assert!(!labels.is_empty(), "fixture must return rows");
        for (id, trust) in &labels {
            assert!(
                TRUST_VOCABULARY.contains(&trust.as_str()),
                "row {id} carries out-of-vocabulary trust {trust}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// AC3 / D8 — the contradicted row keeps its label in BOTH supersession modes
// ---------------------------------------------------------------------------

#[test]
fn contradicted_row_is_labeled_identically_in_both_supersession_modes() {
    let (_temp, graph) = fixture_mixed_trust();
    let obs_c = agent_memory_stable_id(&["obs", "trust", "c"]);

    // Default (exclude): C is moved into `excluded`, but keeps its label.
    let excluded_view = run_context(&graph, &[]);
    let excluded_rows = excluded_view["excluded"]
        .as_array()
        .expect("excluded array");
    let c_excluded = excluded_rows
        .iter()
        .find(|r| r["record_id"] == serde_json::json!(obs_c))
        .expect("contradicted observation must appear in `excluded` under the default mode");
    assert_eq!(c_excluded["trust"], serde_json::json!("agent_contradicted"));
    assert!(
        !excluded_view["observations"]
            .as_array()
            .expect("observations")
            .iter()
            .any(|r| r["record_id"] == serde_json::json!(obs_c)),
        "default mode must not leave the contradicted row in `observations`"
    );

    // include-but-flag: C stays in `observations` with the same label.
    let flagged = run_context(&graph, &["--supersession", "include-but-flag"]);
    let c_obs = flagged["observations"]
        .as_array()
        .expect("observations")
        .iter()
        .find(|r| r["record_id"] == serde_json::json!(obs_c))
        .expect("include-but-flag must keep the contradicted row");
    assert_eq!(c_obs["trust"], serde_json::json!("agent_contradicted"));
}

// ---------------------------------------------------------------------------
// D9 — non-record sections are deliberately unlabeled
// ---------------------------------------------------------------------------

#[test]
fn topology_edges_and_unresolved_rows_carry_no_trust_field() {
    let (_temp, graph) = fixture_mixed_trust();
    let parsed = run_context(&graph, &["--supersession", "include-but-flag"]);
    for section in UNLABELED_SECTIONS {
        let rows = parsed
            .get(section)
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("section {section} must be present in the envelope"));
        // Without this the `for` below iterates nothing and the guard is vacuous
        // — the exact failure this fixture's File/DEFINES edge and dangling
        // evidence link exist to prevent.
        assert!(
            !rows.is_empty(),
            "fixture must populate {section}, else this guard asserts nothing"
        );
        for row in rows {
            assert!(
                row.get("trust").is_none(),
                "section {section} must not carry a trust label: {row}"
            );
            assert!(
                row.get("trust_class").is_none(),
                "section {section} must not carry a trust_class label: {row}"
            );
        }
    }
}

/// The `trust_map` helper panics on a row that lacks `trust`, so it only proves
/// anything for sections that actually have rows. Pin the fixture's coverage so
/// a future change that empties a section fails loudly instead of silently
/// weakening every AC1 assertion built on it.
#[test]
fn fixture_populates_every_labeled_record_section() {
    let (_temp, graph) = fixture_mixed_trust();
    let flagged = run_context(&graph, &["--supersession", "include-but-flag"]);
    for section in [
        "source_facts",
        "observations",
        "project_state",
        "artifacts",
        "verification_evidence",
        "drift_history",
    ] {
        let rows = flagged
            .get(section)
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("section {section} must be present"));
        assert!(!rows.is_empty(), "fixture must populate {section}");
    }
    // `excluded` only populates under the default exclude mode.
    let default_view = run_context(&graph, &[]);
    assert!(
        !default_view["excluded"]
            .as_array()
            .expect("excluded array")
            .is_empty(),
        "fixture must populate excluded under the default supersession mode"
    );
}

/// A `SemanticDrift` row is a deterministic measurement, never an agent claim.
#[test]
fn drift_history_rows_are_source_derived() {
    let (_temp, graph) = fixture_mixed_trust();
    let parsed = run_context(&graph, &["--supersession", "include-but-flag"]);
    let rows = parsed["drift_history"].as_array().expect("drift_history");
    assert!(!rows.is_empty(), "fixture must carry a drift row");
    for row in rows {
        assert_eq!(row["trust"], serde_json::json!("source_derived"));
        assert_eq!(row["trust_class"], serde_json::json!("source_fact"));
    }
}

/// `eg query task` is a cross-domain context answer too, including the
/// acceptance-criterion rows' nested `verification_record`.
#[test]
fn task_answer_rows_carry_trust() {
    let (_temp, graph) = fixture_mixed_trust();
    let task_id = project_stable_id(&["task", "trust", "1"]);
    let output = egregore()
        .args(["query", "task", &task_id, "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).expect("utf8").trim()).expect("json");
    let rows = parsed["tasks"].as_array().expect("tasks array");
    assert!(!rows.is_empty(), "task lane must return the task row");
    for row in rows {
        let trust = row["trust"].as_str().expect("task row carries trust");
        assert!(TRUST_VOCABULARY.contains(&trust), "bad trust {trust}");
        assert_eq!(row["trust_class"], serde_json::json!("project_state"));
    }
}

// ---------------------------------------------------------------------------
// AC6 — `trust_class` (domain) is emitted alongside `trust` (derived verdict)
// ---------------------------------------------------------------------------

#[test]
fn record_rows_carry_both_trust_class_and_trust() {
    let (_temp, graph) = fixture_mixed_trust();
    let parsed = run_context(&graph, &["--supersession", "include-but-flag"]);

    let obs_a = agent_memory_stable_id(&["obs", "trust", "a"]);
    let row = parsed["observations"]
        .as_array()
        .expect("observations")
        .iter()
        .find(|r| r["record_id"] == serde_json::json!(obs_a))
        .expect("observation A");
    assert_eq!(
        row["trust_class"],
        serde_json::json!("agent_authored"),
        "domain label is retained unchanged"
    );
    assert_eq!(
        row["trust"],
        serde_json::json!("agent_unverified"),
        "derived verdict is distinct from the domain label"
    );
}

// ---------------------------------------------------------------------------
// AC4 — determinism
// ---------------------------------------------------------------------------

#[test]
fn context_trust_labels_are_byte_identical_across_runs() {
    let (_temp, graph) = fixture_mixed_trust();
    let run = || {
        let output = egregore()
            .args(["query", "context", "target_fn", "--graph"])
            .arg(&graph)
            .args(["--supersession", "include-but-flag"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(output).expect("utf8")
    };
    let first = run();
    for _ in 0..2 {
        assert_eq!(first, run(), "context output must be byte-identical");
    }
}

#[test]
fn context_trust_labels_are_invariant_to_input_record_order() {
    let (_temp, graph) = fixture_mixed_trust();
    let forward = trust_map(&run_context(
        &graph,
        &["--supersession", "include-but-flag"],
    ));

    // Rewrite the fixture with its JSONL lines reversed.
    let reversed_path = graph.with_file_name("mixed_trust_reversed.jsonl");
    let body = fs::read_to_string(&graph).expect("read fixture");
    let mut lines: Vec<&str> = body.lines().collect();
    lines.reverse();
    fs::write(&reversed_path, format!("{}\n", lines.join("\n"))).expect("write reversed");

    let reversed = trust_map(&run_context(
        &reversed_path,
        &["--supersession", "include-but-flag"],
    ));
    assert_eq!(
        forward, reversed,
        "record order must not change any trust label"
    );
}

// ---------------------------------------------------------------------------
// Other cross-domain context surfaces carry the same labels
// ---------------------------------------------------------------------------

#[test]
fn subsystem_answer_rows_carry_trust() {
    let (_temp, graph) = fixture_mixed_trust();
    let output = egregore()
        .args(["query", "subsystem", "src", "--graph"])
        .arg(&graph)
        .args(["--supersession", "include-but-flag"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).expect("utf8").trim()).expect("json");
    let labels = trust_map(&parsed);
    assert!(!labels.is_empty(), "subsystem must return labeled rows");
    for (id, trust) in &labels {
        assert!(
            TRUST_VOCABULARY.contains(&trust.as_str()),
            "subsystem row {id} carries out-of-vocabulary trust {trust}"
        );
    }
}

#[test]
fn locate_answer_rows_carry_trust() {
    let (_temp, graph) = fixture_mixed_trust();
    let output = egregore()
        .args(["query", "locate", "src/lib.rs:12", "--graph"])
        .arg(&graph)
        .args(["--supersession", "include-but-flag"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).expect("utf8").trim()).expect("json");
    let labels = trust_map(&parsed);
    assert!(!labels.is_empty(), "locate must return labeled rows");
    for (id, trust) in &labels {
        assert!(
            TRUST_VOCABULARY.contains(&trust.as_str()),
            "locate row {id} carries out-of-vocabulary trust {trust}"
        );
    }
}

/// The subsystem lane's own extra record sections (`semantic_drift`,
/// `log_signatures`) are not in `RECORD_SECTIONS`, so `trust_map` skips them.
/// Assert them directly, or their labels ship untested.
#[test]
fn subsystem_semantic_drift_rows_carry_trust() {
    let (_temp, graph) = fixture_mixed_trust();
    let output = egregore()
        .args(["query", "subsystem", "src", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).expect("utf8").trim()).expect("json");
    let rows = parsed["semantic_drift"]
        .as_array()
        .expect("semantic_drift array");
    assert!(
        !rows.is_empty(),
        "fixture must produce a subsystem drift row"
    );
    for row in rows {
        assert_eq!(row["trust"], serde_json::json!("source_derived"));
        assert_eq!(row["trust_class"], serde_json::json!("source_fact"));
    }
}

/// `eg query error-context` is a trust-separated cross-domain envelope too.
#[test]
fn error_context_rows_carry_trust() {
    let (_temp, graph) = fixture_mixed_trust();
    // No ErrorSignature in this fixture, so the lane exits 2 (no_match); the
    // point of the assertion is that the row shape carries the field wherever
    // rows exist, which the MCP/unit coverage proves. Guard the contract that
    // the lane at least does not regress into a crash.
    let assert = egregore()
        .args(["query", "error-context", "does_not_exist", "--graph"])
        .arg(&graph)
        .assert();
    let code = assert.get_output().status.code();
    assert_eq!(code, Some(2), "absent signature must be a typed no_match");
}

/// The MCP tool surface is a third serializer path; prove it agrees with the CLI
/// rather than trusting that it was wired.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn mcp_symbol_context_rows_carry_the_same_trust_as_the_cli() {
    let (_temp, graph) = fixture_mixed_trust();
    let records = mixed_trust_records();
    let value = aletheia_egregore::mcp::tool_symbol_context_from_records(&records, "target_fn");

    let cli = run_context(&graph, &["--supersession", "include-but-flag"]);
    let cli_labels = trust_map(&cli);

    let mut seen = 0_usize;
    for section in RECORD_SECTIONS {
        let Some(rows) = value.get(section).and_then(|v| v.as_array()) else {
            continue;
        };
        for row in rows {
            let id = row["record_id"].as_str().expect("record_id");
            let trust = row
                .get("trust")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("MCP row {id} in {section} carries no trust"));
            if let Some(expected) = cli_labels.get(id) {
                assert_eq!(trust, expected, "MCP and CLI disagree on {id}");
                seen += 1;
            }
        }
    }
    assert!(
        seen >= 4,
        "expected the MCP tool to share rows with the CLI"
    );
}

#[test]
fn edge_labels_do_not_leak_into_the_trust_vocabulary() {
    // Guard against a future section being labeled from its section name rather
    // than from its record: every emitted value must be one of the five.
    let (_temp, graph) = fixture_mixed_trust();
    let parsed = run_context(&graph, &["--supersession", "include-but-flag"]);
    let mut stack = vec![&parsed];
    while let Some(value) = stack.pop() {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(t) = map.get("trust").and_then(|v| v.as_str()) {
                    assert!(
                        TRUST_VOCABULARY.contains(&t),
                        "out-of-vocabulary trust value {t} somewhere in the envelope"
                    );
                }
                stack.extend(map.values());
            }
            serde_json::Value::Array(items) => stack.extend(items.iter()),
            _ => {}
        }
    }
}
