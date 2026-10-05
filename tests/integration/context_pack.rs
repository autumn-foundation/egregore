//! CLI contract tests for budgeted context packs (issue #131).
//!
//! These tests drive the real `eg` binary against a scanned fixture graph:
//! pack completeness under a generous budget, explicit drop accounting under
//! a tight budget, the distinct too-small exit code, byte/token ceiling
//! honesty, read-only behavior, and run-to-run byte identity.

#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

const SYMBOL: &str = "nested::Widget";

/// Sections every context pack carries as bare arrays (AC4: a complete
/// pack's sections are identical to the un-budgeted answer's).
const SECTIONS: [&str; 10] = [
    "source_facts",
    "topology_edges",
    "observations",
    "decisions",
    "project_state",
    "artifacts",
    "verification_evidence",
    "drift_history",
    "unresolved",
    "policy",
];

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

fn scan_graph(work: &Path) -> PathBuf {
    let graph = work.join("graph.jsonl");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph)
        .assert()
        .success();
    graph
}

/// Runs `eg query context <SYMBOL> --graph <graph>` plus extra args;
/// returns (stdout bytes, exit code).
fn run_query(graph: &Path, extra: &[&str]) -> (Vec<u8>, i32) {
    let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
    cmd.arg("query")
        .arg("context")
        .arg(SYMBOL)
        .arg("--graph")
        .arg(graph);
    for arg in extra {
        cmd.arg(arg);
    }
    let output = cmd.output().expect("command should run");
    assert!(
        output.stderr.is_empty(),
        "stderr should stay empty on the JSON lanes: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        output.stdout,
        output
            .status
            .code()
            .expect("process should exit with a code"),
    )
}

fn parse_pack(stdout: &[u8]) -> Value {
    serde_json::from_slice(stdout).expect("pack must be valid JSON")
}

#[test]
fn generous_token_budget_returns_complete_pack_matching_unbudgeted_sections() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = scan_graph(temp.path());

    let (plain_out, plain_code) = run_query(&graph, &[]);
    assert_eq!(plain_code, 0, "unbudgeted query must succeed");
    let plain = parse_pack(&plain_out);

    let (pack_out, pack_code) = run_query(&graph, &["--max-tokens", "1000000"]);
    assert_eq!(pack_code, 0, "generous budget must succeed");
    let pack = parse_pack(&pack_out);

    let budget = &pack["budget"];
    assert_eq!(budget["mode"], "tokens");
    assert_eq!(budget["budget"], 1_000_000);
    assert_eq!(budget["result_complete"], true);
    assert_eq!(budget["token_count_method"], "word-punct-v1");
    assert!(
        budget.get("drop_account").is_none(),
        "a complete pack must carry no drop account"
    );
    let measured = budget["measured"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap();
    assert!(
        measured <= 1_000_000,
        "measured {measured} exceeds the token budget"
    );

    // AC4: every section identical to the un-budgeted answer's.
    for section in SECTIONS {
        assert_eq!(
            &pack[section], &plain[section],
            "section {section} must match the un-budgeted answer"
        );
    }
}

#[test]
fn tight_token_budget_sheds_with_explicit_drop_account() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = scan_graph(temp.path());

    let (full_out, _) = run_query(&graph, &["--max-tokens", "1000000"]);
    let full = parse_pack(&full_out);
    let full_measured = full["budget"]["measured"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap();
    assert!(full_measured > 1, "fixture pack should exceed one token");

    let tight_arg = (full_measured - 1).to_string();
    let (tight_out, tight_code) = run_query(&graph, &["--max-tokens", &tight_arg]);
    assert_eq!(
        tight_code, 0,
        "a budget above the one-record minimum must succeed"
    );
    let pack = parse_pack(&tight_out);
    let budget = &pack["budget"];

    assert_eq!(budget["result_complete"], false);
    let drop_account = budget
        .get("drop_account")
        .expect("an incomplete pack must carry a drop account");
    let dropped_count = drop_account["dropped_count"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap();
    assert!(dropped_count >= 1, "the tight budget must shed something");
    let dropped_ids: Vec<&str> = drop_account["dropped_record_ids"]
        .as_array()
        .expect("dropped_record_ids must be an array")
        .iter()
        .map(|v| v.as_str().expect("dropped ids must be strings"))
        .collect();
    assert_eq!(dropped_ids.len(), dropped_count);

    // No half-records: a dropped ID appears in no kept section.
    let kept: Vec<String> = SECTIONS
        .iter()
        .flat_map(|section| {
            pack[section]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|row| {
                    row.get("record_id")?
                        .as_str()
                        .map(std::string::ToString::to_string)
                })
        })
        .collect();
    for id in &dropped_ids {
        assert!(
            !kept.iter().any(|k| k == id),
            "dropped record {id} must not appear among kept rows"
        );
    }

    let measured = budget["measured"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap();
    assert!(
        measured < full_measured,
        "measured {measured} exceeds the tight budget"
    );
}

#[test]
fn tiny_budget_fails_with_distinct_exit_and_stable_envelope() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = scan_graph(temp.path());

    for (flag, mode) in [
        (["--max-tokens", "1"], "tokens"),
        (["--max-bytes", "1"], "bytes"),
    ] {
        let (out, code) = run_query(&graph, &flag);
        assert_eq!(code, 3, "too-small budget must exit 3, not 0/1/2");
        let envelope = parse_pack(&out);
        assert_eq!(envelope["ok"], false);
        let error = &envelope["error"];
        assert_eq!(error["code"], "budget_too_small");
        assert_eq!(error["symbol_name"], SYMBOL);
        assert_eq!(error["budget_mode"], mode);
        assert_eq!(error["budget"], 1);
        assert!(error["first_record_cost"].as_u64().unwrap() > 1);
        assert!(error["message"].as_str().is_some());

        // Stable: the envelope is byte-identical on re-run.
        let (again, again_code) = run_query(&graph, &flag);
        assert_eq!(again_code, 3);
        assert_eq!(again, out, "too-small envelope must be deterministic");
    }
}

