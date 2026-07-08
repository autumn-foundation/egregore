//! Integration tests for `eg query unreferenced` (issue #113): flag symbols
//! with no recorded inbound reference edges as prune-triage candidates —
//! leads, never proof of dead code.

#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::{
    GraphRecord, NodeKind, TemporalMetadata, ir::EdgeLabel, scan_repository_at_with_override,
    scan_repository_history_with_override, stable_id,
};
use assert_cmd::Command;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

// ---------------------------------------------------------------------------
// Fixture: hand-labeled referenced vs zero-inbound-reference symbols
// ---------------------------------------------------------------------------

/// Crate root. Labels:
/// - referenced: `helper` (called by `entry` and `make`), `UsedType`
///   (referenced by `make`), `shared_fn` (called cross-file from
///   `other::cross_user`).
/// - zero inbound references: `entry`, `orphan`, `make`, `OrphanType`.
const LIB_RS: &str = r#"mod macros;
mod other;

pub fn entry() -> usize {
    helper() + 1
}

fn helper() -> usize {
    2
}

fn orphan() -> &'static str {
    "SECRET_BODY_MARKER_113"
}

pub struct UsedType {
    pub value: usize,
}

pub fn make() -> UsedType {
    UsedType { value: helper() }
}

struct OrphanType;

pub fn shared_fn() -> usize {
    4
}
"#;

/// Cross-file caller: `shared_fn` is defined in `lib.rs`, so the resolved
/// cross-file CALLS edge (issue #152) must keep it out of the candidate set.
/// `cross_user` itself has no inbound references.
const OTHER_RS: &str = r"pub fn cross_user() -> usize {
    shared_fn()
}
";

/// File whose scope contains extractor `Diagnostic` markers: an unsupported
/// macro invocation and an unresolved call target. `macro_neighbor` is a
/// zero-inbound-reference candidate whose confidence must be lowered by an
/// extraction-completeness caveat.
const MACROS_RS: &str = r"pub fn macro_neighbor() -> usize {
    missing_helper_fn()
}

totally_unknown_macro!(macro_payload);
";

fn write_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("src dir");
    fs::write(dir.join("src/lib.rs"), LIB_RS).expect("lib.rs");
    fs::write(dir.join("src/other.rs"), OTHER_RS).expect("other.rs");
    fs::write(dir.join("src/macros.rs"), MACROS_RS).expect("macros.rs");
}

