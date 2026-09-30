#![allow(missing_docs)]

//! Issue #130: re-ingesting an unchanged source converges to a no-op.
//!
//! These tests run `eg ingest` twice against a seeded fixture and assert the
//! convergence contract end to end:
//!
//! 1. the `eg inspect` census is byte-identical after run 2 vs run 1
//!    (records, nodes, edges, tombstones);
//! 2. `eg query symbol` / `eg query file` resolve to the same id set with the
//!    same count — no duplicate current nodes or edges;
//! 3. the re-ingest reports its effect explicitly (`inserted: 0`,
//!    `unchanged: N`) instead of silent `+N` growth;
//! 4. net new *current* records after the no-op re-ingest is 0;
//! 5. `eg import-traj` + `ingest` of the same trajectory twice does not
//!    duplicate `AgentSession` / `AgentRun` / `Observation` / `Failure`
//!    records.
//!
//! The `--embed` variant runs the same convergence contract against a store
//! built with a tiny deterministic local BERT (from the shared
//! `super::embed_fixture` harness, also used by `reembed.rs`): the second
//! `--embed` ingest must report `inserted: 0` / `unchanged: N` and leave a
//! byte-identical census, including the embedding-index identity record.
//!
//! The `--embed` variants need the `embeddings` feature; everything else needs
//! only `embedded-aletheiadb`.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use std::process::Command;

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
use super::embed_fixture::{DEAD_HF_ENDPOINT, stage_local_model};

/// Serializes this module's tests. Concurrent `eg ingest --adapter embedded`
/// processes have intermittently stalled during store open in this
/// environment (`AletheiaDB` group-commit / thread synchronization under
/// contention; the unit-test-only `embedded_store_gate` in
/// `src/adapters/aletheiadb.rs` exists for the same reason), so each test
/// holds this guard for its whole duration and the module never opens two
/// embedded stores at once. This only serializes this module's tests, not
/// embedded-store tests in other integration modules.
static STORE_GUARD: Mutex<()> = Mutex::new(());

/// Locks [`STORE_GUARD`], ignoring poisoning: a previous test's panic must
/// not cascade into the remaining tests.
fn lock_store_guard() -> std::sync::MutexGuard<'static, ()> {
    STORE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

fn traj_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/agent_memory/swe_agent_basic/trajectory.traj")
}

fn eg() -> Command {
    let bin = std::env::var("CARGO_BIN_EXE_egregore").map_or_else(
        |_| {
            let mut p = std::env::current_exe().expect("current exe");
            p.pop();
            if p.file_name().and_then(|n| n.to_str()) == Some("deps") {
                p.pop();
            }
            p.join("egregore")
        },
        std::path::PathBuf::from,
    );
    Command::new(bin)
}

/// Runs an `eg` command, asserting success, and returns stdout.
///
/// Uses `std::process::Command` with stdout/stderr redirected to temp files
/// instead of `assert_cmd`: in this environment, subprocesses piping
/// stdout/stderr have intermittently stalled while the embedded adapter
/// opens the store, while file redirection has been reliable in practice.
/// The stalls look like `AletheiaDB` group-commit / thread synchronization
/// under contention rather than anything specific to pipes, so this is a
/// pragmatic hedge, not a diagnosed root cause.
fn run_eg(args: &[&str]) -> String {
    run_eg_with_env(args, &[])
}

/// [`run_eg`] with extra environment variables (e.g. an isolated `HF_HOME`
/// for embedding-model tests).
fn run_eg_with_env(args: &[&str], env: &[(&str, &str)]) -> String {
    let dir = tempfile::tempdir().expect("temp dir for eg output");
    let out_path = dir.path().join("out.txt");
    let err_path = dir.path().join("err.txt");
    let out_file = std::fs::File::create(&out_path).expect("out file");
    let err_file = std::fs::File::create(&err_path).expect("err file");
    let mut cmd = eg();
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(out_file)
        .stderr(err_file);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let status = cmd.status().expect("eg should spawn");
    let stdout = std::fs::read_to_string(&out_path).expect("read stdout");
    assert!(
        status.success(),
        "eg {args:?} failed: {}",
        std::fs::read_to_string(&err_path).unwrap_or_default()
    );
    stdout
}

