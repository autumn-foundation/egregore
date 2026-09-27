//! Integration tests for issue #162: deterministic per-symbol complexity
//! for hotspot ranking.
//!
//! RED: these tests fail before the implementation (missing
//! `query::symbol_complexity_ranking`, missing `GraphRecord::complexity`,
//! missing `eg query complexity` verb).
#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use aletheia_egregore::SourceSpan;
#[cfg(feature = "embedded-aletheiadb")]
use aletheia_egregore::adapters::{EmbeddedAletheiaSink, ingest_records};
use aletheia_egregore::{
    GraphRecord, NodeKind, query::COMPLEXITY_DEFAULT_LIMIT, query::COMPLEXITY_MAX_LIMIT,
    query::symbol_complexity_ranking, scan_repository_at_with_override,
};
use assert_cmd::Command as CargoCommand;
use serde_json::Value;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_complexity")
}

fn scan_fixture() -> Vec<GraphRecord> {
    scan_repository_at_with_override(
        fixture_repo(),
        "2026-09-27T00:00:00Z",
        Some("complexity-fixture-repo"),
    )
    .expect("complexity fixture should scan")
    .into_records()
}

/// Simple name → `(symbol_kind, complexity)` for every `Symbol` node.
fn symbol_complexities(records: &[GraphRecord]) -> Vec<(String, String, Option<u32>)> {
    let mut out = Vec::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Symbol,
            name: Some(name),
            symbol_kind,
            ..
        } = record
        {
            let simple = name.rsplit("::").next().unwrap_or(name).to_owned();
            out.push((
                simple,
                symbol_kind.clone().unwrap_or_default(),
                record.complexity(),
            ));
        }
    }
    out.sort();
    out
}

fn complexity_of(records: &[GraphRecord], simple_name: &str) -> Option<u32> {
    symbol_complexities(records)
        .into_iter()
        .find(|(name, _, _)| name == simple_name)
        .and_then(|(_, _, complexity)| complexity)
}

// ── AC1: every Rust callable symbol carries an integer complexity ────────────

#[test]
fn all_callable_symbols_carry_complexity_and_only_callables_do() {
    let records = scan_fixture();
    let symbols = symbol_complexities(&records);
    assert!(!symbols.is_empty(), "fixture should yield symbols");

    let mut missing = Vec::new();
    let mut unexpected = Vec::new();
    for (name, kind, complexity) in &symbols {
        let is_callable = matches!(kind.as_str(), "function" | "method" | "test");
        match (is_callable, complexity) {
            (true, None) => missing.push(name.clone()),
            (false, Some(_)) => unexpected.push(name.clone()),
            _ => {}
        }
    }
    assert!(
        missing.is_empty(),
        "callable symbols missing complexity: {missing:?}"
    );
    assert!(
        unexpected.is_empty(),
        "non-callable symbols carrying complexity: {unexpected:?}"
    );
    // The success metric's "0 missing" is exactly this assertion.
    let callable_count = symbols
        .iter()
        .filter(|(_, kind, _)| matches!(kind.as_str(), "function" | "method" | "test"))
        .count();
    assert!(callable_count >= 15, "fixture should cover many callables");
}

// ── AC1/AC2: documented minimum and monotonic sanity ─────────────────────────

#[test]
fn trivial_scores_documented_minimum_and_complex_ranks_above() {
    let records = scan_fixture();
    assert_eq!(
        complexity_of(&records, "trivial_add"),
        Some(1),
        "straight-line function must score the documented minimum"
    );
    assert_eq!(
        complexity_of(&records, "gnarly"),
        Some(11),
        "engineered high-complexity function score"
    );
    assert!(
        complexity_of(&records, "gnarly").unwrap()
            > complexity_of(&records, "trivial_add").unwrap()
    );
}

#[test]
fn monotonic_sanity_chain_is_strictly_increasing() {
    let records = scan_fixture();
    let mut previous = 0u32;
    for (i, expected) in (0..=6).zip(1..=7u32) {
        let name = format!("chain_{i}");
        let score = complexity_of(&records, &name)
            .unwrap_or_else(|| panic!("fixture function {name} should scan"));
        assert_eq!(score, expected, "{name} should score exactly {expected}");
        assert!(
            score > previous,
            "{name} must score strictly above its predecessor"
        );
        previous = score;
    }
    // Each additional decision point strictly increases the score.
    assert_eq!(complexity_of(&records, "chain_try"), Some(3));
    assert_eq!(complexity_of(&records, "chain_loop"), Some(2));
    assert_eq!(complexity_of(&records, "with_closure"), Some(2));
    // A nested fn keeps its own symbol and does not inflate the outer body.
    assert_eq!(complexity_of(&records, "outer_with_nested"), Some(2));
    assert_eq!(complexity_of(&records, "inner"), Some(2));
}

