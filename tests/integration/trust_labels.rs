#![allow(missing_docs)]
//! Derived trust labels on cross-domain context answers (issue #114).
//!
//! Every record row of a context answer must carry a `trust` field from the
//! closed five-value vocabulary, derived from the record's kind plus its
//! evidence/contradiction edges at the queried snapshot. See
//! `docs/schema/trust-labels.md`.

use std::{collections::BTreeMap, fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
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

    // A — agent-authored, no supporting evidence link  -> agent_unverified
    let obs_a = observation(
        &agent_memory_stable_id(&["obs", "trust", "a"]),
        "target_fn probably needs error handling"
            .to_owned()
            .as_str(),
        vec![code_link(&sym_id)],
    );
    // B — linked to the passing TestRun                -> agent_verified
    let obs_b = observation(
        &agent_memory_stable_id(&["obs", "trust", "b"]),
        "target_fn is covered by the suite",
        vec![
            code_link(&sym_id),
            evidence_link("HAS_EVIDENCE", &test_id, "verification"),
        ],
    );
    // C — contradicted by D                            -> agent_contradicted
    let obs_c_id = agent_memory_stable_id(&["obs", "trust", "c"]);
    let obs_c = observation(
        &obs_c_id,
        "target_fn is unreachable",
        vec![code_link(&sym_id)],
    );
    let obs_d_id = agent_memory_stable_id(&["obs", "trust", "d"]);
    let obs_d = observation(
        &obs_d_id,
        "target_fn is called from the CLI",
        vec![evidence_link("CONTRADICTS", &obs_c_id, "agent_memory")],
    );

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

    let mut graph = Graph::new();
    graph.push(sym);
    graph.push(test_run);
    graph.push(obs_a);
    graph.push(obs_b);
    graph.push(obs_c);
    graph.push(obs_d);
    graph.push(task);
    graph.push(artifact);
    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    (temp, path)
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
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| panic!("no row matching {needle} in {labels:?}"))
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
        let Some(rows) = parsed.get(section).and_then(|v| v.as_array()) else {
            continue;
        };
        for row in rows {
            assert!(
                row.get("trust").is_none(),
                "section {section} must not carry a trust label: {row}"
            );
        }
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
