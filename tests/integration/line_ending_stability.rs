//! Issue #242: scans are byte-stable across CRLF, lone-CR, and LF checkouts.
//!
//! A Windows dev box (`core.autocrlf=true`), a Linux CI runner, and assorted
//! agent hosts all scan the same commit into one shared store. The scanner
//! must normalize line endings at the read/parse boundary so the same logical
//! source yields byte-identical canonical graph records no matter which line
//! endings the checkout used.
//!
//! This test is the runnable acceptance check for #242:
//!
//! - A multi-file, multi-symbol Rust fixture (free functions, a struct, an
//!   enum, a trait + impl, inherent impl blocks, a const, and a nested module
//!   path) is written three ways — LF, CRLF, and lone-CR — into three
//!   otherwise-identical trees.
//! - Each tree is scanned under a pinned producer, repo id, and transaction
//!   time via [`aletheia_egregore::scan_repository_at_with_override`].
//! - The canonical JSONL outputs must be byte-for-byte identical: every node
//!   and edge, including `start_byte`/`end_byte`/`start_line`/`end_line`
//!   spans and every content-derived field (summaries, signatures, symbol
//!   text).
//! - Symbol record IDs are asserted identical explicitly, proving ADR 0004's
//!   span-independent identity still holds *and* that span/content fields no
//!   longer diverge.
//! - Content-derived drift inputs are asserted identical across variants, so a
//!   pure line-ending difference registers zero drift: no `SemanticDrift`
//!   records and no `DriftsFrom` edges may appear, and every symbol's content
//!   fields match exactly.
//! - Path portability is asserted, not re-implemented: the nested
//!   `src/nested/mod.rs` fixture file must surface with forward-slash
//!   `repo_relative_path` handles in every variant, proving `normalize_path`
//!   keeps handles stable.
//!
//! The lone-CR variant is the regression beyond the earlier CRLF-only
//! coverage: a bare `.replace("\r\n", "\n")` leaves lone carriage returns in
//! place, which shift every downstream byte offset.

#![allow(missing_docs)]

use std::{collections::BTreeMap, fs, path::Path, path::PathBuf};

use aletheia_egregore::scan_repository_at_with_override;
use aletheia_egregore::{
    fs::SourceFile,
    ir::{Graph, GraphRecord},
    parser::extract_source_text,
};
use serde_json::Value;

const FIXED_TIME: &str = "2026-09-17T00:00:00Z";
const REPO_ID: &str = "line-ending-stability-fixture";

/// (repo-relative path, LF-terminated source) fixture files.
fn fixture_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "src/lib.rs",
            "pub mod engine;\npub mod nested;\npub mod widget;\n\n/// Adds two numbers together.\npub fn add(left: usize, right: usize) -> usize {\n    left + right\n}\n\n/// Crate version marker.\npub const VERSION: &str = \"1.0.0\";\n",
        ),
        (
            "src/widget.rs",
            "/// A clickable widget.\npub struct Widget {\n    pub label: String,\n    pub value: usize,\n}\n\n/// Widget paint colors.\npub enum Color {\n    Red,\n    Green,\n    Blue,\n}\n\nimpl Widget {\n    /// Builds a widget from a label and a value.\n    pub fn new(label: &str, value: usize) -> Self {\n        Self {\n            label: label.to_owned(),\n            value,\n        }\n    }\n\n    /// Renders the widget as text.\n    pub fn render(&self) -> String {\n        format!(\"{}: {}\", self.label, self.value)\n    }\n}\n\n/// Anything that can draw itself.\npub trait Drawable {\n    fn draw(&self) -> String;\n}\n\nimpl Drawable for Widget {\n    fn draw(&self) -> String {\n        self.render()\n    }\n}\n",
        ),
        (
            "src/engine.rs",
            "/// A tiny on/off engine.\npub struct Engine {\n    pub running: bool,\n}\n\nimpl Engine {\n    /// Starts the engine.\n    pub fn start(&mut self) {\n        self.running = true;\n    }\n\n    /// Stops the engine.\n    pub fn stop(&mut self) {\n        self.running = false;\n    }\n}\n\n/// Describes the engine state in words.\npub fn describe(engine: &Engine) -> &'static str {\n    if engine.running {\n        \"running\"\n    } else {\n        \"stopped\"\n    }\n}\n",
        ),
        (
            "src/nested/mod.rs",
            "/// Helper living behind a nested module path.\npub fn nested_helper() -> u32 {\n    42\n}\n",
        ),
    ]
}

fn write_variant(dir: &Path, convert: &dyn Fn(&str) -> String) {
    for (rel, source) in fixture_files() {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().expect("fixture file has a parent"))
            .expect("fixture dirs should be created");
        fs::write(&path, convert(source)).expect("fixture file should be written");
    }
}

