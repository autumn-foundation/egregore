//! Integration tests for `eg query verification-coverage` (issue #109):
//! partition the externally-reachable public API surface into
//! verification-covered / uncovered by joining the recorded verification-domain
//! nodes over the evidence-link registry. A capability-degradation lane by
//! default (no trunk writer links verification to code), it must never flood
//! every symbol into "uncovered" and never leak raw payloads.

#![allow(
    missing_docs,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::large_stack_arrays,
    clippy::large_stack_frames,
    clippy::redundant_clone,
    clippy::needless_collect
)]

use std::{fs, path::Path};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, VERIFICATION_SCHEMA_VERSION, stable_id,
        verification_stable_id,
    },
};
use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

/// Sentinel raw text that must NEVER appear in the allow-list-only output.
const RAW_SECRET_SENTINEL: &str = "RAW_SECRET_SHOULD_NOT_LEAK";

fn repo_node(name: &str) -> (String, GraphRecord) {
    let id = stable_id(&["repository", "operator-override", name]);
    let rec = GraphRecord::node(
        id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some(name.to_owned()),
        format!("Repository {name}"),
    );
    (id, rec)
}

fn code_file(path: &str) -> (String, GraphRecord) {
    let id = stable_id(&["node", "File", path]);
    let rec = GraphRecord::syntax_node(
        id.clone(),
        NodeKind::File,
        path.to_owned(),
        span(1, 100),
        path.rsplit('/').next().unwrap_or(path).to_owned(),
        "rust",
        format!("Source file {path}"),
    );
    (id, rec)
}

fn code_symbol(path: &str, name: &str, kind: &str, line: usize) -> (String, GraphRecord) {
    let id = stable_id(&["node", "Symbol", path, name]);
    let mut rec = GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Symbol,
        path.to_owned(),
        span(line, line + 5),
        name.to_owned(),
        "rust",
        format!("Symbol {name}"),
    );
    if let GraphRecord::Node {
        visibility,
        symbol_kind,
        ..
    } = &mut rec
    {
        *visibility = Some("public".to_owned());
        *symbol_kind = Some(kind.to_owned());
    }
    (id, rec)
}

/// A `pub use` re-export `Import` node at `path`, carrying the use text in its
/// name so `public_api_surface` parses it into a re-export surface row.
fn reexport(path: &str, use_text: &str, line: usize) -> (String, GraphRecord) {
    let id = stable_id(&["node", "Import", path, use_text]);
    let rec = GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Import,
        path.to_owned(),
        span(line, line),
        use_text.to_owned(),
        "rust",
        format!("Import {use_text}"),
    );
    (id, rec)
}

/// A verification-domain node carrying a redaction sentinel in its (never
/// emitted) summary, to prove no raw payload leaks.
fn ver_node(slug: &str, kind: NodeKind, vk: Option<&str>) -> (String, GraphRecord) {
    let id = verification_stable_id(&["verification", slug]);
    let mut rec = GraphRecord::node(
        id.clone(),
        kind,
        None,
        None,
        None,
        format!("verification {slug} {RAW_SECRET_SENTINEL}"),
    );
    if let GraphRecord::Node {
        schema_version,
        verification_kind,
        status,
        ..
    } = &mut rec
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *verification_kind = vk.map(str::to_owned);
        *status = Some("passed".to_owned());
    }
    (id, rec)
}

fn ev_edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        label,
        source.to_owned(),
        target.to_owned(),
        None,
        "evidence link".to_owned(),
    )
}

fn contains(source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Contains,
        source.to_owned(),
        target.to_owned(),
        None,
        "contains".to_owned(),
    )
}

fn defines(source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Defines,
        source.to_owned(),
        target.to_owned(),
        None,
        "defines".to_owned(),
    )
}

