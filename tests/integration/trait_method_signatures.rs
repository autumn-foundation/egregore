//! Issue #342: signature-only trait method declarations
//! (`function_signature_item` inside a trait body) are first-class citable
//! `Symbol` records, mirroring default-bodied trait methods. Extern-block
//! foreign declarations stay symbol-less (out of scope, pinned here).
#![allow(missing_docs)]

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{scan_repository_at_with_override, scan_repository_history_with_override};
use assert_cmd::Command as CargoCommand;
use serde_json::Value;

const FIXED_TIME: &str = "2026-01-01T00:00:00Z";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should be built")
}

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(output.status.success(), "git command failed");
}

fn init_git(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);
}

fn commit(repo: &Path, message: &str, date: &str) {
    git(repo, ["add", "."]);
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(output.status.success(), "git commit failed");
}

fn seeded_repo(dir: &Path, source: &str) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(&repo, "src/lib.rs", source);
    commit(&repo, "seed", FIXED_TIME);
    repo
}

fn scan_jsonl(repo: &Path, repo_id: &str) -> String {
    scan_repository_at_with_override(repo, FIXED_TIME, Some(repo_id))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize")
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn symbol<'a>(records: &'a [Value], symbol_kind: &str, name: &str) -> Option<&'a Value> {
    records.iter().find(|record| {
        record["record_type"] == "node"
            && record["kind"] == "Symbol"
            && record["symbol_kind"] == symbol_kind
            && record["name"] == name
    })
}

/// Every `DEFINES` edge whose target is the given node ID.
fn defines_sources<'a>(records: &'a [Value], target_id: &str) -> Vec<&'a str> {
    records
        .iter()
        .filter(|record| {
            record["record_type"] == "edge"
                && record["label"] == "DEFINES"
                && record["target"] == target_id
        })
        .filter_map(|record| record["source"].as_str())
        .collect()
}

fn file_node_id<'a>(records: &'a [Value], path: &str) -> &'a str {
    records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["kind"] == "File"
                && record["repo_relative_path"] == path
        })
        .and_then(|record| record["id"].as_str())
        .expect("file node should exist")
}

const TRAIT_FIXTURE: &str = "pub trait Device {\n    unsafe fn poke(&self);\n    fn read(&self) -> u32;\n    fn name(&self) -> &str {\n        \"device\"\n    }\n}\n";

// ---------------------------------------------------------------------------
// Signature-only trait methods become first-class Symbols
// ---------------------------------------------------------------------------

#[test]
fn signature_only_trait_methods_are_symbols() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = seeded_repo(temp.path(), TRAIT_FIXTURE);
    let records = parse_jsonl(&scan_jsonl(&repo, "trait-sig-fixture"));

    let file_id = file_node_id(&records, "src/lib.rs").to_owned();

    // The `unsafe fn poke(&self);` signature-only declaration is now its own
    // Symbol, recorded exactly like a default-bodied trait method.
    let poke = symbol(&records, "function", "poke").expect("poke should be a Symbol");
    assert_eq!(poke["visibility"], "private");
    assert_eq!(poke["signature"], "unsafe fn poke(&self);");
    assert!(poke["span"]["start_line"].is_number());
    assert_eq!(poke["span"]["start_line"], 2);
    assert_eq!(poke["span"]["end_line"], 2);
    // DEFINES from the owning scope (the file — a trait establishes no owner
    // scope, matching default-bodied trait methods).
    let poke_id = poke["id"].as_str().expect("poke id");
    assert_eq!(defines_sources(&records, poke_id), vec![file_id.as_str()]);

    // The `fn read(&self) -> u32;` signature-only declaration, likewise.
    let read = symbol(&records, "function", "read").expect("read should be a Symbol");
    assert_eq!(read["visibility"], "private");
    assert_eq!(read["signature"], "fn read(&self)->u32;");
    assert_eq!(read["span"]["start_line"], 3);
    let read_id = read["id"].as_str().expect("read id");
    assert_eq!(defines_sources(&records, read_id), vec![file_id.as_str()]);

    // The default-bodied `name` method is unchanged: still a private function
    // Symbol defined by the file.
    let name = symbol(&records, "function", "name").expect("name should still be a Symbol");
    assert_eq!(name["visibility"], "private");
    let name_id = name["id"].as_str().expect("name id");
    assert_eq!(defines_sources(&records, name_id), vec![file_id.as_str()]);
}

// ---------------------------------------------------------------------------
// CALLS reachability: call sites resolve to trait methods (issue #390)
// ---------------------------------------------------------------------------

fn calls_edge_resolution<'a>(records: &'a [Value], source: &str, target: &str) -> Option<&'a str> {
    records
        .iter()
        .find(|record| {
            record["record_type"] == "edge"
                && record["label"] == "CALLS"
                && record["source"] == source
                && record["target"] == target
        })
        .and_then(|record| record["resolution"].as_str())
}

