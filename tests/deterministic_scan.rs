#![allow(missing_docs)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{
    scan_repository, scan_repository_at_with_override, scan_repository_history_with_override,
};
use serde_json::Value;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

fn shifted_fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic_shifted")
}

fn impl_collision_fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_impl_collision")
}

#[test]
fn deterministic_scan_produces_stable_jsonl() {
    let repo = fixture_repo();
    // Fix the transaction time so that valid_time in output is identical across both calls.
    let fixed_time = "2026-05-19T00:00:00Z";

    // Use both a fixed time and --repo-id-override so the output is deterministic
    // across machines and repeated runs regardless of directory name or wall clock.
    let first =
        scan_repository_at_with_override(&repo, fixed_time, Some("fixture-stable-test-repo"))
            .expect("fixture repo should scan")
            .to_jsonl()
            .expect("graph should serialize");
    let second =
        scan_repository_at_with_override(&repo, fixed_time, Some("fixture-stable-test-repo"))
            .expect("fixture repo should scan")
            .to_jsonl()
            .expect("graph should serialize");

    assert_eq!(first, second);
    assert!(first.contains(r#""kind":"Repository""#));
    assert!(first.contains(r#""kind":"File""#));
    assert!(first.contains(r#""repo_relative_path":"src/lib.rs""#));

    let manifest_dir = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
    let portable_jsonl = first.replace('\\', "/");
    assert!(
        !portable_jsonl.contains(&manifest_dir),
        "absolute manifest dir leaked into JSONL: {portable_jsonl}"
    );

    let lines = first.lines().collect::<Vec<_>>();
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(lines, sorted, "JSONL records should be canonically ordered");
}

#[test]
fn symbol_ids_survive_non_semantic_byte_shift() {
    let fixed_time = "2026-05-19T00:00:00Z";
    let repo_id = Some("symbol-stability-fixture");
    let base = scan_repository_at_with_override(fixture_repo(), fixed_time, repo_id)
        .expect("base fixture should scan")
        .to_jsonl()
        .expect("base graph should serialize");
    let shifted = scan_repository_at_with_override(shifted_fixture_repo(), fixed_time, repo_id)
        .expect("shifted fixture should scan")
        .to_jsonl()
        .expect("shifted graph should serialize");

    let base_records = parse_jsonl(&base);
    let shifted_records = parse_jsonl(&shifted);
    let base_symbols = symbol_records_by_id_without_span(&base_records);
    let shifted_symbols = symbol_records_by_id_without_span(&shifted_records);

    assert_eq!(
        base_symbols.keys().collect::<BTreeSet<_>>(),
        shifted_symbols.keys().collect::<BTreeSet<_>>(),
        "prepended comments must not change Symbol record IDs"
    );
    assert_eq!(
        base_symbols, shifted_symbols,
        "after removing span, shifted fixture Symbol records should be identical"
    );

    let base_spans = symbol_spans_by_id(&base_records);
    let shifted_spans = symbol_spans_by_id(&shifted_records);
    assert!(
        base_spans
            .iter()
            .any(|(id, span)| shifted_spans.get(id) != Some(span)),
        "shifted fixture should move at least one Symbol span"
    );
    assert_eq!(
        diagnostic_ids(&base_records),
        diagnostic_ids(&shifted_records),
        "macro diagnostic IDs should also survive byte shifts"
    );
}

#[test]
fn duplicate_impl_symbols_get_source_order_disambiguators() {
    let fixed_time = "2026-05-19T00:00:00Z";
    let repo = impl_collision_fixture_repo();
    let first = scan_repository_at_with_override(&repo, fixed_time, Some("impl-collision-fixture"))
        .expect("impl collision fixture should scan")
        .to_jsonl()
        .expect("first graph should serialize");
    let second =
        scan_repository_at_with_override(&repo, fixed_time, Some("impl-collision-fixture"))
            .expect("impl collision fixture should rescan")
            .to_jsonl()
            .expect("second graph should serialize");

    let first_records = parse_jsonl(&first);
    let second_records = parse_jsonl(&second);
    let first_impls = impl_widget_symbols_by_disambiguator(&first_records);
    let second_impls = impl_widget_symbols_by_disambiguator(&second_records);

    assert_eq!(
        first_impls.keys().copied().collect::<Vec<_>>(),
        vec![0, 1],
        "two impl Widget blocks should be assigned source-order disambiguators 0 and 1"
    );
    assert_ne!(
        first_impls.get(&0),
        first_impls.get(&1),
        "colliding impl Widget symbols must remain distinct"
    );
    assert_eq!(
        first_impls, second_impls,
        "impl disambiguator assignment must be stable across rescans"
    );
}

#[test]
fn scan_history_symbol_ids_survive_header_only_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first_commit, second_commit] = seed_header_shift_history_repo(&repo);

    let jsonl = scan_repository_history_with_override(&repo, Some("history-symbol-stability"))
        .expect("history fixture should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);

    let symbols_by_commit = symbol_ids_by_commit(&records);
    let first_symbols = symbols_by_commit
        .get(&first_commit)
        .expect("first commit should emit symbols");
    let second_symbols = symbols_by_commit
        .get(&second_commit)
        .expect("second commit should emit symbols");

    assert!(
        !first_symbols.is_empty(),
        "history fixture should emit symbols"
    );
    assert_eq!(
        first_symbols, second_symbols,
        "header-only history edits must keep identical Symbol IDs across commits"
    );
    assert_edges_point_to_existing_nodes(&records);
}

#[test]
fn rust_fixture_covers_common_symbols() {
    let repo = fixture_repo();

    let jsonl = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    assert_node(&records, "Module", "nested");
    assert_import(&records, "std::fmt::Debug");
    assert_import(&records, "super::Debug");

    assert_symbol(&records, "function", "answer");
    assert_symbol(&records, "function", "nested::helper");
    assert_symbol(&records, "function", "nested::debug_name");
    assert_symbol(&records, "struct", "nested::Widget");
    assert_symbol(&records, "enum", "nested::Mode");
    assert_symbol(&records, "trait", "nested::Runner");
    assert_symbol(&records, "impl", "nested::impl Widget");
    assert_symbol(&records, "impl", "nested::impl Runner for Widget");
    assert_symbol(&records, "method", "nested::Widget::new");
    assert_symbol(&records, "method", "nested::Widget::value");
    assert_symbol(&records, "method", "nested::Runner for Widget::run");
    assert_symbol(&records, "const", "nested::LIMIT");
    assert_symbol(&records, "static", "nested::NAME");
    assert_symbol(&records, "type_alias", "nested::Alias");
    assert_symbol(&records, "test", "nested::widget_runs");

    assert_diagnostic(&records, "macro invocation local_macro!");

    assert_edge_label(&records, "CONTAINS");
    assert_edge_label(&records, "DEFINES");
    assert_edge_label(&records, "IMPORTS");
    assert_edge_label(&records, "IMPLEMENTS");
    assert_edge_label(&records, "CALLS");
    assert_edge_label(&records, "MENTIONS");
}

#[test]
fn rust_basic_edges_point_to_valid_endpoint_ids_after_symbol_identity_change() {
    let repo = fixture_repo();
    let jsonl = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    assert_edges_point_to_existing_nodes(&records);
    for label in [
        "CONTAINS",
        "DEFINES",
        "IMPORTS",
        "IMPLEMENTS",
        "CALLS",
        "MENTIONS",
    ] {
        assert_edge_label(&records, label);
    }
}

#[test]
fn split_module_file_symbols_are_qualified_from_repo_path() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir should be created");
    fs::write(repo.join("src/lib.rs"), "pub mod foo;\npub fn bar() {}\n")
        .expect("lib.rs should be written");
    fs::write(repo.join("src/foo.rs"), "pub fn bar() {}\n").expect("foo.rs should be written");

    let jsonl = scan_repository(repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    assert_symbol_in_path(&records, "function", "bar", "src/lib.rs");
    assert_symbol_in_path(&records, "function", "foo::bar", "src/foo.rs");
    assert!(
        !has_symbol_in_path(&records, "function", "bar", "src/foo.rs"),
        "split module function should not collide with root bar"
    );
}

#[test]
fn cargo_crate_root_files_are_not_qualified_as_modules() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src/bin")).expect("src/bin dir should be created");
    fs::create_dir_all(repo.join("src/bin/daemon")).expect("directory bin dir should be created");
    fs::create_dir_all(repo.join("tests")).expect("tests dir should be created");
    fs::create_dir_all(repo.join("examples")).expect("examples dir should be created");
    fs::create_dir_all(repo.join("benches")).expect("benches dir should be created");
    fs::write(repo.join("src/lib.rs"), "pub fn library() {}\n").expect("lib.rs should be written");
    fs::write(repo.join("src/bin/tool.rs"), "fn main() {}\n")
        .expect("bin target should be written");
    fs::write(
        repo.join("src/bin/daemon/main.rs"),
        "mod config;\nfn main() { config::load(); }\n",
    )
    .expect("directory bin main should be written");
    fs::write(repo.join("src/bin/daemon/config.rs"), "pub fn load() {}\n")
        .expect("directory bin module should be written");
    fs::write(repo.join("tests/foo.rs"), "#[test]\nfn smoke() {}\n")
        .expect("integration test should be written");
    fs::write(repo.join("examples/bar.rs"), "fn main() {}\n").expect("example should be written");
    fs::write(repo.join("benches/perf.rs"), "fn bench_entry() {}\n")
        .expect("bench should be written");

    let jsonl = scan_repository(repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    assert_symbol_in_path(&records, "function", "main", "src/bin/tool.rs");
    assert_no_symbol_in_path(&records, "function", "bin::tool::main", "src/bin/tool.rs");
    assert_symbol_in_path(&records, "function", "main", "src/bin/daemon/main.rs");
    assert_no_symbol_in_path(
        &records,
        "function",
        "daemon::main",
        "src/bin/daemon/main.rs",
    );
    assert_symbol_in_path(
        &records,
        "function",
        "config::load",
        "src/bin/daemon/config.rs",
    );
    assert_no_symbol_in_path(&records, "function", "load", "src/bin/daemon/config.rs");
    assert_symbol_in_path(&records, "test", "smoke", "tests/foo.rs");
    assert_no_symbol_in_path(&records, "test", "tests::foo::smoke", "tests/foo.rs");
    assert_symbol_in_path(&records, "function", "main", "examples/bar.rs");
    assert_no_symbol_in_path(
        &records,
        "function",
        "examples::bar::main",
        "examples/bar.rs",
    );
    assert_symbol_in_path(&records, "function", "bench_entry", "benches/perf.rs");
    assert_no_symbol_in_path(
        &records,
        "function",
        "benches::perf::bench_entry",
        "benches/perf.rs",
    );
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn symbol_records_by_id_without_span(records: &[Value]) -> BTreeMap<String, Value> {
    records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "Symbol")
        .map(|record| {
            let id = record["id"]
                .as_str()
                .expect("symbol record should have an ID")
                .to_owned();
            let mut without_span = record.clone();
            without_span
                .as_object_mut()
                .expect("symbol record should be an object")
                .remove("span");
            (id, without_span)
        })
        .collect()
}

fn symbol_spans_by_id(records: &[Value]) -> BTreeMap<String, Value> {
    records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "Symbol")
        .map(|record| {
            (
                record["id"]
                    .as_str()
                    .expect("symbol record should have an ID")
                    .to_owned(),
                record["span"].clone(),
            )
        })
        .collect()
}

fn diagnostic_ids(records: &[Value]) -> BTreeSet<String> {
    records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "Diagnostic")
        .map(|record| {
            record["id"]
                .as_str()
                .expect("diagnostic record should have an ID")
                .to_owned()
        })
        .collect()
}

fn impl_widget_symbols_by_disambiguator(records: &[Value]) -> BTreeMap<u64, String> {
    records
        .iter()
        .filter(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["symbol_kind"] == "impl"
                && record["name"] == "impl Widget"
        })
        .map(|record| {
            let disambiguator = record["disambiguator"]
                .as_u64()
                .expect("impl Symbol should carry a disambiguator");
            let id = record["id"]
                .as_str()
                .expect("impl Symbol should carry an ID")
                .to_owned();
            (disambiguator, id)
        })
        .collect()
}

fn symbol_ids_by_commit(records: &[Value]) -> BTreeMap<String, BTreeSet<String>> {
    let mut by_commit: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for record in records {
        if record["record_type"] != "node" || record["kind"] != "Symbol" {
            continue;
        }
        let Some(commit) = record["temporal"]["git_commit"].as_str() else {
            continue;
        };
        let id = record["id"]
            .as_str()
            .expect("symbol record should have an ID")
            .to_owned();
        by_commit.entry(commit.to_owned()).or_default().insert(id);
    }
    by_commit
}

fn assert_edges_point_to_existing_nodes(records: &[Value]) {
    let node_ids = records
        .iter()
        .filter(|record| record["record_type"] == "node")
        .map(|record| {
            record["id"]
                .as_str()
                .expect("node record should have an ID")
                .to_owned()
        })
        .collect::<BTreeSet<_>>();

    for edge in records
        .iter()
        .filter(|record| record["record_type"] == "edge")
    {
        let source = edge["source"]
            .as_str()
            .expect("edge should have a source ID");
        let target = edge["target"]
            .as_str()
            .expect("edge should have a target ID");
        assert!(
            node_ids.contains(source),
            "edge {} source {source} should exist as a node",
            edge["id"].as_str().unwrap_or("<missing id>")
        );
        assert!(
            node_ids.contains(target),
            "edge {} target {target} should exist as a node",
            edge["id"].as_str().unwrap_or("<missing id>")
        );
    }
}

fn assert_node(records: &[Value], kind: &str, name: &str) {
    let found = records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == kind
            && record["name"] == name
            && record["repo_relative_path"] == "src/lib.rs"
            && record.get("span").is_some()
    });
    assert!(found, "missing {kind} node named {name}");
}

