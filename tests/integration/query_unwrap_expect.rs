//! Integration tests for `eg query unwrap-expect` (issue #223).
//!
//! The lane inventories `.unwrap()` / `.expect()` panic-risk method-call
//! sites detected by the Tree-sitter extractor: real call expressions only
//! (never comment / string / doc-comment / identifier decoys), each carrying
//! a machine-readable category, a production-vs-test context, a repo-relative
//! file/span handle, and the enclosing symbol handle when one exists.
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

/// Production and test call sites plus every decoy class from the issue #223
/// acceptance criteria: `.unwrap()` inside a `//` comment, inside a string
/// literal, inside a doc comment, and unrelated `unwrap` identifiers.
const LIB_RS: &str = r#"/// Doc-comment decoy: the text .unwrap() here is prose, not a call.
pub fn parse_port(input: &str) -> u16 {
    // Comment decoy: .unwrap() inside a line comment.
    let string_decoy = "string decoy: .unwrap() and .expect(\"x\")";
    let _ = string_decoy;
    let fallback: u16 = input.trim().parse().unwrap_or(0);
    let _ = fallback;
    let port: u16 = input.trim().parse().unwrap();
    port
}

pub fn read_config(path: &str) -> String {
    std::fs::read_to_string(path).expect("config file must exist")
}

/// Identifier decoy: a plain function named `unwrap`, invoked without method
/// syntax.
fn unwrap(value: u32) -> u32 {
    value
}

pub fn identifier_decoy() -> u32 {
    let unwrap_count = unwrap(1);
    unwrap_count
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_port() {
        let parsed: u16 = "80".parse().unwrap();
        assert_eq!(parsed, 80);
    }
}
"#;

const CLEAN_RS: &str = "pub fn clean() -> u32 {\n    7\n}\n";

const TESTS_PROBE_RS: &str =
    "pub fn probe() -> u32 {\n    \"7\".parse::<u32>().expect(\"digit\")\n}\n";

/// Builds the seeded fixture checkout, scans it deterministically, and writes
/// the JSONL graph. Returns (`TempDir`, `graph_path`).
fn fixture_scanned() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(&repo, "src/lib.rs", LIB_RS);
    write(&repo, "src/empty/clean.rs", CLEAN_RS);
    write(&repo, "tests/integration_probe.rs", TESTS_PROBE_RS);
    commit(&repo, "seed fixture", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("unwrap-expect-fixture"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");
    (temp, graph_path)
}

fn run_lane(graph: &Path, extra: &[&str]) -> (Value, String) {
    let mut cmd = egregore();
    cmd.args(["query", "unwrap-expect", "--graph"]).arg(graph);
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

/// (`category`, `context`, `repo_relative_path`, enclosing symbol name or `None`).
fn site_tuples(envelope: &Value) -> Vec<(String, String, String, Option<String>)> {
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
                site["category"].as_str().expect("category").to_owned(),
                site["context"].as_str().expect("context").to_owned(),
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
fn unwrap_expect_returns_real_sites_and_never_decoys() {
    let (_temp, graph) = fixture_scanned();
    let (envelope, _) = run_lane(&graph, &[]);

    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["lane"], "unwrap_expect");
    assert_eq!(
        envelope["method_set"],
        serde_json::json!(["expect", "unwrap"]),
        "the known-risk method set is closed for this slice"
    );
    assert!(
        envelope["disclaimer"]
            .as_str()
            .expect("disclaimer")
            .contains("advisory"),
        "results must be labeled advisory"
    );

    // Corpus disclosure (issue #427): a real `eg scan` git store carries a
    // source_snapshot, so with no `--at` this lane discloses the union it reads.
    assert_eq!(envelope["corpus_mode"], "union");
    assert_eq!(envelope["corpus_mode_source"], "default");
    assert!(
        envelope["corpus_disclaimer"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
    );

    let tuples = site_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/lib.rs".to_owned(),
                Some("parse_port".to_owned()),
            ),
            (
                "expect".to_owned(),
                "production".to_owned(),
                "src/lib.rs".to_owned(),
                Some("read_config".to_owned()),
            ),
            (
                "unwrap".to_owned(),
                "test".to_owned(),
                "src/lib.rs".to_owned(),
                Some("tests::parses_port".to_owned()),
            ),
            (
                "expect".to_owned(),
                "test".to_owned(),
                "tests/integration_probe.rs".to_owned(),
                Some("probe".to_owned()),
            ),
        ],
        "the lane must return exactly the real call expressions: no comment, \
         string, doc-comment, or identifier decoys, and no unwrap_or variants"
    );

    assert_eq!(envelope["counts"]["total"], 4);
    assert_eq!(envelope["counts"]["unwrap"], 2);
    assert_eq!(envelope["counts"]["expect"], 2);
    assert_eq!(envelope["counts"]["production"], 2);
    assert_eq!(envelope["counts"]["test"], 2);

    for site in envelope["sites"].as_array().expect("sites") {
        assert_eq!(site["kind"], "PanicRiskSite");
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

    // The unwrap decoy line (comment) is line 3 and the string decoy is line 4
    // of src/lib.rs; no returned span may start there.
    for site in envelope["sites"].as_array().expect("sites") {
        if site["repo_relative_path"] == "src/lib.rs" {
            let start_line = site["span"]["start_line"].as_u64().expect("line");
            assert!(
                !matches!(start_line, 1 | 3 | 4),
                "decoy line {start_line} must never be returned"
            );
        }
    }
}

#[test]
fn unwrap_expect_scan_emits_panic_risk_site_records() {
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
                    kind: NodeKind::PanicRiskSite,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(sites.len(), 4, "scan must emit one record per call site");
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
        assert!(matches!(name.as_deref(), Some("unwrap" | "expect")));
        assert_eq!(language.as_deref(), Some("rust"));
        assert!(span.is_some(), "each site must carry a source span");
        assert!(repo_relative_path.is_some());
    }
}

