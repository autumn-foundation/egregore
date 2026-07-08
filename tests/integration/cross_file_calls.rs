#![allow(missing_docs)]
//! Cross-file call resolution (issue #152).
//!
//! `eg scan` must emit `CALLS` edges whose targets are defined in *other*
//! files of the same repository, labeled with a `resolution` status
//! (`resolved` / `ambiguous` / `unresolved`), without regressing the
//! comment/string/substring precision guarantees coordinated with #134.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{scan_repository_at_with_override, scan_repository_history_with_override};
use serde_json::Value;

const FIXED_TIME: &str = "2026-06-07T00:00:00Z";
const REPO_ID: &str = "cross-file-calls-fixture";

fn write_fixture(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
            .expect("fixture parent dir should be created");
        fs::write(path, contents).expect("fixture file should be written");
    }
}

fn scan_fixture(root: &Path) -> Vec<Value> {
    let jsonl = scan_repository_at_with_override(root, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    parse_jsonl(&jsonl)
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn symbol_id(records: &[Value], symbol_kind: &str, name: &str, path: &str) -> String {
    records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["symbol_kind"] == symbol_kind
                && record["name"] == name
                && record["repo_relative_path"] == path
        })
        .unwrap_or_else(|| panic!("missing {symbol_kind} symbol {name} in {path}"))["id"]
        .as_str()
        .expect("symbol should have an ID")
        .to_owned()
}

fn calls_edge<'a>(records: &'a [Value], source: &str, target: &str) -> Option<&'a Value> {
    records.iter().find(|record| {
        record["record_type"] == "edge"
            && record["label"] == "CALLS"
            && record["source"] == source
            && record["target"] == target
    })
}

fn assert_calls_edge_with_resolution(
    records: &[Value],
    source: &str,
    target: &str,
    resolution: &str,
) {
    let edge = calls_edge(records, source, target).unwrap_or_else(|| {
        panic!("missing CALLS edge from {source} to {target} (expected {resolution})")
    });
    assert_eq!(
        edge["resolution"], resolution,
        "CALLS edge from {source} to {target} should be labeled {resolution}, got: {edge}"
    );
}

#[test]
fn scan_emits_cross_file_call_edges_for_functions_and_methods() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub struct Widget {\n    pub value: usize,\n}\n\nimpl Widget {\n    pub fn render(&self) -> usize {\n        self.value\n    }\n}\n\npub fn shared_helper() -> usize {\n    7\n}\n",
            ),
            (
                "src/beta.rs",
                "use crate::alpha::shared_helper;\n\npub fn beta_caller() -> usize {\n    shared_helper()\n}\n",
            ),
            (
                "src/gamma.rs",
                "pub fn gamma_caller(w: &crate::alpha::Widget) -> usize {\n    crate::alpha::shared_helper() + w.render()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let helper = symbol_id(&records, "function", "alpha::shared_helper", "src/alpha.rs");
    let render = symbol_id(&records, "method", "alpha::Widget::render", "src/alpha.rs");
    let beta_caller = symbol_id(&records, "function", "beta::beta_caller", "src/beta.rs");
    let gamma_caller = symbol_id(&records, "function", "gamma::gamma_caller", "src/gamma.rs");

    // Direct call through an import, path-qualified call, and method call all
    // resolve across the file boundary to the single in-repo definition.
    assert_calls_edge_with_resolution(&records, &beta_caller, &helper, "resolved");
    assert_calls_edge_with_resolution(&records, &gamma_caller, &helper, "resolved");
    assert_calls_edge_with_resolution(&records, &gamma_caller, &render, "resolved");
}