fn assert_import(records: &[Value], name: &str) {
    let found = records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == "Import"
            && record["name"] == name
            && record["repo_relative_path"] == "src/lib.rs"
            && record.get("span").is_some()
    });
    assert!(found, "missing import node named {name}");
}

fn assert_symbol(records: &[Value], symbol_kind: &str, name: &str) {
    assert_symbol_in_path(records, symbol_kind, name, "src/lib.rs");
}

fn assert_symbol_in_path(records: &[Value], symbol_kind: &str, name: &str, path: &str) {
    let found = has_symbol_in_path(records, symbol_kind, name, path);
    assert!(found, "missing {symbol_kind} symbol named {name} in {path}");
}

fn assert_no_symbol_in_path(records: &[Value], symbol_kind: &str, name: &str, path: &str) {
    assert!(
        !has_symbol_in_path(records, symbol_kind, name, path),
        "unexpected {symbol_kind} symbol named {name} in {path}"
    );
}

fn has_symbol_in_path(records: &[Value], symbol_kind: &str, name: &str, path: &str) -> bool {
    records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == "Symbol"
            && record["symbol_kind"] == symbol_kind
            && record["name"] == name
            && record["repo_relative_path"] == path
            && record.get("span").is_some()
    })
}

fn assert_diagnostic(records: &[Value], summary_fragment: &str) {
    let found = records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == "Diagnostic"
            && record["repo_relative_path"] == "src/lib.rs"
            && record["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains(summary_fragment))
    });
    assert!(found, "missing diagnostic containing {summary_fragment}");
}

