//! Integration tests for `eg query public-api` (issue #213): enumerate the
//! crate's externally-reachable public API surface from recorded visibility
//! and module containment — never from a `pub` token grep.

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
// Fixture: the four hand-labeled reachability cases from issue #213
// ---------------------------------------------------------------------------

/// Crate root. Exercises: top-level `pub`, private items, `pub(crate)` items,
/// module declarations of each visibility, and `pub use` re-exports that
/// widen visibility out of a private module.
const LIB_RS: &str = r"pub mod api;
mod internal;
pub(crate) mod tools;

/// Top-level public function.
pub fn top_level() -> usize {
    42
}

fn hidden_fn() {}

pub(crate) fn crate_fn() {}

pub struct TopStruct;

pub use internal::Secret;
pub use internal::{Widget as PublicWidget};
";

/// Public module: one externally reachable item per enumerated kind, plus a
/// `pub(super)` item that must be classified crate-internal.
const API_RS: &str = r#"pub fn exposed() {}

pub struct Thing;

pub enum Mode {
    Fast,
    Slow,
}

pub trait Doer {
    fn go(&self);
}

pub type Alias = usize;

pub const K: usize = 1;

pub static NAME: &str = "n";

pub(super) fn sup_fn() {}

fn api_private() {}
"#;

/// Private module: `pub` items here are trapped — NOT externally reachable.
const INTERNAL_RS: &str = r"pub struct Secret {
    pub value: usize,
}

pub struct Widget;

pub fn trapped_fn() {}
";

/// `pub(crate)` module: `pub` items here are crate-internal, not external.
const TOOLS_RS: &str = r"pub fn tool_fn() {}
";

fn write_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("src dir");
    fs::write(dir.join("src/lib.rs"), LIB_RS).expect("lib.rs");
    fs::write(dir.join("src/api.rs"), API_RS).expect("api.rs");
    fs::write(dir.join("src/internal.rs"), INTERNAL_RS).expect("internal.rs");
    fs::write(dir.join("src/tools.rs"), TOOLS_RS).expect("tools.rs");
}

/// Scans the four-case fixture and writes the JSONL graph. Returns
/// (`TempDir`, graph path). Caller must keep the `TempDir` alive.
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("pubapi-fixture"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_public_api(graph: &Path) -> Value {
    let output = egregore()
        .args(["query", "public-api", "--graph"])
        .arg(graph)
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
        .unwrap_or_else(|| panic!("missing public-api item with path {path}"))
}

// ---------------------------------------------------------------------------
// AC1: enumerated items carry kind, path, visibility, and file:span handle
// ---------------------------------------------------------------------------

#[test]
fn query_public_api_exits_0_with_cited_items() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_public_api(&graph);

    assert_eq!(parsed["ok"], true, "ok must be true on success");
    let items = parsed["items"].as_array().expect("items array");
    assert!(!items.is_empty(), "fixture surface must not be empty");
    for it in items {
        assert!(it["record_id"].as_str().is_some(), "item needs record_id");
        assert!(it["kind"].as_str().is_some(), "item needs kind");
        assert!(it["path"].as_str().is_some(), "item needs path");
        assert_eq!(
            it["visibility"], "public",
            "every externally reachable item is public"
        );
        assert!(
            it["repo_relative_path"].as_str().is_some(),
            "item needs repo-relative file handle"
        );
        assert!(
            it["span"]["start_line"].as_u64().is_some(),
            "item needs span"
        );
    }
}

#[test]
fn query_public_api_enumerates_every_reachable_kind() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_public_api(&graph);

    assert_eq!(item(&parsed, "api")["kind"], "module");
    assert_eq!(item(&parsed, "top_level")["kind"], "function");
    assert_eq!(item(&parsed, "TopStruct")["kind"], "struct");
    assert_eq!(item(&parsed, "api::exposed")["kind"], "function");
    assert_eq!(item(&parsed, "api::Thing")["kind"], "struct");
    assert_eq!(item(&parsed, "api::Mode")["kind"], "enum");
    assert_eq!(item(&parsed, "api::Doer")["kind"], "trait");
    assert_eq!(item(&parsed, "api::Alias")["kind"], "type_alias");
    assert_eq!(item(&parsed, "api::K")["kind"], "const");
    assert_eq!(item(&parsed, "api::NAME")["kind"], "static");
}