#[test]
fn byte_budget_counts_rendered_bytes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = scan_graph(temp.path());

    let (full_out, full_code) = run_query(&graph, &["--max-bytes", "10000000"]);
    assert_eq!(full_code, 0);
    let full = parse_pack(&full_out);
    let budget = &full["budget"];
    assert_eq!(budget["mode"], "bytes");
    assert_eq!(budget["result_complete"], true);
    assert!(budget.get("token_count_method").is_none());
    let full_measured = budget["measured"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap();
    assert_eq!(
        full_measured,
        full_out.len(),
        "byte mode measures the rendered UTF-8 length"
    );

    // NOTE: the budget limit is serialized inside the pack, so shrinking
    // the limit from 8 digits (`10000000`) to fewer digits *saves* bytes.
    // Subtract well past any digit-count effect so the budget is genuinely
    // tight and must shed at least one row.
    let tight_arg = (full_measured - 50).to_string();
    let (tight_out, tight_code) = run_query(&graph, &["--max-bytes", &tight_arg]);
    assert_eq!(tight_code, 0);
    let pack = parse_pack(&tight_out);
    assert_eq!(pack["budget"]["result_complete"], false);
    assert!(
        pack["budget"]["drop_account"]["dropped_count"]
            .as_u64()
            .unwrap()
            >= 1
    );
    let measured = pack["budget"]["measured"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap();
    assert_eq!(measured, tight_out.len());
    assert!(measured < full_measured - 49);
}

#[test]
fn budgeted_queries_are_read_only_and_byte_identical_across_runs() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = scan_graph(temp.path());
    let before = fs::read(&graph).expect("graph should be readable");

    // A generous, a tight, and a too-small budget, five runs each.
    let (full_out, _) = run_query(&graph, &["--max-tokens", "1000000"]);
    let full_measured = parse_pack(&full_out)["budget"]["measured"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap();
    let tight = (full_measured - 1).to_string();

    let mut generous_runs = Vec::new();
    let mut tight_runs = Vec::new();
    let mut too_small_runs = Vec::new();
    for _ in 0..5 {
        let (out, code) = run_query(&graph, &["--max-tokens", "1000000"]);
        assert_eq!(code, 0);
        generous_runs.push(out);
        let (out, code) = run_query(&graph, &["--max-tokens", &tight]);
        assert_eq!(code, 0);
        tight_runs.push(out);
        let (out, code) = run_query(&graph, &["--max-tokens", "1"]);
        assert_eq!(code, 3);
        too_small_runs.push(out);
    }
    for runs in [&generous_runs, &tight_runs, &too_small_runs] {
        for run in &runs[1..] {
            assert_eq!(run, &runs[0], "pack must be byte-identical across runs");
        }
    }

    let after = fs::read(&graph).expect("graph should be readable");
    assert_eq!(before, after, "budgeted queries must not mutate the graph");
}

#[test]
fn size_budget_conflicts_with_max_records() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = scan_graph(temp.path());

    for extra in [
        ["--max-tokens", "100", "--max-records", "10"],
        ["--max-bytes", "100", "--max-records", "10"],
        ["--max-tokens", "100", "--max-bytes", "100"],
    ] {
        let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
        cmd.arg("query")
            .arg("context")
            .arg(SYMBOL)
            .arg("--graph")
            .arg(&graph);
        for arg in extra {
            cmd.arg(arg);
        }
        cmd.assert()
            .failure()
            .stderr(predicates::str::contains("cannot be used with"));
    }
}
