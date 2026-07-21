//! End-to-end tests for issue #456: the remaining ten latent Category-A
//! current-state query lanes flip from a UNION corpus default to a HEAD-anchored
//! corpus default, under the published #427/#457 corpus-mode contract.
//!
//! The ten lanes: change-impact, cycles, evidence-path, context, subsystem,
//! symbol, failures, unsafe-sites, unwrap-expect, debt-markers.
//!
//! Each lane over a scan-history store carrying a `source_snapshot` HEAD must:
//!   * default to `head_anchored` / `default`, excluding records removed at HEAD;
//!   * honor `--all-history` (union / explicit_flag), keeping them;
//!   * honor `--at-head` (head_anchored / explicit_flag);
//!   * reject conflicting corpus/temporal flag pairs with an
//!     `unsupported_combination` envelope (exit 1);
//!   * agree byte-for-byte across `--graph` and `--data-dir`.
#![allow(
    missing_docs,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, SnapshotHead, SourceSnapshotPayload, SourceSpan,
    TemporalMetadata,
    ir::{Graph, stable_id},
};
use assert_cmd::Command;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-02-01T00:00:00Z";
const C1: &str = "aaaa1111";
const C2: &str = "bbbb2222";

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

fn sym_id(path: &str, name: &str) -> String {
    stable_id(&["node", "Symbol", path, name])
}

fn file_id(path: &str) -> String {
    stable_id(&["node", "File", path])
}

fn temporal(commit: &str, parents: &[&str], vt: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|s| (*s).to_owned()).collect(),
        valid_time: vt.to_owned(),
        author_time: Some(vt.to_owned()),
        observed_at: vt.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

/// The stable IDs the fixture exposes for content assertions.
struct Ids {
    keeper: String,
    gone: String,
    staying: String,
    ev_src: String,
    ev_tgt: String,
    unsafe_gone: String,
    unsafe_staying: String,
}

