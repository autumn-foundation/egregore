//! Integration tests for `eg query cycles` (issue #138): enumerate dependency
//! cycles among files/modules over already-extracted `IMPORTS` and `CALLS`
//! edges, deterministically, with stable repo-relative handles.

#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::scan_repository_at_with_override;
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-08T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

fn write_fixture(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
            .expect("fixture parent dir should be created");
        fs::write(path, contents).expect("fixture file should be written");
    }
}

/// Scans a fixture tree and writes the JSONL graph. Returns (`TempDir`, graph
/// path). Caller must keep the `TempDir` alive.
fn fixture_graph(repo_id: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path(), files);
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some(repo_id))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_cycles(graph: &Path, extra: &[&str]) -> Value {
    let output = egregore()
        .args(["query", "cycles"])
        .args(extra)
        .arg("--graph")
        .arg(graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON")
}

fn cycle_paths(parsed: &Value) -> Vec<String> {
    parsed["cycles"]
        .as_array()
        .expect("cycles array")
        .iter()
        .map(|c| c["path"].as_str().expect("cycle path").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Acyclic control: a -> b -> c, no edge back.
const ACYCLIC: &[(&str, &str)] = &[
    (
        "src/alpha.rs",
        "pub fn alpha_fn() -> usize {\n    crate::beta::beta_fn()\n}\n",
    ),
    (
        "src/beta.rs",
        "pub fn beta_fn() -> usize {\n    crate::gamma::gamma_fn()\n}\n",
    ),
    ("src/gamma.rs", "pub fn gamma_fn() -> usize {\n    3\n}\n"),
];

/// One three-file cycle closed by resolved cross-file calls:
/// alpha -> beta -> gamma -> alpha.
const ONE_CYCLE: &[(&str, &str)] = &[
    (
        "src/alpha.rs",
        "pub fn alpha_fn() -> usize {\n    crate::beta::beta_fn()\n}\n",
    ),
    (
        "src/beta.rs",
        "pub fn beta_fn() -> usize {\n    crate::gamma::gamma_fn()\n}\n",
    ),
    (
        "src/gamma.rs",
        "pub fn gamma_fn() -> usize {\n    crate::alpha::alpha_fn()\n}\n",
    ),
];

/// Two disjoint cycles: (alpha <-> beta) and (delta <-> epsilon), plus an
/// uninvolved bystander file.
const TWO_CYCLES: &[(&str, &str)] = &[
    (
        "src/alpha.rs",
        "pub fn alpha_fn() -> usize {\n    crate::beta::beta_fn()\n}\n",
    ),
    (
        "src/beta.rs",
        "pub fn beta_fn() -> usize {\n    crate::alpha::alpha_fn()\n}\n",
    ),
    (
        "src/delta.rs",
        "pub fn delta_fn() -> usize {\n    crate::epsilon::epsilon_fn()\n}\n",
    ),
    (
        "src/epsilon.rs",
        "pub fn epsilon_fn() -> usize {\n    crate::delta::delta_fn()\n}\n",
    ),
    ("src/zeta.rs", "pub fn zeta_fn() -> usize {\n    6\n}\n"),
];

/// Two-file cycle closed by `use` imports alone — no cross-file calls.
const IMPORT_CYCLE: &[(&str, &str)] = &[
    (
        "src/alpha.rs",
        "use crate::beta::beta_only;\n\npub fn alpha_only() -> usize {\n    1\n}\n",
    ),
    (
        "src/beta.rs",
        "use crate::alpha::alpha_only;\n\npub fn beta_only() -> usize {\n    2\n}\n",
    ),
];

/// Would-be cycle whose closing edge is only AMBIGUOUS: beta calls alpha
/// (resolved), and alpha calls `dupe()` which is defined in BOTH beta and
/// gamma (ambiguous fan-out). With ambiguous edges excluded there is no
/// provable cycle.
const AMBIGUOUS_ONLY: &[(&str, &str)] = &[
    (
        "src/alpha.rs",
        "pub fn alpha_fn() -> usize {\n    dupe()\n}\n",
    ),
    (
        "src/beta.rs",
        "pub fn dupe() -> usize {\n    crate::alpha::alpha_fn()\n}\n",
    ),
    ("src/gamma.rs", "pub fn dupe() -> usize {\n    3\n}\n"),
];

// ---------------------------------------------------------------------------
// AC: acyclic graph returns an empty result with exit success, not an error
// ---------------------------------------------------------------------------

#[test]
fn query_cycles_acyclic_graph_is_explicit_empty_success() {
    let (_temp, graph) = fixture_graph("cycles-acyclic", ACYCLIC);
    let parsed = run_cycles(&graph, &[]);

    assert_eq!(
        parsed["ok"], true,
        "acyclic graph is a success, not an error"
    );
    assert_eq!(
        parsed["cycles"].as_array().map(Vec::len),
        Some(0),
        "acyclic control must report zero cycles (zero false positives)"
    );
    let diags = parsed["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diags.iter().any(|d| d["code"] == "acyclic"),
        "acyclic result must be explicit, got {diags:?}"
    );
}

// ---------------------------------------------------------------------------
// AC: a planted multi-node cycle is reported with the ordered closing path
// ---------------------------------------------------------------------------

#[test]
fn query_cycles_reports_single_cycle_with_ordered_closing_path() {
    let (_temp, graph) = fixture_graph("cycles-one", ONE_CYCLE);
    let parsed = run_cycles(&graph, &[]);

    assert_eq!(parsed["ok"], true);
    let cycles = parsed["cycles"].as_array().expect("cycles array");
    assert_eq!(cycles.len(), 1, "exactly one planted cycle: {cycles:?}");

    let cycle = &cycles[0];
    assert_eq!(cycle["length"], 3);
    // Canonical rotation: starts at the lexicographically smallest member and
    // the display path closes the loop back to it.
    assert_eq!(
        cycle["path"],
        "src/alpha.rs -> src/beta.rs -> src/gamma.rs -> src/alpha.rs"
    );

    let members = cycle["members"].as_array().expect("members array");
    assert_eq!(members.len(), 3);
    for member in members {
        assert!(
            member["record_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "every member carries a stable record ID: {member}"
        );
        assert!(
            member["repo_relative_path"].as_str().is_some(),
            "every member carries a repo-relative handle: {member}"
        );
    }

    // Each closing edge cites the underlying graph records it was derived from.
    let edges = cycle["edges"].as_array().expect("edges array");
    assert_eq!(edges.len(), 3, "a 3-cycle closes with 3 edges");
    for edge in edges {
        assert!(edge["from"].as_str().is_some());
        assert!(edge["to"].as_str().is_some());
        let evidence = edge["evidence"].as_array().expect("evidence array");
        assert!(!evidence.is_empty(), "edge must cite evidence: {edge}");
        for ev in evidence {
            assert!(ev["record_id"].as_str().is_some_and(|id| !id.is_empty()));
        }
    }
    // The closing call edges are resolved cross-file CALLS edges.
    assert!(
        edges.iter().any(|edge| {
            edge["evidence"].as_array().is_some_and(|evs| {
                evs.iter()
                    .any(|ev| ev["relation"] == "CALLS" && ev["resolution"] == "resolved")
            })
        }),
        "cycle must be driven by resolved CALLS edges: {edges:?}"
    );
}

// ---------------------------------------------------------------------------
// AC: two disjoint cycles are both reported, in canonical order
// ---------------------------------------------------------------------------

#[test]
fn query_cycles_reports_two_disjoint_cycles() {
    let (_temp, graph) = fixture_graph("cycles-two", TWO_CYCLES);
    let parsed = run_cycles(&graph, &[]);

    let paths = cycle_paths(&parsed);
    assert_eq!(
        paths,
        vec![
            "src/alpha.rs -> src/beta.rs -> src/alpha.rs".to_owned(),
            "src/delta.rs -> src/epsilon.rs -> src/delta.rs".to_owned(),
        ],
        "exactly the two planted cycles, sorted canonically"
    );
}

// ---------------------------------------------------------------------------
// AC: import-only cycles are detected over IMPORTS edges
// ---------------------------------------------------------------------------

#[test]
fn query_cycles_detects_cycle_closed_by_imports_alone() {
    let (_temp, graph) = fixture_graph("cycles-imports", IMPORT_CYCLE);
    let parsed = run_cycles(&graph, &[]);

    let cycles = parsed["cycles"].as_array().expect("cycles array");
    assert_eq!(
        cycles.len(),
        1,
        "import-only cycle must be found: {cycles:?}"
    );
    assert_eq!(
        cycles[0]["path"],
        "src/alpha.rs -> src/beta.rs -> src/alpha.rs"
    );
    let edges = cycles[0]["edges"].as_array().expect("edges array");
    assert!(
        edges.iter().all(|edge| {
            edge["evidence"]
                .as_array()
                .is_some_and(|evs| evs.iter().any(|ev| ev["relation"] == "IMPORTS"))
        }),
        "both closing edges are import-derived: {edges:?}"
    );
}

// ---------------------------------------------------------------------------
// Resolution policy: ambiguous CALLS edges never close a cycle
// ---------------------------------------------------------------------------

#[test]
fn query_cycles_excludes_ambiguous_calls_edges_and_reports_the_exclusion() {
    let (_temp, graph) = fixture_graph("cycles-ambiguous", AMBIGUOUS_ONLY);
    let parsed = run_cycles(&graph, &[]);

    assert_eq!(parsed["ok"], true);
    assert_eq!(
        parsed["cycles"].as_array().map(Vec::len),
        Some(0),
        "an ambiguous closing edge must not fabricate a cycle: {:?}",
        parsed["cycles"]
    );
    assert!(
        parsed["counts"]["calls_ambiguous_excluded"]
            .as_u64()
            .is_some_and(|n| n >= 2),
        "excluded ambiguous edges are tallied, got {:?}",
        parsed["counts"]
    );
    let diags = parsed["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diags
            .iter()
            .any(|d| d["code"] == "ambiguous_dependencies_excluded"),
        "the exclusion must be surfaced, got {diags:?}"
    );
}

#[test]
fn query_cycles_repo_scope_excludes_other_repos_excluded_call_tallies() {
    // repo-noisy carries ambiguous CALLS edges; repo-clean is acyclic and
    // unambiguous. A --repo repo-clean response must not leak repo-noisy's
    // excluded-edge tallies or the ambiguity diagnostic.
    let noisy = {
        let inner = tempfile::tempdir().expect("temp dir");
        write_fixture(inner.path(), AMBIGUOUS_ONLY);
        scan_repository_at_with_override(inner.path(), FIXED_TIME, Some("repo-noisy"))
            .expect("scan")
            .to_jsonl()
            .expect("serialize")
    };
    let clean = {
        let inner = tempfile::tempdir().expect("temp dir");
        write_fixture(inner.path(), ACYCLIC);
        scan_repository_at_with_override(inner.path(), FIXED_TIME, Some("repo-clean"))
            .expect("scan")
            .to_jsonl()
            .expect("serialize")
    };
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("multi.jsonl");
    fs::write(&graph, format!("{noisy}{clean}")).expect("write multi-repo graph");

    let scoped = run_cycles(&graph, &["--repo", "repo-clean"]);
    assert_eq!(
        scoped["counts"]["calls_ambiguous_excluded"].as_u64(),
        Some(0),
        "another repo's ambiguous edges must not be tallied in a scoped \
         response, got {:?}",
        scoped["counts"]
    );
    assert_eq!(
        scoped["counts"]["calls_unresolved_excluded"].as_u64(),
        Some(0),
        "another repo's unresolved edges must not be tallied in a scoped \
         response, got {:?}",
        scoped["counts"]
    );
    let diags = scoped["diagnostics"].as_array().expect("diagnostics");
    assert!(
        !diags
            .iter()
            .any(|d| d["code"] == "ambiguous_dependencies_excluded"),
        "another repo's ambiguity diagnostic must not leak into a scoped \
         response, got {diags:?}"
    );

    // The noisy repo, scoped to itself, still reports its own exclusions.
    let noisy_scoped = run_cycles(&graph, &["--repo", "repo-noisy"]);
    assert!(
        noisy_scoped["counts"]["calls_ambiguous_excluded"]
            .as_u64()
            .is_some_and(|n| n >= 2),
        "the owning repo's scoped response keeps its tallies, got {:?}",
        noisy_scoped["counts"]
    );
}

// ---------------------------------------------------------------------------
// Resolution policy: unlabeled CALLS edges never close a cycle
// ---------------------------------------------------------------------------

#[test]
fn query_cycles_excludes_unlabeled_cross_file_calls_edges() {
    // Simulate an older or third-party store whose cross-file CALLS edges
    // predate the resolution field (issues #152/#134): strip `resolution`
    // from every edge record. Absence means "outside the resolution
    // contract", not "resolved" — such edges must not close cycles.
    let (_temp, graph) = fixture_graph("cycles-unlabeled", ONE_CYCLE);
    let stripped: String = fs::read_to_string(&graph)
        .expect("read graph")
        .lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).expect("valid record");
            if record["record_type"] == "edge" {
                record
                    .as_object_mut()
                    .expect("edge object")
                    .remove("resolution");
            }
            let mut serialized = serde_json::to_string(&record).expect("serialize record");
            serialized.push('\n');
            serialized
        })
        .collect();
    fs::write(&graph, stripped).expect("write stripped graph");

    let parsed = run_cycles(&graph, &[]);
    assert_eq!(parsed["ok"], true);
    assert_eq!(
        parsed["cycles"].as_array().map(Vec::len),
        Some(0),
        "an unlabeled cross-file CALLS edge must not close a cycle: {:?}",
        parsed["cycles"]
    );
    assert!(
        parsed["counts"]["calls_unlabeled_excluded"]
            .as_u64()
            .is_some_and(|n| n >= 3),
        "excluded unlabeled cross-file edges are tallied, got {:?}",
        parsed["counts"]
    );
    let diags = parsed["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diags
            .iter()
            .any(|d| d["code"] == "unlabeled_calls_excluded"),
        "the exclusion must be surfaced, got {diags:?}"
    );
}

// ---------------------------------------------------------------------------
// AC: scoped form reports only cycles through the given node
// ---------------------------------------------------------------------------

#[test]
fn query_cycles_scoped_to_file_reports_only_its_cycles() {
    let (_temp, graph) = fixture_graph("cycles-scope", TWO_CYCLES);
    let parsed = run_cycles(&graph, &["src/delta.rs"]);

    assert_eq!(parsed["ok"], true);
    let paths = cycle_paths(&parsed);
    assert_eq!(
        paths,
        vec!["src/delta.rs -> src/epsilon.rs -> src/delta.rs".to_owned()],
        "scope must filter to cycles containing the given node"
    );
    assert_eq!(parsed["scope"]["handle"], "src/delta.rs");
}

#[test]
fn query_cycles_scoped_to_symbol_reports_its_file_cycles() {
    let (_temp, graph) = fixture_graph("cycles-scope-sym", TWO_CYCLES);
    // Symbol handles use the exact recorded (qualified) symbol name, per the
    // established handle-resolution contract shared with `query change-impact`.
    let parsed = run_cycles(&graph, &["alpha::alpha_fn"]);

    let paths = cycle_paths(&parsed);
    assert_eq!(
        paths,
        vec!["src/alpha.rs -> src/beta.rs -> src/alpha.rs".to_owned()],
        "a symbol handle scopes to its defining file's cycles"
    );
}

#[test]
fn query_cycles_scope_outside_any_cycle_is_empty_success() {
    let (_temp, graph) = fixture_graph("cycles-scope-clean", TWO_CYCLES);
    let parsed = run_cycles(&graph, &["src/zeta.rs"]);

    assert_eq!(parsed["ok"], true, "no participation is a success answer");
    assert_eq!(parsed["cycles"].as_array().map(Vec::len), Some(0));
    let diags = parsed["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diags.iter().any(|d| d["code"] == "acyclic"),
        "scoped no-cycle result must be explicit, got {diags:?}"
    );
}

#[test]
fn query_cycles_unknown_scope_handle_exits_2() {
    let (_temp, graph) = fixture_graph("cycles-scope-miss", ONE_CYCLE);
    egregore()
        .args(["query", "cycles", "does_not_exist", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::contains("no_match"));
}

#[test]
fn query_cycles_malformed_canonical_scope_handle_exits_1() {
    let (_temp, graph) = fixture_graph("cycles-scope-bad", ONE_CYCLE);
    egregore()
        .args(["query", "cycles", "codegraph:v1:zzz", "--graph"])
        .arg(&graph)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Unsupported"));
}

// ---------------------------------------------------------------------------
// AC: deterministic, byte-identical output across runs
// ---------------------------------------------------------------------------

#[test]
fn query_cycles_output_is_byte_identical_across_runs() {
    let (_temp, graph) = fixture_graph("cycles-determinism", TWO_CYCLES);

    let run = || {
        egregore()
            .args(["query", "cycles", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let first = run();
    for _ in 0..4 {
        assert_eq!(
            first,
            run(),
            "identical query must produce byte-identical output"
        );
    }
}

// ---------------------------------------------------------------------------
// Output contract: --format text, empty graph, load errors, --repo scoping
// ---------------------------------------------------------------------------

#[test]
fn query_cycles_format_text_renders_human_readable_paths() {
    let (_temp, graph) = fixture_graph("cycles-text", ONE_CYCLE);
    egregore()
        .args(["query", "cycles", "--format", "text", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "src/alpha.rs -> src/beta.rs -> src/gamma.rs -> src/alpha.rs",
        ));
}

#[test]
fn query_cycles_format_text_acyclic_is_explicit() {
    let (_temp, graph) = fixture_graph("cycles-text-empty", ACYCLIC);
    egregore()
        .args(["query", "cycles", "--format", "text", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .stdout(predicate::str::contains("no dependency cycles detected"));
}

#[test]
fn query_cycles_empty_graph_is_explicit_empty_success() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("empty.jsonl");
    fs::write(&graph, "").expect("write empty graph");

    let parsed = run_cycles(&graph, &[]);
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["cycles"].as_array().map(Vec::len), Some(0));
}

#[test]
fn query_cycles_missing_graph_file_fails_with_error() {
    egregore()
        .args(["query", "cycles", "--graph", "/nonexistent/graph.jsonl"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn query_cycles_unknown_repo_selector_exits_1() {
    let (_temp, graph) = fixture_graph("cycles-repo", ONE_CYCLE);
    egregore()
        .args(["query", "cycles", "--repo", "no-such-repo", "--graph"])
        .arg(&graph)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown_repository_selector"));
}

#[test]
fn query_cycles_repo_scoping_restricts_the_graph() {
    // Two repos: alpha repo has a cycle, beta repo is acyclic.
    let temp = tempfile::tempdir().expect("temp dir");
    let cyclic = {
        let inner = tempfile::tempdir().expect("temp dir");
        write_fixture(inner.path(), ONE_CYCLE);
        scan_repository_at_with_override(inner.path(), FIXED_TIME, Some("repo-cyclic"))
            .expect("scan")
            .to_jsonl()
            .expect("serialize")
    };
    let clean = {
        let inner = tempfile::tempdir().expect("temp dir");
        write_fixture(inner.path(), ACYCLIC);
        scan_repository_at_with_override(inner.path(), FIXED_TIME, Some("repo-clean"))
            .expect("scan")
            .to_jsonl()
            .expect("serialize")
    };
    let graph = temp.path().join("multi.jsonl");
    fs::write(&graph, format!("{cyclic}{clean}")).expect("write multi-repo graph");

    let scoped = run_cycles(&graph, &["--repo", "repo-clean"]);
    assert_eq!(
        scoped["cycles"].as_array().map(Vec::len),
        Some(0),
        "the acyclic repo must not inherit the other repo's cycles"
    );

    let scoped_cyclic = run_cycles(&graph, &["--repo", "repo-cyclic"]);
    assert_eq!(scoped_cyclic["cycles"].as_array().map(Vec::len), Some(1));
}

// ---------------------------------------------------------------------------
// Resolution policy: non-Rust imports are excluded and tallied, never
// silently reported as an acyclic graph
// ---------------------------------------------------------------------------

/// A Python import-only loop. Import name resolution is Rust-only in this
/// slice: these must be excluded and tallied with a diagnostic — never
/// silently folded into the external tally, and never a bare "acyclic" claim.
const PYTHON_IMPORT_CYCLE: &[(&str, &str)] = &[
    (
        "pkg/alpha.py",
        "from pkg.beta import beta_fn\n\n\ndef alpha_fn():\n    return 1\n",
    ),
    (
        "pkg/beta.py",
        "from pkg.alpha import alpha_fn\n\n\ndef beta_fn():\n    return 2\n",
    ),
];

#[test]
fn query_cycles_non_rust_imports_are_excluded_and_tallied() {
    let (_temp, graph) = fixture_graph("cycles-python", PYTHON_IMPORT_CYCLE);
    let parsed = run_cycles(&graph, &[]);

    assert_eq!(parsed["ok"], true);
    assert_eq!(
        parsed["cycles"].as_array().map(Vec::len),
        Some(0),
        "non-Rust imports must not fabricate cycles: {:?}",
        parsed["cycles"]
    );
    assert!(
        parsed["counts"]["imports_non_rust_excluded"]
            .as_u64()
            .is_some_and(|n| n >= 2),
        "non-Rust imports are tallied as excluded, not mislabeled external, got {:?}",
        parsed["counts"]
    );
    let diags = parsed["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diags
            .iter()
            .any(|d| d["code"] == "non_rust_imports_excluded"),
        "the Rust-only import-resolution scope must be surfaced, got {diags:?}"
    );
}

// ---------------------------------------------------------------------------
// --data-dir reads must leave the live embedded store byte-for-byte untouched
// ---------------------------------------------------------------------------

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

/// Opening the embedded engine in place re-persists index files, so the
/// `--data-dir` path must read through a throwaway copy (same contract as the
/// other strictly read-only lanes).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn cycles_query_is_read_only_for_embedded_store() {
    let (temp, graph) = fixture_graph("cycles-readonly", ONE_CYCLE);
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
        .args(["query", "cycles", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    assert_eq!(
        dir_contents(&data_dir),
        bytes_before,
        "querying must leave the live embedded store byte-for-byte untouched"
    );
}