/// A freshly scanned graph containing panic-risk sites is referentially
/// closed: the `File CONTAINS PanicRiskSite` topology must pass the issue
/// #103 pre-ingest validation gate, never trip `edge_target_kind_violation`.
#[test]
fn unwrap_expect_scanned_graph_passes_validate() {
    let (_temp, graph) = fixture_scanned();
    egregore()
        .arg("validate")
        .arg(&graph)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ok\":true"));
}

// ---------------------------------------------------------------------------
// Scope: prefix filter, empty-vs-not-found honesty, --repo
// ---------------------------------------------------------------------------

#[test]
fn unwrap_expect_prefix_scopes_results_segment_aware() {
    let (_temp, graph) = fixture_scanned();
    let (envelope, _) = run_lane(&graph, &["--path", "tests"]);
    let tuples = site_tuples(&envelope);
    assert_eq!(tuples.len(), 1);
    assert_eq!(tuples[0].2, "tests/integration_probe.rs");
    assert_eq!(envelope["path_prefix"], "tests");
}

#[test]
fn unwrap_expect_empty_scope_is_distinguished_from_scope_not_found() {
    let (_temp, graph) = fixture_scanned();

    // Scope exists but contains zero sites: ok envelope with a stable
    // machine-readable empty reason, exit 0.
    let (envelope, _) = run_lane(&graph, &["--path", "src/empty"]);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["sites"], serde_json::json!([]));
    assert_eq!(envelope["counts"]["total"], 0);
    assert_eq!(envelope["empty_reason"], "no_sites_in_scope");

    // Scope not present in the store: scope_not_found, exit 2 — never a
    // silent empty result (issue #196 honesty contract).
    egregore()
        .args(["query", "unwrap-expect", "--graph"])
        .arg(&graph)
        .args(["--path", "src/nonexistent"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"scope_not_found\""));

    // Sibling-prefix bleed: `src/emp` must not match `src/empty`.
    egregore()
        .args(["query", "unwrap-expect", "--graph"])
        .arg(&graph)
        .args(["--path", "src/emp"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"scope_not_found\""));

    // Malformed prefix: exit 1 with a machine-readable diagnostic.
    egregore()
        .args(["query", "unwrap-expect", "--graph"])
        .arg(&graph)
        .args(["--path", "/"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("\"malformed_prefix\""));
}

#[test]
fn unwrap_expect_repo_selector_scopes_and_rejects_unknown() {
    let (_temp, graph) = fixture_scanned();

    // The override identity resolves as a repo selector.
    let (envelope, _) = run_lane(&graph, &["--repo", "unwrap-expect-fixture"]);
    assert_eq!(envelope["counts"]["total"], 4);

    // An unknown selector is a documented diagnostic, never a silent empty.
    egregore()
        .args(["query", "unwrap-expect", "--graph"])
        .arg(&graph)
        .args(["--repo", "no-such-repo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown_repository_selector"));
}

