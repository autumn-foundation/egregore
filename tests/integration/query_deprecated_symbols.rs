//! Integration tests for `eg query deprecated-symbols` (issue #249): inventory
//! `#[deprecated]` symbols with their still-resolvable call sites as a
//! migration worklist — code facts only, no replacement advice.

#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::scan_repository_at_with_override;
use assert_cmd::Command;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

// ---------------------------------------------------------------------------
// Fixture: deprecated symbols in all three attribute forms plus live callers
// ---------------------------------------------------------------------------

/// Labels:
/// - `old_api`: bare `#[deprecated]`, called by `caller_one`.
/// - `older_api`: `#[deprecated = "..."]` note shorthand, called by
///   `caller_two` together with `oldest_api`.
/// - `oldest_api`: `#[deprecated(since = ..., note = ...)]`, called by
///   `caller_two`.
/// - `new_api`: NOT deprecated and never reported, even though it is called.
/// - `lonely`: NOT deprecated and never reported.
/// - `unresolved_user`: calls `missing_helper_fn()`, which the graph cannot
///   resolve — must appear under the unresolved coverage field, never as a
///   call site and never counted as zero.
const LIB_RS: &str = r#"#[deprecated]
pub fn old_api() -> usize {
    1
}

#[deprecated = "use new_api instead"]
pub fn older_api() -> usize {
    2
}

#[deprecated(since = "1.2.0", note = "use new_api")]
pub fn oldest_api() -> usize {
    3
}

pub fn new_api() -> usize {
    4
}

fn lonely() -> usize {
    5
}

pub fn caller_one() -> usize {
    old_api()
}

pub fn caller_two() -> usize {
    older_api() + oldest_api()
}

pub fn unresolved_user() -> usize {
    missing_helper_fn()
}
"#;

fn write_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("src dir");
    fs::write(dir.join("src/lib.rs"), LIB_RS).expect("lib.rs");
}

/// Scans the labeled fixture and writes the JSONL graph. Returns
/// (`TempDir`, graph path). Caller must keep the `TempDir` alive.
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("dep-fixture"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_deprecated(graph: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = egregore();
    cmd.args(["query", "deprecated-symbols", "--graph"]);
    cmd.arg(graph);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.assert()
}

fn run_deprecated_json(graph: &Path) -> Value {
    let output = run_deprecated(graph, &[])
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON")
}

fn symbols(parsed: &Value) -> Vec<&Value> {
    parsed["deprecated_symbols"]
        .as_array()
        .expect("deprecated_symbols array")
        .iter()
        .collect()
}

fn symbol<'a>(parsed: &'a Value, name: &str) -> &'a Value {
    symbols(parsed)
        .into_iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("missing deprecated symbol named {name}"))
}

