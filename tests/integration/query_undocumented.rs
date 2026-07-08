//! Integration tests for `eg query undocumented` (issue #257): list
//! externally-reachable public symbols whose captured doc-comment fact is
//! absent — a citable doc-debt triage lane, never a `pub`-grep and never a
//! doc-quality claim.

#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::scan_repository_at_with_override;
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

const FIXED_TIME: &str = "2026-06-30T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

// ---------------------------------------------------------------------------
// Fixture: the hand-labeled cases from the issue #257 success metric
// ---------------------------------------------------------------------------

/// Crate root. Exercises every labeled case: a `pub fn` with `///`, a
/// `pub fn` with `#[doc = "..."]`, a `pub fn` with `/** */`, a `pub struct`
/// with no doc, a `pub use` re-export of an undocumented item, a private
/// undocumented `fn`, and a `pub fn` preceded only by a `//` non-doc comment.
const LIB_RS: &str = r#"mod internal;
pub mod api;

/// Documented with a line doc.
pub fn line_documented() {}

#[doc = "Documented with a doc attribute."]
pub fn attr_documented() {}

/** Documented with a block doc. */
pub fn block_documented() {}

pub struct BareStruct;

// A plain comment is not documentation.
pub fn plain_commented() {}

fn private_undocumented() {}

pub use internal::Hidden;
pub use internal::Shown;
"#;

/// Private module: `Hidden` has no doc (its re-export must be reported),
/// `Shown` is documented at the declaration (its re-export must be excluded).
const INTERNAL_RS: &str = r"pub struct Hidden;

/// Documented at the declaration.
pub struct Shown;

pub fn trapped_undocumented() {}
";

/// Public module: one documented and one undocumented item.
const API_RS: &str = r"/// Documented function.
pub fn documented() {}

pub const BARE: usize = 1;
";

fn write_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("src dir");
    fs::write(dir.join("src/lib.rs"), LIB_RS).expect("lib.rs");
    fs::write(dir.join("src/internal.rs"), INTERNAL_RS).expect("internal.rs");
    fs::write(dir.join("src/api.rs"), API_RS).expect("api.rs");
}

/// Scans the fixture and writes the JSONL graph. Returns (`TempDir`, graph
/// path). Caller must keep the `TempDir` alive.
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("undoc-fixture"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_undocumented(graph: &Path, extra: &[&str]) -> Value {
    let output = egregore()
        .args(["query", "undocumented", "--graph"])
        .arg(graph)
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON")
}

fn item_paths(parsed: &Value) -> Vec<String> {
    parsed["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|i| i["path"].as_str().expect("path").to_owned())
        .collect()
}

fn item<'a>(parsed: &'a Value, path: &str) -> &'a Value {
    parsed["items"]
        .as_array()
        .expect("items array")
        .iter()
        .find(|i| i["path"] == path)
        .unwrap_or_else(|| panic!("missing undocumented item with path {path}"))
}

// ---------------------------------------------------------------------------
// AC1 + AC2: rows carry handle, kind, path, span, and concrete evidence
// ---------------------------------------------------------------------------

#[test]
fn query_undocumented_exits_0_with_cited_evidence_rows() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_undocumented(&graph, &[]);

    assert_eq!(parsed["ok"], true, "ok must be true on success");
    let items = parsed["items"].as_array().expect("items array");
    assert!(!items.is_empty(), "fixture must yield undocumented items");
    for it in items {
        assert!(it["record_id"].as_str().is_some(), "item needs record_id");
        assert!(it["kind"].as_str().is_some(), "item needs kind");
        assert!(it["path"].as_str().is_some(), "item needs path");
        assert!(
            it["repo_relative_path"].as_str().is_some(),
            "item needs repo-relative file handle"
        );
        assert!(
            it["span"]["start_line"].as_u64().is_some(),
            "item needs span"
        );
        let evidence: Vec<&str> = it["evidence"]
            .as_array()
            .expect("evidence array")
            .iter()
            .map(|e| e.as_str().expect("evidence string"))
            .collect();
        assert!(
            evidence.contains(&"doc_comment_absent"),
            "every row asserts doc absence, got {evidence:?}"
        );
        assert!(
            evidence.contains(&"externally_reachable"),
            "default rows assert external reachability, got {evidence:?}"
        );
    }
}

#[test]
fn query_undocumented_reports_the_labeled_undocumented_set() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_undocumented(&graph, &[]);
    let paths = item_paths(&parsed);

    for expected in ["BareStruct", "plain_commented", "Hidden", "api::BARE"] {
        assert!(
            paths.iter().any(|p| p == expected),
            "{expected} must be reported as undocumented, got {paths:?}"
        );
    }
    assert_eq!(item(&parsed, "BareStruct")["kind"], "struct");
    assert_eq!(item(&parsed, "plain_commented")["kind"], "function");
    assert_eq!(item(&parsed, "api::BARE")["kind"], "const");
}