fn scan_graph(graph_path: &Path) {
    run_eg(&[
        "scan",
        fixture_repo().to_str().expect("fixture path is UTF-8"),
        "--out",
        graph_path.to_str().expect("graph path is UTF-8"),
    ]);
}

/// Runs `eg ingest --adapter embedded` and returns stdout on success.
///
/// Reopening an existing store prints index-restoration diagnostics to
/// stderr; that is normal and not asserted against here.
fn ingest_embedded(graph_path: &Path, data_dir: &Path, extra_args: &[&str]) -> String {
    let mut args: Vec<&str> = vec![
        "ingest",
        graph_path.to_str().expect("graph path is UTF-8"),
        "--adapter",
        "embedded",
        "--data-dir",
        data_dir.to_str().expect("data dir is UTF-8"),
    ];
    args.extend_from_slice(extra_args);
    run_eg(&args)
}

/// Deterministic single-line JSON census of an embedded store.
fn inspect_census_json(data_dir: &Path) -> String {
    run_eg(&[
        "inspect",
        "--data-dir",
        data_dir.to_str().expect("data dir is UTF-8"),
        "--format",
        "json",
    ])
}

fn query_stdout(args: &[&str], data_dir: &Path) -> String {
    let mut full: Vec<&str> = vec!["query"];
    full.extend_from_slice(args);
    full.push("--data-dir");
    full.push(data_dir.to_str().expect("data dir is UTF-8"));
    run_eg(&full)
}

/// Parses the `inserted:` / `unchanged:` / `attempted:` lines of ingest stdout.
fn upsert_summary(stdout: &str) -> (usize, usize, usize) {
    let mut attempted = None;
    let mut inserted = None;
    let mut unchanged = None;
    for line in stdout.lines() {
        let (key, value) = line.split_once(':').unwrap_or(("", ""));
        let value: usize = value.trim().parse().unwrap_or(usize::MAX);
        match key {
            "attempted" => attempted = Some(value),
            "inserted" => inserted = Some(value),
            "unchanged" => unchanged = Some(value),
            _ => {}
        }
    }
    (
        attempted.expect("ingest stdout reports attempted:"),
        inserted.expect("ingest stdout reports inserted: (issue #130)"),
        unchanged.expect("ingest stdout reports unchanged: (issue #130)"),
    )
}

/// Census totals from the JSON inspect output: (records, nodes, edges, tombstones).
fn census_totals(census_json: &str) -> (u64, u64, u64, u64) {
    let value: serde_json::Value =
        serde_json::from_str(census_json).expect("inspect emits valid JSON");
    let get = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| panic!("census JSON has {key}"))
    };
    (
        get("records"),
        get("nodes"),
        get("edges"),
        get("tombstones"),
    )
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn reingest_same_graph_converges_to_noop() {
    let _guard = lock_store_guard();
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    scan_graph(&graph_path);

    // Run 1: every record is new.
    let first = ingest_embedded(&graph_path, &data_dir, &[]);
    let (attempted, inserted, unchanged) = upsert_summary(&first);
    assert!(attempted > 0, "fixture graph must be non-empty");
    assert_eq!(inserted, attempted, "first ingest inserts everything");
    assert_eq!(unchanged, 0, "first ingest has nothing to skip");
    let census1 = inspect_census_json(&data_dir);
    let totals1 = census_totals(&census1);
    assert!(totals1.0 > 0, "census must count records");

    // Run 2: the same bytes must converge to a no-op.
    let second = ingest_embedded(&graph_path, &data_dir, &[]);
    let (attempted2, inserted2, unchanged2) = upsert_summary(&second);
    assert_eq!(attempted2, attempted);
    assert_eq!(
        inserted2, 0,
        "no-op re-ingest must report inserted: 0, got stdout:\n{second}"
    );
    assert_eq!(
        unchanged2, attempted,
        "every re-ingested record must report unchanged"
    );
    let census2 = inspect_census_json(&data_dir);

    // AC 1 + AC 4: byte-identical census, zero net new current records.
    assert_eq!(
        census1, census2,
        "inspect census must be byte-identical after a no-op re-ingest"
    );
    assert_eq!(census_totals(&census2), totals1);
}