fn assert_edge_label(records: &[Value], label: &str) {
    let found = records
        .iter()
        .any(|record| record["record_type"] == "edge" && record["label"] == label);
    assert!(found, "missing {label} edge");
}

fn seed_header_shift_history_repo(repo: &Path) -> [String; 2] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "test@example.invalid"]);
    git(repo, ["config", "user.name", "Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write_file(
        repo,
        "src/lib.rs",
        "pub struct Widget;\n\nimpl Widget {\n    pub fn new() -> Self { Self }\n}\n\npub fn stable() -> Widget {\n    Widget::new()\n}\n",
    );
    let first = commit_with_date(repo, "first", "2026-01-01T00:00:00Z");

    write_file(
        repo,
        "src/lib.rs",
        &format!(
            "{}pub struct Widget;\n\nimpl Widget {{\n    pub fn new() -> Self {{ Self }}\n}}\n\npub fn stable() -> Widget {{\n    Widget::new()\n}}\n",
            forty_line_block_comment()
        ),
    );
    let second = commit_with_date(repo, "prepend header", "2026-01-02T00:00:00Z");

    [first, second]
}

fn forty_line_block_comment() -> String {
    let mut lines = vec!["/*".to_owned()];
    for n in 1..=38 {
        lines.push(format!("shift-only header line {n:02}"));
    }
    lines.push("*/".to_owned());
    format!("{}\n", lines.join("\n"))
}

fn write_file(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("file should have a parent"))
        .expect("parent dir should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn commit_with_date(repo: &Path, message: &str, date: &str) -> String {
    git(repo, ["add", "."]);
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        status.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    git_output(repo, ["rev-parse", "HEAD"])
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

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
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
    String::from_utf8(out.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}