#[test]
fn trait_method_call_sites_resolve_to_the_trait_method() {
    // A trait-qualified path call and a receiver call both resolve to trait
    // methods declared in TRAIT_FIXTURE — the signature-only `read` and the
    // default-bodied `name`. Reachability is the CALLS half of issue #390.
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(&repo, "src/lib.rs", TRAIT_FIXTURE);
    write(
        &repo,
        "src/caller.rs",
        "pub fn path_call() -> u32 {\n    crate::Device::read()\n}\n\npub fn receiver_call(d: &crate::Device) -> u32 {\n    d.read() + d.name()\n}\n",
    );
    commit(&repo, "seed", FIXED_TIME);
    let records = parse_jsonl(&scan_jsonl(&repo, "trait-call-fixture"));

    let read = symbol(&records, "function", "read").expect("read symbol")["id"]
        .as_str()
        .expect("read id");
    let name = symbol(&records, "function", "name").expect("name symbol")["id"]
        .as_str()
        .expect("name id");
    let path_call =
        symbol(&records, "function", "caller::path_call").expect("path_call symbol")["id"]
            .as_str()
            .expect("path_call id");
    let receiver_call = symbol(&records, "function", "caller::receiver_call")
        .expect("receiver_call symbol")["id"]
        .as_str()
        .expect("receiver_call id");

    // Trait-qualified `Device::read()` narrows to the signature-only method.
    assert_eq!(
        calls_edge_resolution(&records, path_call, read),
        Some("resolved"),
        "crate::Device::read() must resolve to the signature-only trait method"
    );
    // Receiver `d.read()` / `d.name()` reach the signature-only and the
    // default-bodied trait methods.
    assert_eq!(
        calls_edge_resolution(&records, receiver_call, read),
        Some("resolved"),
        "d.read() must resolve to the signature-only trait method"
    );
    assert_eq!(
        calls_edge_resolution(&records, receiver_call, name),
        Some("resolved"),
        "d.name() must resolve to the default-bodied trait method"
    );
}

// ---------------------------------------------------------------------------
// Extern boundary: foreign declarations stay symbol-less
// ---------------------------------------------------------------------------

#[test]
fn extern_block_signatures_are_symbolless() {
    // A `function_signature_item` inside an `extern` block
    // (`foreign_mod_item`) is out of scope for issue #342: no Symbol, no
    // DEFINES. Distinguished from the trait case by the nearest enclosing
    // item on the ancestor chain (`foreign_mod_item` vs `trait_item`).
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = seeded_repo(
        temp.path(),
        "extern \"C\" {\n    fn c_poke();\n    fn c_read() -> u32;\n}\n\npub fn call() {\n    unsafe { c_poke() }\n}\n",
    );
    let records = parse_jsonl(&scan_jsonl(&repo, "extern-sig-fixture"));

    assert!(
        symbol(&records, "function", "c_poke").is_none(),
        "extern foreign declaration c_poke must not be symbolized"
    );
    assert!(
        symbol(&records, "function", "c_read").is_none(),
        "extern foreign declaration c_read must not be symbolized"
    );
    // The surrounding free function is unaffected.
    assert!(symbol(&records, "function", "call").is_some());
}

// ---------------------------------------------------------------------------
// Additive-ID guarantee: adding a signature-only method never moves other IDs
// ---------------------------------------------------------------------------

#[test]
fn signature_only_methods_are_additive_to_the_id_set() {
    // A mixed fixture WITHOUT any signature-only trait method establishes the
    // baseline record-ID set. Adding a signature-only trait method to a trait
    // that already exists must be purely additive: every pre-existing record
    // ID stays byte-identical (the new symbol only adds IDs, never moves any).
    let base_source = "pub trait Device {\n    fn name(&self) -> &str {\n        \"device\"\n    }\n}\n\npub struct Widget {\n    pub value: usize,\n}\n\nimpl Widget {\n    pub fn new(value: usize) -> Self {\n        Self { value }\n    }\n}\n\npub fn free() -> usize {\n    1\n}\n";
    // Same fixture with a signature-only method inserted into the trait.
    let ext_source = "pub trait Device {\n    fn read(&self) -> u32;\n    fn name(&self) -> &str {\n        \"device\"\n    }\n}\n\npub struct Widget {\n    pub value: usize,\n}\n\nimpl Widget {\n    pub fn new(value: usize) -> Self {\n        Self { value }\n    }\n}\n\npub fn free() -> usize {\n    1\n}\n";

    let base_temp = tempfile::tempdir().expect("temp dir");
    let base_repo = seeded_repo(base_temp.path(), base_source);
    let base_ids: BTreeSet<String> = parse_jsonl(&scan_jsonl(&base_repo, "additive-fixture"))
        .iter()
        .filter_map(|record| record["id"].as_str().map(str::to_owned))
        .collect();

    let ext_temp = tempfile::tempdir().expect("temp dir");
    let ext_repo = seeded_repo(ext_temp.path(), ext_source);
    let ext_ids: BTreeSet<String> = parse_jsonl(&scan_jsonl(&ext_repo, "additive-fixture"))
        .iter()
        .filter_map(|record| record["id"].as_str().map(str::to_owned))
        .collect();

    // Baseline IDs are a strict subset: none moved, the change only added IDs.
    let missing: Vec<&String> = base_ids.difference(&ext_ids).collect();
    assert!(
        missing.is_empty(),
        "adding a signature-only trait method must not move pre-existing IDs; missing: {missing:?}"
    );
    assert!(
        ext_ids.len() > base_ids.len(),
        "the signature-only method should add at least one new record ID"
    );
}