#[test]
fn query_public_api_items_join_persisted_signature() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_public_api(&graph);

    // Issue #124 persisted the declaration signature; the surface joins it.
    let sig = item(&parsed, "top_level")["signature"]
        .as_str()
        .expect("signature joined from persisted symbol metadata");
    assert!(
        sig.contains("fn top_level"),
        "signature must be the declaration header, got {sig:?}"
    );
}

// ---------------------------------------------------------------------------
// AC2: `pub` items trapped in a non-`pub` module are excluded
// ---------------------------------------------------------------------------

#[test]
fn query_public_api_excludes_pub_items_in_private_modules() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_public_api(&graph);
    let paths = item_paths(&parsed);

    for trapped in [
        "internal",
        "internal::Secret",
        "internal::Widget",
        "internal::trapped_fn",
        "tools",
        "tools::tool_fn",
    ] {
        assert!(
            !paths.iter().any(|p| p == trapped),
            "{trapped} is trapped in a non-pub module and must be excluded"
        );
    }
    assert!(
        parsed["counts"]["trapped_public"].as_u64().unwrap_or(0) >= 3,
        "trapped public items must be counted, got {:?}",
        parsed["counts"]
    );
}

// ---------------------------------------------------------------------------
// AC3: `pub use` re-exports are included, attributed to the re-export site
// ---------------------------------------------------------------------------

#[test]
fn query_public_api_includes_pub_use_reexports_at_reexport_site() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_public_api(&graph);

    let secret = item(&parsed, "Secret");
    assert_eq!(secret["via_reexport"], true, "must be marked as re-export");
    assert_eq!(
        secret["repo_relative_path"], "src/lib.rs",
        "re-export is attributed to the re-export site, not the target"
    );
    assert_eq!(secret["target"], "internal::Secret");
    assert!(
        secret["target_record_id"].as_str().is_some(),
        "in-graph re-export target must be cited by record ID"
    );
    assert!(
        secret["span"]["start_line"].as_u64().is_some(),
        "re-export site must carry a span"
    );

    let widget = item(&parsed, "PublicWidget");
    assert_eq!(widget["via_reexport"], true);
    assert_eq!(widget["target"], "internal::Widget");
}

#[test]
fn query_public_api_plain_use_is_not_a_reexport() {
    // A non-pub `use` must never appear on the surface.
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(
        temp.path().join("src/lib.rs"),
        "mod internal;\nuse internal::Widget;\npub fn f() -> Widget { Widget }\n",
    )
    .expect("lib.rs");
    fs::write(temp.path().join("src/internal.rs"), "pub struct Widget;\n").expect("internal.rs");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("pubapi-plainuse"))
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    let parsed = run_public_api(&graph);
    let paths = item_paths(&parsed);
    assert!(
        !paths.iter().any(|p| p == "Widget"),
        "a plain `use` must not surface as a re-export"
    );
    assert!(paths.iter().any(|p| p == "f"), "pub fn f is reachable");
}

// ---------------------------------------------------------------------------
// AC4: pub(crate) / pub(super) / pub(in path) are crate-internal, excluded
// ---------------------------------------------------------------------------

#[test]
fn query_public_api_excludes_crate_internal_and_private_tiers() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_public_api(&graph);
    let paths = item_paths(&parsed);

    for excluded in ["crate_fn", "api::sup_fn", "hidden_fn", "api::api_private"] {
        assert!(
            !paths.iter().any(|p| p == excluded),
            "{excluded} must not appear in the externally-reachable set"
        );
    }
    assert!(
        parsed["counts"]["crate_internal"].as_u64().unwrap_or(0) >= 2,
        "crate-internal items must be counted, got {:?}",
        parsed["counts"]
    );
    assert!(
        parsed["counts"]["private"].as_u64().unwrap_or(0) >= 2,
        "private items must be counted, got {:?}",
        parsed["counts"]
    );
}

// ---------------------------------------------------------------------------
// AC5: deterministic, byte-stable output
// ---------------------------------------------------------------------------

