//! Issue #124: Rust symbols carry `visibility`, `signature`, and `doc`
//! declaration-surface metadata at extraction.

#![allow(missing_docs)]

use std::{fs, path::Path};

use aletheia_egregore::scan_repository_at_with_override;
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

const FIXED_TIME: &str = "2026-06-30T00:00:00Z";
const REPO_ID: &str = "symbol-metadata-fixture";

/// Rust source covering every symbol kind and visibility class from the
/// issue #124 acceptance criteria.
const METADATA_SOURCE: &str = r#"/// Returns the answer.
pub fn answer() -> usize {
    42
}

fn private_helper(input: usize) -> usize {
    input
}

pub(crate) fn crate_fn() {}

pub(super) fn super_fn() {}

pub(in crate::inner) fn path_fn() {}

/// Widget doc line one.
/// Widget doc line two.
#[derive(Debug)]
pub struct Widget {
    pub value: usize,
}

pub struct Pair(pub usize, pub usize);

/** Block doc for Mode.
 * Second line.
 */
pub enum Mode {
    Fast,
    Slow,
}

// A plain comment, not a doc comment.
pub trait Runner {
    fn run(&self) -> usize;
}

/* plain block comment */
pub type Alias = usize;

//// Four slashes is not a doc comment.
pub const LIMIT: usize = 7;

pub static NAME: &str = "basic";

impl Widget {
    /// Builds a widget.
    pub fn new(value: usize) -> Self {
        Self { value }
    }

    fn hidden(&self) -> usize {
        self.value
    }
}

#[test]
fn widget_answer() {
    assert_eq!(answer(), 42);
}
"#;

fn write_fixture(dir: &Path, source: &str) {
    fs::create_dir_all(dir.join("src")).expect("src dir should be created");
    fs::write(dir.join("src/lib.rs"), source).expect("lib.rs should be written");
}

fn scan_fixture_jsonl(dir: &Path) -> String {
    scan_repository_at_with_override(dir, FIXED_TIME, Some(REPO_ID))
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

fn symbol<'a>(records: &'a [Value], symbol_kind: &str, name: &str) -> &'a Value {
    records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["symbol_kind"] == symbol_kind
                && record["name"] == name
        })
        .unwrap_or_else(|| panic!("missing {symbol_kind} symbol named {name}"))
}