fn call_site_names(row: &Value) -> Vec<String> {
    row["call_sites"]
        .as_array()
        .expect("call_sites array")
        .iter()
        .map(|c| c["caller_name"].as_str().expect("caller_name").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// AC: exact inventory — 100% of the marked symbols, none invented
// ---------------------------------------------------------------------------

#[test]
fn deprecated_symbols_returns_exactly_the_marked_set_with_verbatim_payloads() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_deprecated_json(&graph);

    assert_eq!(parsed["ok"], true, "ok must be true on success");
    let names: Vec<String> = symbols(&parsed)
        .into_iter()
        .map(|s| s["name"].as_str().expect("name").to_owned())
        .collect();
    assert_eq!(
        names,
        vec!["old_api", "older_api", "oldest_api"],
        "inventory must name every marked symbol and invent none"
    );

    for row in symbols(&parsed) {
        let record_id = row["record_id"].as_str().expect("record_id");
        assert!(
            !record_id.is_empty(),
            "every row carries a citable record ID"
        );
        assert_eq!(
            row["repo_relative_path"], "src/lib.rs",
            "every row carries the repo-relative file path"
        );
        assert!(
            row["span"]["start_line"].as_u64().is_some(),
            "every row carries a span"
        );
        assert_eq!(row["kind"], "function", "kind is a code fact");
    }

    // Bare form: absent since/note are explicit nulls, never fabricated text.
    let bare = symbol(&parsed, "old_api");
    assert_eq!(
        bare["deprecated_since"],
        Value::Null,
        "absent since is null"
    );
    assert_eq!(bare["deprecated_note"], Value::Null, "absent note is null");

    // Note shorthand.
    let shorthand = symbol(&parsed, "older_api");
    assert_eq!(shorthand["deprecated_since"], Value::Null);
    assert_eq!(
        shorthand["deprecated_note"], "use new_api instead",
        "note shorthand is surfaced verbatim"
    );

    // since + note meta form.
    let meta = symbol(&parsed, "oldest_api");
    assert_eq!(
        meta["deprecated_since"], "1.2.0",
        "since is surfaced verbatim"
    );
    assert_eq!(
        meta["deprecated_note"], "use new_api",
        "note is surfaced verbatim"
    );
}

#[test]
fn deprecated_call_sites_are_the_migration_worklist_with_citable_handles() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_deprecated_json(&graph);

    let old_api = symbol(&parsed, "old_api");
    let sites = call_site_names(old_api);
    assert_eq!(
        sites,
        vec!["caller_one"],
        "old_api's worklist is exactly its resolvable callers"
    );

    let site = &old_api["call_sites"][0];
    assert!(
        !site["call_edge_record_id"]
            .as_str()
            .unwrap_or("")
            .is_empty(),
        "each call site carries the citable CALLS edge record ID"
    );
    assert!(
        !site["caller_record_id"].as_str().unwrap_or("").is_empty(),
        "each call site carries the citable caller record ID"
    );
    assert_eq!(site["caller_repo_relative_path"], "src/lib.rs");
    assert!(
        site["caller_span"]["start_line"].as_u64().is_some(),
        "each call site carries the caller span"
    );
    assert!(
        matches!(
            site["resolution"].as_str(),
            Some("resolved" | "ambiguous" | "unrecorded")
        ),
        "resolution is a closed code-fact label"
    );

    // caller_two calls both older_api and oldest_api.
    assert_eq!(
        call_site_names(symbol(&parsed, "older_api")),
        vec!["caller_two"]
    );
    assert_eq!(
        call_site_names(symbol(&parsed, "oldest_api")),
        vec!["caller_two"]
    );

    assert_eq!(
        parsed["counts"]["resolved_call_sites"], 3,
        "counts tally the worklist"
    );
    assert_eq!(parsed["counts"]["deprecated_symbols"], 3);
    assert!(
        parsed["counts"]["symbols_considered"].as_u64().unwrap_or(0) >= 3,
        "symbols_considered covers the whole in-scope population"
    );
}

#[test]
fn unresolved_call_edges_are_reported_never_dropped_never_zero() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_deprecated_json(&graph);

    let unresolved = &parsed["coverage"]["unresolved_call_edges"];
    let count = unresolved["count"].as_u64().expect("count");
    assert!(
        count >= 1,
        "the unresolvable missing_helper_fn() call must be tallied, never zero"
    );
    let ids = unresolved["call_edge_record_ids"]
        .as_array()
        .expect("citable edge IDs");
    assert_eq!(
        ids.len() as u64,
        count,
        "every unresolved edge is cited by record ID"
    );
    assert!(
        ids.iter().all(|id| !id.as_str().unwrap_or("").is_empty()),
        "every cited ID is non-empty"
    );

    // The unresolved edge must not leak into any worklist.
    for row in symbols(&parsed) {
        for site in row["call_sites"].as_array().expect("call_sites") {
            assert_ne!(
                site["caller_name"], "unresolved_user",
                "unresolved calls are never reported as resolved call sites"
            );
        }
    }

    // No LLM prose: the payload is IDs, paths, spans, counts, since/note.
    let raw = serde_json::to_string(&parsed).expect("serialize");
    assert!(
        !raw.contains("missing_helper_fn"),
        "unresolved callee names are not required; no prose is synthesized"
    );
}

// ---------------------------------------------------------------------------
// AC: distinct machine-readable diagnostics + distinct exit codes
// ---------------------------------------------------------------------------