fn observation(slug: &str, text: &str, links: Vec<EvidenceLink>) -> (String, GraphRecord) {
    let id = format!("agent_memory:v1:{}", "e".repeat(60) + slug);
    let mut rec = GraphRecord::node(
        id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        format!("Observation {slug}"),
    );
    if let GraphRecord::Node {
        schema_version,
        text: t,
        evidence_links,
        ..
    } = &mut rec
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *t = Some(text.to_owned());
        if !links.is_empty() {
            *evidence_links = Some(links);
        }
    }
    (id, rec)
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

fn write_graph(records: Vec<GraphRecord>) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("verification.graph.jsonl");
    let mut graph = Graph::new();
    for record in records {
        graph.push(record);
    }
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");
    (temp, path)
}

fn run(path: &Path, extra: &[&str]) -> Value {
    let output = egregore()
        .args(["query", "verification-coverage", "--graph"])
        .arg(path)
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("valid JSON envelope")
}

/// The full covered/uncovered partition fixture: 15 public symbols across 3
/// files, wired via MENTIONS_SYMBOL (symbol), FAILED_ON (symbol), VALIDATED_BY
/// (symbol), and TOUCHED_FILE (file-level); the remainder uncovered — including
/// one linked only by an agent-memory node (must NOT count).
fn happy_fixture() -> (tempfile::TempDir, std::path::PathBuf, String, String) {
    let (repo_id, repo) = repo_node("repo-main");
    let (fa, file_a) = code_file("src/alpha/a.rs");
    let (fb, file_b) = code_file("src/beta/b.rs");
    let (fc, file_c) = code_file("src/gamma/c.rs");

    let (a1, s_a1) = code_symbol("src/alpha/a.rs", "a1", "function", 10);
    let (a2, s_a2) = code_symbol("src/alpha/a.rs", "a2", "function", 20);
    let (a3, s_a3) = code_symbol("src/alpha/a.rs", "a3", "struct", 30);
    let (a4, s_a4) = code_symbol("src/alpha/a.rs", "a4", "function", 40);
    let (a5, s_a5) = code_symbol("src/alpha/a.rs", "a5", "function", 50);

    let mut records = vec![repo, file_a, file_b, file_c];
    let (v1, ver1) = ver_node("v1", NodeKind::Verification, Some("test_run"));
    let (v2, ver2) = ver_node("v2", NodeKind::TestRun, None);
    records.push(ver1);
    records.push(ver2);

    // file A symbols
    for (id, rec) in [
        (&a1, s_a1),
        (&a2, s_a2),
        (&a3, s_a3),
        (&a4, s_a4),
        (&a5, s_a5),
    ] {
        records.push(defines(&fa, id));
        records.push(rec);
    }
    records.push(contains(&repo_id, &fa));
    records.push(contains(&repo_id, &fb));
    records.push(contains(&repo_id, &fc));

    // file B symbols (all file-level covered via TOUCHED_FILE to file B)
    for i in 1..=5 {
        let (id, rec) = code_symbol("src/beta/b.rs", &format!("b{i}"), "function", i * 10);
        records.push(defines(&fb, &id));
        records.push(rec);
    }
    // file C symbols (all uncovered)
    for i in 1..=5 {
        let (id, rec) = code_symbol("src/gamma/c.rs", &format!("c{i}"), "function", i * 10);
        records.push(defines(&fc, &id));
        records.push(rec);
    }

    // Symbol-direct evidence from v1.
    records.push(ev_edge(EdgeLabel::MentionsSymbol, &v1, &a1));
    records.push(ev_edge(EdgeLabel::FailedOn, &v1, &a2));
    records.push(ev_edge(EdgeLabel::ValidatedBy, &v1, &a3));
    // File-level evidence from v2 to file B.
    records.push(ev_edge(EdgeLabel::TouchedFile, &v2, &fb));
    // Agent-memory link to a5 must NOT confer coverage.
    let (_obs_id, obs) = observation(
        "m1",
        "note",
        vec![link(&a5, "codegraph", "MENTIONS_SYMBOL")],
    );
    records.push(obs);

    let (temp, path) = write_graph(records);
    (temp, path, v1, v2)
}