#[test]
#[allow(clippy::too_many_lines)]
fn rust_symbols_carry_visibility_and_signature() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_fixture(temp.path(), METADATA_SOURCE);
    let records = parse_jsonl(&scan_fixture_jsonl(temp.path()));

    // Closed visibility set derived from the source `pub` modifier.
    assert_eq!(
        symbol(&records, "function", "answer")["visibility"],
        "public"
    );
    assert_eq!(
        symbol(&records, "function", "private_helper")["visibility"],
        "private"
    );
    assert_eq!(
        symbol(&records, "function", "crate_fn")["visibility"],
        "crate"
    );
    assert_eq!(
        symbol(&records, "function", "super_fn")["visibility"],
        "restricted"
    );
    assert_eq!(
        symbol(&records, "function", "path_fn")["visibility"],
        "restricted"
    );
    assert_eq!(symbol(&records, "struct", "Widget")["visibility"], "public");
    assert_eq!(symbol(&records, "enum", "Mode")["visibility"], "public");
    assert_eq!(symbol(&records, "trait", "Runner")["visibility"], "public");
    assert_eq!(
        symbol(&records, "type_alias", "Alias")["visibility"],
        "public"
    );
    assert_eq!(symbol(&records, "const", "LIMIT")["visibility"], "public");
    assert_eq!(symbol(&records, "static", "NAME")["visibility"], "public");
    assert_eq!(
        symbol(&records, "method", "Widget::new")["visibility"],
        "public"
    );
    assert_eq!(
        symbol(&records, "method", "Widget::hidden")["visibility"],
        "private"
    );
    assert_eq!(
        symbol(&records, "test", "widget_answer")["visibility"],
        "private"
    );

    // Signatures: item keyword through the declaration header, body excluded,
    // interior whitespace collapsed deterministically.
    assert_eq!(
        symbol(&records, "function", "answer")["signature"],
        "fn answer()->usize"
    );
    assert_eq!(
        symbol(&records, "function", "private_helper")["signature"],
        "fn private_helper(input:usize)->usize"
    );
    assert_eq!(
        symbol(&records, "struct", "Widget")["signature"],
        "struct Widget"
    );
    assert_eq!(
        symbol(&records, "struct", "Pair")["signature"],
        "struct Pair(pub usize,pub usize);"
    );
    assert_eq!(symbol(&records, "enum", "Mode")["signature"], "enum Mode");
    assert_eq!(
        symbol(&records, "trait", "Runner")["signature"],
        "trait Runner"
    );
    assert_eq!(
        symbol(&records, "type_alias", "Alias")["signature"],
        "type Alias=usize;"
    );
    assert_eq!(
        symbol(&records, "const", "LIMIT")["signature"],
        "const LIMIT:usize=7;"
    );
    assert_eq!(
        symbol(&records, "static", "NAME")["signature"],
        "static NAME:&str=\"basic\";"
    );
    assert_eq!(
        symbol(&records, "method", "Widget::new")["signature"],
        "fn new(value:usize)->Self"
    );

    // 100% of fn/struct/enum/trait symbols carry a non-empty signature and a
    // visibility class (issue #124 success metric, fixture-scale).
    for record in &records {
        if record["record_type"] != "node" || record["kind"] != "Symbol" {
            continue;
        }
        let kind = record["symbol_kind"].as_str().unwrap_or("");
        if matches!(
            kind,
            "function"
                | "method"
                | "test"
                | "struct"
                | "enum"
                | "trait"
                | "type_alias"
                | "const"
                | "static"
        ) {
            assert!(
                record["signature"].as_str().is_some_and(|s| !s.is_empty()),
                "symbol {} should carry a non-empty signature",
                record["name"]
            );
            assert!(
                record["visibility"]
                    .as_str()
                    .is_some_and(|v| matches!(v, "public" | "crate" | "restricted" | "private")),
                "symbol {} should carry a closed-set visibility",
                record["name"]
            );
        }
    }

    // `impl` symbols are outside the issue #124 declaration-surface set.
    let impl_symbol = symbol(&records, "impl", "impl Widget");
    assert!(impl_symbol.get("visibility").is_none());
    assert!(impl_symbol.get("signature").is_none());

    // Non-symbol code-graph nodes never carry the new fields.
    for record in &records {
        if record["record_type"] == "node" && record["kind"] != "Symbol" {
            assert!(record.get("visibility").is_none());
            assert!(record.get("signature").is_none());
            assert!(record.get("doc").is_none());
        }
    }
}

#[test]
fn rust_symbols_carry_doc_comments_and_omit_absent_docs() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_fixture(temp.path(), METADATA_SOURCE);
    let records = parse_jsonl(&scan_fixture_jsonl(temp.path()));

    assert_eq!(
        symbol(&records, "function", "answer")["doc"],
        "Returns the answer."
    );
    // Multiple /// lines join with newlines; attributes between the docs and
    // the item do not break the association.
    assert_eq!(
        symbol(&records, "struct", "Widget")["doc"],
        "Widget doc line one.\nWidget doc line two."
    );
    // Block doc comments strip the /** */ delimiters and the * gutter.
    assert_eq!(
        symbol(&records, "enum", "Mode")["doc"],
        "Block doc for Mode.\nSecond line."
    );
    assert_eq!(
        symbol(&records, "method", "Widget::new")["doc"],
        "Builds a widget."
    );

    // Symbols with no doc comment omit the field entirely — never "".
    assert!(
        symbol(&records, "function", "private_helper")
            .get("doc")
            .is_none()
    );
    // Plain `//`, `/* */`, and `////` comments are not doc comments.
    assert!(symbol(&records, "trait", "Runner").get("doc").is_none());
    assert!(symbol(&records, "type_alias", "Alias").get("doc").is_none());
    assert!(symbol(&records, "const", "LIMIT").get("doc").is_none());
}

