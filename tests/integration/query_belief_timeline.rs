//! Integration tests for `eg query belief-timeline` (issue #235).
//!
//! The fixture holds one code target (`symbol:foo`) with an A←B←C
//! supersession chain, one contradiction pair, and two uncontested
//! observations — across two agent identities and distinct `observed_at`
//! values — plus a second, empty-but-present target (`symbol:empty`) and a
//! cyclic target (`symbol:cyc`). Record bodies carry a sentinel string so the
//! tests can prove no body ever leaks into lane output.
#![allow(missing_docs)]

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use aletheia_egregore::{
    AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
};
use assert_cmd::Command;

const BODY_SENTINEL: &str = "SENTINEL_BODY_MUST_NOT_LEAK_235";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 10,
        start_line,
        end_line,
        start_column: None,
        end_column: None,
    }
}

fn symbol(id: &str, name: &str, path: &str, start_line: usize, end_line: usize) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(span(start_line, end_line)),
        Some(name.to_owned()),
        format!("symbol {name}"),
    )
}

fn link(target: &str, relation: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: "codegraph".to_owned(),
        relation: relation.to_owned(),
        confidence: "0.9".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn observation(
    id: &str,
    agent: &str,
    session: &str,
    observed_at: &str,
    links: Vec<EvidenceLink>,
    superseded_by: Option<&str>,
) -> GraphRecord {
    let mut record = GraphRecord::node(
        id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        format!("observation {id}"),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        agent_kind,
        session_id,
        observed_at: observed,
        ingested_at,
        confidence,
        evidence_links,
        superseded_by: superseded,
        text,
        ..
    } = &mut record
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some(agent.to_owned());
        *agent_kind = Some("claude-code".to_owned());
        *session_id = Some(session.to_owned());
        *observed = Some(observed_at.to_owned());
        *ingested_at = Some(observed_at.to_owned());
        *confidence = Some("0.8".to_owned());
        *evidence_links = Some(links);
        *superseded = superseded_by.map(str::to_owned);
        *text = Some(format!("{BODY_SENTINEL} {id}"));
    }
    record
}

/// Writes the fixture graph and returns its path (plus the owning temp dir).
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("graph.jsonl");
    let target = "symbol:foo";
    let records = vec![
        symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
        symbol("symbol:empty", "empty", "src/e.rs", 1, 10),
        symbol("symbol:cyc", "cyc", "src/c.rs", 1, 10),
        // A←B←C supersession chain.
        observation(
            "obs:a",
            "agent_1",
            "sess-1",
            "2026-09-10T10:00:00Z",
            vec![link(target, "MENTIONS_SYMBOL")],
            Some("obs:b"),
        ),
        observation(
            "obs:b",
            "agent_2",
            "sess-2",
            "2026-09-11T10:00:00Z",
            vec![link(target, "MENTIONS_SYMBOL")],
            Some("obs:c"),
        ),
        observation(
            "obs:c",
            "agent_1",
            "sess-3",
            "2026-09-12T10:00:00Z",
            vec![link(target, "MENTIONS_SYMBOL")],
            None,
        ),
        // Contradiction pair.
        observation(
            "obs:x",
            "agent_1",
            "sess-4",
            "2026-09-10T11:00:00Z",
            vec![link(target, "MENTIONS_SYMBOL")],
            None,
        ),
        observation(
            "obs:y",
            "agent_2",
            "sess-5",
            "2026-09-11T11:00:00Z",
            vec![
                link(target, "MENTIONS_SYMBOL"),
                link("obs:x", "CONTRADICTS"),
            ],
            None,
        ),
        // Two uncontested observations.
        observation(
            "obs:u1",
            "agent_1",
            "sess-6",
            "2026-09-10T12:00:00Z",
            vec![link(target, "MENTIONS_SYMBOL")],
            None,
        ),
        observation(
            "obs:u2",
            "agent_2",
            "sess-7",
            "2026-09-11T12:00:00Z",
            vec![link(target, "MENTIONS_SYMBOL")],
            None,
        ),
        // Supersession cycle on a separate target.
        observation(
            "obs:p",
            "agent_1",
            "sess-8",
            "2026-09-10T10:00:00Z",
            vec![link("symbol:cyc", "MENTIONS_SYMBOL")],
            Some("obs:q"),
        ),
        observation(
            "obs:q",
            "agent_2",
            "sess-9",
            "2026-09-11T10:00:00Z",
            vec![link("symbol:cyc", "MENTIONS_SYMBOL")],
            Some("obs:p"),
        ),
        // A graph edge citing the target without evidence links.
        GraphRecord::edge(
            EdgeLabel::MentionsSymbol,
            "obs:u2".to_owned(),
            target.to_owned(),
            None,
            "edge citation".to_owned(),
        ),
    ];
    let jsonl = records
        .iter()
        .map(|r| serde_json::to_string(r).expect("record serializes"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&graph, format!("{jsonl}\n")).expect("write fixture");
    (temp, graph)
}