/// Scans the labeled fixture and writes the JSONL graph. Returns
/// (`TempDir`, graph path). Caller must keep the `TempDir` alive.
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("unref-fixture"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_unreferenced(graph: &Path) -> Value {
    let output = egregore()
        .args(["query", "unreferenced", "--graph"])
        .arg(graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON")
}

fn candidate_names(parsed: &Value) -> Vec<String> {
    parsed["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .map(|c| c["name"].as_str().expect("name").to_owned())
        .collect()
}

fn candidate<'a>(parsed: &'a Value, name: &str) -> &'a Value {
    parsed["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("missing unreferenced candidate named {name}"))
}

fn diagnostic_codes(parsed: &Value) -> Vec<String> {
    parsed["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .map(|d| d["code"].as_str().expect("code").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// AC1 + success metric: exactly the labeled zero-inbound set, nothing else
// ---------------------------------------------------------------------------

#[test]
fn unreferenced_returns_exactly_the_labeled_zero_inbound_set() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_unreferenced(&graph);

    assert_eq!(parsed["ok"], true, "ok must be true on success");
    // Sorted by (repo_relative_path, span.start_line, record_id): lib.rs
    // candidates in line order, then macros.rs, then other.rs.
    assert_eq!(
        candidate_names(&parsed),
        vec![
            "entry",
            "orphan",
            "make",
            "OrphanType",
            "macros::macro_neighbor",
            "other::cross_user",
        ],
        "recall and precision must both be 100% against the label set"
    );
}

#[test]
fn referenced_symbols_are_never_misreported_as_candidates() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_unreferenced(&graph);
    let names = candidate_names(&parsed);

    for referenced in ["helper", "UsedType", "shared_fn"] {
        assert!(
            !names.contains(&referenced.to_owned()),
            "{referenced} carries a recorded inbound reference and must not be a candidate"
        );
    }
}

// ---------------------------------------------------------------------------
// AC3: every candidate carries citable handles and the selecting count
// ---------------------------------------------------------------------------

#[test]
fn candidate_rows_carry_citation_fields_and_zero_inbound_count() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_unreferenced(&graph);

    let candidates = parsed["candidates"].as_array().expect("candidates array");
    assert!(!candidates.is_empty(), "fixture must yield candidates");
    for row in candidates {
        assert!(row["record_id"].as_str().is_some(), "row needs record_id");
        assert!(
            row["schema_version"].as_u64().is_some(),
            "row needs schema_version"
        );
        assert!(row["name"].as_str().is_some(), "row needs name");
        assert!(row["kind"].as_str().is_some(), "row needs kind");
        assert!(
            row["repo_relative_path"]
                .as_str()
                .is_some_and(|p| p.starts_with("src/")),
            "row needs a repo-relative path"
        );
        assert!(
            row["span"]["start_line"].as_u64().is_some(),
            "row needs a span"
        );
        assert_eq!(
            row["inbound_reference_count"], 0,
            "the count that selected the candidate must be present and zero"
        );
    }
    assert_eq!(candidate(&parsed, "entry")["kind"], "function");
    assert_eq!(candidate(&parsed, "OrphanType")["kind"], "struct");
}

// ---------------------------------------------------------------------------
// AC4: leads, not proof — the response says so
// ---------------------------------------------------------------------------

#[test]
fn response_labels_candidates_as_leads_not_proof() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_unreferenced(&graph);

    let disclaimer = parsed["disclaimer"].as_str().expect("disclaimer");
    assert!(
        disclaimer.contains("not proof"),
        "disclaimer must state that no recorded inbound reference is not proof of dead code"
    );
    let classes: Vec<&str> = parsed["reference_edge_classes"]
        .as_array()
        .expect("reference_edge_classes")
        .iter()
        .map(|c| c.as_str().expect("class"))
        .collect();
    for required in ["CALLS", "IMPORTS", "MENTIONS"] {
        assert!(
            classes.contains(&required),
            "documented reference class {required} must be listed"
        );
    }
}

// ---------------------------------------------------------------------------
// AC5: extraction-completeness caveat for Diagnostic-marked file scopes
// ---------------------------------------------------------------------------

#[test]
fn extraction_caveat_present_exactly_for_diagnostic_marked_file_scopes() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_unreferenced(&graph);

    let flagged = candidate(&parsed, "macros::macro_neighbor");
    let caveat = &flagged["extraction_caveat"];
    assert_eq!(
        caveat["code"], "diagnostics_in_file_scope",
        "candidate in a Diagnostic-marked file must carry the caveat"
    );
    assert!(
        caveat["diagnostic_count"].as_u64().is_some_and(|n| n >= 1),
        "caveat must count the markers in scope"
    );
    assert!(
        caveat["diagnostic_record_ids"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty()),
        "caveat must cite the Diagnostic records"
    );

    // The caveat is advisory: the candidate row itself is unchanged.
    assert_eq!(flagged["inbound_reference_count"], 0);

    // Candidates in clean file scopes carry no caveat.
    for clean in ["entry", "orphan", "other::cross_user"] {
        assert!(
            candidate(&parsed, clean)["extraction_caveat"].is_null(),
            "{clean} sits in a Diagnostic-free file and must carry no caveat"
        );
    }
}

#[test]
fn unresolved_call_edges_surface_a_store_level_diagnostic() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_unreferenced(&graph);

    assert!(
        diagnostic_codes(&parsed).contains(&"unresolved_call_edges_present".to_owned()),
        "the fixture's unresolved call must yield the honesty diagnostic"
    );
}

// ---------------------------------------------------------------------------
// AC6: tombstoned symbols are excluded from current-state results
// ---------------------------------------------------------------------------