#[test]
fn doc_comment_with_secret_shaped_value_is_redacted() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_fixture(
        temp.path(),
        "/// Example configuration: API_KEY=abcd1234efgh5678\npub fn configure() {}\n",
    );
    let records = parse_jsonl(&scan_fixture_jsonl(temp.path()));

    let configure = symbol(&records, "function", "configure");
    let doc = configure["doc"].as_str().expect("doc should be present");
    assert!(
        doc.starts_with("<REDACTED:env_secret:"),
        "secret-shaped doc must be redacted before persistence, got: {doc}"
    );
    assert!(
        !doc.contains("abcd1234efgh5678"),
        "raw secret must never be persisted"
    );
    assert_eq!(configure["redaction_policy_version"], "v1");

    // A doc without secrets does not carry a marker.
    let temp_clean = tempfile::tempdir().expect("temp dir should be created");
    write_fixture(temp_clean.path(), "/// Plain doc.\npub fn plain() {}\n");
    let clean_records = parse_jsonl(&scan_fixture_jsonl(temp_clean.path()));
    assert_eq!(
        symbol(&clean_records, "function", "plain")["doc"],
        "Plain doc."
    );
}

#[test]
fn symbol_metadata_is_deterministic_across_runs_and_line_endings() {
    let temp_lf = tempfile::tempdir().expect("temp dir LF should be created");
    let temp_crlf = tempfile::tempdir().expect("temp dir CRLF should be created");
    write_fixture(temp_lf.path(), METADATA_SOURCE);
    write_fixture(temp_crlf.path(), &METADATA_SOURCE.replace('\n', "\r\n"));

    let first = scan_fixture_jsonl(temp_lf.path());
    let second = scan_fixture_jsonl(temp_lf.path());
    assert_eq!(first, second, "re-scan must be byte-identical");

    let crlf = scan_fixture_jsonl(temp_crlf.path());
    assert_eq!(
        first, crlf,
        "signature/visibility/doc must be byte-identical across line-ending checkouts"
    );
}

#[test]
fn query_symbol_surfaces_signature_visibility_and_doc() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_fixture(temp.path(), METADATA_SOURCE);
    let jsonl = scan_fixture_jsonl(temp.path());
    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, &jsonl).expect("graph should be written");

    // JSON format carries the new fields.
    let assert_json = Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "symbol", "answer", "--format", "json", "--graph"])
        .arg(&graph_path)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert_json.get_output().stdout).into_owned();
    let row: Value = serde_json::from_str(stdout.lines().next().expect("one JSON row"))
        .expect("row should be valid JSON");
    assert_eq!(row["visibility"], "public");
    assert_eq!(row["signature"], "fn answer()->usize");
    assert_eq!(row["doc"], "Returns the answer.");

    // A symbol without a doc comment omits `doc` from the JSON row.
    let assert_nodoc = Command::cargo_bin("egregore")
        .expect("binary should run")
        .args([
            "query",
            "symbol",
            "private_helper",
            "--format",
            "json",
            "--graph",
        ])
        .arg(&graph_path)
        .assert()
        .success();
    let nodoc_stdout = String::from_utf8_lossy(&assert_nodoc.get_output().stdout).into_owned();
    let nodoc_row: Value = serde_json::from_str(nodoc_stdout.lines().next().expect("one JSON row"))
        .expect("row should be valid JSON");
    assert!(nodoc_row.get("doc").is_none());
    assert_eq!(nodoc_row["visibility"], "private");

    // Text format surfaces the same fields.
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "symbol", "answer", "--format", "text", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("visibility: public")
                .and(predicate::str::contains("signature: fn answer()->usize"))
                .and(predicate::str::contains("doc: Returns the answer.")),
        );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_store_round_trips_symbol_metadata() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_fixture(temp.path(), METADATA_SOURCE);
    let jsonl = scan_fixture_jsonl(temp.path());
    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, &jsonl).expect("graph should be written");
    let data_dir = temp.path().join("store");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let assert_json = Command::cargo_bin("egregore")
        .expect("binary should run")
        .args([
            "query",
            "symbol",
            "answer",
            "--format",
            "json",
            "--data-dir",
        ])
        .arg(&data_dir)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert_json.get_output().stdout).into_owned();
    let row: Value = serde_json::from_str(stdout.lines().next().expect("one JSON row"))
        .expect("row should be valid JSON");
    assert_eq!(row["visibility"], "public");
    assert_eq!(row["signature"], "fn answer()->usize");
    assert_eq!(row["doc"], "Returns the answer.");
}
