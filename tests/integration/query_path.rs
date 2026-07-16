//! End-to-end tests for `eg query path <A> <B>` (issue #225): a directed
//! shortest call-path witness between two symbols over resolved `CALLS` edges.
#![allow(missing_docs, clippy::similar_names, clippy::doc_markdown)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    CallResolution, EdgeLabel, GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    ir::{Graph, SCHEMA_VERSION, stable_id},
};
use assert_cmd::Command;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn eg() -> Command {
    Command::cargo_bin("eg").expect("eg binary should run")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

fn sym_id(path: &str, name: &str) -> String {
    stable_id(&["node", "Symbol", path, name])
}

fn file_id(path: &str) -> String {
    stable_id(&["node", "File", path])
}

fn symbol(graph: &mut Graph, path: &str, name: &str, lines: (usize, usize)) -> String {
    let id = sym_id(path, name);
    graph.push(GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Symbol,
        path.to_owned(),
        span(lines.0, lines.1),
        name.to_owned(),
        "rust",
        format!("fn {name} in {path}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Defines,
        file_id(path),
        id.clone(),
        None,
        format!("{path} defines {name}"),
    ));
    id
}

fn file(graph: &mut Graph, repo_id: &str, path: &str) {
    let fid = file_id(path);
    graph.push(GraphRecord::syntax_node(
        fid.clone(),
        NodeKind::File,
        path.to_owned(),
        span(1, 200),
        path.rsplit('/').next().unwrap().to_owned(),
        "rust",
        format!("Source file {path}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        fid,
        None,
        format!("repo contains {path}"),
    ));
}

fn calls(graph: &mut Graph, from: &str, to: &str, resolution: Option<CallResolution>) {
    let mut edge = GraphRecord::edge(
        EdgeLabel::Calls,
        from.to_owned(),
        to.to_owned(),
        Some("1.0".to_owned()),
        "call edge".to_owned(),
    );
    if let Some(r) = resolution {
        edge = edge.with_resolution(r);
    }
    graph.push(edge);
}

fn references(graph: &mut Graph, from: &str, to: &str) {
    graph.push(GraphRecord::edge(
        EdgeLabel::References,
        from.to_owned(),
        to.to_owned(),
        Some("1.0".to_owned()),
        "reference edge".to_owned(),
    ));
}

// ---------------------------------------------------------------------------
// Fixture — resolved chain entry_a -> mid_m -> sink_b, plus a SHORTER
// unresolved short-circuit (entry_a -unresolved-> sink_b) that must be
// excluded, an ambiguous edge, a REFERENCES-only edge, a disconnected island,
// and an unrelated same-name pair.
// ---------------------------------------------------------------------------

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    entry_a_id: String,
    mid_m_id: String,
    sink_b_id: String,
    amb_target_id: String,
    ref_only_id: String,
    island_id: String,
    dup_a_id: String,
    dup_b_id: String,
    tombstoned_id: String,
}

#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("query_path.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-path"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-path".to_owned()),
        "Repository repo-path".to_owned(),
    ));

    for p in [
        "src/a.rs",
        "src/m.rs",
        "src/b.rs",
        "src/x.rs",
        "src/z.rs",
        "src/dup_a.rs",
        "src/dup_b.rs",
    ] {
        file(&mut graph, &repo_id, p);
    }

    // Resolved 2-hop chain: entry_a -> mid_m -> sink_b.
    let entry_a_id = symbol(&mut graph, "src/a.rs", "entry_a", (5, 20));
    let mid_m_id = symbol(&mut graph, "src/m.rs", "mid_m", (5, 20));
    let sink_b_id = symbol(&mut graph, "src/b.rs", "sink_b", (5, 20));
    calls(
        &mut graph,
        &entry_a_id,
        &mid_m_id,
        Some(CallResolution::Resolved),
    );
    calls(
        &mut graph,
        &mid_m_id,
        &sink_b_id,
        Some(CallResolution::Resolved),
    );

    // A SHORTER unresolved short-circuit entry_a -unresolved-> sink_b: if
    // unresolved edges were traversed the witness would be 1 hop; excluded, so
    // the witness must be the 2-hop resolved chain.
    calls(
        &mut graph,
        &entry_a_id,
        &sink_b_id,
        Some(CallResolution::Unresolved),
    );

    // Ambiguous CALLS edge: entry_a -ambiguous-> amb_target must never form a
    // witness (amb_target is otherwise unreachable).
    let amb_target_id = symbol(&mut graph, "src/x.rs", "amb_target", (5, 12));
    calls(
        &mut graph,
        &entry_a_id,
        &amb_target_id,
        Some(CallResolution::Ambiguous),
    );

    // REFERENCES-only edge: entry_a -REFERENCES-> ref_only must never form a
    // witness (references are excluded).
    let ref_only_id = symbol(&mut graph, "src/x.rs", "ref_only", (14, 20));
    references(&mut graph, &entry_a_id, &ref_only_id);

    // A completely disconnected island (no edges).
    let island_id = symbol(&mut graph, "src/z.rs", "island", (5, 12));

    // Unrelated same-name pair (ambiguity fixture).
    let dup_a_id = symbol(&mut graph, "src/dup_a.rs", "dup_name", (5, 12));
    let dup_b_id = symbol(&mut graph, "src/dup_b.rs", "dup_name", (5, 12));
    // Give dup_a a resolved edge to sink_b so a record-ID query has a witness.
    calls(
        &mut graph,
        &dup_a_id,
        &sink_b_id,
        Some(CallResolution::Resolved),
    );

    // Tombstoned symbol for the stale_handle test.
    let tombstoned_id = symbol(&mut graph, "src/z.rs", "deleted_fn", (60, 70));
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &tombstoned_id]),
        schema_version: SCHEMA_VERSION,
        deleted_id: tombstoned_id.clone(),
        summary: "deleted_fn was deleted".to_owned(),
        producer: None,
    });

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    Fixture {
        _temp: temp,
        graph: path,
        entry_a_id,
        mid_m_id,
        sink_b_id,
        amb_target_id,
        ref_only_id,
        island_id,
        dup_a_id,
        dup_b_id,
        tombstoned_id,
    }
}