// ---------------------------------------------------------------------------
// Context classification: closed test-attribute contract
// ---------------------------------------------------------------------------

/// `#[cfg(not(test))]` and `#[cfg_attr(test, ...)]` functions are production
/// context; only `#[test]` and path attributes ending in `::test` (e.g.
/// `#[tokio::test]`) mark a function as test context.
const CFG_SHAPES_RS: &str = r#"#[cfg(not(test))]
pub fn not_test_guard() -> u32 {
    "1".parse().unwrap()
}

#[cfg_attr(test, allow(dead_code))]
pub fn attr_guard() -> u32 {
    "2".parse::<u32>().expect("digit")
}

#[tokio::test]
async fn tokio_style() {
    let _: u32 = "3".parse().unwrap();
}
"#;

#[test]
fn unwrap_expect_context_uses_closed_test_attribute_contract() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(&repo, "src/cfg_shapes.rs", CFG_SHAPES_RS);
    commit(&repo, "seed cfg shapes", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("unwrap-expect-cfg-shapes"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");

    let (envelope, _) = run_lane(&graph_path, &["--path", "src/cfg_shapes.rs"]);
    let tuples = site_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/cfg_shapes.rs".to_owned(),
                Some("cfg_shapes::not_test_guard".to_owned()),
            ),
            (
                "expect".to_owned(),
                "production".to_owned(),
                "src/cfg_shapes.rs".to_owned(),
                Some("cfg_shapes::attr_guard".to_owned()),
            ),
            (
                "unwrap".to_owned(),
                "test".to_owned(),
                "src/cfg_shapes.rs".to_owned(),
                Some("cfg_shapes::tokio_style".to_owned()),
            ),
        ],
        "#[cfg(not(test))] and #[cfg_attr(test, ...)] must classify as \
         production; #[tokio::test]-style attributes as test"
    );
}

#[test]
fn unwrap_expect_marks_out_of_line_cfg_test_modules_as_test_context() {
    // `#[cfg(test)] mod tests;` in a parent file puts the module body in
    // src/tests.rs — its sites (and those of its transitive out-of-line
    // submodules) are test context. A non-gated out-of-line module stays
    // production.
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(
        &repo,
        "src/lib.rs",
        "#[cfg(test)]\nmod tests;\nmod regular;\n\npub fn prod() -> u32 {\n    \"1\".parse().unwrap()\n}\n",
    );
    write(
        &repo,
        "src/tests.rs",
        "mod nested;\n\npub fn helper() -> u32 {\n    \"2\".parse().unwrap()\n}\n",
    );
    write(
        &repo,
        "src/tests/nested.rs",
        "pub fn deep() -> u32 {\n    \"3\".parse::<u32>().expect(\"digit\")\n}\n",
    );
    write(
        &repo,
        "src/regular.rs",
        "pub fn ordinary() -> u32 {\n    \"4\".parse().unwrap()\n}\n",
    );
    commit(&repo, "seed out-of-line modules", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("unwrap-expect-out-of-line"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");

    let (envelope, _) = run_lane(&graph_path, &[]);
    let tuples = site_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/lib.rs".to_owned(),
                Some("prod".to_owned()),
            ),
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/regular.rs".to_owned(),
                Some("regular::ordinary".to_owned()),
            ),
            (
                "unwrap".to_owned(),
                "test".to_owned(),
                "src/tests.rs".to_owned(),
                Some("tests::helper".to_owned()),
            ),
            (
                "expect".to_owned(),
                "test".to_owned(),
                "src/tests/nested.rs".to_owned(),
                Some("tests::nested::deep".to_owned()),
            ),
        ],
        "out-of-line #[cfg(test)] modules and their transitive out-of-line \
         submodules are test context; ungated out-of-line modules are not"
    );
}