#[test]
fn zero_deprecated_symbols_is_exit_3_with_stable_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn plain() -> usize { 1 }\n",
    )
    .expect("lib.rs");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("plain-fixture"))
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    let assert = run_deprecated(&graph, &[]).code(3);
    let output = assert.get_output().stdout.clone();
    let parsed: Value = serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON");
    assert_eq!(parsed["deprecated_symbols"].as_array().unwrap().len(), 0);
    let codes: Vec<&str> = parsed["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .map(|d| d["code"].as_str().expect("code"))
        .collect();
    assert!(
        codes.contains(&"no_deprecated_symbols"),
        "stable no_deprecated_symbols diagnostic, got {codes:?}"
    );
}

#[test]
fn zero_code_nodes_is_exit_4_distinct_from_zero_deprecated() {
    let temp = tempfile::tempdir().expect("temp dir");
    // A graph with a Repository record but no Symbol records at all.
    fs::write(temp.path().join("empty.jsonl"), "").expect("empty graph");
    let graph = temp.path().join("empty.jsonl");

    let assert = run_deprecated(&graph, &[]).code(4);
    let output = assert.get_output().stdout.clone();
    let parsed: Value = serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON");
    let codes: Vec<&str> = parsed["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .map(|d| d["code"].as_str().expect("code"))
        .collect();
    assert!(
        codes.contains(&"no_code_symbols"),
        "stable no_code_symbols diagnostic, got {codes:?}"
    );
}

// ---------------------------------------------------------------------------
// AC: determinism — byte-identical rows and ordering across 5 runs
// ---------------------------------------------------------------------------

#[test]
fn five_consecutive_runs_are_byte_identical() {
    let (_temp, graph) = fixture_graph();
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let output = run_deprecated(&graph, &[])
            .success()
            .get_output()
            .stdout
            .clone();
        outputs.push(output);
    }
    for (i, output) in outputs.iter().enumerate().skip(1) {
        assert_eq!(
            outputs[0], *output,
            "run {i} must be byte-identical to run 0"
        );
    }
}

// ---------------------------------------------------------------------------
// AC: human-readable text format
// ---------------------------------------------------------------------------

#[test]
fn text_format_names_symbols_callers_and_counts() {
    let (_temp, graph) = fixture_graph();
    let output = run_deprecated(&graph, &["--format", "text"])
        .success()
        .get_output()
        .stdout
        .clone();
    let text = std::str::from_utf8(&output).expect("utf8");
    for name in [
        "old_api",
        "older_api",
        "oldest_api",
        "caller_one",
        "caller_two",
    ] {
        assert!(text.contains(name), "text output names {name}:\n{text}");
    }
    assert!(text.contains("src/lib.rs"), "text output cites the file");
    assert!(
        text.contains("deprecated_since=1.2.0"),
        "text output surfaces since verbatim:\n{text}"
    );
    assert!(
        text.contains("deprecated_note=use new_api"),
        "text output surfaces note verbatim:\n{text}"
    );
    assert!(
        !text.contains("SECRET_BODY_MARKER") && !text.contains("fn old_api() -> usize {\n    1"),
        "text output never embeds raw source text"
    );
}

#[test]
fn text_format_zero_deprecated_exits_3() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn plain() -> usize { 1 }\n",
    )
    .expect("lib.rs");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("plain-fixture"))
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    run_deprecated(&graph, &["--format", "text"]).code(3);
}

// ---------------------------------------------------------------------------
// AC: --file filter scopes the inventory; --repo unknown is machine-readable
// ---------------------------------------------------------------------------

