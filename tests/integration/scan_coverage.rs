//! `eg scan` / `eg inspect` file-level scan-coverage accounting (issue #135).
//!
//! Proves the acceptance criteria: `eg scan` reports files walked/indexed and a
//! per-extension skip tally (AC1); the counts fully account for the walk
//! (`indexed + sum(skipped) == walked`, AC4); excluded directories are never
//! counted (AC7); the summary names the real Rust/Python/TypeScript/Go language
//! scope (AC6); the coverage is a machine-readable `ScanCoverage` graph node
//! (AC3); `eg inspect` surfaces the coverage block over both `--graph` and
//! `--data-dir` (AC2); output is byte-stable across runs (AC5); and a graph
//! carrying the node validates cleanly while a malformed case is still caught.

#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf, process::Command};

use aletheia_egregore::scan_repository_at_with_override;
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/coverage_mixed")
}

/// Recursively copies `src` into `dst`.
fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("dest dir should be created");
    for entry in fs::read_dir(src).expect("source dir should be readable") {
        let entry = entry.expect("dir entry should be readable");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to);
        } else {
            fs::copy(&from, &to).expect("file should copy");
        }
    }
}

fn run_git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git should run");
    assert!(status.success(), "git {args:?} failed");
}

/// Copies the mixed-language fixture into a fresh temp Git repository so the
/// walk's Git-tracked-files branch (which yields a complete coverage tally) is
/// exercised. Returns the repo path (owned by `temp`).
fn git_repo_from_fixture(temp: &tempfile::TempDir) -> PathBuf {
    let repo = temp.path().join("repo");
    copy_dir_all(&fixture_dir(), &repo);
    run_git(&repo, &["init"]);
    run_git(&repo, &["config", "user.email", "test@example.invalid"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    run_git(&repo, &["config", "commit.gpgsign", "false"]);
    // Force-add so the `target/` file is tracked (proving AC7 excludes tracked
    // files under an excluded directory, not merely untracked ones).
    run_git(&repo, &["add", "-A", "-f"]);
    run_git(&repo, &["commit", "-m", "fixture"]);
    repo
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn coverage_node(records: &[Value]) -> Value {
    let nodes: Vec<&Value> = records
        .iter()
        .filter(|r| r["record_type"] == "node" && r["kind"] == "ScanCoverage")
        .collect();
    assert_eq!(nodes.len(), 1, "exactly one ScanCoverage node: {nodes:?}");
    nodes[0].clone()
}

#[test]
fn scan_reports_coverage_summary_and_node() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_from_fixture(&temp);
    let graph_path = repo.join("graph.jsonl");

    let output = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .output()
        .expect("scan should run");
    assert!(output.status.success());

    // AC1: human-readable coverage summary on stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("scan coverage: 8 files walked, 4 indexed, 4 skipped"),
        "stderr missing coverage summary: {stderr}"
    );
    // AC6: names the real 4-language indexed scope.
    assert!(
        stderr.contains("indexed languages: Rust, Python, TypeScript, Go"),
        "stderr missing language scope: {stderr}"
    );
    assert!(
        stderr.contains("md: 2") && stderr.contains("toml: 1") && stderr.contains("(no-ext): 1"),
        "stderr missing per-extension skips: {stderr}"
    );

    let jsonl = fs::read_to_string(&graph_path).expect("scan should write JSONL");
    let records = parse_jsonl(&jsonl);
    let node = coverage_node(&records);

    // AC3: machine-readable coverage payload.
    let walked = node["scan_coverage"]["files_walked"]
        .as_u64()
        .expect("files_walked");
    let indexed = node["scan_coverage"]["files_indexed"]
        .as_u64()
        .expect("files_indexed");
    assert_eq!(walked, 8);
    assert_eq!(indexed, 4);
    assert!(
        node["scan_coverage"]["coverage_complete"]
            .as_bool()
            .unwrap()
    );

    let skipped = node["scan_coverage"]["skipped_by_extension"]
        .as_object()
        .expect("skipped_by_extension object");
    assert_eq!(skipped["md"], 2);
    assert_eq!(skipped["toml"], 1);
    assert_eq!(skipped[""], 1, "no-extension file keyed on empty string");

    // AC4: complete accounting — indexed + sum(skipped) == walked, nothing lost.
    let skipped_total: u64 = skipped.values().map(|v| v.as_u64().unwrap()).sum();
    assert_eq!(
        indexed + skipped_total,
        walked,
        "coverage must fully account"
    );

    // AC6: node names the indexed language scope.
    let langs: Vec<String> = node["scan_coverage"]["indexed_languages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(langs, vec!["Rust", "Python", "TypeScript", "Go"]);

    // AC7: the tracked file under target/ is neither indexed nor skip-tallied,
    // and never appears in the graph.
    assert!(
        !skipped.contains_key("rs"),
        "the only .rs files are the indexed lib.rs and the excluded target file"
    );
    assert!(
        !jsonl.contains("target/leftover.rs"),
        "excluded-directory file must not enter the graph"
    );

    // The coverage node is attached to its Repository by a CONTAINS edge.
    let coverage_id = node["id"].as_str().unwrap();
    let has_edge = records.iter().any(|r| {
        r["record_type"] == "edge" && r["label"] == "CONTAINS" && r["target"] == coverage_id
    });
    assert!(has_edge, "Repository -> ScanCoverage CONTAINS edge missing");
}

#[test]
fn coverage_node_is_byte_stable_across_scans() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_from_fixture(&temp);
    let fixed_time = "2026-05-19T00:00:00Z";
    let id = Some("coverage-stable-fixture");

    // AC5: two scans of the unchanged repo produce byte-identical JSONL,
    // including the ScanCoverage node.
    let first = scan_repository_at_with_override(&repo, fixed_time, id)
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let second = scan_repository_at_with_override(&repo, fixed_time, id)
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    assert_eq!(first, second, "scan JSONL must be byte-identical");
    assert!(first.contains(r#""kind":"ScanCoverage""#));
}

#[test]
fn inspect_graph_surfaces_coverage_block() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_from_fixture(&temp);
    let graph_path = repo.join("graph.jsonl");
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .args(["scan"])
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // AC2 (--graph): JSON coverage block.
    let json_out = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("inspect")
        .arg(&graph_path)
        .arg("--format")
        .arg("json")
        .output()
        .expect("inspect json");
    assert!(json_out.status.success());
    let value: Value = serde_json::from_slice(&json_out.stdout).expect("inspect JSON should parse");
    let coverage = value["coverage"].as_array().expect("coverage array");
    assert_eq!(coverage.len(), 1);
    assert_eq!(coverage[0]["files_walked"], 8);
    assert_eq!(coverage[0]["files_indexed"], 4);
    assert_eq!(coverage[0]["skipped_by_extension"]["md"], 2);
    assert_eq!(coverage[0]["coverage_complete"], true);

    // The coverage block is deterministic across inspects (AC5). The `--graph`
    // envelope also carries a wall-clock `snapshot_timestamp`, so compare the
    // coverage sub-value rather than the whole output.
    let json_out2 = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("inspect")
        .arg(&graph_path)
        .arg("--format")
        .arg("json")
        .output()
        .expect("inspect json");
    let value2: Value =
        serde_json::from_slice(&json_out2.stdout).expect("inspect JSON should parse");
    assert_eq!(value["coverage"], value2["coverage"]);

    // AC2 (--graph): text coverage block.
    let text_out = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("inspect")
        .arg(&graph_path)
        .arg("--format")
        .arg("text")
        .output()
        .expect("inspect text");
    let text = String::from_utf8_lossy(&text_out.stdout);
    assert!(
        text.contains("coverage: 8 files walked, 4 indexed, 4 skipped (complete: true)"),
        "text output missing coverage line: {text}"
    );
    assert!(text.contains("indexed languages: Rust, Python, TypeScript, Go"));
}

#[test]
fn validate_accepts_coverage_node_and_still_catches_defects() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_from_fixture(&temp);
    let graph_path = repo.join("graph.jsonl");
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .args(["scan"])
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let jsonl = fs::read_to_string(&graph_path).expect("read graph");
    assert!(jsonl.contains(r#""kind":"ScanCoverage""#));

    // A scanned graph carrying the ScanCoverage node validates cleanly (exit 0).
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("validate")
        .arg(&graph_path)
        .assert()
        .success();

    // Dropping the ScanCoverage node while keeping its inbound CONTAINS edge
    // leaves a dangling endpoint the validator must still catch (exit 1).
    let mut broken = String::new();
    for line in jsonl.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        if v["record_type"] == "node" && v["kind"] == "ScanCoverage" {
            continue;
        }
        broken.push_str(line);
        broken.push('\n');
    }
    let broken_path = repo.join("broken.jsonl");
    fs::write(&broken_path, broken).expect("write broken graph");
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("validate")
        .arg(&broken_path)
        .assert()
        .failure();
}

#[test]
fn validate_rejects_scan_coverage_contained_by_a_file() {
    // Issue #135 (PR #400 review): a `ScanCoverage` summary must be contained by
    // its `Repository`. Reparent the real `Repository —CONTAINS→ ScanCoverage`
    // edge onto a `File` source: this passes the CONTAINS target-kind check and
    // keeps the coverage node incident (so it is not an orphan), yet is malformed
    // attribution the validator must reject with a source-kind defect (exit 1).
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_from_fixture(&temp);
    let graph_path = repo.join("graph.jsonl");
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .args(["scan"])
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let jsonl = fs::read_to_string(&graph_path).expect("read graph");
    let records = parse_jsonl(&jsonl);
    let coverage_id = coverage_node(&records)["id"].as_str().unwrap().to_owned();
    // Any indexed source File makes an illegitimate (non-Repository) container.
    let file_id = records
        .iter()
        .find(|r| r["record_type"] == "node" && r["kind"] == "File")
        .and_then(|r| r["id"].as_str())
        .expect("a File node in the graph")
        .to_owned();

    let mut rewritten = String::new();
    for line in jsonl.lines() {
        let mut v: Value = serde_json::from_str(line).unwrap();
        if v["record_type"] == "edge" && v["label"] == "CONTAINS" && v["target"] == coverage_id {
            v["source"] = Value::from(file_id.clone());
        }
        rewritten.push_str(&serde_json::to_string(&v).unwrap());
        rewritten.push('\n');
    }
    let rewritten_path = repo.join("file_contains_coverage.jsonl");
    fs::write(&rewritten_path, rewritten).expect("write rewritten graph");

    let output = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("validate")
        .arg(&rewritten_path)
        .output()
        .expect("run validate");
    assert!(!output.status.success(), "validate must fail (exit 1)");
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(
        stdout.contains("edge_source_kind_violation"),
        "expected source-kind defect, got: {stdout}"
    );
    assert!(
        stdout.contains(&coverage_id),
        "defect must name the ScanCoverage target: {stdout}"
    );
}

/// A canonical `eg export` JSONL (issue #402) sorts its lines LEXICOGRAPHICALLY,
/// not in scan/append order, so a store holding two physical `ScanCoverage`
/// versions of one stable ID can serialize the STALE version's line AFTER the
/// current one. `eg inspect --graph` must therefore pick the FRESHEST coverage
/// by the serialized `valid_time` (the scan transaction time), never by physical
/// line order (issue #135). The stale version here is BOTH physically last and
/// lexically greatest, yet carries an OLDER instant expressed with a positive
/// timezone offset — so a lexical timestamp comparison would also pick wrong;
/// only a parsed-instant comparison reports the fresh version.
#[test]
fn inspect_graph_reports_freshest_coverage_independent_of_line_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_from_fixture(&temp);
    let graph_path = repo.join("graph.jsonl");
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .args(["scan"])
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let real_jsonl = fs::read_to_string(&graph_path).expect("read graph");
    let records = parse_jsonl(&real_jsonl);
    let coverage = coverage_node(&records);

    // FRESH: newer instant (12:00Z), fewer files (files removed since the stale
    // scan). This is the current coverage inspect must report.
    let mut fresh = coverage.clone();
    fresh["valid_time"] = Value::from("2026-05-19T12:00:00Z");
    fresh["scan_coverage"]["files_walked"] = Value::from(4);
    fresh["scan_coverage"]["files_indexed"] = Value::from(4);

    // STALE: an OLDER instant (00:00Z) written as +13:00 so its RFC 3339 string
    // is lexically GREATER than fresh's, and MORE files. Every field that differs
    // from fresh is lexically larger, so the stale line sorts last under export's
    // `lines.sort_unstable()` regardless of serialized field order.
    let mut stale = coverage;
    stale["valid_time"] = Value::from("2026-05-19T13:00:00+13:00");
    stale["scan_coverage"]["files_walked"] = Value::from(9);
    stale["scan_coverage"]["files_indexed"] = Value::from(9);

    let fresh_line = serde_json::to_string(&fresh).unwrap();
    let stale_line = serde_json::to_string(&stale).unwrap();

    // Reproduce a canonical `eg export` file: keep every non-coverage record,
    // add both coverage versions, then sort lexicographically exactly as export
    // does.
    let mut lines: Vec<String> = real_jsonl
        .lines()
        .filter(|line| {
            let v: Value = serde_json::from_str(line).unwrap();
            !(v["record_type"] == "node" && v["kind"] == "ScanCoverage")
        })
        .map(str::to_owned)
        .collect();
    lines.push(fresh_line.clone());
    lines.push(stale_line.clone());
    lines.sort_unstable();

    // Precondition: the stale line really does sort AFTER the fresh line, so a
    // "keep-last-by-line-order" rule would report the stale coverage.
    let fresh_pos = lines.iter().position(|l| *l == fresh_line).unwrap();
    let stale_pos = lines.iter().position(|l| *l == stale_line).unwrap();
    assert!(
        stale_pos > fresh_pos,
        "stale line must sort after fresh to exercise the regression"
    );

    let export_like = repo.join("export_like.graph.jsonl");
    fs::write(&export_like, format!("{}\n", lines.join("\n"))).expect("write export-like graph");

    let out = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("inspect")
        .arg(&export_like)
        .args(["--format", "json"])
        .output()
        .expect("inspect graph");
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).expect("inspect JSON should parse");
    let coverage = value["coverage"].as_array().expect("coverage array");
    // Exactly one summary per stable ID, and it is the FRESH one (files_walked =
    // 4) — never the stale, physically-last, lexically-greatest version (9).
    assert_eq!(coverage.len(), 1, "coverage block: {value}");
    assert_eq!(coverage[0]["files_walked"], 4, "freshest coverage: {value}");
    assert_eq!(
        coverage[0]["files_indexed"], 4,
        "freshest coverage: {value}"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn inspect_data_dir_surfaces_coverage_block() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_from_fixture(&temp);
    let graph_path = repo.join("graph.jsonl");
    let data_dir = temp.path().join("store");
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .args(["scan"])
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("ingest")
        .arg(&graph_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    // AC2 (--data-dir): the coverage block survives an embedded round-trip.
    let out = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("inspect")
        .args(["--data-dir"])
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("inspect data-dir");
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).expect("inspect JSON should parse");
    let coverage = value["coverage"].as_array().expect("coverage array");
    assert_eq!(coverage.len(), 1, "coverage block: {value}");
    assert_eq!(coverage[0]["files_walked"], 8);
    assert_eq!(coverage[0]["files_indexed"], 4);
    assert_eq!(coverage[0]["skipped_by_extension"]["toml"], 1);
    assert_eq!(coverage[0]["indexed_languages"][0], "Rust");
}

/// A repository re-ingested after files changed leaves the store holding two
/// physical versions of the same stable `ScanCoverage` ID. `eg inspect
/// --data-dir` must report the CURRENT (latest) coverage, never a superseded
/// earlier version (issue #135).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn inspect_data_dir_reports_latest_superseded_coverage() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_from_fixture(&temp);
    let graph_path = repo.join("graph.jsonl");
    let data_dir = temp.path().join("store");
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .args(["scan"])
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // The real scan records files_walked = 8. Build a STALE variant that keeps
    // the same ScanCoverage record ID but reports a different files_walked, so
    // ingesting it first and the real graph second produces two physical
    // versions of one stable ID (an older, superseded one and the current one).
    let real_jsonl = fs::read_to_string(&graph_path).expect("read graph");
    let mut stale = String::new();
    for line in real_jsonl.lines() {
        let mut v: Value = serde_json::from_str(line).unwrap();
        if v["record_type"] == "node" && v["kind"] == "ScanCoverage" {
            v["scan_coverage"]["files_walked"] = Value::from(3);
            v["scan_coverage"]["files_indexed"] = Value::from(1);
        }
        stale.push_str(&serde_json::to_string(&v).unwrap());
        stale.push('\n');
    }
    let stale_path = repo.join("stale.graph.jsonl");
    fs::write(&stale_path, stale).expect("write stale graph");

    // Ingest the STALE coverage first, then supersede it with the real scan.
    for graph in [&stale_path, &graph_path] {
        assert_cmd::Command::cargo_bin("egregore")
            .expect("binary")
            .arg("ingest")
            .arg(graph)
            .args(["--adapter", "embedded", "--data-dir"])
            .arg(&data_dir)
            .assert()
            .success();
    }

    let out = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("inspect")
        .args(["--data-dir"])
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("inspect data-dir");
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).expect("inspect JSON should parse");
    let coverage = value["coverage"].as_array().expect("coverage array");
    // Exactly one summary per stable ID, and it is the LATEST (files_walked = 8)
    // rather than the stale superseded version (files_walked = 3).
    assert_eq!(coverage.len(), 1, "coverage block: {value}");
    assert_eq!(coverage[0]["files_walked"], 8, "latest coverage: {value}");
    assert_eq!(coverage[0]["files_indexed"], 4, "latest coverage: {value}");
}