// ---------------------------------------------------------------------------
// AC4: every doc form excludes; a plain `//` comment does not
// ---------------------------------------------------------------------------

#[test]
fn query_undocumented_excludes_each_doc_comment_form() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_undocumented(&graph, &[]);
    let paths = item_paths(&parsed);

    for documented in [
        "line_documented",
        "attr_documented",
        "block_documented",
        "api::documented",
        "Shown", // re-export of a documented item
    ] {
        assert!(
            !paths.iter().any(|p| p == documented),
            "{documented} carries a doc comment and must be excluded, got {paths:?}"
        );
    }
    assert!(
        parsed["counts"]["documented"].as_u64().unwrap_or(0) >= 4,
        "documented items must be tallied, got {:?}",
        parsed["counts"]
    );
}

#[test]
fn query_undocumented_reports_symbol_with_only_a_non_doc_comment() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_undocumented(&graph, &[]);
    assert_eq!(
        item(&parsed, "plain_commented")["kind"],
        "function",
        "a `//` comment is not documentation; the symbol must be reported"
    );
}

// ---------------------------------------------------------------------------
// AC3: public-surface filter reuses the #213 reachability rule
// ---------------------------------------------------------------------------

#[test]
fn query_undocumented_excludes_non_reachable_symbols_by_default() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_undocumented(&graph, &[]);
    let paths = item_paths(&parsed);

    for excluded in [
        "private_undocumented",
        "internal::Hidden",
        "internal::trapped_undocumented",
    ] {
        assert!(
            !paths.iter().any(|p| p == excluded),
            "{excluded} is not externally reachable and must be excluded, got {paths:?}"
        );
    }
}

#[test]
fn query_undocumented_reexport_row_is_attributed_to_the_reexport_site() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_undocumented(&graph, &[]);

    let hidden = item(&parsed, "Hidden");
    assert_eq!(hidden["via_reexport"], true, "must be marked as re-export");
    assert_eq!(
        hidden["repo_relative_path"], "src/lib.rs",
        "re-export row cites the re-export site"
    );
    assert_eq!(hidden["target"], "internal::Hidden");
    assert!(
        hidden["target_record_id"].as_str().is_some(),
        "resolved re-export target must be cited by record ID"
    );
}

#[test]
fn query_undocumented_include_private_widens_to_all_symbols() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_undocumented(&graph, &["--include-private"]);
    let paths = item_paths(&parsed);

    for widened in ["private_undocumented", "internal::trapped_undocumented"] {
        assert!(
            paths.iter().any(|p| p == widened),
            "--include-private must widen to {widened}, got {paths:?}"
        );
    }
    let private_row = item(&parsed, "private_undocumented");
    assert_eq!(private_row["visibility"], "private");
    let evidence: Vec<&str> = private_row["evidence"]
        .as_array()
        .expect("evidence array")
        .iter()
        .map(|e| e.as_str().expect("evidence string"))
        .collect();
    assert!(
        !evidence.contains(&"externally_reachable"),
        "a private symbol must not claim external reachability, got {evidence:?}"
    );
    // Documented items stay excluded even in the widened audit.
    assert!(
        !paths.iter().any(|p| p == "line_documented"),
        "documented items stay excluded under --include-private"
    );
}

// ---------------------------------------------------------------------------
// AC5: soundness boundary is stated in response metadata
// ---------------------------------------------------------------------------

#[test]
fn query_undocumented_states_its_soundness_boundary() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_undocumented(&graph, &[]);
    let disclaimer = parsed["disclaimer"].as_str().expect("disclaimer");
    assert!(
        disclaimer.contains("presence") && disclaimer.contains("quality"),
        "disclaimer must state presence/absence-not-quality, got {disclaimer:?}"
    );
}

// ---------------------------------------------------------------------------
// AC6: deterministic byte-stable output, --limit, --repo, --format text
// ---------------------------------------------------------------------------