#[test]
fn ambiguous_simple_names_emit_labeled_edges_to_all_candidates() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            ("src/alpha.rs", "pub fn dupe() -> usize {\n    1\n}\n"),
            ("src/delta.rs", "pub fn dupe() -> usize {\n    2\n}\n"),
            (
                "src/beta.rs",
                "pub fn calls_dupe() -> usize {\n    dupe()\n}\n",
            ),
            (
                "src/epsilon.rs",
                "pub fn calls_alpha_dupe() -> usize {\n    crate::alpha::dupe()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let alpha_dupe = symbol_id(&records, "function", "alpha::dupe", "src/alpha.rs");
    let delta_dupe = symbol_id(&records, "function", "delta::dupe", "src/delta.rs");
    let calls_dupe = symbol_id(&records, "function", "beta::calls_dupe", "src/beta.rs");
    let calls_alpha_dupe = symbol_id(
        &records,
        "function",
        "epsilon::calls_alpha_dupe",
        "src/epsilon.rs",
    );

    // An unqualified call matching two in-repo definitions is labeled
    // ambiguous and carries an edge to every candidate.
    assert_calls_edge_with_resolution(&records, &calls_dupe, &alpha_dupe, "ambiguous");
    assert_calls_edge_with_resolution(&records, &calls_dupe, &delta_dupe, "ambiguous");

    // A path-qualified call narrows to exactly one candidate.
    assert_calls_edge_with_resolution(&records, &calls_alpha_dupe, &alpha_dupe, "resolved");
    assert!(
        calls_edge(&records, &calls_alpha_dupe, &delta_dupe).is_none(),
        "path-qualified call must not fan out to the non-matching candidate"
    );
}

#[test]
fn unresolved_external_calls_are_labeled_not_dropped_or_invented() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            ("src/alpha.rs", "pub fn in_repo() -> usize {\n    1\n}\n"),
            (
                "src/beta.rs",
                "pub fn uses_external() -> usize {\n    external_dep::render_widget()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let uses_external = symbol_id(&records, "function", "beta::uses_external", "src/beta.rs");

    // The unresolved call is recorded against a Diagnostic node, never an
    // invented in-repo Symbol.
    let diagnostic = records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Diagnostic"
                && record["name"] == "external_dep::render_widget"
                && record["repo_relative_path"] == "src/beta.rs"
        })
        .expect("unresolved call should emit a Diagnostic node");
    let diagnostic_id = diagnostic["id"]
        .as_str()
        .expect("diagnostic should have ID");
    assert_calls_edge_with_resolution(&records, &uses_external, diagnostic_id, "unresolved");

    let symbol_ids: Vec<&str> = records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "Symbol")
        .filter_map(|record| record["id"].as_str())
        .collect();
    for target in symbol_ids {
        assert!(
            calls_edge(&records, &uses_external, target).is_none(),
            "unresolved external call must not invent an edge to in-repo symbol {target}"
        );
    }
}

#[test]
fn comment_string_and_substring_mentions_produce_no_cross_file_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub fn quiet_target() -> usize {\n    3\n}\n",
            ),
            (
                "src/beta.rs",
                "// quiet_target() is discussed in this comment only\npub fn documented() -> &'static str {\n    \"call quiet_target() later\"\n}\n\npub fn quiet_target_extended() -> usize {\n    4\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let quiet_target = symbol_id(&records, "function", "alpha::quiet_target", "src/alpha.rs");

    let offending: Vec<&Value> = records
        .iter()
        .filter(|record| {
            record["record_type"] == "edge"
                && record["target"] == quiet_target.as_str()
                && matches!(
                    record["label"].as_str(),
                    Some("CALLS" | "REFERENCES" | "MENTIONS")
                )
        })
        .collect();
    assert!(
        offending.is_empty(),
        "comment/string/substring occurrences must not produce cross-file edges: {offending:?}"
    );
}