#[test]
fn unwrap_expect_resolves_inline_nested_path_override_against_module_dir() {
    // Per the Rust reference, a `#[path]` attribute on a declaration inside an
    // inline module block resolves relative to the module directory plus the
    // inline components: `mod parent { #[path = "child.rs"] mod child; }` in
    // src/lib.rs loads src/parent/child.rs — never src/child.rs. The decoy at
    // src/child.rs must stay production; the real child must be test context.
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(
        &repo,
        "src/lib.rs",
        "mod parent {\n    #[cfg(test)]\n    #[path = \"child.rs\"]\n    mod child;\n}\n\npub fn prod() -> u32 {\n    \"1\".parse().unwrap()\n}\n",
    );
    write(
        &repo,
        "src/parent/child.rs",
        "pub fn hidden() -> u32 {\n    \"2\".parse().unwrap()\n}\n",
    );
    write(
        &repo,
        "src/child.rs",
        "pub fn decoy() -> u32 {\n    \"3\".parse().unwrap()\n}\n",
    );
    commit(&repo, "seed inline path override", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("unwrap-expect-inline-path"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");

    let (envelope, _) = run_lane(&graph_path, &[]);
    let tuples = site_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/child.rs".to_owned(),
                Some("child::decoy".to_owned()),
            ),
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/lib.rs".to_owned(),
                Some("prod".to_owned()),
            ),
            (
                "unwrap".to_owned(),
                "test".to_owned(),
                "src/parent/child.rs".to_owned(),
                Some("parent::child::hidden".to_owned()),
            ),
        ],
        "an inline-nested #[path] override resolves against the module \
         directory plus the inline components, never the file's own directory"
    );
}

#[test]
fn unwrap_expect_normalizes_relative_path_override_components() {
    // A `#[path]` override may carry relative components: `mod tests {
    // #[path = "../support.rs"] mod support; }` in src/lib.rs resolves to
    // src/tests/../support.rs, which normalizes to src/support.rs. The pass
    // must probe the normalized path — and a `..` chain that would escape the
    // repository root is unresolvable, never a panic or a wrong probe.
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(
        &repo,
        "src/lib.rs",
        "#[cfg(test)]\nmod tests {\n    #[path = \"../support.rs\"]\n    mod support;\n}\n\n#[cfg(test)]\n#[path = \"../../escape.rs\"]\nmod escapee;\n\npub fn prod() -> u32 {\n    \"1\".parse().unwrap()\n}\n",
    );
    write(
        &repo,
        "src/support.rs",
        "pub fn support_helper() -> u32 {\n    \"2\".parse().unwrap()\n}\n",
    );
    commit(&repo, "seed relative path override", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("unwrap-expect-relative-path"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");

    let (envelope, _) = run_lane(&graph_path, &[]);
    let tuples = site_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/lib.rs".to_owned(),
                Some("prod".to_owned()),
            ),
            (
                "unwrap".to_owned(),
                "test".to_owned(),
                "src/support.rs".to_owned(),
                Some("support::support_helper".to_owned()),
            ),
        ],
        "`../` components in a #[path] override must normalize before probing \
         known paths; a root-escaping `..` chain is skipped, never a panic"
    );
}

#[test]
fn unwrap_expect_dual_use_module_file_stays_production() {
    // A module file loaded through BOTH a production declaration and a
    // test-gated one still compiles into the production build: its sites are
    // production panic risk and must never be hidden behind a test label.
    // Production takes precedence for dual-use files — a file reached via a
    // production chain stays production even when also reachable via a test
    // chain — and no duplicate both-context rows are emitted.
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(
        &repo,
        "src/lib.rs",
        "mod support;\n\n#[cfg(test)]\nmod tests {\n    #[path = \"../support.rs\"]\n    mod support_again;\n}\n\npub fn prod() -> u32 {\n    \"1\".parse().unwrap()\n}\n",
    );
    write(
        &repo,
        "src/support.rs",
        "mod deeper;\n\npub fn shared() -> u32 {\n    \"2\".parse().unwrap()\n}\n",
    );
    write(
        &repo,
        "src/support/deeper.rs",
        "pub fn shared_deep() -> u32 {\n    \"3\".parse().unwrap()\n}\n",
    );
    commit(&repo, "seed dual-use module", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("unwrap-expect-dual-use"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");

    let (envelope, _) = run_lane(&graph_path, &[]);
    let tuples = site_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/lib.rs".to_owned(),
                Some("prod".to_owned()),
            ),
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/support.rs".to_owned(),
                Some("support::shared".to_owned()),
            ),
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/support/deeper.rs".to_owned(),
                Some("support::deeper::shared_deep".to_owned()),
            ),
        ],
        "a dual-use module file (and its transitive production chain) stays \
         production: misclassifying it as test would hide production panic risk"
    );
    assert_eq!(envelope["counts"]["production"], 3);
    assert_eq!(envelope["counts"]["test"], 0);
    assert_eq!(envelope["counts"]["total"], 3, "no duplicate rows");
}