/// A snapshot-bearing two-commit fixture (HEAD pinned to c2). At c1 the symbol
/// `gone` and a stale unsafe site exist and call/target `keeper`; at c2 they are
/// removed and `staying` takes over. The `Repository` node carries a
/// `source_snapshot` HEAD at c2 so HEAD-anchoring is possible: the deleted-at-HEAD
/// records must be excluded from the default / `--at-head` corpus but present in
/// the `--all-history` union. Two non-temporal Symbol nodes (`ev_src`/`ev_tgt`)
/// are linked only by a stale (c1) `HAS_EVIDENCE` edge for the evidence-path lane.
fn seed_snapshot() -> (tempfile::TempDir, PathBuf, Ids) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("snapshot.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-456"]);
    graph.push(
        GraphRecord::node(
            repo_id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some("repo-456".to_owned()),
            "Repository repo-456".to_owned(),
        )
        .with_source_snapshot(SourceSnapshotPayload {
            head: SnapshotHead::Commit { sha: C2.to_owned() },
            dirty: false,
            repository_id: repo_id.clone(),
            scanned_at: T2.to_owned(),
        }),
    );

    let commit_in = |graph: &mut Graph, sha: &str, parents: &[&str], vt: &str| {
        let commit_id = stable_id(&["node", "commit", "repo-456", sha]);
        graph.push(
            GraphRecord::node(
                commit_id.clone(),
                NodeKind::Commit,
                None,
                None,
                Some(sha.to_owned()),
                format!("Commit {sha}"),
            )
            .with_temporal(temporal(sha, parents, vt)),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.clone(),
            commit_id,
            None,
            format!("repo contains commit {sha}"),
        ));
    };
    commit_in(&mut graph, C1, &[], T1);
    commit_in(&mut graph, C2, &[C1], T2);

    // File attributed to the repo; symbols attributed to the file via DEFINES so
    // `RepositoryIndex::owner_of` resolves each to `repo-456`.
    let fpath = "src/h.rs";
    let fid = file_id(fpath);
    graph.push(GraphRecord::syntax_node(
        fid.clone(),
        NodeKind::File,
        fpath.to_owned(),
        span(1, 100),
        "h.rs".to_owned(),
        "rust",
        "file src/h.rs".to_owned(),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.clone(),
        fid.clone(),
        None,
        "repo contains file".to_owned(),
    ));

    let hist_symbol = |graph: &mut Graph, name: &str, commit: &str, vt: &str| -> String {
        let id = sym_id(fpath, name);
        graph.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::Symbol,
                fpath.to_owned(),
                span(1, 10),
                name.to_owned(),
                "rust",
                format!("fn {name}"),
            )
            .with_temporal(temporal(commit, &[], vt)),
        );
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Defines,
                fid.clone(),
                id.clone(),
                None,
                format!("defines {name}"),
            )
            .with_temporal(temporal(commit, &[], vt)),
        );
        id
    };

    let keeper = hist_symbol(&mut graph, "keeper", C1, T1);
    hist_symbol(&mut graph, "keeper", C2, T2);
    let gone = hist_symbol(&mut graph, "gone", C1, T1);
    let staying = hist_symbol(&mut graph, "staying", C2, T2);

    // `gone` CALLS `keeper` at c1 (off-HEAD); `staying` CALLS `keeper` at c2
    // (HEAD). change-impact of `keeper`: default → {staying}; union → {gone, staying}.
    let hist_call = |graph: &mut Graph, from: &str, to: &str, commit: &str, vt: &str| {
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Calls,
                from.to_owned(),
                to.to_owned(),
                Some("1.0".to_owned()),
                "call".to_owned(),
            )
            .with_resolution(aletheia_egregore::CallResolution::Resolved)
            .with_temporal(temporal(commit, &[], vt)),
        );
    };
    hist_call(&mut graph, &gone, &keeper, C1, T1);
    hist_call(&mut graph, &staying, &keeper, C2, T2);

    // Evidence-path lane: two ALWAYS-current (non-temporal) endpoints joined only
    // by a stale (c1) HAS_EVIDENCE edge. Default head-anchoring drops the edge
    // (no witness path); --all-history keeps it (path found).
    let ev_src = sym_id("src/ev.rs", "ev_src");
    let ev_tgt = sym_id("src/ev.rs", "ev_tgt");
    for (id, name) in [(&ev_src, "ev_src"), (&ev_tgt, "ev_tgt")] {
        graph.push(GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Symbol,
            "src/ev.rs".to_owned(),
            span(1, 5),
            name.to_owned(),
            "rust",
            format!("fn {name}"),
        ));
    }
    graph.push(
        GraphRecord::edge(
            EdgeLabel::HasEvidence,
            ev_src.clone(),
            ev_tgt.clone(),
            None,
            "stale evidence link".to_owned(),
        )
        .with_temporal(temporal(C1, &[], T1)),
    );

    // Unsafe-sites lane: one UnsafeSite at c1 (removed at HEAD) and one at c2.
    let unsafe_site = |graph: &mut Graph, disambig: &str, commit: &str, vt: &str| -> String {
        let id = stable_id(&["node", "UnsafeSite", fpath, disambig]);
        graph.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::UnsafeSite,
                fpath.to_owned(),
                span(3, 3),
                "block".to_owned(),
                "rust",
                format!("unsafe block {disambig}"),
            )
            .with_temporal(temporal(commit, &[], vt)),
        );
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Contains,
                fid.clone(),
                id.clone(),
                None,
                format!("file contains unsafe {disambig}"),
            )
            .with_temporal(temporal(commit, &[], vt)),
        );
        id
    };
    let unsafe_gone = unsafe_site(&mut graph, "u1", C1, T1);
    let unsafe_staying = unsafe_site(&mut graph, "u2", C2, T2);

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    (
        temp,
        path,
        Ids {
            keeper,
            gone,
            staying,
            ev_src,
            ev_tgt,
            unsafe_gone,
            unsafe_staying,
        },
    )
}

/// Runs a query expected to succeed and returns the first stdout line parsed as
/// JSON (the envelope) plus the raw stdout.
fn run_ok(args: &[&str]) -> (Value, String) {
    let out = egregore()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).expect("utf8");
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("{}");
    (serde_json::from_str(first).expect("json envelope"), text)
}

/// Parses a single pretty-printed JSON object from full stdout.
fn run_ok_object(args: &[&str]) -> Value {
    let out = egregore()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).expect("json object")
}