fn parse_ndjson(stdout: &[u8]) -> (serde_json::Value, Vec<serde_json::Value>) {
    let out = String::from_utf8(stdout.to_vec()).expect("utf8");
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let header: serde_json::Value =
        serde_json::from_str(lines.next().expect("header line")).expect("header is JSON");
    let rows: Vec<serde_json::Value> = lines
        .map(|l| serde_json::from_str(l).expect("row is JSON"))
        .collect();
    (header, rows)
}

/// Runs a path query that is expected to succeed (exit 0).
fn run_ok(args: &[&str]) -> (serde_json::Value, Vec<serde_json::Value>) {
    let stdout = egregore()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    parse_ndjson(&stdout)
}

// ---------------------------------------------------------------------------
// AC1/AC2/AC3 — resolved multi-hop witness with citable hops.
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn resolved_chain_returns_ordered_witness_path() {
    let f = seed();
    let (header, hops) = run_ok(&[
        "query",
        "path",
        &f.entry_a_id,
        &f.sink_b_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);

    assert_eq!(header["ok"], true);
    assert_eq!(header["verdict"], "path_found");
    assert_eq!(header["path_found"], true);
    assert_eq!(header["direction"], "outbound");
    assert_eq!(header["resolution_scope"], "resolved");
    assert_eq!(
        header["edge_labels"].as_array().unwrap(),
        &vec![serde_json::json!("CALLS")]
    );
    assert_eq!(
        header["from"]["record_id"].as_str(),
        Some(f.entry_a_id.as_str())
    );
    assert_eq!(
        header["to"]["record_id"].as_str(),
        Some(f.sink_b_id.as_str())
    );
    let disclaimer = header["disclaimer"].as_str().expect("disclaimer");
    assert!(
        disclaimer.contains("resolved") && disclaimer.contains("not proof"),
        "disclaimer must state resolved-only + leads-not-proof; got {disclaimer:?}"
    );

    // The unresolved short-circuit (1 hop) is excluded: the witness is the
    // 2-hop resolved chain entry_a -> mid_m -> sink_b.
    assert_eq!(
        header["hops"].as_u64(),
        Some(2),
        "unresolved 1-hop excluded"
    );
    assert_eq!(hops.len(), 2, "one NDJSON line per hop");

    assert_eq!(hops[0]["index"].as_u64(), Some(1));
    assert_eq!(
        hops[0]["from"]["record_id"].as_str(),
        Some(f.entry_a_id.as_str())
    );
    assert_eq!(
        hops[0]["to"]["record_id"].as_str(),
        Some(f.mid_m_id.as_str())
    );
    assert_eq!(hops[1]["index"].as_u64(), Some(2));
    assert_eq!(
        hops[1]["from"]["record_id"].as_str(),
        Some(f.mid_m_id.as_str())
    );
    assert_eq!(
        hops[1]["to"]["record_id"].as_str(),
        Some(f.sink_b_id.as_str())
    );

    // Every hop carries the AC2 citable fields.
    for hop in &hops {
        for side in ["from", "to"] {
            assert!(hop[side]["record_id"].as_str().is_some(), "record_id");
            assert!(hop[side]["name"].as_str().is_some(), "name");
            assert!(hop[side]["kind"].as_str().is_some(), "kind");
            assert!(
                hop[side]["repo_relative_path"].as_str().is_some(),
                "repo_relative_path"
            );
            assert!(hop[side]["span"].is_object(), "span");
        }
        assert_eq!(hop["edge_label"], "CALLS");
        assert_eq!(hop["resolution"], "resolved");
        assert_eq!(hop["confidence"], "1.0", "edge confidence carried");
        assert_eq!(hop["trust"], "reachability_lead");
        assert!(hop["edge_record_id"].as_str().is_some(), "edge handle");
    }

    // Consecutive hops chain: hop[i].to == hop[i+1].from.
    assert_eq!(hops[0]["to"]["record_id"], hops[1]["from"]["record_id"]);
}

// ---------------------------------------------------------------------------
// AC6 — direction is honored: a one-way edge does not produce a reverse path.
// ---------------------------------------------------------------------------

#[test]
fn direction_is_honored_reverse_is_no_path() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "path",
            &f.sink_b_id,
            &f.entry_a_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let (header, hops) = parse_ndjson(&stdout);
    assert_eq!(header["verdict"], "no_path");
    assert_eq!(header["path_found"], false);
    assert!(hops.is_empty(), "no hops on a no_path verdict");
}