// ---------------------------------------------------------------------------
// Public-API: signature-only trait methods behave like default-bodied ones
// ---------------------------------------------------------------------------

#[test]
fn signature_only_trait_methods_excluded_from_public_api() {
    // Recorded with the same kind/visibility/owner as default-bodied trait
    // methods (`function` / `private` / file-owned), so they are excluded
    // from the public-API surface as private exactly as default-bodied trait
    // methods are (empirically verified — see docs/cli/public-api.md).
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = seeded_repo(temp.path(), TRAIT_FIXTURE);
    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, scan_jsonl(&repo, "public-api-sig-fixture"))
        .expect("graph should write");

    let output = egregore()
        .args(["query", "public-api", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let envelope: Value = serde_json::from_slice(&output).expect("public-api JSON");

    let surfaced: Vec<&str> = envelope["items"]
        .as_array()
        .expect("items array")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    // The trait itself surfaces; none of its methods (default-bodied or
    // signature-only) do — they are private.
    assert!(surfaced.contains(&"Device"));
    assert!(!surfaced.contains(&"poke"));
    assert!(!surfaced.contains(&"read"));
    assert!(!surfaced.contains(&"name"));
    // All three methods are counted as private (never externally reachable).
    assert_eq!(envelope["counts"]["private"], 3);
}

// ---------------------------------------------------------------------------
// undocumented / unreferenced sanity: the new symbols behave sensibly
// ---------------------------------------------------------------------------

#[test]
fn signature_only_trait_methods_are_unreferenced_prune_candidates() {
    // A signature-only trait method with zero inbound reference edges is an
    // observed prune-triage candidate, exactly like a default-bodied private
    // trait method. No lane code changed — this asserts the observed behavior.
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = seeded_repo(temp.path(), TRAIT_FIXTURE);
    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, scan_jsonl(&repo, "unreferenced-sig-fixture"))
        .expect("graph should write");

    let output = egregore()
        .args(["query", "unreferenced", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let envelope: Value = serde_json::from_slice(&output).expect("unreferenced JSON");
    let names: Vec<&str> = envelope["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert!(
        names.contains(&"read"),
        "signature-only method read should be a prune candidate; got {names:?}"
    );
}

// ---------------------------------------------------------------------------
// validate stays clean over the new symbols
// ---------------------------------------------------------------------------

#[test]
fn validate_is_clean_over_signature_only_symbols() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = seeded_repo(temp.path(), TRAIT_FIXTURE);
    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, scan_jsonl(&repo, "validate-sig-fixture")).expect("graph should write");

    egregore()
        .args(["validate"])
        .arg(&graph_path)
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// History replay carries the new symbols
// ---------------------------------------------------------------------------

#[test]
fn history_replay_carries_signature_only_symbols() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = seeded_repo(temp.path(), TRAIT_FIXTURE);
    let jsonl = scan_repository_history_with_override(&repo, Some("history-sig-fixture"))
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);

    assert!(
        symbol(&records, "function", "poke").is_some(),
        "history replay should carry the poke signature Symbol"
    );
    assert!(
        symbol(&records, "function", "read").is_some(),
        "history replay should carry the read signature Symbol"
    );
}

// ---------------------------------------------------------------------------
// Determinism: byte-identical scan output across repeated runs
// ---------------------------------------------------------------------------

#[test]
fn signature_only_symbol_scan_is_byte_identical() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = seeded_repo(temp.path(), TRAIT_FIXTURE);
    let first = scan_jsonl(&repo, "determinism-sig-fixture");
    for _ in 0..4 {
        let again = scan_jsonl(&repo, "determinism-sig-fixture");
        assert_eq!(
            first, again,
            "scan output must be byte-identical across runs"
        );
    }
}
