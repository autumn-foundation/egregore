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

    // AC1: human-readable coverage summary on stderr. The tracked `Cargo.toml`
    // declares dependencies, so manifest extraction (issue #180) mints a `File`
    // node for it: it is honestly counted under `files_indexed` (5), never left
    // mislabeled under the `toml` skip bucket (issue #135).
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("scan coverage: 8 files walked, 5 indexed, 3 skipped"),
        "stderr missing coverage summary: {stderr}"
    );
    // AC6: names the real 4-language indexed scope.
    assert!(
        stderr.contains("indexed languages: Rust, Python, TypeScript, Go"),
        "stderr missing language scope: {stderr}"
    );
    assert!(
        stderr.contains("md: 2") && stderr.contains("(no-ext): 1"),
        "stderr missing per-extension skips: {stderr}"
    );
    // The indexed `Cargo.toml` no longer appears in the skip tally.
    assert!(
        !stderr.contains("toml:"),
        "indexed manifest must not appear under skipped extensions: {stderr}"
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
    assert_eq!(indexed, 5);
    assert!(
        node["scan_coverage"]["coverage_complete"]
            .as_bool()
            .unwrap()
    );

    let skipped = node["scan_coverage"]["skipped_by_extension"]
        .as_object()
        .expect("skipped_by_extension object");
    assert_eq!(skipped["md"], 2);
    assert!(
        !skipped.contains_key("toml"),
        "the dependency-declaring Cargo.toml is indexed, not skipped: {skipped:?}"
    );
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

    // The coverage claim must match reality: the `Cargo.toml` counted under
    // `files_indexed` genuinely has a `File` node AND queryable dependency
    // facts, so an agent reading the coverage summary is never told a file was
    // "never indexed" when the graph in fact holds a node for it (issue #135).
    let manifest_has_file_node = records.iter().any(|r| {
        r["record_type"] == "node" && r["kind"] == "File" && r["repo_relative_path"] == "Cargo.toml"
    });
    assert!(
        manifest_has_file_node,
        "the indexed Cargo.toml must have a File node in the graph"
    );
    let has_dependency_fact = records
        .iter()
        .any(|r| r["record_type"] == "node" && r["kind"] == "DependencyDeclaration");
    assert!(
        has_dependency_fact,
        "the indexed Cargo.toml must emit DependencyDeclaration facts"
    );
}