#[test]
fn tombstoned_symbols_are_excluded_from_candidates() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_unreferenced(&graph);
    let orphan_id = candidate(&parsed, "orphan")["record_id"]
        .as_str()
        .expect("record_id")
        .to_owned();

    let tombstone = format!(
        "{{\"record_type\":\"tombstone\",\"id\":\"tombstone:test:1\",\"schema_version\":1,\
         \"deleted_id\":\"{orphan_id}\",\"summary\":\"orphan deleted\"}}\n"
    );
    let mut jsonl = fs::read_to_string(&graph).expect("read graph");
    jsonl.push_str(&tombstone);
    fs::write(&graph, jsonl).expect("append tombstone");

    let parsed = run_unreferenced(&graph);
    assert!(
        !candidate_names(&parsed).contains(&"orphan".to_owned()),
        "a tombstoned symbol must not be reported as a prune candidate"
    );
}

// ---------------------------------------------------------------------------
// AC7: empty candidate set is a distinct documented signal, not an error
// ---------------------------------------------------------------------------

#[test]
fn fully_referenced_fixture_reports_no_candidates_success() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn ping() -> usize {\n    pong()\n}\n\npub fn pong() -> usize {\n    ping()\n}\n",
    )
    .expect("lib.rs");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("unref-full"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    let parsed = run_unreferenced(&graph);
    assert_eq!(parsed["ok"], true, "empty candidate set is a success");
    assert_eq!(
        parsed["candidates"].as_array().expect("candidates").len(),
        0,
        "every symbol is referenced"
    );
    let codes = diagnostic_codes(&parsed);
    assert!(
        codes.contains(&"no_candidates".to_owned()),
        "empty candidate set must carry the distinct no_candidates signal"
    );
    assert!(
        !codes.contains(&"no_symbols".to_owned()),
        "no_candidates must not be conflated with a symbol-free store"
    );
}

#[test]
fn symbol_free_graph_reports_no_symbols_distinctly() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("README.md"), "no code here\n").expect("readme");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("unref-empty"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    let parsed = run_unreferenced(&graph);
    assert_eq!(parsed["ok"], true);
    assert_eq!(
        parsed["candidates"].as_array().expect("candidates").len(),
        0
    );
    let codes = diagnostic_codes(&parsed);
    assert!(
        codes.contains(&"no_symbols".to_owned()),
        "a symbol-free store must be reported distinctly"
    );
    assert!(
        !codes.contains(&"no_candidates".to_owned()),
        "a symbol-free store is not an everything-is-referenced answer"
    );
}

// ---------------------------------------------------------------------------
// AC8: strictly read-only and byte-identical across 5 consecutive runs
// ---------------------------------------------------------------------------

fn dir_snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                entries.push((
                    path.to_string_lossy().into_owned(),
                    fs::read(&path).expect("read file"),
                ));
            }
        }
    }
    entries.sort();
    entries
}