/// Runs `eg ingest --adapter embedded` with extra environment variables.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn ingest_embedded_with_env(
    graph_path: &Path,
    data_dir: &Path,
    extra_args: &[&str],
    env: &[(&str, &str)],
) -> String {
    let mut args: Vec<&str> = vec![
        "ingest",
        graph_path.to_str().expect("graph path is UTF-8"),
        "--adapter",
        "embedded",
        "--data-dir",
        data_dir.to_str().expect("data dir is UTF-8"),
    ];
    args.extend_from_slice(extra_args);
    run_eg_with_env(&args, env)
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn reingest_same_graph_with_embed_converges_to_noop() {
    let _guard = lock_store_guard();
    let temp = tempfile::tempdir().expect("temp dir");

    // Tiny deterministic local BERT from the shared fixture harness: no
    // network, no warm HF cache. `HF_ENDPOINT` points at a dead address so
    // any download attempt fails fast instead of hanging.
    let hub_id = "local/reingest-384";
    let hf_home = stage_local_model(temp.path(), hub_id, 384, 0x8BAD_F00D_0000_0130);
    let hf_env = [
        ("HF_HOME", hf_home.to_str().expect("hf home is UTF-8")),
        ("HF_ENDPOINT", DEAD_HF_ENDPOINT),
    ];

    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    scan_graph(&graph_path);

    // The CLI appends the embedding index identity record to `records`
    // before ingest, so `attempted` already counts it on both runs.
    let embed_args = ["--embed", "--embed-model", hub_id];
    let first = ingest_embedded_with_env(&graph_path, &data_dir, &embed_args, &hf_env);
    let (attempted, inserted, unchanged) = upsert_summary(&first);
    assert!(attempted > 0, "fixture graph must be non-empty");
    assert_eq!(
        inserted, attempted,
        "first --embed ingest inserts everything, including the index identity record"
    );
    assert_eq!(unchanged, 0);
    let census1 = inspect_census_json(&data_dir);
    let totals1 = census_totals(&census1);
    assert!(totals1.0 > 0, "census must count records");

    // The unchanged re-ingest must converge to a no-op even with the vector
    // index present: every record (including the identity record) reports
    // unchanged, and the census is byte-identical.
    let second = ingest_embedded_with_env(&graph_path, &data_dir, &embed_args, &hf_env);
    let (attempted2, inserted2, unchanged2) = upsert_summary(&second);
    assert_eq!(attempted2, attempted);
    assert_eq!(
        inserted2, 0,
        "no-op --embed re-ingest must report inserted: 0, got stdout:\n{second}"
    );
    assert_eq!(
        unchanged2, attempted2,
        "every --embed re-ingested record must report unchanged"
    );
    let census2 = inspect_census_json(&data_dir);

    assert_eq!(
        census1, census2,
        "--embed inspect census must be byte-identical after a no-op re-ingest"
    );
    assert_eq!(census_totals(&census2), totals1);
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn reingest_preserves_query_identity() {
    let _guard = lock_store_guard();
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    scan_graph(&graph_path);

    ingest_embedded(&graph_path, &data_dir, &[]);
    let symbol1 = query_stdout(&["symbol", "answer"], &data_dir);
    let file1 = query_stdout(&["file", "src/lib.rs"], &data_dir);
    assert!(
        !symbol1.trim().is_empty(),
        "query symbol answer must match the fixture"
    );
    assert!(
        !file1.trim().is_empty(),
        "query file src/lib.rs must match the fixture"
    );

    // AC 2: the same identities resolve after the re-ingest — same id set,
    // same count, no duplicate current nodes or edges.
    ingest_embedded(&graph_path, &data_dir, &[]);
    let symbol2 = query_stdout(&["symbol", "answer"], &data_dir);
    let file2 = query_stdout(&["file", "src/lib.rs"], &data_dir);
    assert_eq!(
        symbol1, symbol2,
        "query symbol must resolve identically after a no-op re-ingest"
    );
    assert_eq!(
        file1, file2,
        "query file must resolve identically after a no-op re-ingest"
    );
}

/// Counts exported JSONL lines whose node `kind` equals `kind`.
fn count_exported_kind(export_path: &Path, kind: &str) -> usize {
    let jsonl = fs::read_to_string(export_path).expect("export JSONL readable");
    let marker = format!("\"kind\":\"{kind}\"");
    jsonl
        .lines()
        .filter(|line| line.contains("\"record_type\":\"node\"") && line.contains(&marker))
        .count()
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn import_traj_ingest_twice_has_no_duplicate_agent_records() {
    let _guard = lock_store_guard();
    let temp = tempfile::tempdir().expect("temp dir");
    let records_path = temp.path().join("records.jsonl");
    let data_dir = temp.path().join("store");

    run_eg(&[
        "import-traj",
        traj_fixture().to_str().expect("fixture path is UTF-8"),
        "--out",
        records_path.to_str().expect("records path is UTF-8"),
    ]);

    // Run 1.
    let first = ingest_embedded(&records_path, &data_dir, &[]);
    let (attempted, inserted, unchanged) = upsert_summary(&first);
    assert!(attempted > 0);
    assert_eq!(inserted, attempted);
    assert_eq!(unchanged, 0);
    let census1 = inspect_census_json(&data_dir);

    // AC 5: re-ingesting the same trajectory artifact must not duplicate
    // AgentSession / AgentRun / Observation / Failure records.
    let second = ingest_embedded(&records_path, &data_dir, &[]);
    let (_, inserted2, unchanged2) = upsert_summary(&second);
    assert_eq!(inserted2, 0, "trajectory re-ingest must be a no-op");
    assert_eq!(unchanged2, attempted);
    let census2 = inspect_census_json(&data_dir);
    assert_eq!(
        census1, census2,
        "census must be byte-identical after re-ingesting the same trajectory"
    );

    // Exactly one physical record per agent identity — no duplicates.
    // `eg export` emits every physical version (superseded ones included),
    // so a count of 1 here means no duplicate physical version exists.
    let export_path = temp.path().join("export.jsonl");
    run_eg(&[
        "export",
        "--data-dir",
        data_dir.to_str().expect("data dir is UTF-8"),
        "--out",
        export_path.to_str().expect("export path is UTF-8"),
    ]);
    assert_eq!(count_exported_kind(&export_path, "AgentSession"), 1);
    assert_eq!(count_exported_kind(&export_path, "AgentRun"), 1);
    for kind in ["Observation", "Failure"] {
        let count = count_exported_kind(&export_path, kind);
        let census_count = {
            let value: serde_json::Value =
                serde_json::from_str(&census2).expect("valid census JSON");
            // Census `schema_versions` keys are `domain:kind:version`
            // (src/cli/inspect.rs) and, like the export, count every physical
            // version — so agreement means no hidden duplicate physical
            // versions beyond what the export shows.
            value
                .get("schema_versions")
                .and_then(|versions| versions.as_object())
                .map_or(0, |versions| {
                    versions
                        .iter()
                        .filter(|(key, _)| {
                            let mut parts = key.split(':');
                            parts.next() == Some("agent_memory") && parts.next() == Some(kind)
                        })
                        .map(|(_, count)| count.as_u64().unwrap_or(0))
                        .sum::<u64>()
                })
        };
        assert_eq!(
            count as u64, census_count,
            "{kind}: exported physical records must match the census (no hidden duplicates)"
        );
    }
}