// ---------------------------------------------------------------------------
// AC6 — unresolved / ambiguous / REFERENCES edges are excluded.
// ---------------------------------------------------------------------------

#[test]
fn ambiguous_edge_target_is_unreachable() {
    let f = seed();
    // amb_target is reachable from entry_a ONLY via an ambiguous CALLS edge.
    let stdout = egregore()
        .args([
            "query",
            "path",
            &f.entry_a_id,
            &f.amb_target_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let (header, _) = parse_ndjson(&stdout);
    assert_eq!(header["verdict"], "no_path");
}

#[test]
fn references_edge_target_is_unreachable() {
    let f = seed();
    // ref_only is reachable from entry_a ONLY via a REFERENCES edge.
    egregore()
        .args([
            "query",
            "path",
            &f.entry_a_id,
            &f.ref_only_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2);
}

// ---------------------------------------------------------------------------
// AC4 — both endpoints resolve but no directed path exists: explicit verdict,
// exit 2.
// ---------------------------------------------------------------------------

#[test]
fn no_path_between_disconnected_symbols_exit2() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "path",
            &f.entry_a_id,
            &f.island_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let (header, hops) = parse_ndjson(&stdout);
    assert_eq!(header["ok"], true, "the query answered definitively");
    assert_eq!(header["verdict"], "no_path");
    assert_eq!(header["path_found"], false);
    assert!(hops.is_empty());
}

// ---------------------------------------------------------------------------
// Trivial A == B zero-hop path.
// ---------------------------------------------------------------------------

#[test]
fn identical_endpoints_is_trivial_zero_hop_path() {
    let f = seed();
    let (header, hops) = run_ok(&[
        "query",
        "path",
        &f.entry_a_id,
        &f.entry_a_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(header["verdict"], "path_found");
    assert_eq!(header["path_found"], true);
    assert_eq!(header["hops"].as_u64(), Some(0), "zero-hop trivial path");
    assert!(hops.is_empty(), "no hops for a trivial path");
}

// ---------------------------------------------------------------------------
// AC5 — ambiguous endpoint names exit 1 listing all candidate record IDs.
// ---------------------------------------------------------------------------

#[test]
fn ambiguous_from_endpoint_exit1_lists_candidates() {
    let f = seed();
    let stderr = egregore()
        .args([
            "query",
            "path",
            "dup_name",
            &f.sink_b_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(stderr).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(err.trim()).expect("stderr JSON");
    let candidates = v["Ambiguous"]["candidates"]
        .as_array()
        .unwrap_or_else(|| panic!("Ambiguous.candidates required; got {v}"));
    let ids: Vec<&str> = candidates.iter().filter_map(|c| c.as_str()).collect();
    assert!(ids.contains(&f.dup_a_id.as_str()), "candidate A: {ids:?}");
    assert!(ids.contains(&f.dup_b_id.as_str()), "candidate B: {ids:?}");
    // The diagnostic attributes the failing side.
    assert_eq!(
        v["endpoint"], "from",
        "an ambiguous FROM handle must carry endpoint:\"from\"; got {v}"
    );
}

#[test]
fn ambiguous_to_endpoint_exit1() {
    let f = seed();
    let stderr = egregore()
        .args([
            "query",
            "path",
            &f.entry_a_id,
            "dup_name",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stderr).expect("utf8").trim()).expect("stderr JSON");
    assert!(v["Ambiguous"]["candidates"].is_array(), "candidates: {v}");
    assert_eq!(
        v["endpoint"], "to",
        "an ambiguous TO handle must carry endpoint:\"to\"; got {v}"
    );
}

// The SAME ambiguous handle passed for BOTH endpoints must still be
// attributable: `from` is resolved first, so its diagnostic names the FROM
// side — without the `endpoint` field the two failures would be
// indistinguishable (the exact bug this pins).
#[test]
fn same_ambiguous_handle_both_endpoints_attributes_to_from() {
    let f = seed();
    let stderr = egregore()
        .args([
            "query",
            "path",
            "dup_name",
            "dup_name",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stderr).expect("utf8").trim()).expect("stderr JSON");
    assert!(v["Ambiguous"]["candidates"].is_array(), "candidates: {v}");
    assert_eq!(
        v["endpoint"], "from",
        "the first-resolved (FROM) endpoint must be named; got {v}"
    );
}

#[test]
fn record_id_disambiguates_endpoint() {
    let f = seed();
    // dup_a (by record ID) resolves cleanly and has a resolved edge to sink_b.
    let (header, hops) = run_ok(&[
        "query",
        "path",
        &f.dup_a_id,
        &f.sink_b_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(header["verdict"], "path_found");
    assert_eq!(header["hops"].as_u64(), Some(1));
    assert_eq!(hops.len(), 1);
    assert_eq!(
        hops[0]["from"]["record_id"].as_str(),
        Some(f.dup_a_id.as_str())
    );
}

// ---------------------------------------------------------------------------
// Unknown / stale endpoints — exit 2.
// ---------------------------------------------------------------------------

#[test]
fn unknown_from_endpoint_no_match_exit2() {
    let f = seed();
    let absent = format!("codegraph:v{}:{}", SCHEMA_VERSION, "b".repeat(64));
    let stdout = egregore()
        .args([
            "query",
            "path",
            &absent,
            &f.sink_b_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("json");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_match");
    assert_eq!(v["error"]["endpoint"], "from");
}

#[test]
fn unknown_to_endpoint_no_match_exit2() {
    let f = seed();
    let absent = format!("codegraph:v{}:{}", SCHEMA_VERSION, "d".repeat(64));
    let stdout = egregore()
        .args([
            "query",
            "path",
            &f.entry_a_id,
            &absent,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("json");
    assert_eq!(v["error"]["code"], "no_match");
    assert_eq!(v["error"]["endpoint"], "to");
}

#[test]
fn stale_endpoint_exit2() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "path",
            &f.tombstoned_id,
            &f.sink_b_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("json");
    assert_eq!(v["error"]["code"], "stale_handle");
}

#[test]
fn malformed_id_exit1() {
    let f = seed();
    egregore()
        .args([
            "query",
            "path",
            "codegraph:v1:zzz",
            &f.sink_b_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(1);
}

#[test]
fn file_handle_rejected_exit1() {
    let f = seed();
    let stderr = egregore()
        .args([
            "query",
            "path",
            "src/a.rs",
            &f.sink_b_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stderr).expect("utf8").trim()).expect("json");
    assert!(
        v.get("Unsupported").is_some(),
        "file handle rejected; got {v}"
    );
}

// ---------------------------------------------------------------------------
// AC3/Success metric — byte-identical output across 5 repeated runs.
// ---------------------------------------------------------------------------

#[test]
fn deterministic_output_x5() {
    let f = seed();
    let run = || -> Vec<u8> {
        egregore()
            .args([
                "query",
                "path",
                &f.entry_a_id,
                &f.sink_b_id,
                "--graph",
                f.graph.to_str().unwrap(),
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let first = run();
    for i in 2..=5 {
        assert_eq!(first, run(), "run {i} not byte-identical");
    }
}

// ---------------------------------------------------------------------------
// --format text mode.
// ---------------------------------------------------------------------------

#[test]
fn text_format_prints_chain() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "path",
            &f.entry_a_id,
            &f.sink_b_id,
            "--graph",
            f.graph.to_str().unwrap(),
            "--format",
            "text",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    assert!(out.contains("entry_a"), "text mode shows source: {out}");
    assert!(out.contains("sink_b"), "text mode shows target: {out}");
    assert!(out.contains("mid_m"), "text mode shows the mid hop: {out}");
}

#[test]
fn text_format_no_path_exit2() {
    let f = seed();
    egregore()
        .args([
            "query",
            "path",
            &f.entry_a_id,
            &f.island_id,
            "--graph",
            f.graph.to_str().unwrap(),
            "--format",
            "text",
        ])
        .assert()
        .code(2);
}

// ---------------------------------------------------------------------------
// Redaction: output never includes raw node summary payloads.
// ---------------------------------------------------------------------------

#[test]
fn output_carries_handles_not_payloads() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "path",
            &f.entry_a_id,
            &f.sink_b_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    assert!(
        !out.contains("Source file src/"),
        "node summaries (potential payload text) must not be emitted"
    );
}

// ---------------------------------------------------------------------------
// eg alias.
// ---------------------------------------------------------------------------

#[test]
fn eg_alias_works() {
    let f = seed();
    eg().args(["query", "path", &f.entry_a_id, &f.sink_b_id, "--graph"])
        .arg(&f.graph)
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Multi-repo endpoints: distinct repos, no cross-repo resolved path.
// ---------------------------------------------------------------------------

#[test]
fn multi_repo_endpoints_no_cross_repo_path_exit2() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("multi-repo.jsonl");
    let mut graph = Graph::new();

    let repo_a = stable_id(&["node", "Repository", "repo-mra"]);
    let repo_b = stable_id(&["node", "Repository", "repo-mrb"]);
    for (id, tag) in [(&repo_a, "repo-mra"), (&repo_b, "repo-mrb")] {
        graph.push(GraphRecord::node(
            id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some(tag.to_owned()),
            format!("Repository {tag}"),
        ));
    }
    file(&mut graph, &repo_a, "src/ra.rs");
    file(&mut graph, &repo_b, "src/rb.rs");
    let a_fn = symbol(&mut graph, "src/ra.rs", "a_only_fn", (5, 12));
    let b_fn = symbol(&mut graph, "src/rb.rs", "b_only_fn", (5, 12));
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    // Both endpoints resolve (unique names), but no edge crosses repos.
    let stdout = egregore()
        .args([
            "query",
            "path",
            &a_fn,
            &b_fn,
            "--graph",
            path.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let (header, _) = parse_ndjson(&stdout);
    assert_eq!(header["verdict"], "no_path");
}

// ---------------------------------------------------------------------------
// AC7 — temporal selector --at over a history graph.
// ---------------------------------------------------------------------------

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-02-01T00:00:00Z";

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

/// History fixture: at c1 `a_h` calls `b_h` (resolved); at c2 that call is
/// gone.
fn seed_history() -> (tempfile::TempDir, PathBuf, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("history.jsonl");
    let mut graph = Graph::new();

    let commit_node = |sha: &str, parents: &[&str], vt: &str| {
        GraphRecord::node(
            stable_id(&["node", "commit", "repo-ph", sha]),
            NodeKind::Commit,
            None,
            None,
            Some(sha.to_owned()),
            format!("Commit {sha}"),
        )
        .with_temporal(temporal(sha, parents, vt))
    };
    graph.push(commit_node("aaaa1111", &[], T1));
    graph.push(commit_node("bbbb2222", &["aaaa1111"], T2));

    let hist_symbol = |name: &str, commit: &str, vt: &str| -> (String, GraphRecord) {
        let id = sym_id("src/h.rs", name);
        let rec = GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Symbol,
            "src/h.rs".to_owned(),
            span(1, 10),
            name.to_owned(),
            "rust",
            format!("fn {name}"),
        )
        .with_temporal(temporal(commit, &[], vt));
        (id, rec)
    };

    let (a_id, a1) = hist_symbol("a_h", "aaaa1111", T1);
    let (b_id, b1) = hist_symbol("b_h", "aaaa1111", T1);
    graph.push(a1);
    graph.push(b1);
    let (_, a2) = hist_symbol("a_h", "bbbb2222", T2);
    let (_, b2) = hist_symbol("b_h", "bbbb2222", T2);
    graph.push(a2);
    graph.push(b2);

    let hist_call = |from: &str, to: &str, commit: &str, vt: &str| {
        GraphRecord::edge(
            EdgeLabel::Calls,
            from.to_owned(),
            to.to_owned(),
            Some("1.0".to_owned()),
            "historical call".to_owned(),
        )
        .with_resolution(CallResolution::Resolved)
        .with_temporal(temporal(commit, &[], vt))
    };
    graph.push(hist_call(&a_id, &b_id, "aaaa1111", T1));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    (temp, path, a_id, b_id)
}

#[test]
fn at_commit_returns_path_as_of_that_commit() {
    let (_t, path, a_id, b_id) = seed_history();

    let (header, hops) = run_ok(&[
        "query",
        "path",
        &a_id,
        &b_id,
        "--graph",
        path.to_str().unwrap(),
        "--at",
        "aaaa1111",
    ]);
    assert_eq!(header["at_commit"], "aaaa1111");
    assert_eq!(header["verdict"], "path_found");
    assert_eq!(header["hops"].as_u64(), Some(1));
    assert_eq!(hops.len(), 1);

    // At c2 the call is gone: no path.
    egregore()
        .args([
            "query",
            "path",
            &a_id,
            &b_id,
            "--graph",
            path.to_str().unwrap(),
            "--at",
            "bbbb2222",
        ])
        .assert()
        .code(2);
}

#[test]
fn at_on_history_free_graph_exit2() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "path",
            &f.entry_a_id,
            &f.sink_b_id,
            "--graph",
            f.graph.to_str().unwrap(),
            "--at",
            "aaaa1111",
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("json");
    assert_eq!(v["error"]["code"], "empty_history");
}

#[test]
fn at_and_as_of_conflict_fails() {
    let (_t, path, a_id, b_id) = seed_history();
    // clap rejects the two mutually-exclusive selectors (non-zero exit).
    egregore()
        .args([
            "query",
            "path",
            &a_id,
            &b_id,
            "--graph",
            path.to_str().unwrap(),
            "--at",
            "aaaa1111",
            "--as-of",
            "2026-01-15T00:00:00Z",
        ])
        .assert()
        .failure();
}

// ---------------------------------------------------------------------------
// AC1 — the same workflow answers from an ingested embedded store, and the
// --data-dir view is byte-identical to the --graph view.
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_parity_with_graph() {
    let f = seed();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&f.graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let graph_stdout = egregore()
        .args([
            "query",
            "path",
            &f.entry_a_id,
            &f.sink_b_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let store_stdout = egregore()
        .args(["query", "path", &f.entry_a_id, &f.sink_b_id, "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        graph_stdout, store_stdout,
        "graph and store views must be byte-identical"
    );
}