#[test]
fn unwrap_expect_crate_root_targeted_by_gated_path_stays_production() {
    // A conventional crate root (here a binary root under src/bin/) always
    // compiles into a production build, even when a test-gated #[path]
    // declaration also loads it as a test module. Its sites are production
    // panic risk and must never be hidden behind a test label.
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(
        &repo,
        "src/lib.rs",
        "#[cfg(test)]\n#[path = \"bin/tool.rs\"]\nmod tool;\n\npub fn prod() -> u32 {\n    \"1\".parse().unwrap()\n}\n",
    );
    write(
        &repo,
        "src/bin/tool.rs",
        "fn main() {\n    let _: u32 = \"2\".parse().unwrap();\n}\n",
    );
    commit(
        &repo,
        "seed gated crate-root target",
        "2026-01-01T00:00:00Z",
    );

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("unwrap-expect-crate-root"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");

    let (envelope, _) = run_lane(&graph_path, &[]);
    let tuples = site_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/bin/tool.rs".to_owned(),
                Some("main".to_owned()),
            ),
            (
                "unwrap".to_owned(),
                "production".to_owned(),
                "src/lib.rs".to_owned(),
                Some("prod".to_owned()),
            ),
        ],
        "a conventional crate root targeted by a test-gated #[path] \
         declaration keeps its production seed"
    );
    assert_eq!(envelope["counts"]["test"], 0);
}

// ---------------------------------------------------------------------------
// Enclosing symbol: repository-boundary honesty
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn unwrap_expect_enclosing_symbol_never_bleeds_across_repositories() {
    // Two repositories sharing the same repo-relative path. Repo B's symbol
    // span is tighter than repo A's, so a repository-blind innermost-span
    // lookup would wrongly pick it for repo A's site.
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("two-repos.jsonl");

    let node = |id: &str, kind: &str, extra: serde_json::Value| {
        let mut base = serde_json::json!({
            "record_type": "node",
            "id": id,
            "kind": kind,
            "schema_version": 5,
            "summary": format!("{kind} {id}"),
        });
        base.as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        base
    };
    let edge = |id: &str, label: &str, source: &str, target: &str| {
        serde_json::json!({
            "record_type": "edge",
            "id": id,
            "schema_version": 5,
            "label": label,
            "source": source,
            "target": target,
            "summary": format!("{source} {label} {target}"),
        })
    };

    let span = |start: usize, end: usize| {
        serde_json::json!({
            "start_byte": start, "end_byte": end, "start_line": 1, "end_line": 9
        })
    };

    let records = [
        node(
            "codegraph:v5:repoa",
            "Repository",
            serde_json::json!({"name": "repo-a"}),
        ),
        node(
            "codegraph:v5:repob",
            "Repository",
            serde_json::json!({"name": "repo-b"}),
        ),
        node(
            "codegraph:v5:filea",
            "File",
            serde_json::json!({"repo_relative_path": "src/lib.rs", "name": "src/lib.rs"}),
        ),
        node(
            "codegraph:v5:fileb",
            "File",
            serde_json::json!({"repo_relative_path": "src/lib.rs", "name": "src/lib.rs"}),
        ),
        node(
            "codegraph:v5:syma",
            "Symbol",
            serde_json::json!({
                "repo_relative_path": "src/lib.rs",
                "name": "alpha_owner",
                "symbol_kind": "function",
                "language": "rust",
                "span": span(0, 500),
            }),
        ),
        node(
            "codegraph:v5:symb",
            "Symbol",
            serde_json::json!({
                "repo_relative_path": "src/lib.rs",
                "name": "beta_tight",
                "symbol_kind": "function",
                "language": "rust",
                "span": span(90, 160),
            }),
        ),
        node(
            "codegraph:v5:sitea",
            "PanicRiskSite",
            serde_json::json!({
                "repo_relative_path": "src/lib.rs",
                "name": "unwrap",
                "call_context": "production",
                "language": "rust",
                "span": span(100, 120),
            }),
        ),
        edge(
            "codegraph:v5:e1",
            "CONTAINS",
            "codegraph:v5:repoa",
            "codegraph:v5:filea",
        ),
        edge(
            "codegraph:v5:e2",
            "CONTAINS",
            "codegraph:v5:repob",
            "codegraph:v5:fileb",
        ),
        edge(
            "codegraph:v5:e3",
            "DEFINES",
            "codegraph:v5:filea",
            "codegraph:v5:syma",
        ),
        edge(
            "codegraph:v5:e4",
            "DEFINES",
            "codegraph:v5:fileb",
            "codegraph:v5:symb",
        ),
        edge(
            "codegraph:v5:e5",
            "CONTAINS",
            "codegraph:v5:filea",
            "codegraph:v5:sitea",
        ),
    ];
    let jsonl = records
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&graph_path, jsonl).expect("seeded graph should write");

    for extra in [&[][..], &["--repo", "repo-a"][..]] {
        let (envelope, _) = run_lane(&graph_path, extra);
        assert_eq!(envelope["counts"]["total"], 1);
        assert_eq!(
            envelope["sites"][0]["enclosing_symbol"]["name"], "alpha_owner",
            "the enclosing symbol must come from the site's own repository, \
             never from a same-path symbol in another repository (args: {extra:?})"
        );
    }
}