fn run_timeline(graph: &PathBuf, target: &str, extra: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = egregore();
    cmd.args(["query", "belief-timeline", target, "--graph"]);
    cmd.arg(graph);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.assert()
}

fn timeline_value(graph: &PathBuf, target: &str) -> serde_json::Value {
    let assert = run_timeline(graph, target, &[]).success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    assert!(
        !stdout.contains(BODY_SENTINEL),
        "lane output must never carry a record body"
    );
    serde_json::from_str(&stdout).expect("valid JSON envelope")
}

fn entry<'v>(value: &'v serde_json::Value, id: &str) -> &'v serde_json::Value {
    value
        .pointer("/timeline/entries")
        .and_then(serde_json::Value::as_array)
        .expect("entries array")
        .iter()
        .find(|e| e.get("record_id").and_then(serde_json::Value::as_str) == Some(id))
        .unwrap_or_else(|| panic!("no entry for {id}"))
}

#[test]
fn chain_records_carry_statuses_and_immediate_forward_pointers() {
    let (_temp, graph) = fixture();
    let value = timeline_value(&graph, "symbol:foo");

    assert_eq!(value.pointer("/ok"), Some(&serde_json::json!(true)));
    assert_eq!(
        value.pointer("/lane"),
        Some(&serde_json::json!("belief-timeline"))
    );
    // 3 chain + 2 contradiction + 2 uncontested = 7 entries.
    assert_eq!(
        value.pointer("/timeline/entry_count"),
        Some(&serde_json::json!(7))
    );

    let order: Vec<&str> = value
        .pointer("/timeline/entries")
        .and_then(serde_json::Value::as_array)
        .expect("entries")
        .iter()
        .map(|e| {
            e.get("record_id")
                .and_then(serde_json::Value::as_str)
                .expect("record_id")
        })
        .collect();
    assert_eq!(
        order,
        vec![
            "obs:a", "obs:x", "obs:u1", "obs:b", "obs:y", "obs:u2", "obs:c"
        ],
        "oldest first by observed_at"
    );

    let a = entry(&value, "obs:a");
    assert_eq!(a.get("status"), Some(&serde_json::json!("superseded")));
    assert_eq!(
        a.get("forward_record_id"),
        Some(&serde_json::json!("obs:b"))
    );
    assert_eq!(
        a.get("forward_handle"),
        Some(&serde_json::json!("agent_2:sess-2"))
    );
    assert_eq!(a.get("relation"), Some(&serde_json::json!("SUPERSEDES")));
    let b = entry(&value, "obs:b");
    assert_eq!(b.get("status"), Some(&serde_json::json!("superseded")));
    assert_eq!(
        b.get("forward_record_id"),
        Some(&serde_json::json!("obs:c"))
    );
    assert_eq!(
        b.get("forward_handle"),
        Some(&serde_json::json!("agent_1:sess-3"))
    );
    let c = entry(&value, "obs:c");
    assert_eq!(c.get("status"), Some(&serde_json::json!("current")));
    assert!(c.get("forward_record_id").is_none());
}

#[test]
fn contradiction_pair_stays_fully_visible() {
    let (_temp, graph) = fixture();
    let value = timeline_value(&graph, "symbol:foo");
    let x = entry(&value, "obs:x");
    let y = entry(&value, "obs:y");
    assert_eq!(x.get("status"), Some(&serde_json::json!("contradicted")));
    assert_eq!(y.get("status"), Some(&serde_json::json!("contradicted")));
    assert_eq!(
        x.get("forward_record_id"),
        Some(&serde_json::json!("obs:y"))
    );
    assert_eq!(
        y.get("forward_record_id"),
        Some(&serde_json::json!("obs:x"))
    );
}