#[test]
fn query_undocumented_output_is_byte_identical_across_runs() {
    let (_temp, graph) = fixture_graph();
    let run = || {
        egregore()
            .args(["query", "undocumented", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    assert_eq!(
        run(),
        run(),
        "identical query must produce byte-identical output"
    );
}

#[test]
fn query_undocumented_limit_truncates_deterministically_with_diagnostic() {
    let (_temp, graph) = fixture_graph();
    let full = run_undocumented(&graph, &[]);
    let full_paths = item_paths(&full);
    assert!(full_paths.len() > 1, "fixture must yield multiple rows");

    let limited = run_undocumented(&graph, &["--limit", "1"]);
    let limited_paths = item_paths(&limited);
    assert_eq!(limited_paths.len(), 1, "--limit must bound the row count");
    assert_eq!(
        limited_paths[0], full_paths[0],
        "truncation must keep the deterministic sort prefix"
    );
    assert!(
        limited["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["code"] == "results_truncated"),
        "truncation must be reported, got {:?}",
        limited["diagnostics"]
    );
}

fn scan_named(repo_id: &str, lib_rs: &str) -> String {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(temp.path().join("src/lib.rs"), lib_rs).expect("lib.rs");
    scan_repository_at_with_override(temp.path(), FIXED_TIME, Some(repo_id))
        .expect("scan")
        .to_jsonl()
        .expect("serialize")
}

#[test]
fn query_undocumented_supports_repo_scoping() {
    let alpha = scan_named("undoc-alpha", "pub fn alpha_bare() {}\n");
    let beta = scan_named("undoc-beta", "pub fn beta_bare() {}\n");
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("multi.jsonl");
    fs::write(&graph, format!("{alpha}{beta}")).expect("write multi-repo graph");

    let parsed = run_undocumented(&graph, &["--repo", "undoc-alpha"]);
    let paths = item_paths(&parsed);
    assert!(
        paths.iter().any(|p| p == "alpha_bare"),
        "scoped repo item must appear, got {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p == "beta_bare"),
        "other repo must not leak into a scoped result"
    );
}

#[test]
fn query_undocumented_unknown_repo_selector_exits_1() {
    let (_temp, graph) = fixture_graph();
    egregore()
        .args(["query", "undocumented", "--repo", "no-such-repo", "--graph"])
        .arg(&graph)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown_repository_selector"));
}

#[test]
fn query_undocumented_missing_graph_file_fails_with_error() {
    egregore()
        .args([
            "query",
            "undocumented",
            "--graph",
            "/nonexistent/graph.jsonl",
        ])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn query_undocumented_format_text_lists_rows_with_citations() {
    let (_temp, graph) = fixture_graph();
    let output = egregore()
        .args(["query", "undocumented", "--format", "text", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(
        text.contains("BareStruct") && text.contains("src/lib.rs"),
        "text view must list rows with file citations, got {text:?}"
    );
    assert!(
        !text.contains("line_documented"),
        "documented items must not appear in the text view"
    );
}

// ---------------------------------------------------------------------------
// AC7: empty result and capability-absent verdicts are explicit
// ---------------------------------------------------------------------------

#[test]
fn query_undocumented_fully_documented_crate_is_explicit_success() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(
        temp.path().join("src/lib.rs"),
        "/// Fully documented.\npub fn only_documented() {}\n",
    )
    .expect("lib.rs");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("undoc-clean"))
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    let parsed = run_undocumented(&graph, &[]);
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["items"].as_array().map(Vec::len), Some(0));
    assert_eq!(parsed["capability"], "doc_facts_recorded");
    assert!(
        parsed["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["code"] == "no_undocumented_items"),
        "an empty result must be explicit, got {:?}",
        parsed["diagnostics"]
    );
}

#[test]
fn query_undocumented_pre_124_store_reports_capability_absent() {
    // Simulate a pre-#124 store: strip the declaration-surface fields from
    // every symbol record so no doc fact was ever captured.
    let (_temp, graph) = fixture_graph();
    let stripped: String = fs::read_to_string(&graph)
        .expect("read graph")
        .lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).expect("record");
            if record["kind"] == "Symbol" {
                let obj = record.as_object_mut().expect("object");
                obj.remove("visibility");
                obj.remove("signature");
                obj.remove("doc");
            }
            serde_json::to_string(&record).expect("serialize")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let temp = tempfile::tempdir().expect("temp dir");
    let old_graph = temp.path().join("old.jsonl");
    fs::write(&old_graph, stripped).expect("write stripped graph");

    let parsed = run_undocumented(&old_graph, &[]);
    assert_eq!(
        parsed["ok"], true,
        "capability absence is a verdict, not an error"
    );
    assert_eq!(
        parsed["capability"], "doc_facts_unavailable",
        "a pre-#124 store must report the capability as absent"
    );
    assert_eq!(
        parsed["items"].as_array().map(Vec::len),
        Some(0),
        "symbols must never be silently treated as undocumented"
    );
    assert!(
        parsed["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["code"] == "doc_capture_unavailable"),
        "capability absence must be diagnosed, got {:?}",
        parsed["diagnostics"]
    );
}

// ---------------------------------------------------------------------------
// Extraction prerequisite: `#[doc = "..."]` counts as documentation
// ---------------------------------------------------------------------------

#[test]
fn rust_doc_attribute_is_captured_as_doc_fact() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(
        temp.path().join("src/lib.rs"),
        "#[doc = \"Attribute-documented.\"]\npub fn attr_fn() {}\n",
    )
    .expect("lib.rs");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("undoc-attr"))
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let symbol = jsonl
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("record"))
        .find(|r| r["kind"] == "Symbol" && r["name"] == "attr_fn")
        .expect("attr_fn symbol record");
    assert_eq!(
        symbol["doc"], "Attribute-documented.",
        "#[doc = \"...\"] must be captured as the doc fact"
    );
}