/// Encodes `text` as UTF-16LE with a byte-order mark, yielding bytes that are
/// genuine text yet never valid UTF-8 (a leading `0xFF` byte is impossible in
/// UTF-8), so writing them to a `.rs` file exercises the non-UTF-8 decode-skip
/// path (issue #438).
fn utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Issue #438: `eg scan` must not hard-abort on the first non-UTF-8 tracked
/// source file. It skips the undecodable file, records a deterministic
/// `Diagnostic` naming its repo-relative path, continues extracting every
/// decodable file, and reconciles the skip into `ScanCoverage` so the file is
/// honestly counted UNINDEXED (AC4 invariant preserved).
#[test]
#[allow(clippy::too_many_lines)]
fn scan_skips_non_utf8_source_and_records_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/good.rs"), "pub fn good() -> u32 { 1 }\n").expect("good source");
    fs::write(
        repo.join("src/also_good.rs"),
        "pub fn also() -> u32 { 2 }\n",
    )
    .expect("second good source");
    fs::write(
        repo.join("src/bad.rs"),
        utf16le_with_bom("pub fn hidden() {}"),
    )
    .expect("bad source");
    fs::write(repo.join("README.md"), "# doc\n").expect("readme");
    run_git(&repo, &["init"]);
    run_git(&repo, &["config", "user.email", "test@example.invalid"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    run_git(&repo, &["config", "commit.gpgsign", "false"]);
    run_git(&repo, &["add", "-A", "-f"]);
    run_git(&repo, &["commit", "-m", "fixture"]);

    let graph_path = repo.join("graph.jsonl");
    let output = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .output()
        .expect("scan should run");
    // The scan completes rather than aborting on the undecodable file.
    assert!(
        output.status.success(),
        "scan must exit 0 on a non-UTF-8 source: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let jsonl = fs::read_to_string(&graph_path).expect("scan should write JSONL");
    let records = parse_jsonl(&jsonl);

    // The decodable files are fully extracted.
    let good_file = records.iter().any(|r| {
        r["record_type"] == "node"
            && r["kind"] == "File"
            && r["repo_relative_path"] == "src/good.rs"
    });
    let good_symbol = records.iter().any(|r| {
        r["record_type"] == "node"
            && r["kind"] == "Symbol"
            && r["repo_relative_path"] == "src/good.rs"
            && r["name"]
                .as_str()
                .is_some_and(|name| name.ends_with("good"))
    });
    assert!(good_file, "decodable file must have a File node");
    assert!(good_symbol, "decodable file's symbol must be extracted");

    // The bad file gets no File node.
    let bad_file = records.iter().any(|r| {
        r["record_type"] == "node" && r["kind"] == "File" && r["repo_relative_path"] == "src/bad.rs"
    });
    assert!(!bad_file, "the non-UTF-8 file must not receive a File node");

    // A Diagnostic names the bad file's repo-relative path with a non-UTF-8
    // reason.
    let diagnostic = records.iter().find(|r| {
        r["record_type"] == "node"
            && r["kind"] == "Diagnostic"
            && r["repo_relative_path"] == "src/bad.rs"
    });
    let diagnostic = diagnostic.expect("a Diagnostic must name the skipped non-UTF-8 file");
    let summary = diagnostic["summary"].as_str().unwrap_or_default();
    assert!(
        summary.contains("UTF-8"),
        "diagnostic must state the non-UTF-8 decode failure: {summary}"
    );

    // Coverage counts the bad file as skipped/unindexed. 4 walked (good, also_good,
    // bad, README), 2 indexed (the two decodable .rs), skipped rs=1 + md=1.
    let node = coverage_node(&records);
    let walked = node["scan_coverage"]["files_walked"].as_u64().unwrap();
    let indexed = node["scan_coverage"]["files_indexed"].as_u64().unwrap();
    assert_eq!(walked, 4, "coverage: {node}");
    assert_eq!(
        indexed, 2,
        "the bad .rs must not be counted indexed: {node}"
    );
    let skipped = node["scan_coverage"]["skipped_by_extension"]
        .as_object()
        .expect("skipped_by_extension object");
    assert_eq!(skipped["rs"], 1, "the undecodable .rs is skipped: {node}");
    assert_eq!(skipped["md"], 1, "coverage: {node}");
    let skipped_total: u64 = skipped.values().map(|v| v.as_u64().unwrap()).sum();
    assert_eq!(
        indexed + skipped_total,
        walked,
        "AC4: indexed + skipped == walked"
    );

    // A graph carrying the diagnostic validates cleanly.
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("validate")
        .arg(&graph_path)
        .assert()
        .success();

    // Determinism: two fixed-time scans of the unchanged repo (including the
    // skip diagnostic and the reconciled coverage) are byte-identical. A fixed
    // time + id pins the only wall-clock fields (producer/valid time), so any
    // residual difference would be genuine non-determinism.
    let fixed_time = "2026-05-19T00:00:00Z";
    let id = Some("non-utf8-stable-fixture");
    let first = scan_repository_at_with_override(&repo, fixed_time, id)
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let second = scan_repository_at_with_override(&repo, fixed_time, id)
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    assert_eq!(
        first, second,
        "scan over a non-UTF-8 file must be byte-stable"
    );
    assert!(
        first.contains("skipped source file: not valid UTF-8"),
        "the skip diagnostic must be present in the fixed-time scan"
    );
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

/// A real production `eg scan` populates `coverage_generation` (issue #406) at
/// full nanosecond precision, and it round-trips through the typed `GraphRecord`
/// serde path `eg export`/`ingest` use, so the freshness signal an exported
/// graph relies on survives.
#[test]
fn scan_populates_and_roundtrips_coverage_generation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_from_fixture(&temp);

    // Production path (no fixed-time override): `coverage_generation` carries a
    // full-precision instant so same-UTC-second re-scans stay orderable.
    let graph = aletheia_egregore::scan_repository(&repo).expect("scan");
    let jsonl = graph.to_jsonl().expect("serialize");

    let coverage_line = jsonl
        .lines()
        .find(|line| {
            let v: Value = serde_json::from_str(line).unwrap();
            v["record_type"] == "node" && v["kind"] == "ScanCoverage"
        })
        .expect("scan emits a ScanCoverage line");

    let value: Value = serde_json::from_str(coverage_line).unwrap();
    let generation = value["scan_coverage"]["coverage_generation"]
        .as_str()
        .expect("coverage_generation present on a production scan");
    // Full nanosecond precision: a fractional-seconds component, not a bare
    // seconds instant, so two same-second scans differ.
    assert!(
        generation.contains('.'),
        "coverage_generation must be full precision: {generation}"
    );
    chrono::DateTime::parse_from_rfc3339(generation)
        .expect("coverage_generation must be a valid RFC 3339 instant");

    // Round-trip through the exact typed path `eg ingest` uses: deserialize the
    // line into a `GraphRecord`, re-serialize, and confirm the field survives.
    let record: aletheia_egregore::GraphRecord =
        serde_json::from_str(coverage_line).expect("coverage line deserializes to GraphRecord");
    let reserialized = serde_json::to_string(&record).expect("reserialize record");
    assert!(
        reserialized.contains(&format!(r#""coverage_generation":"{generation}""#)),
        "coverage_generation must round-trip: {reserialized}"
    );
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
    assert_eq!(coverage[0]["files_indexed"], 5);
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
        text.contains("coverage: 8 files walked, 5 indexed, 3 skipped (complete: true)"),
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

/// Two full scans within one UTC second stamp identical `valid_time`
/// (seconds precision), so the sub-second `coverage_generation` field
/// (issue #406) is the tie-break signal that lets `eg inspect --graph`
/// deterministically report the NEWER version — never the physically-last
/// or lexically-greatest one that `eg export`'s `lines.sort_unstable()`
/// might surface.
#[test]
fn inspect_graph_reports_newer_of_two_same_second_coverage_versions() {
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

    // Both versions share one seconds-precision `valid_time` (the same-UTC-second
    // scenario). Only `coverage_generation` distinguishes them.
    let shared_valid_time = "2026-05-19T00:00:00Z";

    // FRESH: later generation instant, fewer files (files removed since the stale
    // scan). This is the current coverage inspect must report.
    let mut fresh = coverage.clone();
    fresh["valid_time"] = Value::from(shared_valid_time);
    fresh["scan_coverage"]["coverage_generation"] = Value::from("2026-05-19T00:00:00.500000000Z");
    fresh["scan_coverage"]["files_walked"] = Value::from(4);
    fresh["scan_coverage"]["files_indexed"] = Value::from(4);

    // STALE: an OLDER generation instant (previous day, via a +13:00 offset) whose
    // RFC 3339 string is lexically GREATER than fresh's, and MORE files — so the
    // stale line sorts LAST under export's `lines.sort_unstable()`. Selection by
    // parsed generation instant (not line order, not lexical string order) must
    // still pick fresh.
    let mut stale = coverage;
    stale["valid_time"] = Value::from(shared_valid_time);
    stale["scan_coverage"]["coverage_generation"] =
        Value::from("2026-05-19T00:00:00.900000000+13:00");
    stale["scan_coverage"]["files_walked"] = Value::from(9);
    stale["scan_coverage"]["files_indexed"] = Value::from(9);

    let fresh_line = serde_json::to_string(&fresh).unwrap();
    let stale_line = serde_json::to_string(&stale).unwrap();

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

    let run_inspect = || {
        let out = assert_cmd::Command::cargo_bin("egregore")
            .expect("binary")
            .arg("inspect")
            .arg(&export_like)
            .args(["--format", "json"])
            .output()
            .expect("inspect graph");
        assert!(out.status.success());
        out.stdout
    };

    let first = run_inspect();
    let value: Value = serde_json::from_slice(&first).expect("inspect JSON should parse");
    let coverage = value["coverage"].as_array().expect("coverage array");
    // Exactly one summary per stable ID, and it is the FRESH one (files_walked =
    // 4) — never the stale, physically-last, lexically-greatest version (9).
    assert_eq!(coverage.len(), 1, "coverage block: {value}");
    assert_eq!(coverage[0]["files_walked"], 4, "newer coverage: {value}");
    assert_eq!(coverage[0]["files_indexed"], 4, "newer coverage: {value}");

    // Selection is deterministic across inspects. The `--graph` envelope carries a
    // wall-clock `snapshot_timestamp`, so compare the coverage sub-value rather
    // than the whole output.
    let second = run_inspect();
    let value2: Value = serde_json::from_slice(&second).expect("inspect JSON should parse");
    assert_eq!(
        value["coverage"], value2["coverage"],
        "coverage selection must be deterministic across inspects"
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
    assert_eq!(coverage[0]["files_indexed"], 5);
    assert_eq!(coverage[0]["skipped_by_extension"]["md"], 2);
    assert!(
        coverage[0]["skipped_by_extension"].get("toml").is_none(),
        "indexed manifest must not appear under skipped extensions: {value}"
    );
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
    assert_eq!(coverage[0]["files_indexed"], 5, "latest coverage: {value}");
}

/// Issue #404: after a tracked directory becomes its own nested Git checkout,
/// `git ls-files` still lists its files (they remain in the superproject index),
/// but the superproject cannot verify their spans. The Git-tracked-files walk of
/// `discover_files_matching` must therefore exclude files under a nested Git root
/// (any ancestor directory below the scan root carrying a `.git` sentinel) — the
/// same guard the filesystem-walk fallback's `should_descend` already applies —
/// and the exclusion must be reflected consistently in scan coverage
/// (excluded-directory semantics: the nested file is neither walked nor indexed,
/// exactly like a file under `.git`/`target`).
#[test]
fn git_walk_excludes_files_under_nested_git_checkout() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_from_fixture(&temp);

    // Track a source file under a subdirectory, then turn that subdirectory into
    // a nested Git checkout by dropping a `.git` FILE sentinel (submodule /
    // linked-worktree style). The file stays in the superproject index, so
    // `git ls-files` keeps returning it.
    let nested = repo.join("dep/src");
    fs::create_dir_all(&nested).expect("nested dir");
    fs::write(nested.join("lib.rs"), "pub fn nested() {}\n").expect("nested source");
    run_git(&repo, &["add", "-A", "-f"]);
    run_git(&repo, &["commit", "-m", "add tracked dep source"]);
    fs::write(repo.join("dep/.git"), "gitdir: /elsewhere/.git\n").expect("nested .git sentinel");

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

    let jsonl = fs::read_to_string(&graph_path).expect("scan should write JSONL");
    let records = parse_jsonl(&jsonl);

    // The nested checkout's source must not enter the graph.
    assert!(
        !jsonl.contains("dep/src/lib.rs"),
        "file under a nested Git checkout must be excluded from the graph"
    );

    // Excluded-directory semantics: the nested file is neither walked nor
    // indexed, so coverage matches the base fixture exactly (8 walked, 5
    // indexed) rather than counting the extra nested source.
    let node = coverage_node(&records);
    let walked = node["scan_coverage"]["files_walked"].as_u64().unwrap();
    let indexed = node["scan_coverage"]["files_indexed"].as_u64().unwrap();
    assert_eq!(walked, 8, "nested-Git file must not dilute files_walked");
    assert_eq!(indexed, 5, "nested-Git file must not be indexed");

    // The nested-checkout source never appears in the skip tally either.
    let skipped = node["scan_coverage"]["skipped_by_extension"]
        .as_object()
        .expect("skipped_by_extension object");
    let skipped_total: u64 = skipped.values().map(|v| v.as_u64().unwrap()).sum();
    assert_eq!(
        indexed + skipped_total,
        walked,
        "coverage must fully account (indexed + skipped == walked)"
    );
    assert!(
        node["scan_coverage"]["coverage_complete"]
            .as_bool()
            .unwrap()
    );
}

/// Issue #404 (fallback branch): the non-Git filesystem walk must likewise skip
/// files under a nested Git checkout — here proved with a `.git` DIRECTORY
/// sentinel. This is the existing `should_descend` precedent; the assertion
/// guards it against regression alongside the Git-tracked-files fix.
#[test]
fn filesystem_walk_excludes_files_under_nested_git_checkout() {
    let temp = tempfile::tempdir().expect("temp dir");
    // A non-Git tree (no outer `.git`) so discovery takes the filesystem-walk
    // fallback rather than the Git-tracked-files branch.
    let root = temp.path().join("tree");
    fs::create_dir_all(root.join("dep/src")).expect("dirs");
    fs::write(root.join("outer.rs"), "pub fn outer() {}\n").expect("outer source");
    fs::write(root.join("dep/src/lib.rs"), "pub fn nested() {}\n").expect("nested source");
    // `.git` DIRECTORY sentinel marking `dep/` as its own checkout.
    fs::create_dir_all(root.join("dep/.git")).expect("nested .git dir");

    let graph_path = temp.path().join("graph.jsonl");
    let output = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&root)
        .arg("--out")
        .arg(&graph_path)
        .output()
        .expect("scan should run");
    assert!(output.status.success());

    let jsonl = fs::read_to_string(&graph_path).expect("scan should write JSONL");
    assert!(
        jsonl.contains("outer.rs"),
        "top-level source outside the nested checkout must be indexed"
    );
    assert!(
        !jsonl.contains("dep/src/lib.rs"),
        "file under a nested Git checkout must be excluded from the filesystem walk"
    );
}

/// Builds a fresh temp Git repo containing a dependency-declaring `Cargo.toml`,
/// two Rust source files, and a skipped Markdown file, returning the repo path.
#[cfg(feature = "embedded-aletheiadb")]
fn git_repo_with_manifest(temp: &tempfile::TempDir) -> PathBuf {
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"cov403\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nserde = \"1\"\n",
    )
    .expect("write Cargo.toml");
    fs::write(repo.join("src/lib.rs"), "pub fn a() -> usize { 1 }\n").expect("write lib.rs");
    fs::write(repo.join("src/other.rs"), "pub fn b() -> usize { 2 }\n").expect("write other.rs");
    fs::write(repo.join("README.md"), "# doc\n").expect("write README.md");
    run_git(&repo, &["init"]);
    run_git(&repo, &["config", "user.email", "test@example.invalid"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    run_git(&repo, &["config", "commit.gpgsign", "false"]);
    run_git(&repo, &["add", "-A", "-f"]);
    run_git(&repo, &["commit", "-m", "init"]);
    repo
}

/// Reads the current `ScanCoverage` block from `eg inspect --data-dir`.
#[cfg(feature = "embedded-aletheiadb")]
fn inspect_coverage(data_dir: &Path) -> Value {
    let out = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("inspect")
        .args(["--data-dir"])
        .arg(data_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("inspect data-dir");
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).expect("inspect JSON should parse");
    let coverage = value["coverage"]
        .as_array()
        .expect("coverage array")
        .clone();
    assert_eq!(coverage.len(), 1, "exactly one coverage summary: {value}");
    coverage[0].clone()
}

/// `eg refresh` must keep the store's `ScanCoverage` node current after the file
/// set changes (issue #403). Before the fix, the incremental path emitted no
/// `ScanCoverage` record, so `eg inspect --data-dir` kept reporting the stale
/// pre-refresh `files_walked`/`files_indexed`. After the fix, refresh re-emits a
/// superseding coverage node reflecting the new tree, and a dependency-declaring
/// `Cargo.toml` stays counted `files_indexed`, never `skipped_by_extension.toml`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_updates_stored_scan_coverage() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_repo_with_manifest(&temp);
    let graph_path = temp.path().join("initial.jsonl");
    let data_dir = temp.path().join("store");

    // Full scan + ingest: 4 walked (Cargo.toml, lib.rs, other.rs, README.md),
    // 3 indexed (both .rs + the dependency-declaring Cargo.toml), 1 skipped (md).
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

    let before = inspect_coverage(&data_dir);
    assert_eq!(before["files_walked"], 4, "pre-refresh coverage: {before}");
    assert_eq!(before["files_indexed"], 3, "pre-refresh coverage: {before}");

    // Change the file set: add one Rust source file and remove another.
    fs::write(repo.join("src/added.rs"), "pub fn c() -> usize { 3 }\n").expect("write added.rs");
    fs::remove_file(repo.join("src/other.rs")).expect("remove other.rs");
    run_git(&repo, &["add", "-A", "-f"]);
    run_git(&repo, &["commit", "-m", "change file set"]);

    // Incremental refresh + ingest into the SAME data dir (the CLI path the
    // refresh command uses under the hood).
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("refresh")
        .arg(&repo)
        .args(["--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    // The stored coverage now reflects the NEW tree: still 4 walked (added one
    // .rs, removed one .rs, README.md + Cargo.toml unchanged) but the indexed set
    // is recomputed against the current files. Crucially, the numbers are the
    // freshly reconciled ones, not the stale pre-refresh snapshot, and the
    // dependency-declaring Cargo.toml is still indexed rather than skipped.
    let after = inspect_coverage(&data_dir);
    assert_eq!(after["files_walked"], 4, "post-refresh coverage: {after}");
    assert_eq!(after["files_indexed"], 3, "post-refresh coverage: {after}");
    assert!(
        after["skipped_by_extension"].get("toml").is_none(),
        "indexed manifest must not appear under skipped extensions: {after}"
    );
    assert_eq!(
        after["skipped_by_extension"]["md"], 1,
        "post-refresh coverage: {after}"
    );

    // Add a SECOND new source file so the walked/indexed counts strictly increase,
    // proving the stored coverage tracks the live tree rather than any fixed prior
    // value (the exact stale-node regression issue #403 describes).
    fs::write(repo.join("src/more.rs"), "pub fn d() -> usize { 4 }\n").expect("write more.rs");
    run_git(&repo, &["add", "-A", "-f"]);
    run_git(&repo, &["commit", "-m", "add more"]);
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("refresh")
        .arg(&repo)
        .args(["--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let grown = inspect_coverage(&data_dir);
    assert_eq!(grown["files_walked"], 5, "grown coverage: {grown}");
    assert_eq!(grown["files_indexed"], 4, "grown coverage: {grown}");
    assert!(
        grown["skipped_by_extension"].get("toml").is_none(),
        "indexed manifest must not appear under skipped extensions: {grown}"
    );
}