/// Asserts the argument set exits 1 with an `unsupported_combination` envelope.
fn assert_unsupported(args: &[&str]) {
    let assert = egregore().args(args).assert().failure().code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let value: Value =
        serde_json::from_str(out.lines().next().expect("envelope")).expect("json envelope");
    assert_eq!(value["ok"], false, "{out}");
    assert_eq!(
        value["error"]["code"], "unsupported_combination",
        "expected unsupported_combination: {out}"
    );
}

fn g(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

// ---------------------------------------------------------------------------
// change-impact
// ---------------------------------------------------------------------------

#[test]
fn change_impact_default_head_anchors() {
    let (_t, path, ids) = seed_snapshot();
    let env = run_ok_object(&["query", "change-impact", &ids.keeper, "--graph", &g(&path)]);
    assert_eq!(env["corpus_mode"], "head_anchored");
    assert_eq!(env["corpus_mode_source"], "default");
    let callers: Vec<&str> = env["direct_callers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(
        callers.contains(&ids.staying.as_str()),
        "HEAD caller present: {callers:?}"
    );
    assert!(
        !callers.contains(&ids.gone.as_str()),
        "deleted-at-HEAD caller excluded by default: {callers:?}"
    );
}

#[test]
fn change_impact_all_history_is_union() {
    let (_t, path, ids) = seed_snapshot();
    let env = run_ok_object(&[
        "query",
        "change-impact",
        &ids.keeper,
        "--graph",
        &g(&path),
        "--all-history",
    ]);
    assert_eq!(env["corpus_mode"], "union");
    assert_eq!(env["corpus_mode_source"], "explicit_flag");
    let callers: Vec<&str> = env["direct_callers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(callers.contains(&ids.gone.as_str()), "{callers:?}");
    assert!(callers.contains(&ids.staying.as_str()), "{callers:?}");
}

#[test]
fn change_impact_at_head_is_explicit() {
    let (_t, path, ids) = seed_snapshot();
    let env = run_ok_object(&[
        "query",
        "change-impact",
        &ids.keeper,
        "--graph",
        &g(&path),
        "--at-head",
    ]);
    assert_eq!(env["corpus_mode"], "head_anchored");
    assert_eq!(env["corpus_mode_source"], "explicit_flag");
}

#[test]
fn change_impact_conflicting_flags_rejected() {
    let (_t, path, ids) = seed_snapshot();
    assert_unsupported(&[
        "query",
        "change-impact",
        &ids.keeper,
        "--graph",
        &g(&path),
        "--at-head",
        "--all-history",
    ]);
}

// ---------------------------------------------------------------------------
// context
// ---------------------------------------------------------------------------

#[test]
fn context_default_head_anchors_hides_deleted_symbol() {
    let (_t, path, ids) = seed_snapshot();
    // `gone` was removed before HEAD: the default corpus has no such symbol.
    egregore()
        .args(["query", "context", "gone", "--graph", &g(&path)])
        .assert()
        .failure()
        .code(2);
    // Under --all-history the earlier-commit symbol is found.
    let env = run_ok_object(&[
        "query",
        "context",
        "gone",
        "--graph",
        &g(&path),
        "--all-history",
    ]);
    assert_eq!(env["corpus_mode"], "union");
    assert_eq!(env["ok"], true);
    let _ = &ids;
}

#[test]
fn context_keeper_default_reports_head_anchored() {
    let (_t, path, _ids) = seed_snapshot();
    let env = run_ok_object(&["query", "context", "keeper", "--graph", &g(&path)]);
    assert_eq!(env["corpus_mode"], "head_anchored");
    assert_eq!(env["corpus_mode_source"], "default");
}

#[test]
fn context_conflicting_flags_rejected() {
    let (_t, path, _ids) = seed_snapshot();
    assert_unsupported(&[
        "query",
        "context",
        "keeper",
        "--graph",
        &g(&path),
        "--at-head",
        "--all-history",
    ]);
}

// ---------------------------------------------------------------------------
// subsystem
// ---------------------------------------------------------------------------

#[test]
fn subsystem_default_head_anchors() {
    let (_t, path, ids) = seed_snapshot();
    let env = run_ok_object(&["query", "subsystem", "src/h.rs", "--graph", &g(&path)]);
    assert_eq!(env["corpus_mode"], "head_anchored");
    assert_eq!(env["corpus_mode_source"], "default");
    let facts: Vec<&str> = env["source_facts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(
        !facts.contains(&ids.gone.as_str()),
        "gone excluded: {facts:?}"
    );
    assert!(
        facts.contains(&ids.staying.as_str()),
        "staying present: {facts:?}"
    );
}

#[test]
fn subsystem_all_history_includes_deleted() {
    let (_t, path, ids) = seed_snapshot();
    let env = run_ok_object(&[
        "query",
        "subsystem",
        "src/h.rs",
        "--graph",
        &g(&path),
        "--all-history",
    ]);
    assert_eq!(env["corpus_mode"], "union");
    let facts: Vec<&str> = env["source_facts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(facts.contains(&ids.gone.as_str()), "{facts:?}");
}

#[test]
fn subsystem_conflicting_flags_rejected() {
    let (_t, path, _ids) = seed_snapshot();
    assert_unsupported(&[
        "query",
        "subsystem",
        "src/h.rs",
        "--graph",
        &g(&path),
        "--at-head",
        "--all-history",
    ]);
}

// ---------------------------------------------------------------------------
// symbol (row-level disclosure)
// ---------------------------------------------------------------------------

#[test]
fn symbol_default_head_anchors_hides_deleted() {
    let (_t, path, _ids) = seed_snapshot();
    // `gone` removed before HEAD: default has no match (exit 2).
    egregore()
        .args(["query", "symbol", "gone", "--graph", &g(&path)])
        .assert()
        .failure()
        .code(2);
    // Under --all-history it is found and stamped union.
    let (env, _) = run_ok(&[
        "query",
        "symbol",
        "gone",
        "--graph",
        &g(&path),
        "--all-history",
    ]);
    assert_eq!(env["corpus_mode"], "union");
    assert_eq!(env["corpus_mode_source"], "explicit_flag");
}

#[test]
fn symbol_keeper_default_single_head_row() {
    let (_t, path, _ids) = seed_snapshot();
    let out = egregore()
        .args(["query", "symbol", "keeper", "--graph", &g(&path)])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).expect("utf8");
    let rows: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect();
    // The union would return one row per commit (c1 + c2); head-anchoring keeps
    // only the HEAD version.
    assert_eq!(
        rows.len(),
        1,
        "head-anchored keeper is a single row: {text}"
    );
    assert_eq!(rows[0]["corpus_mode"], "head_anchored");
    assert_eq!(rows[0]["corpus_mode_source"], "default");
    assert_eq!(rows[0]["git_commit"], C2);
}

#[test]
fn symbol_all_history_returns_both_versions() {
    let (_t, path, _ids) = seed_snapshot();
    let out = egregore()
        .args([
            "query",
            "symbol",
            "keeper",
            "--graph",
            &g(&path),
            "--all-history",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).expect("utf8");
    let rows = text.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(rows, 2, "union returns both commit versions: {text}");
}

#[test]
fn symbol_conflicting_flags_rejected() {
    let (_t, path, _ids) = seed_snapshot();
    assert_unsupported(&[
        "query",
        "symbol",
        "keeper",
        "--graph",
        &g(&path),
        "--at-head",
        "--all-history",
    ]);
    assert_unsupported(&[
        "query",
        "symbol",
        "keeper",
        "--graph",
        &g(&path),
        "--at-head",
        "--at",
        C2,
    ]);
    assert_unsupported(&[
        "query",
        "symbol",
        "keeper",
        "--graph",
        &g(&path),
        "--all-history",
        "--as-of",
        T2,
    ]);
}

// ---------------------------------------------------------------------------
// cycles
// ---------------------------------------------------------------------------

#[test]
fn cycles_default_reports_head_anchored() {
    let (_t, path, _ids) = seed_snapshot();
    let env = run_ok_object(&["query", "cycles", "--graph", &g(&path)]);
    assert_eq!(env["corpus_mode"], "head_anchored");
    assert_eq!(env["corpus_mode_source"], "default");
}

#[test]
fn cycles_all_history_is_union() {
    let (_t, path, _ids) = seed_snapshot();
    let env = run_ok_object(&["query", "cycles", "--graph", &g(&path), "--all-history"]);
    assert_eq!(env["corpus_mode"], "union");
    assert_eq!(env["corpus_mode_source"], "explicit_flag");
}

#[test]
fn cycles_conflicting_flags_rejected() {
    let (_t, path, _ids) = seed_snapshot();
    assert_unsupported(&[
        "query",
        "cycles",
        "--graph",
        &g(&path),
        "--at-head",
        "--all-history",
    ]);
}

// ---------------------------------------------------------------------------
// failures
// ---------------------------------------------------------------------------

#[test]
fn failures_default_reports_head_anchored() {
    let (_t, path, ids) = seed_snapshot();
    let env = run_ok_object(&["query", "failures", &ids.keeper, "--graph", &g(&path)]);
    assert_eq!(env["corpus_mode"], "head_anchored");
    assert_eq!(env["corpus_mode_source"], "default");
}

#[test]
fn failures_all_history_is_union() {
    let (_t, path, ids) = seed_snapshot();
    let env = run_ok_object(&[
        "query",
        "failures",
        &ids.keeper,
        "--graph",
        &g(&path),
        "--all-history",
    ]);
    assert_eq!(env["corpus_mode"], "union");
}

#[test]
fn failures_conflicting_flags_rejected() {
    let (_t, path, ids) = seed_snapshot();
    assert_unsupported(&[
        "query",
        "failures",
        &ids.keeper,
        "--graph",
        &g(&path),
        "--at-head",
        "--all-history",
    ]);
}

// ---------------------------------------------------------------------------
// evidence-path
// ---------------------------------------------------------------------------

#[test]
fn evidence_path_default_head_anchors_drops_stale_edge() {
    let (_t, path, ids) = seed_snapshot();
    // The only witness edge is stale (c1); head-anchoring drops it → no_path.
    let assert = egregore()
        .args([
            "query",
            "evidence-path",
            &ids.ev_src,
            &ids.ev_tgt,
            "--graph",
            &g(&path),
        ])
        .assert()
        .failure()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let env: Value = serde_json::from_str(out.lines().next().unwrap()).expect("json");
    assert_eq!(env["error"]["error_type"], "no_path", "{out}");
    assert_eq!(env["error"]["corpus_mode"], "head_anchored");
}

#[test]
fn evidence_path_all_history_finds_witness() {
    let (_t, path, ids) = seed_snapshot();
    let (env, _) = run_ok(&[
        "query",
        "evidence-path",
        &ids.ev_src,
        &ids.ev_tgt,
        "--graph",
        &g(&path),
        "--all-history",
    ]);
    assert_eq!(env["ok"], true);
    assert_eq!(env["corpus_mode"], "union");
    assert!(env["hop_count"].as_u64().unwrap() >= 1);
}

#[test]
fn evidence_path_conflicting_flags_rejected() {
    let (_t, path, ids) = seed_snapshot();
    assert_unsupported(&[
        "query",
        "evidence-path",
        &ids.ev_src,
        &ids.ev_tgt,
        "--graph",
        &g(&path),
        "--at-head",
        "--all-history",
    ]);
}

// ---------------------------------------------------------------------------
// unsafe-sites (Group 2: has --at)
// ---------------------------------------------------------------------------

#[test]
fn unsafe_sites_default_head_anchors() {
    let (_t, path, ids) = seed_snapshot();
    let env = run_ok_object(&["query", "unsafe-sites", "--graph", &g(&path)]);
    assert_eq!(env["corpus_mode"], "head_anchored");
    assert_eq!(env["corpus_mode_source"], "default");
    let sites: Vec<&str> = env["sites"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(
        sites.contains(&ids.unsafe_staying.as_str()),
        "HEAD unsafe site present: {sites:?}"
    );
    assert!(
        !sites.contains(&ids.unsafe_gone.as_str()),
        "deleted-at-HEAD unsafe site excluded: {sites:?}"
    );
}

#[test]
fn unsafe_sites_all_history_includes_deleted() {
    let (_t, path, ids) = seed_snapshot();
    let env = run_ok_object(&[
        "query",
        "unsafe-sites",
        "--graph",
        &g(&path),
        "--all-history",
    ]);
    assert_eq!(env["corpus_mode"], "union");
    let sites: Vec<&str> = env["sites"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(sites.contains(&ids.unsafe_gone.as_str()), "{sites:?}");
    assert!(sites.contains(&ids.unsafe_staying.as_str()), "{sites:?}");
}

#[test]
fn unsafe_sites_conflicting_flags_rejected() {
    let (_t, path, _ids) = seed_snapshot();
    assert_unsupported(&[
        "query",
        "unsafe-sites",
        "--graph",
        &g(&path),
        "--at-head",
        "--all-history",
    ]);
    assert_unsupported(&[
        "query",
        "unsafe-sites",
        "--graph",
        &g(&path),
        "--at-head",
        "--at",
        C2,
    ]);
    assert_unsupported(&[
        "query",
        "unsafe-sites",
        "--graph",
        &g(&path),
        "--all-history",
        "--at",
        C2,
    ]);
}

// ---------------------------------------------------------------------------
// unwrap-expect + debt-markers (Group 2 envelope + conflict coverage)
// ---------------------------------------------------------------------------

#[test]
fn unwrap_expect_default_reports_head_anchored() {
    let (_t, path, _ids) = seed_snapshot();
    let env = run_ok_object(&["query", "unwrap-expect", "--graph", &g(&path)]);
    assert_eq!(env["corpus_mode"], "head_anchored");
    assert_eq!(env["corpus_mode_source"], "default");
}

#[test]
fn unwrap_expect_all_history_is_union() {
    let (_t, path, _ids) = seed_snapshot();
    let env = run_ok_object(&[
        "query",
        "unwrap-expect",
        "--graph",
        &g(&path),
        "--all-history",
    ]);
    assert_eq!(env["corpus_mode"], "union");
}

#[test]
fn unwrap_expect_conflicting_flags_rejected() {
    let (_t, path, _ids) = seed_snapshot();
    assert_unsupported(&[
        "query",
        "unwrap-expect",
        "--graph",
        &g(&path),
        "--at-head",
        "--at",
        C2,
    ]);
}

#[test]
fn debt_markers_default_reports_head_anchored() {
    let (_t, path, _ids) = seed_snapshot();
    let env = run_ok_object(&["query", "debt-markers", "--graph", &g(&path)]);
    assert_eq!(env["corpus_mode"], "head_anchored");
    assert_eq!(env["corpus_mode_source"], "default");
}

#[test]
fn debt_markers_all_history_is_union() {
    let (_t, path, _ids) = seed_snapshot();
    let env = run_ok_object(&[
        "query",
        "debt-markers",
        "--graph",
        &g(&path),
        "--all-history",
    ]);
    assert_eq!(env["corpus_mode"], "union");
}

#[test]
fn debt_markers_conflicting_flags_rejected() {
    let (_t, path, _ids) = seed_snapshot();
    assert_unsupported(&[
        "query",
        "debt-markers",
        "--graph",
        &g(&path),
        "--all-history",
        "--at",
        C2,
    ]);
}

// ---------------------------------------------------------------------------
// Transport parity (--graph vs --data-dir), representative lanes.
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn change_impact_graph_and_data_dir_agree_head_anchored() {
    let (_t, path, ids) = seed_snapshot();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");
    egregore()
        .arg("ingest")
        .arg(&path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let graph_env = run_ok_object(&["query", "change-impact", &ids.keeper, "--graph", &g(&path)]);
    let store_out = egregore()
        .args(["query", "change-impact", &ids.keeper, "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let store_env: Value = serde_json::from_slice(&store_out).expect("json");
    assert_eq!(graph_env["corpus_mode"], "head_anchored");
    assert_eq!(store_env["corpus_mode"], "head_anchored");

    let ids_of = |env: &Value| -> Vec<String> {
        let mut v: Vec<String> = env["direct_callers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["record_id"].as_str().map(str::to_owned))
            .collect();
        v.sort_unstable();
        v
    };
    assert_eq!(ids_of(&graph_env), ids_of(&store_env), "transport parity");
    assert!(!ids_of(&graph_env).contains(&ids.gone));
}