#[test]
fn covered_uncovered_partition_is_correct_and_buckets_never_merge() {
    let (_temp, path, v1, v2) = happy_fixture();
    let report = run(&path, &[]);

    assert_eq!(report["ok"], Value::Bool(true));
    assert_eq!(report["capability"], "verification_links_recorded");
    assert_eq!(report["language"], "Rust");
    // Issue #427: this hand-built fixture carries no `source_snapshot`, so the
    // default view discloses single_snapshot.
    assert_eq!(report["corpus_mode"], "single_snapshot");
    assert_eq!(report["corpus_mode_source"], "default");

    let covered = report["covered"].as_array().unwrap();
    let uncovered = report["uncovered"].as_array().unwrap();
    // Covered: a1, a2, a3 (symbol-direct) + b1..b5 (file-level) = 8.
    assert_eq!(covered.len(), 8, "covered: {covered:#?}");
    // Uncovered: a4, a5 (agent-memory only) + c1..c5 = 7.
    assert_eq!(uncovered.len(), 7, "uncovered: {uncovered:#?}");
    assert_eq!(report["counts"]["covered"], 8);
    assert_eq!(report["counts"]["uncovered"], 7);
    assert_eq!(report["counts"]["symbols_in_scope"], 15);

    let covered_paths: Vec<&str> = covered
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(covered_paths.contains(&"a1"));
    assert!(covered_paths.contains(&"a2"));
    assert!(covered_paths.contains(&"a3"));
    assert!(covered_paths.contains(&"b1"));
    let uncovered_paths: Vec<&str> = uncovered
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(uncovered_paths.contains(&"a4"));
    assert!(
        uncovered_paths.contains(&"a5"),
        "agent-memory link must not confer coverage"
    );
    assert!(uncovered_paths.contains(&"c1"));

    // a1: MENTIONS_SYMBOL, symbol level, from v1, kind test_run.
    let a1_row = covered.iter().find(|c| c["path"] == "a1").unwrap();
    let a1_ev = &a1_row["verification"][0];
    assert_eq!(a1_ev["record_id"], Value::String(v1.clone()));
    assert_eq!(a1_ev["edge_label"], "MENTIONS_SYMBOL");
    assert_eq!(a1_ev["link_level"], "symbol");
    assert_eq!(a1_ev["verification_kind"], "test_run");

    // a2: FAILED_ON, symbol level.
    let a2_ev = &covered.iter().find(|c| c["path"] == "a2").unwrap()["verification"][0];
    assert_eq!(a2_ev["edge_label"], "FAILED_ON");
    assert_eq!(a2_ev["link_level"], "symbol");

    // b1: TOUCHED_FILE, file level, from v2, kind falls back to node kind.
    let b1_ev = &covered.iter().find(|c| c["path"] == "b1").unwrap()["verification"][0];
    assert_eq!(b1_ev["record_id"], Value::String(v2.clone()));
    assert_eq!(b1_ev["edge_label"], "TOUCHED_FILE");
    assert_eq!(b1_ev["link_level"], "file");
    assert_eq!(b1_ev["verification_kind"], "TestRun");
}

#[test]
fn zero_verification_records_is_capability_absent_not_flooded_uncovered() {
    let (repo_id, repo) = repo_node("repo-empty");
    let (fa, file_a) = code_file("src/lib.rs");
    let mut records = vec![repo, file_a, contains(&repo_id, &fa)];
    for i in 1..=4 {
        let (id, rec) = code_symbol("src/lib.rs", &format!("f{i}"), "function", i * 10);
        records.push(defines(&fa, &id));
        records.push(rec);
    }
    let (_temp, path) = write_graph(records);
    let report = run(&path, &[]);

    assert_eq!(report["capability"], "verification_facts_unavailable");
    assert_eq!(report["covered"].as_array().unwrap().len(), 0);
    assert_eq!(
        report["uncovered"].as_array().unwrap().len(),
        0,
        "must NOT flood every symbol into uncovered"
    );
    assert_eq!(report["counts"]["verification_records_in_store"], 0);
    let diags = report["diagnostics"].as_array().unwrap();
    assert!(
        diags
            .iter()
            .any(|d| d["code"] == "verification_facts_unavailable"),
        "diagnostics: {diags:#?}"
    );
}