#[test]
fn trait_methods_impl_methods_and_tests_carry_complexity() {
    let records = scan_fixture();
    assert_eq!(complexity_of(&records, "hello"), Some(1));
    assert_eq!(complexity_of(&records, "loud_hello"), Some(2));
    assert_eq!(complexity_of(&records, "bump"), Some(2));
    assert_eq!(complexity_of(&records, "complexity_smoke"), Some(1));
    let symbols = symbol_complexities(&records);
    let kind_of = |simple: &str| {
        symbols
            .iter()
            .find(|(name, _, _)| name == simple)
            .map(|(_, kind, _)| kind.clone())
            .unwrap()
    };
    assert_eq!(kind_of("bump"), "method");
    assert_eq!(kind_of("complexity_smoke"), "test");
}

// ── AC3/AC5: ranking verb semantics ──────────────────────────────────────────

#[test]
fn ranking_orders_descending_with_documented_stable_tiebreak() {
    let records = scan_fixture();
    let report = symbol_complexity_ranking(&records, None, COMPLEXITY_MAX_LIMIT)
        .expect("ranking should succeed");
    assert_eq!(report.ranking_basis, "structural_complexity");
    assert_eq!(report.tie_break, "qualified_name");
    assert!(!report.symbols.is_empty());

    // Strictly descending complexity; the engineered gnarly fn is rank 1.
    let top = &report.symbols[0];
    assert_eq!(top.rank, 1);
    assert!(
        top.name.ends_with("gnarly"),
        "gnarly should top the ranking"
    );
    assert_eq!(top.complexity, 11);
    let mut previous = u32::MAX;
    for row in &report.symbols {
        assert!(
            row.complexity <= previous,
            "ranking must be descending: {} ({}) after {previous}",
            row.name,
            row.complexity
        );
        previous = row.complexity;
    }
    // Ties break on ascending qualified name.
    for window in report.symbols.windows(2) {
        let (a, b) = (&window[0], &window[1]);
        if a.complexity == b.complexity {
            assert!(
                a.name <= b.name,
                "tie at complexity {} must break on qualified name: {} vs {}",
                a.complexity,
                a.name,
                b.name
            );
        }
    }
    // Every row's handle resolves to an existing Symbol node.
    for row in &report.symbols {
        let resolves = records.iter().any(|r| match r {
            GraphRecord::Node {
                kind: NodeKind::Symbol,
                id,
                ..
            } => id == &row.symbol_record_id,
            _ => false,
        });
        assert!(
            resolves,
            "row {} cites a symbol_record_id that resolves: {}",
            row.name, row.symbol_record_id
        );
        assert!(
            row.repo_relative_path.ends_with("src/lib.rs"),
            "row carries a repo-relative file handle"
        );
    }
    // The trivial function sits at the bottom with the documented minimum.
    let bottom = report.symbols.last().unwrap();
    assert_eq!(bottom.complexity, 1);
    assert!(!report.truncated);
    assert_eq!(report.returned_symbol_count, report.total_symbol_count);
}

#[test]
fn limit_caps_output_and_reports_truncation() {
    let records = scan_fixture();
    let full = symbol_complexity_ranking(&records, None, COMPLEXITY_MAX_LIMIT)
        .expect("ranking should succeed");
    assert!(full.total_symbol_count > 3);

    let capped = symbol_complexity_ranking(&records, None, 3).expect("ranking should succeed");
    assert_eq!(capped.limit, 3);
    assert_eq!(capped.symbols.len(), 3);
    assert_eq!(capped.returned_symbol_count, 3);
    assert_eq!(capped.total_symbol_count, full.total_symbol_count);
    assert!(capped.truncated, "answer must state that it was truncated");
    // Truncation keeps the ranking head: still gnarly first.
    assert!(capped.symbols[0].name.ends_with("gnarly"));
}

#[test]
fn limit_bounds_are_documented_and_enforced() {
    assert_eq!(COMPLEXITY_DEFAULT_LIMIT, 50);
    assert_eq!(COMPLEXITY_MAX_LIMIT, 500);
}

// ── AC4: byte-identical complexity across runs and line endings ───────────────

#[test]
fn complexity_values_are_byte_identical_across_five_runs() {
    let mut serialized = Vec::new();
    for _ in 0..5 {
        let records = scan_fixture();
        let report = symbol_complexity_ranking(&records, None, COMPLEXITY_MAX_LIMIT)
            .expect("ranking should succeed");
        serialized.push(serde_json::to_string(&report).expect("report should serialize"));
    }
    for (i, run) in serialized.iter().enumerate().skip(1) {
        assert_eq!(
            run, &serialized[0],
            "run {i} ranking differs from run 0 — complexity is not deterministic"
        );
    }
}