#[test]
fn query_public_api_output_is_byte_identical_across_runs() {
    let (_temp, graph) = fixture_graph();

    let run = || {
        egregore()
            .args(["query", "public-api", "--graph"])
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

// ---------------------------------------------------------------------------
// AC6: empty/absent surface is an explicit machine-readable result
// ---------------------------------------------------------------------------

#[test]
fn query_public_api_empty_surface_is_explicit_not_an_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(temp.path().join("src/lib.rs"), "fn only_private() {}\n").expect("lib.rs");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("pubapi-empty"))
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    let output = egregore()
        .args(["query", "public-api", "--graph"])
        .arg(&graph)
        .assert()
        .success() // exit 0: an empty surface is a real answer, not an error
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["items"].as_array().map(Vec::len), Some(0));
    let diags = parsed["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diags.iter().any(|d| d["code"] == "empty_surface"),
        "empty surface must be reported explicitly, got {diags:?}"
    );
}

#[test]
fn query_public_api_graph_without_rust_records_reports_empty_surface() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("empty.jsonl");
    fs::write(&graph, "").expect("write empty graph");

    let output = egregore()
        .args(["query", "public-api", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["items"].as_array().map(Vec::len), Some(0));
    assert!(
        parsed["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["code"] == "empty_surface"),
        "absent surface must be explicit"
    );
}

#[test]
fn query_public_api_missing_graph_file_fails_with_error() {
    egregore()
        .args(["query", "public-api", "--graph", "/nonexistent/graph.jsonl"])
        .assert()
        .failure()
        .code(1);
}

// ---------------------------------------------------------------------------
// Glob re-exports: never synthesized, surfaced as a diagnostic
// ---------------------------------------------------------------------------

#[test]
fn query_public_api_glob_reexport_yields_diagnostic_not_synthesized_items() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(
        temp.path().join("src/lib.rs"),
        "mod internal;\npub use internal::*;\n",
    )
    .expect("lib.rs");
    fs::write(temp.path().join("src/internal.rs"), "pub fn g() {}\n").expect("internal.rs");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("pubapi-glob"))
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    let parsed = run_public_api(&graph);
    let paths = item_paths(&parsed);
    assert!(
        !paths.iter().any(|p| p.contains('*')),
        "no synthesized glob items"
    );
    assert!(
        parsed["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["code"] == "glob_reexport_unresolved"),
        "glob re-export must surface as a diagnostic, got {:?}",
        parsed["diagnostics"]
    );
}

// ---------------------------------------------------------------------------
// --repo scoping in a multi-repo store
// ---------------------------------------------------------------------------

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
fn query_public_api_supports_repo_scoping() {
    let alpha = scan_named("repo-alpha", "pub fn alpha_only() {}\n");
    let beta = scan_named("repo-beta", "pub fn beta_only() {}\n");
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("multi.jsonl");
    fs::write(&graph, format!("{alpha}{beta}")).expect("write multi-repo graph");

    let output = egregore()
        .args(["query", "public-api", "--repo", "repo-alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    let paths = item_paths(&parsed);
    assert!(
        paths.iter().any(|p| p == "alpha_only"),
        "scoped repo item must appear, got {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p == "beta_only"),
        "other repo must not leak into a scoped surface"
    );
}

#[test]
fn query_public_api_unknown_repo_selector_exits_1() {
    let (_temp, graph) = fixture_graph();
    egregore()
        .args(["query", "public-api", "--repo", "no-such-repo", "--graph"])
        .arg(&graph)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown_repository_selector"));
}

// ---------------------------------------------------------------------------
// Extraction prerequisite: Rust module records carry visibility
// ---------------------------------------------------------------------------

#[test]
fn rust_module_records_carry_visibility() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("pubapi-mods"))
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let records: Vec<Value> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid record"))
        .collect();

    let module = |name: &str| -> &Value {
        records
            .iter()
            .find(|r| r["record_type"] == "node" && r["kind"] == "Module" && r["name"] == name)
            .unwrap_or_else(|| panic!("missing module record {name}"))
    };
    assert_eq!(module("api")["visibility"], "public");
    assert_eq!(module("internal")["visibility"], "private");
    assert_eq!(module("tools")["visibility"], "crate");
}