#[test]
fn verification_present_but_unlinked_is_capability_absent() {
    let (repo_id, repo) = repo_node("repo-unlinked");
    let (fa, file_a) = code_file("src/lib.rs");
    let (id1, s1) = code_symbol("src/lib.rs", "f1", "function", 10);
    let (v1, ver1) = ver_node("v1", NodeKind::CommandRun, Some("command_run"));
    let (v2, ver2) = ver_node("v2", NodeKind::TestRun, Some("test_run"));
    // A verification->verification link exists but no verification->code link.
    let records = vec![
        repo,
        file_a,
        contains(&repo_id, &fa),
        defines(&fa, &id1),
        s1,
        ver1,
        ver2,
        ev_edge(EdgeLabel::ValidatedBy, &v1, &v2),
    ];
    let (_temp, path) = write_graph(records);
    let report = run(&path, &[]);

    assert_eq!(report["capability"], "verification_facts_unavailable");
    assert_eq!(report["counts"]["verification_records_in_store"], 2);
    assert_eq!(report["counts"]["verification_code_links_in_store"], 0);
    assert_eq!(report["covered"].as_array().unwrap().len(), 0);
    assert_eq!(report["uncovered"].as_array().unwrap().len(), 0);
    let diags = report["diagnostics"].as_array().unwrap();
    assert!(
        diags
            .iter()
            .any(|d| d["code"] == "verification_facts_unavailable"
                && d["detail"]
                    .as_str()
                    .unwrap()
                    .contains("none link to a code")),
        "diagnostics: {diags:#?}"
    );
}

#[test]
fn scope_handle_filters_by_path_name_and_id_and_unknown_exits_two() {
    let (_temp, path, _v1, _v2) = happy_fixture();

    // Path prefix, segment-aware: src/alpha matches a*, never src/alphabet.
    let report = run(&path, &["src/alpha"]);
    let all: Vec<&str> = report["covered"]
        .as_array()
        .unwrap()
        .iter()
        .chain(report["uncovered"].as_array().unwrap().iter())
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(all.len(), 5, "src/alpha holds exactly a1..a5: {all:?}");
    assert!(all.iter().all(|p| p.starts_with('a')));

    // Symbol name.
    let report = run(&path, &["a1"]);
    assert_eq!(report["counts"]["symbols_in_scope"], 1);
    assert_eq!(report["covered"][0]["path"], "a1");

    // Record id.
    let a4_id = stable_id(&["node", "Symbol", "src/alpha/a.rs", "a4"]);
    let report = run(&path, &[a4_id.as_str()]);
    assert_eq!(report["counts"]["symbols_in_scope"], 1);
    assert_eq!(report["uncovered"][0]["path"], "a4");

    // Unknown path-shaped scope -> exit 2 scope_not_found.
    let assert = egregore()
        .args(["query", "verification-coverage", "--graph"])
        .arg(&path)
        .arg("src/nonexistent")
        .assert()
        .failure()
        .code(2);
    let out: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(out["error"]["code"], "scope_not_found");

    // Unknown name-shaped scope -> exit 2 no_match.
    let assert = egregore()
        .args(["query", "verification-coverage", "--graph"])
        .arg(&path)
        .arg("does_not_exist")
        .assert()
        .failure()
        .code(2);
    let out: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(out["error"]["code"], "no_match");
}