#[test]
fn uncontested_observations_are_current() {
    let (_temp, graph) = fixture();
    let value = timeline_value(&graph, "symbol:foo");
    for id in ["obs:u1", "obs:u2"] {
        let e = entry(&value, id);
        assert_eq!(e.get("status"), Some(&serde_json::json!("current")));
    }
}

#[test]
fn empty_but_present_target_yields_explicit_empty_result() {
    let (_temp, graph) = fixture();
    let value = timeline_value(&graph, "symbol:empty");
    assert_eq!(
        value.pointer("/timeline/entry_count"),
        Some(&serde_json::json!(0))
    );
    assert_eq!(
        value.pointer("/timeline/entries"),
        Some(&serde_json::json!([]))
    );
    assert_eq!(
        value.pointer("/timeline/target/record_id"),
        Some(&serde_json::json!("symbol:empty"))
    );
}

#[test]
fn unknown_handle_exits_2_with_no_match() {
    let (_temp, graph) = fixture();
    let assert = run_timeline(&graph, "nope", &[]);
    assert_eq!(assert.get_output().status.code(), Some(2));
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
    let value: serde_json::Value = serde_json::from_str(&stderr).expect("JSON diagnostic");
    assert_eq!(
        value.pointer("/error/code"),
        Some(&serde_json::json!("no_match"))
    );
}

#[test]
fn cycle_surfaces_stable_diagnostic_and_closed_statuses() {
    let (_temp, graph) = fixture();
    let value = timeline_value(&graph, "symbol:cyc");
    let diagnostics = value
        .pointer("/timeline/diagnostics")
        .and_then(serde_json::Value::as_array)
        .expect("diagnostics");
    let cycle = diagnostics
        .iter()
        .find(|d| d.get("code").and_then(serde_json::Value::as_str) == Some("supersession_cycle"))
        .expect("supersession_cycle diagnostic");
    assert_eq!(
        cycle.get("record_ids"),
        Some(&serde_json::json!(["obs:p", "obs:q"]))
    );
    // Closed vocabulary: each cycle member is superseded with its immediate
    // forward record.
    let p = entry(&value, "obs:p");
    assert_eq!(p.get("status"), Some(&serde_json::json!("superseded")));
    assert_eq!(
        p.get("forward_record_id"),
        Some(&serde_json::json!("obs:q"))
    );
}

#[test]
fn five_runs_are_byte_identical() {
    let (_temp, graph) = fixture();
    let first = run_timeline(&graph, "symbol:foo", &[])
        .success()
        .get_output()
        .stdout
        .clone();
    for _ in 0..4 {
        let again = run_timeline(&graph, "symbol:foo", &[])
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(first, again, "runs must be byte-identical");
    }
}

#[test]
fn fixture_query_completes_under_two_seconds() {
    let (_temp, graph) = fixture();
    let started = Instant::now();
    run_timeline(&graph, "symbol:foo", &[]).success();
    assert!(
        started.elapsed().as_secs() < 2,
        "fixture query took {:?}",
        started.elapsed()
    );
}

#[test]
fn text_format_renders_without_bodies() {
    let (_temp, graph) = fixture();
    let assert = run_timeline(&graph, "symbol:foo", &["--format", "text"]).success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    assert!(stdout.contains("belief timeline for src/a.rs#foo"));
    assert!(stdout.contains("superseded"));
    assert!(!stdout.contains(BODY_SENTINEL));
}

/// Ingests the fixture into a fresh embedded store and returns its path plus
/// the owning temp dir.
#[cfg(feature = "embedded-aletheiadb")]
fn ingest_fixture(graph: &PathBuf) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    egregore()
        .arg("ingest")
        .arg(graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    (temp, data_dir)
}

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
fn data_dir_query_is_strictly_read_only() {
    let (_fixture_temp, graph) = fixture();
    let (_temp, data_dir) = ingest_fixture(&graph);

    let before = dir_fingerprint(&data_dir);
    egregore()
        .args(["query", "belief-timeline", "symbol:foo", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "belief-timeline must not modify any store file when reading --data-dir"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn graph_and_data_dir_transports_agree() {
    let (_fixture_temp, graph) = fixture();
    let (_temp, data_dir) = ingest_fixture(&graph);

    let from_graph = run_timeline(&graph, "symbol:foo", &[])
        .success()
        .get_output()
        .stdout
        .clone();
    let from_data_dir = egregore()
        .args(["query", "belief-timeline", "symbol:foo", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        from_graph, from_data_dir,
        "--graph and --data-dir must return byte-identical timelines"
    );
}