#[test]
fn cross_file_edges_are_byte_stable_across_repeated_scans() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub fn shared_helper() -> usize {\n    7\n}\npub fn dupe() {}\n",
            ),
            (
                "src/beta.rs",
                "pub fn beta_caller() -> usize {\n    shared_helper() + external_dep::widget()\n}\npub fn dupe() {}\npub fn calls_dupe() {\n    dupe();\n}\n",
            ),
        ],
    );

    let first = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    for run in 2..=5 {
        let next = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
            .expect("fixture repo should rescan")
            .to_jsonl()
            .expect("graph should reserialize");
        assert_eq!(first, next, "scan {run} must be byte-identical to scan 1");
    }
    assert!(
        first.contains(r#""resolution":"resolved""#)
            && first.contains(r#""resolution":"ambiguous""#)
            && first.contains(r#""resolution":"unresolved""#),
        "stability check must cover all three resolution statuses: {first}"
    );
}

#[test]
fn incremental_scan_emits_and_retires_cross_file_call_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    write_fixture(
        &repo,
        &[
            (
                "src/alpha.rs",
                "pub fn shared_helper() -> usize {\n    7\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn beta_caller() -> usize {\n    shared_helper()\n}\n",
            ),
        ],
    );
    let cache_path = temp.path().join("codegraph-cache.json");

    let first = aletheia_egregore::incremental::scan_repository_incremental_at(
        &repo,
        &cache_path,
        FIXED_TIME,
    )
    .expect("first incremental scan should work");
    let first_records = parse_jsonl(
        &first
            .graph
            .to_jsonl()
            .expect("first incremental graph should serialize"),
    );
    let helper = symbol_id(
        &first_records,
        "function",
        "alpha::shared_helper",
        "src/alpha.rs",
    );
    let beta_caller = symbol_id(
        &first_records,
        "function",
        "beta::beta_caller",
        "src/beta.rs",
    );
    assert_calls_edge_with_resolution(&first_records, &beta_caller, &helper, "resolved");
    let edge_id = calls_edge(&first_records, &beta_caller, &helper)
        .expect("cross-file edge should exist")["id"]
        .as_str()
        .expect("edge should have ID")
        .to_owned();

    // Removing the call retires the edge with a tombstone on the next scan.
    fs::write(
        repo.join("src/beta.rs"),
        "pub fn beta_caller() -> usize {\n    9\n}\n",
    )
    .expect("fixture should update");
    let second = aletheia_egregore::incremental::scan_repository_incremental_at(
        &repo,
        &cache_path,
        "2026-06-08T00:00:00Z",
    )
    .expect("second incremental scan should work");
    let second_records = parse_jsonl(
        &second
            .graph
            .to_jsonl()
            .expect("second incremental graph should serialize"),
    );
    assert!(
        calls_edge(&second_records, &beta_caller, &helper).is_none(),
        "removed call must not re-emit the cross-file edge"
    );
    assert!(
        second_records.iter().any(|record| {
            record["record_type"] == "tombstone" && record["deleted_id"] == edge_id.as_str()
        }),
        "stale cross-file edge must be tombstoned so persisted stores can retire it"
    );
}

#[test]
fn history_replay_emits_cross_file_call_edges_per_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "test@example.invalid"]);
    git(&repo, ["config", "user.name", "Test"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);
    write_fixture(
        &repo,
        &[
            (
                "src/alpha.rs",
                "pub fn shared_helper() -> usize {\n    7\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn beta_caller() -> usize {\n    shared_helper()\n}\n",
            ),
        ],
    );
    commit_with_date(&repo, "seed", "2026-01-01T00:00:00Z");

    let records = parse_jsonl(
        &scan_repository_history_with_override(&repo, Some("cross-file-history-fixture"))
            .expect("history fixture should scan")
            .to_jsonl()
            .expect("history graph should serialize"),
    );
    let helper = symbol_id(&records, "function", "alpha::shared_helper", "src/alpha.rs");
    let beta_caller = symbol_id(&records, "function", "beta::beta_caller", "src/beta.rs");
    let edge = calls_edge(&records, &beta_caller, &helper)
        .expect("history replay should emit the cross-file CALLS edge");
    assert_eq!(edge["resolution"], "resolved");
    assert!(
        edge["temporal"]["git_commit"].is_string(),
        "history cross-file edge should carry commit provenance: {edge}"
    );
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        out.status.success(),
        "git command {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit_with_date(repo: &Path, message: &str, date: &str) {
    git(repo, ["add", "."]);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[allow(dead_code)]
fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}