fn scan_to_jsonl(dir: &Path) -> String {
    scan_repository_at_with_override(dir, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize")
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON record"))
        .collect()
}

fn symbol_records(records: &[Value]) -> BTreeMap<String, Value> {
    records
        .iter()
        .filter(|record| record.get("kind").and_then(Value::as_str) == Some("Symbol"))
        .map(|record| {
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .expect("Symbol record has an id")
                .to_owned();
            (id, (*record).clone())
        })
        .collect()
}

#[test]
fn scans_are_byte_stable_across_line_ending_variants() {
    let temp_lf = tempfile::tempdir().expect("LF temp dir should be created");
    let temp_crlf = tempfile::tempdir().expect("CRLF temp dir should be created");
    let temp_cr = tempfile::tempdir().expect("CR temp dir should be created");

    // Same logical sources, three checkout line-ending conventions.
    write_variant(temp_lf.path(), &|s| s.replace("\r\n", "\n"));
    write_variant(temp_crlf.path(), &|s| s.replace('\n', "\r\n"));
    write_variant(temp_cr.path(), &|s| s.replace('\n', "\r"));

    let jsonl_lf = scan_to_jsonl(temp_lf.path());
    let jsonl_crlf = scan_to_jsonl(temp_crlf.path());
    let jsonl_cr = scan_to_jsonl(temp_cr.path());

    // AC1: byte-for-byte identical canonical graph records across variants —
    // every node and edge, spans and content-derived fields included.
    assert_eq!(
        jsonl_lf, jsonl_crlf,
        "LF and CRLF checkouts must produce byte-identical canonical records"
    );
    assert_eq!(
        jsonl_lf, jsonl_cr,
        "LF and lone-CR checkouts must produce byte-identical canonical records"
    );

    let variants = [
        ("LF", parse_jsonl(&jsonl_lf)),
        ("CRLF", parse_jsonl(&jsonl_crlf)),
        ("CR", parse_jsonl(&jsonl_cr)),
    ];
    let baseline_symbols = symbol_records(&variants[0].1);
    // Multi-symbol fixture guard: the assertions below are vacuous on a
    // one-symbol fixture, so require a genuinely multi-symbol extraction.
    assert!(
        baseline_symbols.len() >= 10,
        "fixture should extract at least 10 symbols, got {}",
        baseline_symbols.len()
    );

    for (name, records) in &variants {
        // AC5: path portability asserted — the nested module file keeps its
        // forward-slash handle in every variant.
        let nested_paths: Vec<&str> = records
            .iter()
            .filter_map(|record| {
                record
                    .get("repo_relative_path")
                    .and_then(Value::as_str)
                    .filter(|path| path.contains("nested"))
            })
            .collect();
        assert!(
            nested_paths.contains(&"src/nested/mod.rs"),
            "{name} variant: expected forward-slash handle src/nested/mod.rs, saw {nested_paths:?}"
        );
        assert!(
            nested_paths.iter().all(|path| !path.contains('\\')),
            "{name} variant: repo_relative_path contains a backslash: {nested_paths:?}"
        );

        // AC2: symbol record IDs identical across variants (ADR 0004
        // span-independent identity), with span fields no longer diverging.
        let symbols = symbol_records(records);
        assert_eq!(
            symbols.keys().collect::<Vec<_>>(),
            baseline_symbols.keys().collect::<Vec<_>>(),
            "{name} variant: symbol record IDs diverged"
        );
        for (id, record) in &symbols {
            let baseline = &baseline_symbols[id];
            assert_eq!(
                record.get("span"),
                baseline.get("span"),
                "{name} variant: span diverged for symbol {id}"
            );
        }

        // AC3: a pure line-ending difference registers zero drift — no drift
        // records are minted, and every content-derived field feeding drift
        // scoring (summary, signature, doc, symbol text) is identical.
        assert!(
            !records.iter().any(|record| {
                record.get("kind").and_then(Value::as_str) == Some("SemanticDrift")
            }),
            "{name} variant: unexpected SemanticDrift record"
        );
        assert!(
            !records.iter().any(|record| {
                record.get("label").and_then(Value::as_str) == Some("DRIFTS_FROM")
            }),
            "{name} variant: unexpected DriftsFrom edge"
        );
        for (id, record) in &symbols {
            let baseline = &baseline_symbols[id];
            for field in ["summary", "signature", "doc"] {
                assert_eq!(
                    record.get(field),
                    baseline.get(field),
                    "{name} variant: content field {field} diverged for symbol {id}"
                );
            }
        }
    }
}

/// Extracts syntax records for one in-memory source through the public text
/// dispatch, bypassing the scan funnel.
fn extract_via_dispatch(rel: &str, source: &str) -> Vec<GraphRecord> {
    let file = SourceFile {
        path: PathBuf::from(rel),
        repo_relative_path: rel.to_owned(),
    };
    let mut graph = Graph::new();
    extract_source_text(
        &file,
        source,
        "file:direct-dispatch",
        "repo:direct-dispatch",
        &mut graph,
    )
    .expect("source should parse");
    graph.records().to_vec()
}

/// Issue #242 refactor: the public text dispatch (`parser::extract_source_text`,
/// reachable by external hosts) normalizes at the parse boundary, so a direct
/// caller with CRLF source gets exactly the same records as one with LF
/// source — no funnel required. Covers all four supported languages.
#[test]
fn direct_source_text_dispatch_is_line_ending_stable() {
    let cases: &[(&str, &str)] = &[
        (
            "src/main.rs",
            "/// Adds two numbers.\npub fn add(left: usize, right: usize) -> usize {\n    left + right\n}\n",
        ),
        (
            "src/main.py",
            "\"\"\"Adds two numbers.\"\"\"\ndef add(left, right):\n    return left + right\n",
        ),
        (
            "src/main.ts",
            "/** Adds two numbers. */\nexport function add(left: number, right: number): number {\n    return left + right;\n}\n",
        ),
        (
            "src/main.go",
            "// Adds two numbers.\npackage main\n\nfunc Add(left, right int) int {\n    return left + right\n}\n",
        ),
    ];
    for (rel, lf_source) in cases {
        let crlf_source = lf_source.replace('\n', "\r\n");
        let records_lf = extract_via_dispatch(rel, lf_source);
        let records_crlf = extract_via_dispatch(rel, &crlf_source);
        assert!(
            !records_lf.is_empty(),
            "expected records from the LF fixture for {rel}"
        );
        assert_eq!(
            records_lf, records_crlf,
            "direct dispatch diverged on CRLF source for {rel}"
        );
    }
}