#[test]
fn query_is_read_only_and_byte_identical_across_five_runs() {
    let (temp, graph) = fixture_graph();
    let before = dir_snapshot(temp.path());

    let mut outputs: Vec<Vec<u8>> = Vec::new();
    for _ in 0..5 {
        let output = egregore()
            .args(["query", "unreferenced", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        outputs.push(output);
    }
    for run in &outputs[1..] {
        assert_eq!(
            run, &outputs[0],
            "output must be byte-identical across consecutive runs"
        );
    }

    let after = dir_snapshot(temp.path());
    assert_eq!(
        before, after,
        "the query must create, modify, and delete nothing"
    );
}

#[test]
fn candidates_are_sorted_by_path_then_start_line_then_record_id() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_unreferenced(&graph);

    let keys: Vec<(String, u64, String)> = parsed["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .map(|c| {
            (
                c["repo_relative_path"].as_str().expect("path").to_owned(),
                c["span"]["start_line"].as_u64().expect("start_line"),
                c["record_id"].as_str().expect("record_id").to_owned(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "documented candidate ordering must hold");
}

// ---------------------------------------------------------------------------
// AC9: redaction safety — never raw source text
// ---------------------------------------------------------------------------

#[test]
fn output_never_includes_raw_source_text() {
    let (_temp, graph) = fixture_graph();
    let output = egregore()
        .args(["query", "unreferenced", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(
        !stdout.contains("SECRET_BODY_MARKER_113"),
        "raw source text must never appear in the response"
    );
    assert!(
        !stdout.contains("Source:"),
        "record summaries carry source text and must never be emitted"
    );
}

// ---------------------------------------------------------------------------
// Temporal records carry git_commit; keep-last dedupe on history graphs
// ---------------------------------------------------------------------------

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

fn history_symbol(name: &str, path: &str, commit: &str, valid_time: &str) -> GraphRecord {
    let id = stable_id(&["node", "symbol", "unref-history", path, name]);
    GraphRecord::node(
        id,
        NodeKind::Symbol,
        Some(path.to_owned()),
        None,
        Some(name.to_owned()),
        format!("Symbol {name} in {path}"),
    )
    .with_temporal(temporal(commit, valid_time))
}

#[test]
fn temporal_candidates_carry_git_commit_and_dedupe_keeps_last() {
    let older = history_symbol("lonely", "src/lib.rs", "c1sha0000", "2026-01-01T00:00:00Z");
    let newer = history_symbol("lonely", "src/lib.rs", "c2sha0000", "2026-01-02T00:00:00Z");
    let used = history_symbol("busy", "src/lib.rs", "c2sha0000", "2026-01-02T00:00:00Z");
    let caller = history_symbol("driver", "src/main.rs", "c2sha0000", "2026-01-02T00:00:00Z");
    let call_edge = GraphRecord::edge(
        EdgeLabel::Calls,
        caller.id().to_owned(),
        used.id().to_owned(),
        Some("1.0".to_owned()),
        "driver calls busy".to_owned(),
    );

    let mut jsonl = String::new();
    for record in [&older, &newer, &used, &caller, &call_edge] {
        jsonl.push_str(&serde_json::to_string(record).expect("serialize record"));
        jsonl.push('\n');
    }
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("history.graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    let parsed = run_unreferenced(&graph);
    let names = candidate_names(&parsed);
    assert!(
        names.contains(&"lonely".to_owned()),
        "zero-inbound temporal symbol must be a candidate"
    );
    assert!(
        !names.contains(&"busy".to_owned()),
        "called temporal symbol must not be a candidate"
    );
    assert_eq!(
        candidate(&parsed, "lonely")["git_commit"],
        "c2sha0000",
        "temporal candidates must carry the latest record's git_commit"
    );
    let lonely_rows = candidate_names(&parsed)
        .iter()
        .filter(|n| n.as_str() == "lonely")
        .count();
    assert_eq!(lonely_rows, 1, "keep-last dedupe must yield one row per ID");
}

// ---------------------------------------------------------------------------
// Repository scoping (issue #67)
// ---------------------------------------------------------------------------

#[test]
fn repo_scope_limits_candidates_to_one_repository() {
    let temp_a = tempfile::tempdir().expect("temp dir a");
    fs::create_dir_all(temp_a.path().join("src")).expect("src dir");
    fs::write(temp_a.path().join("src/lib.rs"), "fn alpha_only() {}\n").expect("lib.rs");
    let temp_b = tempfile::tempdir().expect("temp dir b");
    fs::create_dir_all(temp_b.path().join("src")).expect("src dir");
    fs::write(temp_b.path().join("src/lib.rs"), "fn beta_only() {}\n").expect("lib.rs");

    let jsonl_a = scan_repository_at_with_override(temp_a.path(), FIXED_TIME, Some("repo-alpha"))
        .expect("scan a")
        .to_jsonl()
        .expect("serialize a");
    let jsonl_b = scan_repository_at_with_override(temp_b.path(), FIXED_TIME, Some("repo-beta"))
        .expect("scan b")
        .to_jsonl()
        .expect("serialize b");
    let graph = temp_a.path().join("merged.graph.jsonl");
    fs::write(&graph, format!("{jsonl_a}{jsonl_b}")).expect("write merged graph");

    let output = egregore()
        .args(["query", "unreferenced", "--repo", "repo-alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value = serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON");
    let names = candidate_names(&parsed);
    assert!(names.contains(&"alpha_only".to_owned()));
    assert!(
        !names.contains(&"beta_only".to_owned()),
        "--repo must exclude the other repository's symbols"
    );
}

#[test]
fn unknown_repo_selector_is_rejected_with_exit_1() {
    let (_temp, graph) = fixture_graph();
    egregore()
        .args(["query", "unreferenced", "--repo", "nope", "--graph"])
        .arg(&graph)
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("unknown_repository_selector"));
}

// ---------------------------------------------------------------------------
// History graphs: only edges valid at the stamped HEAD commit count
// ---------------------------------------------------------------------------

fn run_git<const N: usize>(repo: &Path, args: [&str; N]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("git command should execute");
    assert!(
        output.status.success(),
        "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_commit_all(repo: &Path, message: &str, date: &str) -> String {
    run_git(repo, ["add", "."]);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        output.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let sha = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse should execute");
    String::from_utf8_lossy(&sha.stdout).trim().to_owned()
}

/// Two-commit history: at c1 `caller` calls both `stale_called` and
/// `kept_called`, and `gone_fn` exists; at HEAD (c2) the `stale_called` call
/// and `gone_fn` are removed. Only HEAD-valid edges and symbols may shape the
/// current-state answer.
#[test]
fn history_graph_ignores_stale_edges_and_symbols_from_older_commits() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path();
    run_git(repo, ["init"]);
    run_git(repo, ["config", "user.email", "unref@example.invalid"]);
    run_git(repo, ["config", "user.name", "Unref Test"]);
    run_git(repo, ["config", "core.autocrlf", "false"]);
    run_git(repo, ["config", "commit.gpgsign", "false"]);

    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn caller() -> usize {\n    stale_called() + kept_called()\n}\n\n\
         pub fn stale_called() -> usize {\n    1\n}\n\n\
         pub fn kept_called() -> usize {\n    2\n}\n\n\
         pub fn gone_fn() -> usize {\n    3\n}\n",
    )
    .expect("lib.rs at c1");
    git_commit_all(repo, "c1: call both", "2026-01-01T00:00:00Z");

    fs::write(
        repo.join("src/lib.rs"),
        "pub fn caller() -> usize {\n    kept_called()\n}\n\n\
         pub fn stale_called() -> usize {\n    1\n}\n\n\
         pub fn kept_called() -> usize {\n    2\n}\n",
    )
    .expect("lib.rs at c2");
    let head_sha = git_commit_all(
        repo,
        "c2: drop stale call and gone_fn",
        "2026-01-02T00:00:00Z",
    );

    let jsonl = scan_repository_history_with_override(repo, Some("unref-history"))
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let graph_dir = tempfile::tempdir().expect("graph dir");
    let graph = graph_dir.path().join("history.graph.jsonl");
    fs::write(&graph, jsonl).expect("write history graph");

    let parsed = run_unreferenced(&graph);
    let names = candidate_names(&parsed);
    assert!(
        names.contains(&"stale_called".to_owned()),
        "a call edge removed before HEAD must not keep its target out of the \
         candidate set; got {names:?}"
    );
    assert!(
        !names.contains(&"kept_called".to_owned()),
        "a call edge still present at HEAD must keep its target referenced"
    );
    assert!(
        !names.contains(&"gone_fn".to_owned()),
        "a symbol absent at HEAD is deleted and must not be a current-state candidate"
    );
    assert_eq!(
        candidate(&parsed, "stale_called")["git_commit"],
        Value::String(head_sha),
        "history candidates must cite the HEAD-commit record"
    );
}

// ---------------------------------------------------------------------------
// --repo scoping must not drop extraction Diagnostic markers (issue #87)
// ---------------------------------------------------------------------------

#[test]
fn repo_scoped_run_keeps_extraction_caveats_for_diagnostic_marked_files() {
    let (_temp, graph) = fixture_graph();
    let output = egregore()
        .args([
            "query",
            "unreferenced",
            "--repo",
            "unref-fixture",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value = serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON");

    let flagged = candidate(&parsed, "macros::macro_neighbor");
    assert_eq!(
        flagged["extraction_caveat"]["code"], "diagnostics_in_file_scope",
        "repo-scoped runs must keep the documented extraction caveat"
    );
    assert!(
        parsed["counts"]["files_with_diagnostic_markers"]
            .as_u64()
            .is_some_and(|n| n >= 1),
        "Diagnostic-marked files must stay tallied under --repo"
    );
}

// ---------------------------------------------------------------------------
// --repo scoping must not import another repository's Diagnostic caveats
// ---------------------------------------------------------------------------

/// Both repositories record `src/lib.rs`; only `repo-dirty`'s copy contains an
/// extraction Diagnostic. A run scoped to the clean repository must not
/// caveat its candidates off the other repository's marker, and a run scoped
/// to the dirty repository must keep its own caveat.
#[test]
fn repo_scope_excludes_other_repositories_diagnostic_caveats_on_shared_paths() {
    let temp_clean = tempfile::tempdir().expect("temp dir clean");
    fs::create_dir_all(temp_clean.path().join("src")).expect("src dir");
    fs::write(
        temp_clean.path().join("src/lib.rs"),
        "fn clean_orphan() {}\n",
    )
    .expect("clean lib.rs");
    let temp_dirty = tempfile::tempdir().expect("temp dir dirty");
    fs::create_dir_all(temp_dirty.path().join("src")).expect("src dir");
    fs::write(
        temp_dirty.path().join("src/lib.rs"),
        "fn dirty_orphan() {}\n\ntotally_unknown_macro!(marker);\n",
    )
    .expect("dirty lib.rs");

    let jsonl_clean =
        scan_repository_at_with_override(temp_clean.path(), FIXED_TIME, Some("repo-clean"))
            .expect("scan clean")
            .to_jsonl()
            .expect("serialize clean");
    let jsonl_dirty =
        scan_repository_at_with_override(temp_dirty.path(), FIXED_TIME, Some("repo-dirty"))
            .expect("scan dirty")
            .to_jsonl()
            .expect("serialize dirty");
    let graph = temp_clean.path().join("merged.graph.jsonl");
    fs::write(&graph, format!("{jsonl_clean}{jsonl_dirty}")).expect("write merged graph");

    let scoped = |selector: &str| -> Value {
        let output = egregore()
            .args(["query", "unreferenced", "--repo", selector, "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
            .expect("stdout must be valid JSON")
    };

    let clean = scoped("repo-clean");
    assert!(
        candidate(&clean, "clean_orphan")["extraction_caveat"].is_null(),
        "the clean repository must not inherit the other repository's marker \
         through the shared repo-relative path"
    );
    assert_eq!(
        clean["counts"]["files_with_diagnostic_markers"], 0,
        "out-of-scope Diagnostic-marked files must not be tallied"
    );

    let dirty = scoped("repo-dirty");
    assert_eq!(
        dirty["candidates"]
            .as_array()
            .expect("candidates")
            .iter()
            .find(|c| c["name"] == "dirty_orphan")
            .expect("dirty_orphan candidate")["extraction_caveat"]["code"],
        "diagnostics_in_file_scope",
        "the owning repository keeps its own extraction caveat"
    );
    assert_eq!(dirty["counts"]["files_with_diagnostic_markers"], 1);
}

// ---------------------------------------------------------------------------
// --data-dir reads must leave the live embedded store byte-for-byte untouched
// ---------------------------------------------------------------------------

/// Recursive path -> content map for byte-exact store comparisons.
#[cfg(feature = "embedded-aletheiadb")]
fn dir_contents(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut contents = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("dir should read") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = fs::read(&path).expect("file should read");
                contents.insert(path.display().to_string(), bytes);
            }
        }
    }
    contents
}

/// Opening the embedded engine in place re-persists index files, so the
/// `--data-dir` path must read through a throwaway copy (same contract as the
/// other strictly read-only lanes).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn unreferenced_query_is_read_only_for_embedded_store() {
    let (temp, graph) = fixture_graph();
    let data_dir = temp.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let bytes_before = dir_contents(&data_dir);

    egregore()
        .args(["query", "unreferenced", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    assert_eq!(
        dir_contents(&data_dir),
        bytes_before,
        "querying must leave the live embedded store byte-for-byte untouched"
    );
}