#[test]
fn segment_aware_scope_does_not_bleed_into_sibling_path() {
    let (repo_id, repo) = repo_node("repo-seg");
    let (fa, file_a) = code_file("src/alpha/a.rs");
    let (fb, file_b) = code_file("src/alphabet/b.rs");
    let (ia, sa) = code_symbol("src/alpha/a.rs", "alpha_one", "function", 10);
    let (ib, sb) = code_symbol("src/alphabet/b.rs", "beta_one", "function", 10);
    let (v1, ver1) = ver_node("v1", NodeKind::Verification, Some("test_run"));
    let records = vec![
        repo,
        file_a,
        file_b,
        contains(&repo_id, &fa),
        contains(&repo_id, &fb),
        defines(&fa, &ia),
        defines(&fb, &ib),
        sa,
        sb,
        ver1,
        ev_edge(EdgeLabel::MentionsSymbol, &v1, &ia),
    ];
    let (_temp, path) = write_graph(records);
    let report = run(&path, &["src/alpha"]);
    let all: Vec<&str> = report["covered"]
        .as_array()
        .unwrap()
        .iter()
        .chain(report["uncovered"].as_array().unwrap().iter())
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        all,
        vec!["alpha_one"],
        "src/alpha must not match src/alphabet"
    );
}

