//! Integration tests for `eg query unsafe-sites` (issue #222).
//!
//! The lane inventories the scanned repo's own `unsafe` surface as detected
//! by the Tree-sitter extractor: genuine `unsafe { .. }` blocks, `unsafe fn`
//! declarations, and `unsafe impl` blocks only (never comment / string /
//! doc-comment / identifier decoys), each carrying a closed machine-readable
//! kind, a repo-relative file/span handle, and the enclosing symbol handle
//! when one exists, plus an aggregate count equal to the number of sites.
#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{
    GraphRecord, NodeKind, scan_repository_at_with_override, scan_repository_history_with_override,
};
use assert_cmd::Command as CargoCommand;
use predicates::prelude::*;
use serde_json::Value;

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
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(output.status.success(), "git command failed");
    String::from_utf8(output.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}

fn init_git(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);
}

fn commit(repo: &Path, message: &str, date: &str) -> String {
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
    assert!(
        output.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    git_output(repo, ["rev-parse", "HEAD"])
}

/// Genuine `unsafe` sites of all three closed kinds plus every decoy class
/// from the issue #222 acceptance criteria: `unsafe` inside a doc comment, a
/// line comment, a string literal, and an `unsafe_`-prefixed identifier. The
/// `unsafe trait` declaration proves the kind set is closed (`block` / `fn` /
/// `impl` only).
const LIB_RS: &str = r#"/// Doc-comment decoy: the word unsafe here is prose, not code.
pub fn read_flag(register: *const u32) -> u32 {
    // Comment decoy: unsafe { } inside a line comment.
    let string_decoy = "string decoy: unsafe { } and unsafe fn";
    let _ = string_decoy;
    let unsafe_flag_count = 1u32;
    let _ = unsafe_flag_count;
    unsafe { register.read_volatile() }
}

pub unsafe fn duplicate_flag(value: u32) -> u32 {
    value
}

pub struct RawHandle(pub *mut u8);

pub unsafe trait Sendable {}

unsafe impl Sendable for RawHandle {}

pub fn not_unsafe_helper() -> u32 {
    7
}
"#;

const CLEAN_RS: &str = "pub fn clean() -> u32 {\n    7\n}\n";

const FFI_BIND_RS: &str = "pub unsafe fn raw_write(pointer: *mut u8) {\n    pointer.write(0);\n}\n";

/// Builds the seeded fixture checkout, scans it deterministically, and writes
/// the JSONL graph. Returns (`TempDir`, `graph_path`).
fn fixture_scanned() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(&repo, "src/lib.rs", LIB_RS);
    write(&repo, "src/empty/clean.rs", CLEAN_RS);
    write(&repo, "src/ffi/bind.rs", FFI_BIND_RS);
    commit(&repo, "seed fixture", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("unsafe-sites-fixture"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");
    (temp, graph_path)
}

fn run_lane(graph: &Path, extra: &[&str]) -> (Value, String) {
    let mut cmd = egregore();
    cmd.args(["query", "unsafe-sites", "--graph"]).arg(graph);
    for arg in extra {
        cmd.arg(arg);
    }
    let output = cmd.output().expect("query should execute");
    assert!(
        output.status.success(),
        "query failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let value: Value = serde_json::from_str(&stdout).expect("stdout should be one JSON envelope");
    (value, stdout)
}

/// (`site_kind`, `repo_relative_path`, enclosing symbol name or `None`).
fn site_tuples(envelope: &Value) -> Vec<(String, String, Option<String>)> {
    envelope["sites"]
        .as_array()
        .expect("sites array")
        .iter()
        .map(|site| {
            let enclosing = if site["enclosing_symbol"].is_null() {
                None
            } else {
                Some(
                    site["enclosing_symbol"]["name"]
                        .as_str()
                        .expect("enclosing symbol name")
                        .to_owned(),
                )
            };
            (
                site["site_kind"].as_str().expect("site_kind").to_owned(),
                site["repo_relative_path"]
                    .as_str()
                    .expect("repo_relative_path")
                    .to_owned(),
                enclosing,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Extraction + lane correctness
// ---------------------------------------------------------------------------

#[test]
fn unsafe_sites_returns_real_sites_and_never_decoys() {
    let (_temp, graph) = fixture_scanned();
    let (envelope, _) = run_lane(&graph, &[]);

    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["lane"], "unsafe_sites");
    assert_eq!(
        envelope["kind_set"],
        serde_json::json!(["block", "fn", "impl"]),
        "the unsafe-site kind set is closed for this slice"
    );
    assert!(
        envelope["disclaimer"]
            .as_str()
            .expect("disclaimer")
            .contains("never"),
        "results must state they are an inventory, not a soundness verdict"
    );

    let tuples = site_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "fn".to_owned(),
                "src/ffi/bind.rs".to_owned(),
                Some("ffi::bind::raw_write".to_owned()),
            ),
            (
                "block".to_owned(),
                "src/lib.rs".to_owned(),
                Some("read_flag".to_owned()),
            ),
            (
                "fn".to_owned(),
                "src/lib.rs".to_owned(),
                Some("duplicate_flag".to_owned()),
            ),
            (
                "impl".to_owned(),
                "src/lib.rs".to_owned(),
                Some("unsafe impl Sendable for RawHandle".to_owned()),
            ),
        ],
        "the lane must return exactly the genuine unsafe sites: no comment, \
         string, doc-comment, or identifier decoys, and no unsafe trait \
         declarations (outside the closed kind set)"
    );

    assert_eq!(envelope["counts"]["total"], 4);
    assert_eq!(envelope["counts"]["block"], 1);
    assert_eq!(envelope["counts"]["fn"], 2);
    assert_eq!(envelope["counts"]["impl"], 1);
    assert_eq!(
        envelope["counts"]["total"].as_u64(),
        Some(envelope["sites"].as_array().expect("sites").len() as u64),
        "the aggregate count must equal the number of returned sites"
    );

    for site in envelope["sites"].as_array().expect("sites") {
        assert_eq!(site["kind"], "UnsafeSite");
        assert_eq!(site["trust"], "source_fact");
        assert!(
            site["record_id"]
                .as_str()
                .expect("record_id")
                .starts_with("codegraph:v"),
            "each site must carry a stable record ID"
        );
        let span = &site["span"];
        assert!(span["start_line"].as_u64().expect("start_line") >= 1);
        assert!(
            span["end_byte"].as_u64().expect("end_byte")
                > span["start_byte"].as_u64().expect("start_byte")
        );
        let enclosing = &site["enclosing_symbol"];
        assert!(
            enclosing["record_id"]
                .as_str()
                .expect("enclosing record_id")
                .starts_with("codegraph:v"),
            "the enclosing symbol handle must be a stable record ID"
        );
    }

    // The doc-comment decoy is line 1, the comment decoy line 3, the string
    // decoy line 4, and the identifier decoy line 6 of src/lib.rs; no
    // returned span may start there.
    for site in envelope["sites"].as_array().expect("sites") {
        if site["repo_relative_path"] == "src/lib.rs" {
            let start_line = site["span"]["start_line"].as_u64().expect("line");
            assert!(
                !matches!(start_line, 1 | 3 | 4 | 6),
                "decoy line {start_line} must never be returned"
            );
        }
    }
}

#[test]
fn unsafe_sites_scan_emits_unsafe_site_records() {
    let (_temp, graph) = fixture_scanned();
    let jsonl = fs::read_to_string(&graph).expect("graph should read");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();

    let sites: Vec<&GraphRecord> = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::UnsafeSite,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(sites.len(), 4, "scan must emit one record per unsafe site");
    for record in sites {
        let GraphRecord::Node {
            name,
            language,
            span,
            repo_relative_path,
            ..
        } = record
        else {
            unreachable!()
        };
        assert!(matches!(name.as_deref(), Some("block" | "fn" | "impl")));
        assert_eq!(language.as_deref(), Some("rust"));
        assert!(span.is_some(), "each site must carry a source span");
        assert!(repo_relative_path.is_some());
    }
}

#[test]
fn unsafe_sites_graph_passes_referential_integrity_validation() {
    // The `CONTAINS File -> UnsafeSite` edges emitted by the extractor must
    // satisfy the issue #103 pre-ingest referential-integrity gate.
    let (_temp, graph) = fixture_scanned();
    egregore().arg("validate").arg(&graph).assert().success();
}

// ---------------------------------------------------------------------------
// Scope: prefix filter, empty-vs-not-found honesty, --repo
// ---------------------------------------------------------------------------

#[test]
fn unsafe_sites_prefix_scopes_results_segment_aware() {
    let (_temp, graph) = fixture_scanned();
    let (envelope, _) = run_lane(&graph, &["--path", "src/ffi"]);
    let tuples = site_tuples(&envelope);
    assert_eq!(tuples.len(), 1);
    assert_eq!(tuples[0].1, "src/ffi/bind.rs");
    assert_eq!(envelope["path_prefix"], "src/ffi");
    assert_eq!(envelope["counts"]["total"], 1);
}

#[test]
fn unsafe_sites_empty_scope_is_distinguished_from_scope_not_found() {
    let (_temp, graph) = fixture_scanned();

    // Scope exists but contains zero unsafe sites: ok envelope with a stable
    // machine-readable empty reason, exit 0.
    let (envelope, _) = run_lane(&graph, &["--path", "src/empty"]);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["sites"], serde_json::json!([]));
    assert_eq!(envelope["counts"]["total"], 0);
    assert_eq!(envelope["empty_reason"], "no_sites_in_scope");

    // Scope not present in the store: scope_not_found, exit 2 — never a
    // silent empty result (issue #196 honesty contract).
    egregore()
        .args(["query", "unsafe-sites", "--graph"])
        .arg(&graph)
        .args(["--path", "src/nonexistent"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"scope_not_found\""));

    // Sibling-prefix bleed: `src/emp` must not match `src/empty`.
    egregore()
        .args(["query", "unsafe-sites", "--graph"])
        .arg(&graph)
        .args(["--path", "src/emp"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"scope_not_found\""));

    // Malformed prefix: exit 1 with a machine-readable diagnostic.
    egregore()
        .args(["query", "unsafe-sites", "--graph"])
        .arg(&graph)
        .args(["--path", "/"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("\"malformed_prefix\""));
}

#[test]
fn unsafe_sites_repo_selector_scopes_and_rejects_unknown() {
    let (_temp, graph) = fixture_scanned();

    // The override identity resolves as a repo selector.
    let (envelope, _) = run_lane(&graph, &["--repo", "unsafe-sites-fixture"]);
    assert_eq!(envelope["counts"]["total"], 4);

    // An unknown selector is a documented diagnostic, never a silent empty.
    egregore()
        .args(["query", "unsafe-sites", "--graph"])
        .arg(&graph)
        .args(["--repo", "no-such-repo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown_repository_selector"));
}

// ---------------------------------------------------------------------------
// Temporal selector (--at, valid-time axis keyed by commit)
// ---------------------------------------------------------------------------

#[test]
fn unsafe_sites_at_commit_pins_the_inventory_to_valid_time() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);

    write(&repo, "src/lib.rs", "pub fn calm() -> u32 {\n    1\n}\n");
    let first = commit(&repo, "no unsafe yet", "2026-01-01T00:00:00Z");

    write(
        &repo,
        "src/lib.rs",
        "pub fn risky(register: *const u32) -> u32 {\n    unsafe { register.read_volatile() }\n}\n",
    );
    let second = commit(&repo, "introduce unsafe block", "2026-01-02T00:00:00Z");

    write(&repo, "src/lib.rs", "pub fn calm() -> u32 {\n    1\n}\n");
    let third = commit(&repo, "remove unsafe block", "2026-01-03T00:00:00Z");

    let graph_path = temp.path().join("history.graph.jsonl");
    let jsonl = scan_repository_history_with_override(&repo, Some("unsafe-sites-history"))
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");

    // Pinned before the introduction: the site must not appear.
    let (at_first, _) = run_lane(&graph_path, &["--at", &first]);
    assert_eq!(at_first["counts"]["total"], 0);
    assert_eq!(at_first["empty_reason"], "no_sites_in_scope");
    assert_eq!(at_first["at_commit"], Value::String(first));

    // Pinned at the introducing commit: exactly one site.
    let (at_second, _) = run_lane(&graph_path, &["--at", &second]);
    assert_eq!(at_second["counts"]["total"], 1);
    let tuples = site_tuples(&at_second);
    assert_eq!(tuples[0].0, "block");
    assert_eq!(tuples[0].2, Some("risky".to_owned()));
    let site = &at_second["sites"][0];
    assert_eq!(site["git_commit"].as_str(), Some(second.as_str()));
    assert!(site["valid_time"].as_str().is_some());

    // Pinned after the removal: gone again — a site retired by a later
    // commit does not appear.
    let (at_third, _) = run_lane(&graph_path, &["--at", &third]);
    assert_eq!(at_third["counts"]["total"], 0);

    // A commit the store has never seen is a documented diagnostic (exit 2),
    // never a silent empty result.
    egregore()
        .args(["query", "unsafe-sites", "--graph"])
        .arg(&graph_path)
        .args(["--at", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"unknown_commit\""));
}

// ---------------------------------------------------------------------------
// Determinism + read-only guarantees
// ---------------------------------------------------------------------------

#[test]
fn unsafe_sites_output_is_byte_identical_across_five_runs() {
    let (_temp, graph) = fixture_scanned();
    let (_, first) = run_lane(&graph, &[]);
    for _ in 0..4 {
        let (_, next) = run_lane(&graph, &[]);
        assert_eq!(
            first, next,
            "re-running the identical query against an unchanged store must \
             yield byte-identical output"
        );
    }
}

#[test]
fn unsafe_sites_query_is_read_only() {
    let (temp, graph) = fixture_scanned();
    let bytes_before = fs::read(&graph).expect("graph should read");
    let listing_before = dir_listing(temp.path());

    let (_, _) = run_lane(&graph, &[]);

    assert_eq!(
        fs::read(&graph).expect("graph should read"),
        bytes_before,
        "querying must not modify the store"
    );
    assert_eq!(
        dir_listing(temp.path()),
        listing_before,
        "querying must not create or delete any files"
    );
}

fn dir_listing(root: &Path) -> Vec<String> {
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("dir should read") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            entries.push(path.display().to_string());
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    entries.sort();
    entries
}

// ---------------------------------------------------------------------------
// Enclosing symbol: explicit null when no DEFINES owner exists
// ---------------------------------------------------------------------------

#[test]
fn unsafe_sites_enclosing_symbol_is_explicit_null_when_top_level() {
    // Seeded fixture: a site record with no Symbol span covering it.
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("seeded.jsonl");
    let site = serde_json::json!({
        "record_type": "node",
        "id": "codegraph:v5:seededsite0000000000000000000000000000000000000000000000000001",
        "kind": "UnsafeSite",
        "schema_version": 5,
        "repo_relative_path": "src/orphan.rs",
        "span": {"start_byte": 10, "end_byte": 30, "start_line": 2, "end_line": 2},
        "name": "block",
        "language": "rust",
        "summary": "Rust unsafe block site"
    });
    let file = serde_json::json!({
        "record_type": "node",
        "id": "codegraph:v5:seededfile0000000000000000000000000000000000000000000000000001",
        "kind": "File",
        "schema_version": 5,
        "repo_relative_path": "src/orphan.rs",
        "name": "src/orphan.rs",
        "summary": "Rust source file src/orphan.rs"
    });
    fs::write(&graph_path, format!("{file}\n{site}\n")).expect("seeded graph should write");

    let (envelope, stdout) = run_lane(&graph_path, &[]);
    assert_eq!(envelope["counts"]["total"], 1);
    assert!(
        envelope["sites"][0]["enclosing_symbol"].is_null(),
        "a top-level site must carry an explicit null enclosing symbol"
    );
    assert!(
        stdout.contains("\"enclosing_symbol\": null"),
        "the null must be serialized explicitly, not omitted: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Enclosing symbol: signature-only trait methods attach to the trait
// ---------------------------------------------------------------------------

#[test]
fn unsafe_fn_trait_signature_encloses_to_containing_trait() {
    // Documented decision, not an accident: the extractor never symbolizes
    // signature-only declarations (`function_signature_item`), so an
    // `unsafe fn` trait-method signature has no `Symbol` of its own and the
    // innermost `DEFINES`-owned covering symbol is the containing trait.
    // The row's own file/span handle still cites the exact declaration.
    // Symbolizing signature-only declarations would be a repo-wide extractor
    // change outside this lane's scope (see docs/cli/unsafe-sites.md).
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(
        &repo,
        "src/lib.rs",
        "pub trait Device {\n    unsafe fn poke(&self);\n\n    fn name(&self) -> &str {\n        \"device\"\n    }\n}\n",
    );
    commit(&repo, "seed trait", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("unsafe-trait-signature-fixture"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");

    let (envelope, _) = run_lane(&graph_path, &[]);
    assert_eq!(envelope["counts"]["total"], 1);
    assert_eq!(envelope["counts"]["fn"], 1);
    let site = &envelope["sites"][0];
    assert_eq!(site["site_kind"], "fn");
    // The site's own citable handle is the exact signature declaration
    // (`unsafe fn poke(&self);` sits on line 2 of the fixture).
    assert_eq!(site["span"]["start_line"], 2);
    assert_eq!(site["span"]["end_line"], 2);
    // The enclosing symbol is the containing trait, because no Symbol record
    // exists for the signature-only method.
    assert_eq!(site["enclosing_symbol"]["name"], "Device");
    assert_eq!(site["enclosing_symbol"]["symbol_kind"], "trait");
}

// ---------------------------------------------------------------------------
// Enclosing symbol: repository-boundary honesty
// ---------------------------------------------------------------------------

/// Repo alpha: the true owner of the unsafe site. The function is made wide
/// so a narrower same-path symbol from another repository would win an
/// ownership-blind innermost-span pick.
const COLLIDE_ALPHA_RS: &str = r"pub fn alpha_owner(register: *const u32) -> u32 {
    let padding_alpha_one = 1u32;
    let padding_alpha_two = 2u32;
    let padding_alpha_three = 3u32;
    let padding_alpha_four = 4u32;
    let _ = (padding_alpha_one, padding_alpha_two);
    let _ = (padding_alpha_three, padding_alpha_four);
    let observed = unsafe { register.read_volatile() };
    let padding_alpha_five = 5u32;
    let padding_alpha_six = 6u32;
    let _ = (padding_alpha_five, padding_alpha_six);
    observed
}
";

/// Repo beta: shares the repo-relative path `src/lib.rs`, contains zero
/// unsafe code, and (after the leading comments) defines a symbol whose span
/// covers alpha's site byte range while being strictly narrower than
/// `alpha_owner`.
const COLLIDE_BETA_RS: &str = r"// beta padding comment line number one ......................................
// beta padding comment line number two ......................................
pub fn beta_owner(register: *const u32) -> u32 {
    let padding_beta_one = 1u32;
    let padding_beta_two = 2u32;
    let _ = (padding_beta_one, padding_beta_two);
    register.align_offset(4) as u32
}
";

#[test]
fn unsafe_sites_enclosing_symbol_never_bleeds_across_repositories() {
    // Two repositories share one store and both define `src/lib.rs`. Repo
    // beta's same-path symbol span contains alpha's site and is narrower
    // than alpha's true owner, so an ownership-blind innermost-span pick
    // would attach beta's symbol to alpha's unsafe site. The citable
    // enclosing symbol must come from the site's own repository.
    let temp = tempfile::tempdir().expect("temp dir");

    let repo_a = temp.path().join("alpha");
    fs::create_dir_all(&repo_a).expect("alpha repo dir");
    init_git(&repo_a);
    write(&repo_a, "src/lib.rs", COLLIDE_ALPHA_RS);
    commit(&repo_a, "seed alpha", "2026-01-01T00:00:00Z");

    let repo_b = temp.path().join("beta");
    fs::create_dir_all(&repo_b).expect("beta repo dir");
    init_git(&repo_b);
    write(&repo_b, "src/lib.rs", COLLIDE_BETA_RS);
    commit(&repo_b, "seed beta", "2026-01-01T00:00:00Z");

    let jsonl_a =
        scan_repository_at_with_override(&repo_a, "2026-01-01T00:00:00Z", Some("collide-alpha"))
            .expect("alpha repo should scan")
            .to_jsonl()
            .expect("alpha graph should serialize");
    let jsonl_b =
        scan_repository_at_with_override(&repo_b, "2026-01-01T00:00:00Z", Some("collide-beta"))
            .expect("beta repo should scan")
            .to_jsonl()
            .expect("beta graph should serialize");

    // Sanity: the trap is armed. Beta's same-path symbol must contain the
    // site span and be strictly narrower than alpha's owner, or this test
    // could pass without exercising the repository-boundary filter.
    let records: Vec<GraphRecord> = format!("{jsonl_a}{jsonl_b}")
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();
    let span_of = |name: &str| {
        records
            .iter()
            .find_map(|r| match r {
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name: Some(n),
                    span: Some(s),
                    ..
                } if n == name => Some(*s),
                _ => None,
            })
            .unwrap_or_else(|| panic!("symbol {name} should exist with a span"))
    };
    let site_span = records
        .iter()
        .find_map(|r| match r {
            GraphRecord::Node {
                kind: NodeKind::UnsafeSite,
                span: Some(s),
                ..
            } => Some(*s),
            _ => None,
        })
        .expect("alpha's unsafe site should exist");
    let alpha_span = span_of("alpha_owner");
    let beta_span = span_of("beta_owner");
    assert!(
        beta_span.start_byte <= site_span.start_byte
            && site_span.end_byte <= beta_span.end_byte
            && (beta_span.end_byte - beta_span.start_byte)
                < (alpha_span.end_byte - alpha_span.start_byte),
        "fixture must arm the cross-repo trap: beta {beta_span:?} must contain \
         site {site_span:?} and be narrower than alpha {alpha_span:?}"
    );

    let graph_path = temp.path().join("combined.jsonl");
    fs::write(&graph_path, format!("{jsonl_a}{jsonl_b}")).expect("combined graph should write");

    let (envelope, stdout) = run_lane(&graph_path, &[]);
    assert_eq!(
        envelope["counts"]["total"], 1,
        "only alpha's unsafe block exists across both repositories"
    );
    let site = &envelope["sites"][0];
    assert_eq!(
        site["enclosing_symbol"]["name"], "alpha_owner",
        "the enclosing symbol must come from the site's own repository"
    );
    assert!(
        !stdout.contains("beta_owner"),
        "a same-path symbol from another repository must never be attached \
         as the citable enclosing symbol: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Embedded store round-trip
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn unsafe_sites_round_trips_through_embedded_store() {
    let (temp, graph) = fixture_scanned();
    let data_dir = temp.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let output = egregore()
        .args(["query", "unsafe-sites", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("query should execute");
    assert!(
        output.status.success(),
        "embedded query failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be one JSON envelope");
    assert_eq!(envelope["counts"]["total"], 4);
    assert_eq!(envelope["counts"]["block"], 1);
    assert_eq!(envelope["counts"]["fn"], 2);
    assert_eq!(envelope["counts"]["impl"], 1);
}