#[test]
fn line_endings_do_not_change_complexity() {
    let lf_dir = tempfile::tempdir().expect("tempdir");
    let crlf_dir = tempfile::tempdir().expect("tempdir");
    let source = fs::read_to_string(fixture_repo().join("src/lib.rs")).expect("fixture source");
    fs::create_dir_all(lf_dir.path().join("src")).unwrap();
    fs::create_dir_all(crlf_dir.path().join("src")).unwrap();
    fs::write(lf_dir.path().join("src/lib.rs"), &source).unwrap();
    fs::write(
        crlf_dir.path().join("src/lib.rs"),
        source.replace('\n', "\r\n"),
    )
    .unwrap();

    let scan = |dir: &Path| {
        scan_repository_at_with_override(dir, "2026-09-27T00:00:00Z", Some("eol-fixture-repo"))
            .expect("eol fixture should scan")
            .into_records()
    };
    let lf = symbol_complexities(&scan(lf_dir.path()));
    let crlf = symbol_complexities(&scan(crlf_dir.path()));
    assert_eq!(
        lf, crlf,
        "complexity must be identical across LF and CRLF checkouts"
    );
}

// ── AC3/AC6: CLI surface ─────────────────────────────────────────────────────

fn write_graph_jsonl(dir: &Path) -> PathBuf {
    let jsonl = scan_repository_at_with_override(
        fixture_repo(),
        "2026-09-27T00:00:00Z",
        Some("complexity-cli-repo"),
    )
    .expect("fixture should scan")
    .to_jsonl()
    .expect("graph should serialize");
    let path = dir.join("graph.jsonl");
    fs::write(&path, jsonl).expect("graph jsonl should be written");
    path
}

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should be built")
}

#[test]
fn query_complexity_cli_returns_stable_json_envelope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let graph = write_graph_jsonl(dir.path());

    let output = egregore()
        .args(["query", "complexity", "--graph"])
        .arg(&graph)
        .output()
        .expect("eg should run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // One JSON object on one line: the stable JSONL query contract.
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert_eq!(stdout.lines().count(), 1);
    let envelope: Value = serde_json::from_str(stdout.trim()).expect("valid json");
    assert_eq!(envelope["ok"], true);
    let result = &envelope["result"];
    assert_eq!(result["ranking_basis"], "structural_complexity");
    assert_eq!(result["tie_break"], "qualified_name");
    assert_eq!(result["limit"], 50);
    assert_eq!(result["truncated"], false);
    let symbols = result["symbols"].as_array().expect("symbols array");
    assert!(!symbols.is_empty());
    let top = &symbols[0];
    assert_eq!(top["complexity"], 11);
    assert!(top["name"].as_str().unwrap().ends_with("gnarly"));
    assert!(
        top["repo_relative_path"]
            .as_str()
            .unwrap()
            .ends_with("src/lib.rs")
    );
    assert!(top["symbol_record_id"].is_string());
    assert!(top["span"]["start_line"].is_number());
}

#[test]
fn query_complexity_cli_text_format_and_truncation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let graph = write_graph_jsonl(dir.path());

    let output = egregore()
        .args(["query", "complexity", "--graph"])
        .arg(&graph)
        .args(["--limit", "2", "--format", "text"])
        .output()
        .expect("eg should run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(
        stdout.contains("truncated:"),
        "text format must state truncation: {stdout}"
    );
    assert!(
        stdout.contains("gnarly"),
        "text format names the top symbol"
    );
}

#[test]
fn query_complexity_cli_rejects_bad_limits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let graph = write_graph_jsonl(dir.path());
    for bad in ["0", "501"] {
        let output = egregore()
            .args(["query", "complexity", "--graph"])
            .arg(&graph)
            .args(["--limit", bad])
            .output()
            .expect("eg should run");
        assert_eq!(output.status.code(), Some(1), "limit {bad} must exit 1");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("invalid_limit"),
            "limit {bad} must emit invalid_limit: {stderr}"
        );
    }
}

#[test]
fn query_symbol_row_returns_complexity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let graph = write_graph_jsonl(dir.path());

    let output = egregore()
        .args(["query", "symbol", "gnarly", "--graph"])
        .arg(&graph)
        .output()
        .expect("eg should run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let mut found = false;
    for line in stdout.lines() {
        let row: Value = serde_json::from_str(line).expect("valid json row");
        if row["name"].as_str().is_some_and(|n| n.ends_with("gnarly")) {
            assert_eq!(row["complexity"], 11);
            found = true;
        }
    }
    assert!(found, "gnarly row should appear in query symbol output");
}