#[test]
fn unwrap_expect_at_commit_pins_the_inventory_to_valid_time() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);

    write(&repo, "src/lib.rs", "pub fn calm() -> u32 {\n    1\n}\n");
    let first = commit(&repo, "no panic risk yet", "2026-01-01T00:00:00Z");

    write(
        &repo,
        "src/lib.rs",
        "pub fn risky() -> u32 {\n    \"1\".parse().unwrap()\n}\n",
    );
    let second = commit(&repo, "introduce unwrap", "2026-01-02T00:00:00Z");

    write(&repo, "src/lib.rs", "pub fn calm() -> u32 {\n    1\n}\n");
    let third = commit(&repo, "remove unwrap", "2026-01-03T00:00:00Z");

    let graph_path = temp.path().join("history.graph.jsonl");
    let jsonl = scan_repository_history_with_override(&repo, Some("unwrap-expect-history"))
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
    assert_eq!(tuples[0].0, "unwrap");
    assert_eq!(tuples[0].3, Some("risky".to_owned()));
    let site = &at_second["sites"][0];
    assert_eq!(site["git_commit"].as_str(), Some(second.as_str()));
    assert!(site["valid_time"].as_str().is_some());

    // Corpus disclosure (issue #427): `--at` pins a single commit, so the corpus
    // is commit-pinned and the source is the selector.
    assert_eq!(at_second["corpus_mode"], "commit_pinned");
    assert_eq!(at_second["corpus_mode_source"], "selector");

    // Pinned after the removal: gone again.
    let (at_third, _) = run_lane(&graph_path, &["--at", &third]);
    assert_eq!(at_third["counts"]["total"], 0);

    // A commit the store has never seen is a documented diagnostic (exit 2),
    // never a silent empty result.
    egregore()
        .args(["query", "unwrap-expect", "--graph"])
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
fn unwrap_expect_output_is_byte_identical_across_five_runs() {
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
fn unwrap_expect_query_is_read_only() {
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

/// The lane's read-only guarantee must hold for the embedded store too:
/// opening the engine in place re-persists index files, so the `--data-dir`
/// path must read from a throwaway copy and leave the live store
/// byte-for-byte untouched.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn unwrap_expect_query_is_read_only_for_embedded_store() {
    let (temp, graph) = fixture_scanned();
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
        .args(["query", "unwrap-expect", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    assert_eq!(
        dir_contents(&data_dir),
        bytes_before,
        "querying must leave the live embedded store byte-for-byte untouched"
    );
}

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
fn unwrap_expect_enclosing_symbol_is_explicit_null_when_top_level() {
    // Seeded fixture: a site record with no Symbol span covering it.
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("seeded.jsonl");
    let site = serde_json::json!({
        "record_type": "node",
        "id": "codegraph:v5:seededsite0000000000000000000000000000000000000000000000000001",
        "kind": "PanicRiskSite",
        "schema_version": 5,
        "repo_relative_path": "src/orphan.rs",
        "span": {"start_byte": 10, "end_byte": 30, "start_line": 2, "end_line": 2},
        "name": "unwrap",
        "language": "rust",
        "call_context": "production",
        "summary": "Rust .unwrap() panic-risk call site (production)"
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
// Embedded store round-trip
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn unwrap_expect_round_trips_through_embedded_store() {
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
        .args(["query", "unwrap-expect", "--data-dir"])
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
    let tuples = site_tuples(&envelope);
    assert_eq!(
        tuples
            .iter()
            .filter(|(_, context, _, _)| context == "test")
            .count(),
        2,
        "call_context must round-trip through the embedded adapter"
    );
}