#[test]
fn file_filter_scopes_the_inventory_and_keeps_callers_whole() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(
        temp.path().join("src/a.rs"),
        "#[deprecated]\npub fn old_in_a() -> usize { 1 }\n",
    )
    .expect("a.rs");
    fs::write(
        temp.path().join("src/b.rs"),
        "#[deprecated]\npub fn old_in_b() -> usize { old_in_a() }\n",
    )
    .expect("b.rs");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("file-fixture"))
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    // Scoped to src/a.rs: only old_in_a is inventoried, but its worklist
    // still cites the caller in src/b.rs. Names are module-qualified by the
    // extractor.
    let output = run_deprecated(&graph, &["--file", "src/a.rs"])
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value = serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON");
    let names: Vec<String> = symbols(&parsed)
        .into_iter()
        .map(|s| s["name"].as_str().expect("name").to_owned())
        .collect();
    assert_eq!(
        names,
        vec!["a::old_in_a"],
        "only the file's symbol is inventoried"
    );
    assert_eq!(
        parsed["file_filter"], "src/a.rs",
        "the envelope echoes the file scope"
    );
    let sites = call_site_names(symbol(&parsed, "a::old_in_a"));
    assert_eq!(
        sites,
        vec!["b::old_in_b"],
        "the worklist cites the cross-file caller whole"
    );
    let site = &symbol(&parsed, "a::old_in_a")["call_sites"][0];
    assert_eq!(
        site["caller_repo_relative_path"], "src/b.rs",
        "caller attribution resolves across the file boundary"
    );

    // A file with no symbols at all is the no_code_symbols outcome (exit 4).
    run_deprecated(&graph, &["--file", "src/nowhere.rs"]).code(4);

    // Unscoped, both deprecated symbols are inventoried.
    let parsed_all = run_deprecated_json(&graph);
    let names_all: Vec<String> = symbols(&parsed_all)
        .into_iter()
        .map(|s| s["name"].as_str().expect("name").to_owned())
        .collect();
    assert_eq!(names_all, vec!["a::old_in_a", "b::old_in_b"]);
}

#[test]
fn unknown_repo_selector_is_machine_readable_exit_1() {
    let (_temp, graph) = fixture_graph();

    let assert = run_deprecated(&graph, &["--repo", "no-such-repo"]).code(1);
    let output = assert.get_output();
    assert!(
        output.stdout.is_empty(),
        "no inventory rows on an unknown selector"
    );
    let stderr = std::str::from_utf8(&output.stderr).expect("utf8");
    let diag: Value =
        serde_json::from_str(stderr.trim()).expect("stderr must be machine-readable JSON");
    assert_eq!(
        diag["code"], "unknown_repository_selector",
        "same machine-readable code as eg query symbol"
    );
}

// ---------------------------------------------------------------------------
// AC: both stores round-trip the facts — JSONL and embedded agree byte-for-byte
// ---------------------------------------------------------------------------

/// Ingests the fixture graph into an embedded store and asserts the
/// `--data-dir` inventory is byte-identical to the `--graph` one, with the
/// deprecation payloads intact (issue #249 AC: both stores round-trip the
/// facts; the embedded write/read pair in `EmbeddedAletheiaSink` must stay
/// symmetric because `compare_node_record` is full structural equality).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_store_round_trips_deprecation_facts_byte_identical() {
    let (_temp, graph) = fixture_graph();

    let store = tempfile::tempdir().expect("store dir");
    let data_dir = store.path().join("store");
    egregore()
        .arg("ingest")
        .arg(&graph)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let from_graph = run_deprecated(&graph, &[])
        .success()
        .get_output()
        .stdout
        .clone();
    let from_store = {
        let mut cmd = egregore();
        cmd.args(["query", "deprecated-symbols", "--data-dir"]);
        cmd.arg(&data_dir);
        cmd.assert().success().get_output().stdout.clone()
    };

    assert_eq!(
        from_graph, from_store,
        "--graph and --data-dir must produce byte-identical inventories"
    );

    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&from_store).expect("utf8").trim())
            .expect("stdout must be valid JSON");
    assert_eq!(parsed["counts"]["deprecated_symbols"], 3);
    let meta = symbol(&parsed, "oldest_api");
    assert_eq!(
        meta["deprecated_since"], "1.2.0",
        "since survives the embedded round-trip"
    );
    assert_eq!(
        meta["deprecated_note"], "use new_api",
        "note survives the embedded round-trip"
    );
    let bare = symbol(&parsed, "old_api");
    assert_eq!(bare["deprecated_since"], Value::Null);
    assert_eq!(bare["deprecated_note"], Value::Null);
}