// ── Success metric: 0 missing complexity fields on the repo's own scan ──────
//
// The repository scans itself (`CARGO_MANIFEST_DIR`) and asserts every Rust
// callable Symbol carries a complexity score. A single missing field means
// the extractor skipped a callable the metric was meant to cover.
//
// Complexity is computed by the Rust extractor only (issue scope), so the
// gate covers Rust callables; other-language callables are outside the
// metric's domain and are not counted here.

#[test]
fn egregore_self_scan_has_zero_missing_complexity_fields() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let records = scan_repository_at_with_override(
        &manifest_dir,
        "2026-09-27T00:00:00Z",
        Some("egregore-self-complexity"),
    )
    .expect("self scan should succeed")
    .into_records();
    let mut missing = Vec::new();
    let mut checked = 0usize;
    for record in &records {
        if let GraphRecord::Node {
            kind: NodeKind::Symbol,
            language: Some(language),
            symbol_kind: Some(kind),
            name,
            repo_relative_path,
            complexity,
            ..
        } = record
            && language == "rust"
            && matches!(kind.as_str(), "function" | "method" | "test")
        {
            checked += 1;
            if complexity.is_none() {
                missing.push(format!(
                    "{} {} {}",
                    name.as_deref().unwrap_or("?"),
                    repo_relative_path.as_deref().unwrap_or("?"),
                    record.id()
                ));
            }
        }
    }
    assert!(checked > 0, "self scan should find Rust callable symbols");
    assert!(
        missing.is_empty(),
        "0 missing complexity fields required; {} of {checked} callables missing: {:?}",
        missing.len(),
        &missing[..missing.len().min(10)]
    );
}

// ── Portability: repo-relative paths use `/`, never absolute, never `\` ─────

#[test]
fn complexity_ranking_paths_are_portable_repo_relative() {
    let records = scan_fixture();
    let report = symbol_complexity_ranking(&records, None, COMPLEXITY_MAX_LIMIT).expect("ranking");
    assert!(
        !report.symbols.is_empty(),
        "ranking should contain fixture symbols"
    );
    for row in &report.symbols {
        let path = &row.repo_relative_path;
        assert!(
            !Path::new(path).is_absolute(),
            "repo-relative path must not be absolute: {path}"
        );
        assert!(
            !path.contains('\\'),
            "repo-relative path must not contain backslashes: {path}"
        );
        assert!(
            !path.contains("rust_complexity"),
            "path must be relative to the repo root, not the checkout: {path}"
        );
    }
}

// ── AletheiaDB round trip: complexity survives write/read, re-ingest is ─────
// idempotent (no phantom duplicate versions) and score updates land.

/// The `complexity` property must survive the embedded-store write/read round
/// trip, repeated ingestion of the same record must not create phantom
/// versions, and a changed score must replace the old one on re-ingest.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_complexity_survives_round_trip_and_reingest_is_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let data_dir = temp.path().join("complexity-store");
    let span = SourceSpan {
        start_byte: 0,
        end_byte: 42,
        start_line: 1,
        end_line: 1,
        start_column: None,
        end_column: None,
    };
    let record = GraphRecord::symbol(
        "test:complexity-round-trip".to_owned(),
        "function",
        "src/lib.rs".to_owned(),
        span,
        "gnarly".to_owned(),
        "round-trip fixture".to_owned(),
    )
    .with_complexity(7);

    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("store should open");
    let report = ingest_records(std::slice::from_ref(&record), &mut sink);
    assert!(report.is_success(), "{report:?}");
    assert_eq!(
        sink.read_back("test:complexity-round-trip")
            .expect("read-back should succeed"),
        Some(record.clone()),
        "complexity must survive the write/read round trip"
    );

    // Re-ingest the identical record: no phantom version may appear and the
    // read-back must stay byte-identical across a close/reopen boundary.
    let second = ingest_records(std::slice::from_ref(&record), &mut sink);
    assert!(second.is_success(), "{second:?}");
    sink.persist_indexes().expect("indexes should persist");
    drop(sink);
    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("store should reopen");
    assert_eq!(
        reopened
            .read_back("test:complexity-round-trip")
            .expect("read-back after reopen should succeed"),
        Some(record),
        "re-ingest must be idempotent; no phantom duplicate versions"
    );

    // A changed score replaces the old one on re-ingest rather than layering.
    let mut sink = reopened;
    let updated = GraphRecord::symbol(
        "test:complexity-round-trip".to_owned(),
        "function",
        "src/lib.rs".to_owned(),
        span,
        "gnarly".to_owned(),
        "round-trip fixture".to_owned(),
    )
    .with_complexity(12);
    let third = ingest_records(std::slice::from_ref(&updated), &mut sink);
    assert!(third.is_success(), "{third:?}");
    assert_eq!(
        sink.read_back("test:complexity-round-trip")
            .expect("read-back should succeed"),
        Some(updated),
        "updated complexity must replace the stored score"
    );
}