#[test]
fn limit_truncates_each_bucket_independently_without_dropping_a_bucket() {
    let (_temp, path, _v1, _v2) = happy_fixture();
    // 8 covered, 7 uncovered; limit 2 truncates both, drops neither.
    let report = run(&path, &["--limit", "2"]);
    assert_eq!(report["covered"].as_array().unwrap().len(), 2);
    assert_eq!(report["uncovered"].as_array().unwrap().len(), 2);
    assert_eq!(report["counts"]["covered_truncated"], Value::Bool(true));
    assert_eq!(report["counts"]["uncovered_truncated"], Value::Bool(true));
    // Pre-truncation totals preserved in counts.
    assert_eq!(report["counts"]["covered"], 8);
    assert_eq!(report["counts"]["uncovered"], 7);
    let diags = report["diagnostics"].as_array().unwrap();
    let trunc: Vec<&Value> = diags
        .iter()
        .filter(|d| d["code"] == "results_truncated")
        .collect();
    assert_eq!(trunc.len(), 2, "one per truncated bucket: {diags:#?}");

    // Out-of-range limit -> exit 1.
    egregore()
        .args(["query", "verification-coverage", "--graph"])
        .arg(&path)
        .args(["--limit", "0"])
        .assert()
        .failure()
        .code(1);
    egregore()
        .args(["query", "verification-coverage", "--graph"])
        .arg(&path)
        .args(["--limit", "1001"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn output_is_deterministic_and_byte_identical_across_runs() {
    let (_temp, path, _v1, _v2) = happy_fixture();
    let first = egregore()
        .args(["query", "verification-coverage", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    for _ in 0..4 {
        let again = egregore()
            .args(["query", "verification-coverage", "--graph"])
            .arg(&path)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(first, again, "output must be byte-identical across runs");
    }
}

#[test]
fn no_raw_payload_text_leaks_into_output() {
    let (_temp, path, _v1, _v2) = happy_fixture();
    let output = egregore()
        .args(["query", "verification-coverage", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(
        !text.contains(RAW_SECRET_SENTINEL),
        "raw verification summary text must never leak"
    );
}

#[test]
fn repo_scoping_only_counts_links_within_the_scoped_repo() {
    let (repo_a, repo_a_rec) = repo_node("repo-alpha");
    let (repo_b, repo_b_rec) = repo_node("repo-beta");
    let (fa, file_a) = code_file("src/lib.rs");
    let (fb, file_b) = code_file("src/lib.rs"); // same path, different repo
    // Distinct ids for the two files despite the same path.
    let fa = format!("{fa}-a");
    let fb = format!("{fb}-b");
    let mut file_a_rec = file_a;
    if let GraphRecord::Node { id, .. } = &mut file_a_rec {
        *id = fa.clone();
    }
    let mut file_b_rec = file_b;
    if let GraphRecord::Node { id, .. } = &mut file_b_rec {
        *id = fb.clone();
    }
    let (sa, s_a) = code_symbol("src/lib.rs", "alpha_sym", "function", 10);
    let sa = format!("{sa}-a");
    let mut s_a_rec = s_a;
    if let GraphRecord::Node { id, .. } = &mut s_a_rec {
        *id = sa.clone();
    }
    let (sb, s_b) = code_symbol("src/lib.rs", "beta_sym", "function", 20);
    let sb = format!("{sb}-b");
    let mut s_b_rec = s_b;
    if let GraphRecord::Node { id, .. } = &mut s_b_rec {
        *id = sb.clone();
    }
    let (va, ver_a) = ver_node("va", NodeKind::Verification, Some("test_run"));
    let (vb, ver_b) = ver_node("vb", NodeKind::Verification, Some("ci_status"));

    let records = vec![
        repo_a_rec,
        repo_b_rec,
        file_a_rec,
        file_b_rec,
        s_a_rec,
        s_b_rec,
        ver_a,
        ver_b,
        contains(&repo_a, &fa),
        contains(&repo_b, &fb),
        defines(&fa, &sa),
        defines(&fb, &sb),
        // Attribute each verification node to its repository.
        contains(&repo_a, &va),
        contains(&repo_b, &vb),
        // Same-repo link (must count under repo-alpha).
        ev_edge(EdgeLabel::MentionsSymbol, &va, &sa),
        // Cross-repo link from repo-beta's verification to repo-alpha's symbol
        // (must NOT count under repo-alpha).
        ev_edge(EdgeLabel::MentionsSymbol, &vb, &sa),
    ];
    let (_temp, path) = write_graph(records);
    let report = run(&path, &["--repo", "repo-alpha"]);

    // Only repo-alpha's symbol is in scope; it is covered only by va.
    assert_eq!(report["counts"]["symbols_in_scope"], 1);
    let covered = report["covered"].as_array().unwrap();
    assert_eq!(covered.len(), 1);
    assert_eq!(covered[0]["path"], "alpha_sym");
    let evidence = covered[0]["verification"].as_array().unwrap();
    assert_eq!(
        evidence.len(),
        1,
        "cross-repo verification link must not count: {evidence:#?}"
    );
    assert_eq!(evidence[0]["record_id"], Value::String(va));
    assert_ne!(evidence[0]["record_id"], Value::String(vb));
}

#[test]
fn tombstoned_records_are_excluded_from_surface_and_coverage() {
    let (repo_id, repo) = repo_node("repo-tomb");
    let (fa, file_a) = code_file("src/lib.rs");
    let (s1, s_1) = code_symbol("src/lib.rs", "s1", "function", 10);
    let (s2, s_2) = code_symbol("src/lib.rs", "s2", "function", 20);
    let (s3, s_3) = code_symbol("src/lib.rs", "s3", "function", 30);
    let (v1, ver1) = ver_node("v1", NodeKind::Verification, Some("test_run"));
    let (v2, ver2) = ver_node("v2", NodeKind::Verification, Some("test_run"));

    let records = vec![
        repo,
        file_a,
        contains(&repo_id, &fa),
        defines(&fa, &s1),
        defines(&fa, &s2),
        defines(&fa, &s3),
        s_1,
        s_2,
        s_3,
        ver1,
        ver2,
        // v1 links s1, but v1 is tombstoned -> confers no coverage.
        ev_edge(EdgeLabel::MentionsSymbol, &v1, &s1),
        // v2 links s3 and is live -> s3 covered (keeps capability present).
        ev_edge(EdgeLabel::MentionsSymbol, &v2, &s3),
        GraphRecord::Tombstone {
            id: format!("{v1}-tomb"),
            schema_version: VERIFICATION_SCHEMA_VERSION,
            deleted_id: v1.clone(),
            summary: "retract v1".to_owned(),
            producer: None,
        },
        // s2 tombstoned -> absent from surface entirely.
        GraphRecord::Tombstone {
            id: format!("{s2}-tomb"),
            schema_version: VERIFICATION_SCHEMA_VERSION,
            deleted_id: s2.clone(),
            summary: "retract s2".to_owned(),
            producer: None,
        },
    ];
    let (_temp, path) = write_graph(records);
    let report = run(&path, &[]);

    assert_eq!(report["capability"], "verification_links_recorded");
    let all: Vec<&str> = report["covered"]
        .as_array()
        .unwrap()
        .iter()
        .chain(report["uncovered"].as_array().unwrap().iter())
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(!all.contains(&"s2"), "tombstoned symbol must be absent");
    assert_eq!(report["counts"]["symbols_in_scope"], 2);
    // s1 is uncovered because its only verification link is tombstoned.
    let uncovered: Vec<&str> = report["uncovered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(
        uncovered.contains(&"s1"),
        "tombstoned verification confers no coverage: {uncovered:?}"
    );
    let covered: Vec<&str> = report["covered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(covered, vec!["s3"]);
}

#[test]
fn at_commit_pins_the_surface_snapshot() {
    let commit_a = "a".repeat(40);
    let commit_b = "b".repeat(40);
    let (repo_id, repo) = repo_node("repo-history");
    let (fa, file_a) = code_file("src/lib.rs");
    let (alpha, mut s_alpha) = code_symbol("src/lib.rs", "alpha", "function", 10);
    let (beta, mut s_beta) = code_symbol("src/lib.rs", "beta", "function", 20);
    let temporal = |commit: &str| TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    };
    if let GraphRecord::Node { temporal: t, .. } = &mut s_alpha {
        *t = Some(temporal(&commit_a));
    }
    if let GraphRecord::Node { temporal: t, .. } = &mut s_beta {
        *t = Some(temporal(&commit_b));
    }
    let (v1, ver1) = ver_node("v1", NodeKind::Verification, Some("test_run"));
    let records = vec![
        repo,
        file_a,
        contains(&repo_id, &fa),
        defines(&fa, &alpha),
        defines(&fa, &beta),
        s_alpha,
        s_beta,
        ver1,
        ev_edge(EdgeLabel::MentionsSymbol, &v1, &alpha),
    ];
    let (_temp, path) = write_graph(records);

    // Full view: both symbols present.
    let report = run(&path, &[]);
    assert_eq!(report["counts"]["symbols_in_scope"], 2);

    // Pinned to commit A: only alpha present, and it is covered.
    let report = run(&path, &["--at", &commit_a[..8]]);
    assert_eq!(report["counts"]["symbols_in_scope"], 1);
    assert_eq!(report["covered"][0]["path"], "alpha");
    // Issue #427: a `--at` pin discloses the commit-pinned corpus.
    assert_eq!(report["corpus_mode"], "commit_pinned");
    assert_eq!(report["corpus_mode_source"], "selector");
    let all: Vec<&str> = report["covered"]
        .as_array()
        .unwrap()
        .iter()
        .chain(report["uncovered"].as_array().unwrap().iter())
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(!all.contains(&"beta"), "commit-A snapshot excludes beta");
}

#[test]
fn reexport_is_covered_when_its_target_declaration_is_verified() {
    // A symbol exposed ONLY through `pub use crate::internal::Widget`: the
    // public surface reports it as a re-export row keyed on the `Import` site,
    // while the declaration lives at `target_record_id`. A verifier links to
    // the DECLARATION, so crediting the target is required (issue #109 P2).
    let (repo_id, repo) = repo_node("repo-reexport");
    let (lib, file_lib) = code_file("src/lib.rs");
    let (internal, file_internal) = code_file("src/internal.rs");
    // Declaration trapped in module `internal` (no recorded module visibility),
    // so it is NOT its own surface row — reachable only via the re-export.
    let (widget, s_widget) = code_symbol("src/internal.rs", "internal::Widget", "struct", 10);
    let (imp, imp_rec) = reexport("src/lib.rs", "pub use crate::internal::Widget", 3);
    let (v1, ver1) = ver_node("v1", NodeKind::Verification, Some("test_run"));

    let records = vec![
        repo,
        file_lib,
        file_internal,
        contains(&repo_id, &lib),
        contains(&repo_id, &internal),
        defines(&internal, &widget),
        s_widget,
        imp_rec,
        ver1,
        // Verification links the DECLARATION (target), not the re-export site.
        ev_edge(EdgeLabel::MentionsSymbol, &v1, &widget),
    ];
    let (_temp, path) = write_graph(records);
    let report = run(&path, &[]);

    assert_eq!(report["capability"], "verification_links_recorded");
    let covered = report["covered"].as_array().unwrap();
    let uncovered = report["uncovered"].as_array().unwrap();
    // The re-export row must be COVERED via its verified target declaration.
    assert_eq!(report["counts"]["symbols_in_scope"], 1);
    assert_eq!(covered.len(), 1, "covered: {covered:#?}");
    assert_eq!(uncovered.len(), 0, "uncovered: {uncovered:#?}");
    let row = &covered[0];
    assert_eq!(row["path"], "Widget");
    assert_eq!(row["record_id"], Value::String(imp.clone()));
    let ev = &row["verification"][0];
    assert_eq!(ev["record_id"], Value::String(v1.clone()));
    assert_eq!(ev["link_level"], "symbol");
    assert_eq!(ev["edge_label"], "MENTIONS_SYMBOL");
}

#[test]
fn reexport_stays_uncovered_when_neither_site_nor_target_is_verified() {
    // Same re-export shape, but no verification link on either the `Import`
    // site or the target declaration: it must stay UNCOVERED. A separate
    // verified symbol keeps the capability present so the lane partitions
    // rather than degrading to capability-absent.
    let (repo_id, repo) = repo_node("repo-reexport-neg");
    let (lib, file_lib) = code_file("src/lib.rs");
    let (internal, file_internal) = code_file("src/internal.rs");
    let (widget, s_widget) = code_symbol("src/internal.rs", "internal::Widget", "struct", 10);
    let (_imp, imp_rec) = reexport("src/lib.rs", "pub use crate::internal::Widget", 3);
    // A separately-declared public symbol carrying the only verification link,
    // keeping capability present without touching the re-export or its target.
    let (linked, s_linked) = code_symbol("src/lib.rs", "keeper", "function", 20);
    let (v1, ver1) = ver_node("v1", NodeKind::Verification, Some("test_run"));

    let records = vec![
        repo,
        file_lib,
        file_internal,
        contains(&repo_id, &lib),
        contains(&repo_id, &internal),
        defines(&internal, &widget),
        defines(&lib, &linked),
        s_widget,
        s_linked,
        imp_rec,
        ver1,
        ev_edge(EdgeLabel::MentionsSymbol, &v1, &linked),
    ];
    let (_temp, path) = write_graph(records);
    let report = run(&path, &[]);

    assert_eq!(report["capability"], "verification_links_recorded");
    let uncovered: Vec<&str> = report["uncovered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    let covered: Vec<&str> = report["covered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(
        uncovered.contains(&"Widget"),
        "unverified re-export must be uncovered: {uncovered:?}"
    );
    assert!(
        !covered.contains(&"Widget"),
        "unverified re-export must not be covered: {covered:?}"
    );
    assert!(covered.contains(&"keeper"));
}
