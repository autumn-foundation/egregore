//! Issue #229 — `eg init`: one-command repository bootstrap.
//!
//! Representative integration coverage for the one-command path
//! (`eg init <repo> --data-dir <dir>`) against the documented manual
//! multi-step sequence (`eg scan` → `eg ingest --embed` →
//! `eg scan-history` → `eg ingest`).
//!
//! The end-to-end equivalence test uses REAL semantic embeddings: the
//! fixture is a small Rust crate with real Git history, so the tree batch
//! has embedding candidates and `init` downloads the default model on first
//! use (cached afterward by the embeddings feature's model cache). That
//! test needs Hugging Face reachability; every other test here is offline
//! (`--no-embed`, `HF_HUB_OFFLINE=1` with an empty `HF_HUB_CACHE`, or a
//! non-Git fixture).

#![allow(missing_docs)]
#![cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]

use assert_cmd::Command;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};

fn eg() -> Command {
    Command::cargo_bin("egregore").expect("egregore binary should run")
}

fn run_git(repo: &Path, args: &[&str]) -> String {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed: git {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn init_git_repo(repo: &Path) {
    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.email", "init-test@example.invalid"]);
    run_git(repo, &["config", "user.name", "Init Integration Test"]);
    run_git(repo, &["config", "core.autocrlf", "false"]);
    run_git(repo, &["config", "commit.gpgsign", "false"]);
}

fn commit_file(repo: &Path, relative: &str, content: &str, msg: &str) {
    let path = repo.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    run_git(repo, &["add", relative]);
    run_git(repo, &["commit", "-m", msg]);
}

const LIB_V1: &str = r#"/// Request timeout handling for the init fixture crate.
pub fn process_timeout(config: &str) -> String {
    format!("processing {config} with timeout")
}
"#;

const LIB_V2: &str = r#"/// Request timeout handling for the init fixture crate.
pub fn process_timeout(config: &str) -> String {
    format!("processing {config} with timeout")
}

/// A named widget in the init fixture crate.
pub struct Widget {
    pub name: String,
}

impl Widget {
    /// Build a widget with a timeout-aware name.
    pub fn new(name: &str) -> Self {
        Self {
            name: format!("{name}-with-timeout"),
        }
    }
}
"#;

/// A representative temporary Rust crate with real Git history: two commits,
/// the second adding a struct plus an impl block.
fn fixture_git_repo() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    init_git_repo(repo);
    commit_file(repo, "src/lib.rs", LIB_V1, "initial commit");
    commit_file(
        repo,
        "src/lib.rs",
        LIB_V2,
        "add Widget with timeout-aware constructor",
    );
    temp
}

/// Runs `eg init` and parses the JSON bootstrap report from stdout.
/// Returns `(exit code, report, stderr)`.
fn run_init(
    repo: &Path,
    data_dir: &Path,
    extra: &[&str],
    env: &[(&str, &str)],
) -> (i32, Value, String) {
    let mut cmd = eg();
    cmd.arg("init")
        .arg(repo)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--format")
        .arg("json");
    for arg in extra {
        cmd.arg(arg);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("eg init should execute");
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let report: Value = serde_json::from_str(&stdout).unwrap_or_else(|_| {
        panic!("eg init stdout should be a JSON report, got:\n{stdout}\nstderr:\n{stderr}")
    });
    (code, report, stderr)
}

fn stage_statuses(report: &Value) -> BTreeMap<String, String> {
    report["stages"]
        .as_array()
        .expect("report should carry a stages array")
        .iter()
        .map(|stage| {
            (
                stage["name"].as_str().unwrap().to_owned(),
                stage["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Snapshot of the checkout safety invariants: HEAD, porcelain status, and
/// content hashes of every non-.git file.
fn checkout_snapshot(repo: &Path) -> (String, String, BTreeMap<String, String>) {
    let head = run_git(repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let status = run_git(repo, &["status", "--porcelain"]);
    let mut files = BTreeMap::new();
    let mut dirs = vec![repo.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().unwrap() != ".git" {
                    dirs.push(path);
                }
            } else {
                let rel = path
                    .strip_prefix(repo)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let hash = blake3::hash(&fs::read(&path).unwrap()).to_hex().to_string();
                files.insert(rel, hash);
            }
        }
    }
    (head, status, files)
}

fn query_lane(lane: &str, arg: &str, data_dir: &Path) -> std::process::Output {
    let mut cmd = eg();
    cmd.arg("query").arg(lane).arg(arg);
    cmd.arg("--data-dir").arg(data_dir);
    // `query context` always emits JSON and does not accept `--format`;
    // the symbol and semantic lanes need it for machine-readable output.
    if lane != "context" {
        cmd.arg("--format").arg("json");
    }
    cmd.output().expect("eg query should execute")
}

fn stdout_json(output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    // Try the whole stdout first (pretty-printed JSON).
    if let Ok(v) = serde_json::from_str::<Value>(&stdout) {
        return v;
    }
    // Some lanes (e.g. semantic) emit a provenance line followed by the
    // result object; use the last non-empty line.
    let last_line = stdout.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
    serde_json::from_str(last_line).unwrap_or_else(|_| {
        panic!(
            "query stdout should be JSON, got:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Normalize volatile `valid_time` timestamps so `eg init` output can be
/// compared against the manual sequence run at a different wall-clock time.
fn normalize_valid_time(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if k == "valid_time" {
                    *v = Value::String("<normalized>".to_owned());
                } else {
                    normalize_valid_time(v);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                normalize_valid_time(item);
            }
        }
        _ => {}
    }
}

/// Runs the documented manual multi-step sequence into `data_dir`.
fn run_manual_sequence(repo: &Path, workdir: &Path, data_dir: &Path) {
    let tree = workdir.join("tree.jsonl");
    let history = workdir.join("history.jsonl");
    eg().arg("scan")
        .arg(repo)
        .arg("--out")
        .arg(&tree)
        .assert()
        .success();
    eg().arg("ingest")
        .arg(&tree)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--embed")
        .assert()
        .success();
    eg().arg("scan-history")
        .arg(repo)
        .arg("--out")
        .arg(&history)
        .assert()
        .success();
    eg().arg("ingest")
        .arg(&history)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success();
}

// ── AC: one command builds a queryable store with real embeddings ───────────

/// The full issue #229 path with real semantic embeddings: `eg init` on a
/// small Git-backed Rust crate exits 0, reports every stage completed, names
/// the indexed commit tip, carries the embedding-model identity, leaves the
/// checkout untouched, and the store answers symbol, context, and semantic
/// queries.
///
/// Needs Hugging Face reachability for the one-time model download.
#[test]
fn init_bootstraps_git_repo_with_real_embeddings() {
    let repo_temp = fixture_git_repo();
    let repo = repo_temp.path();
    let work_temp = tempfile::tempdir().expect("temp dir should be created");
    let init_store = work_temp.path().join("init-store");
    let manual_store = work_temp.path().join("manual-store");

    let (head_before, status_before, files_before) = checkout_snapshot(repo);

    let (code, report, _stderr) = run_init(repo, &init_store, &[], &[]);
    assert_eq!(code, 0, "eg init should exit 0");
    assert_eq!(report["status"], "rebuilt");
    assert_eq!(
        stage_statuses(&report),
        BTreeMap::from([
            ("scan_tree".to_owned(), "completed".to_owned()),
            ("ingest_tree".to_owned(), "completed".to_owned()),
            ("scan_history".to_owned(), "completed".to_owned()),
            ("ingest_history".to_owned(), "completed".to_owned()),
        ]),
        "every stage should complete"
    );

    // Bootstrap metadata: identity, counts, semantics, indexed commit tip.
    assert!(
        report["repository"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "report should carry the repository identity"
    );
    assert_eq!(report["data_dir"], init_store.to_string_lossy().as_ref());
    let total = report["records"]["total"].as_u64().expect("records.total");
    assert!(total > 0, "the store should hold records");
    assert!(
        report["records"]["per_domain"]["code"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "code-domain records should be indexed"
    );
    assert_eq!(report["semantics"]["present"], true);
    assert_eq!(
        report["semantics"]["model"]["name"],
        "sentence-transformers/all-MiniLM-L6-v2"
    );
    assert_eq!(report["semantics"]["model"]["dim"], 384);
    assert!(report["semantics"]["skipped_reason"].is_null());
    assert_eq!(report["snapshot"]["head"], head_before.trim());
    assert_eq!(report["snapshot"]["dirty"], false);
    assert_eq!(report["store_state"], "complete");

    // Checkout safety: HEAD, git status, and every file byte-identical.
    let (head_after, status_after, files_after) = checkout_snapshot(repo);
    assert_eq!(head_before, head_after, "HEAD must not move");
    assert_eq!(status_before, status_after, "git status must not change");
    assert_eq!(files_before, files_after, "worktree files must not change");

    // The store answers symbol, context, and semantic queries…
    let symbol = query_lane("symbol", "process_timeout", &init_store);
    assert!(symbol.status.success());
    let symbol_json = stdout_json(&symbol);
    assert!(
        symbol_json.to_string().contains("process_timeout"),
        "symbol query should find process_timeout"
    );
    let context = query_lane("context", "process_timeout", &init_store);
    assert!(context.status.success());
    let semantic = query_lane("semantic", "timeout handling", &init_store);
    assert!(
        semantic.status.success(),
        "semantic query should succeed on the embedded store: {}",
        String::from_utf8_lossy(&semantic.stderr)
    );

    // …identically to the documented manual sequence.
    // `valid_time` stamps differ because the two runs happen at different
    // wall-clock times; normalize them before comparing.
    run_manual_sequence(repo, work_temp.path(), &manual_store);
    for (lane, arg) in [
        ("symbol", "process_timeout"),
        ("context", "process_timeout"),
        ("semantic", "timeout handling"),
    ] {
        let from_init = query_lane(lane, arg, &init_store);
        let from_manual = query_lane(lane, arg, &manual_store);
        assert!(from_manual.status.success(), "{lane} on manual store");
        let mut init_json = stdout_json(&from_init);
        let mut manual_json = stdout_json(&from_manual);
        normalize_valid_time(&mut init_json);
        normalize_valid_time(&mut manual_json);
        assert_eq!(
            init_json, manual_json,
            "eg init and the manual sequence should answer `{lane}` identically"
        );
    }
}

// ── AC: idempotency — second run is a no-op with exit 3 ─────────────────────

/// A second unchanged `eg init` converges to a no-op: exit code 3,
/// `status: "already_current"`, and unchanged counts.
#[test]
fn init_second_run_is_already_current() {
    let repo_temp = fixture_git_repo();
    let repo = repo_temp.path();
    let work_temp = tempfile::tempdir().expect("temp dir should be created");
    let store = work_temp.path().join("store");

    // `--no-embed` keeps this offline: idempotency does not depend on the
    // semantic path (a structural store is already current when embeddings
    // were never requested).
    let (code, first, _) = run_init(repo, &store, &["--no-embed"], &[]);
    assert_eq!(code, 0);
    assert_eq!(first["status"], "rebuilt");
    let total = first["records"]["total"].clone();

    let (code, second, _) = run_init(repo, &store, &["--no-embed"], &[]);
    assert_eq!(code, 3, "second unchanged init should exit 3");
    assert_eq!(second["status"], "already_current");
    assert_eq!(second["records"]["total"], total);
    assert_eq!(
        second["stages"].as_array().unwrap().len(),
        0,
        "the no-op report carries no rebuilt stages"
    );

    // A third run after touching a tracked file rebuilds again.
    fs::write(repo.join("src/lib.rs"), format!("{LIB_V2}\n// touched\n")).unwrap();
    let (code, third, _) = run_init(repo, &store, &["--no-embed"], &[]);
    assert_eq!(code, 0, "init after a tree change should rebuild");
    assert_eq!(third["status"], "rebuilt");
}

// ── AC: --no-embed is loud, structural queries keep working ─────────────────

/// Explicit `--no-embed` is loudly reported in the JSON report and the
/// resulting structural store answers symbol/context queries while the
/// semantic lane reports `semantic_index_absent`.
#[test]
fn init_no_embed_reports_skipped_loudly() {
    let repo_temp = fixture_git_repo();
    let repo = repo_temp.path();
    let work_temp = tempfile::tempdir().expect("temp dir should be created");
    let store = work_temp.path().join("store");

    let (code, report, _) = run_init(repo, &store, &["--no-embed"], &[]);
    assert_eq!(code, 0);
    assert_eq!(report["status"], "rebuilt");
    assert_eq!(report["semantics"]["present"], false);
    let reason = report["semantics"]["skipped_reason"]
        .as_str()
        .expect("skipped_reason should name why embeddings were skipped");
    assert!(
        reason.contains("--no-embed"),
        "the skip reason should be explicit, got: {reason}"
    );
    assert_eq!(report["store_state"], "structural_only");

    let symbol = query_lane("symbol", "Widget::new", &store);
    assert!(
        symbol.status.success(),
        "structural queries should keep working"
    );
    let context = query_lane("context", "Widget::new", &store);
    assert!(
        context.status.success(),
        "structural queries should keep working"
    );

    let semantic = query_lane("semantic", "timeout handling", &store);
    assert_eq!(semantic.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&semantic.stderr).contains("semantic_index_absent"),
        "semantic query on a structure-only store should report semantic_index_absent"
    );
}

// ── AC: embedding failure degrades to structural, never fatal ───────────────

/// When embedding generation is unavailable, `eg init` continues
/// structurally: exit 0, the skip reason is recorded, the structural store
/// stays queryable — but the run is honestly `partial`, because the requested
/// semantic state was not reached.
///
/// Unavailability is simulated with the variables the Rust `hf-hub` backend
/// actually honors (`ApiBuilder::from_env`): `HF_HOME` pointed at an empty
/// directory (so a model cached by another test is not found) and
/// `HF_ENDPOINT` pointed at a dead local address (so the download fails fast
/// with connection refused). Note `HF_HUB_CACHE`/`HF_HUB_OFFLINE` are
/// Python-`huggingface_hub` conventions that `hf-hub` ignores — using them
/// lets a warm default cache silently succeed and the test observes
/// `rebuilt` instead of `partial`.
#[test]
fn init_embedding_unavailable_continues_structurally() {
    let repo_temp = fixture_git_repo();
    let repo = repo_temp.path();
    let work_temp = tempfile::tempdir().expect("temp dir should be created");
    let store = work_temp.path().join("store");
    let hf_home = work_temp.path().join("empty-hf-home");
    fs::create_dir_all(&hf_home).unwrap();

    let mut cmd = eg();
    cmd.arg("init")
        .arg(repo)
        .arg("--data-dir")
        .arg(&store)
        .arg("--format")
        .arg("json")
        .env("HF_HOME", &hf_home)
        .env("HF_ENDPOINT", "http://127.0.0.1:9");
    let output = cmd.output().expect("eg init should execute");
    assert_eq!(output.status.code(), Some(0));
    let report: Value =
        serde_json::from_slice(&output.stdout).expect("eg init stdout should be a JSON report");
    // The store was built, but the requested semantic state was not reached.
    assert_eq!(report["status"], "partial");
    assert_eq!(report["semantics"]["present"], false);
    let reason = report["semantics"]["skipped_reason"]
        .as_str()
        .expect("skipped_reason should record the generation failure");
    assert!(
        reason.contains("embedding generation failed"),
        "the skip reason should record the failure, got: {reason}"
    );
    assert_eq!(report["store_state"], "structural_only");
    // Every required stage still completed; only the optional embedding step
    // degraded.
    assert_eq!(
        stage_statuses(&report),
        BTreeMap::from([
            ("scan_tree".to_owned(), "completed".to_owned()),
            ("ingest_tree".to_owned(), "completed".to_owned()),
            ("scan_history".to_owned(), "completed".to_owned()),
            ("ingest_history".to_owned(), "completed".to_owned()),
        ])
    );

    let symbol = query_lane("symbol", "process_timeout", &store);
    assert!(
        symbol.status.success(),
        "structural queries should keep working"
    );
    let semantic = query_lane("semantic", "timeout handling", &store);
    assert_eq!(semantic.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&semantic.stderr).contains("semantic_index_absent"),
        "semantic query should report semantic_index_absent"
    );
}

// ── AC: non-Git repos skip history with named stages ────────────────────────

/// On a repository that is not a Git checkout, the history stages are
/// skipped with recorded reasons, the report names every stage, and the
/// resulting structural store is valid and queryable. The bootstrap is
/// `partial` (history was never available), not `rebuilt`.
#[test]
fn init_non_git_repo_skips_history_with_named_stages() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("plain");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/lib.rs"), LIB_V1).unwrap();
    let store = temp.path().join("store");

    let (code, report, _) = run_init(&repo, &store, &["--no-embed"], &[]);
    assert_eq!(code, 0, "init on a non-Git repo should still succeed");
    assert_eq!(report["status"], "partial");
    let stages = stage_statuses(&report);
    assert_eq!(stages["scan_tree"], "completed");
    assert_eq!(stages["ingest_tree"], "completed");
    assert_eq!(stages["scan_history"], "skipped");
    assert_eq!(stages["ingest_history"], "skipped");
    let skip_detail = report["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["name"] == "scan_history")
        .unwrap()["detail"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        skip_detail.contains("not_a_git_repository"),
        "the skip reason should name the preflight code, got: {skip_detail}"
    );
    assert_eq!(report["store_state"], "structural_only");
    assert!(report["records"]["total"].as_u64().unwrap() > 0);

    let symbol = query_lane("symbol", "process_timeout", &store);
    assert!(
        symbol.status.success(),
        "the tree store should be queryable"
    );
}

// ── AC: text format renders the same report ─────────────────────────────────

/// `--format text` renders the same bootstrap report as human-readable lines.
#[test]
fn init_text_format_renders_report() {
    let repo_temp = fixture_git_repo();
    let repo = repo_temp.path();
    let work_temp = tempfile::tempdir().expect("temp dir should be created");
    let store = work_temp.path().join("store");

    let output = eg()
        .arg("init")
        .arg(repo)
        .arg("--data-dir")
        .arg(&store)
        .arg("--format")
        .arg("text")
        .arg("--no-embed")
        .output()
        .expect("eg init should execute");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    for needle in [
        "init: rebuilt",
        "stage scan_tree: completed",
        "stage ingest_history: completed",
        "semantics: absent",
        "semantics_skipped_reason:",
        "store_state: structural_only",
    ] {
        assert!(
            stdout.contains(needle),
            "text report should contain `{needle}`, got:\n{stdout}"
        );
    }
}

/// Resolves the real `git` binary on PATH, before any test-local shim
/// shadows it.
fn real_git_binary() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH should be set"))
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("a git binary should be on PATH for this test")
}

/// A first run whose history replay was unavailable must not poison later
/// runs: the second init rebuilds (history gets another chance) instead of
/// converging to `already_current` over a history-less store. This is the
/// history-completeness half of the idempotency probe (issue #229).
#[test]
fn init_rebuilds_when_previous_run_skipped_history() {
    let repo_temp = fixture_git_repo();
    let repo = repo_temp.path();
    let head = run_git(repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let work_temp = tempfile::tempdir().expect("temp dir should be created");
    let store = work_temp.path().join("store");

    // A `git` shim that fails only `git log`: the tree scan's git usage
    // (`rev-parse`, `status`) and identity resolution (`rev-list`, `remote`)
    // behave normally, so the first init stamps a regular snapshot — but the
    // history preflight's `git log -1` fails and history replay is skipped.
    let shim_dir = work_temp.path().join("shimbin");
    fs::create_dir(&shim_dir).unwrap();
    let real_git = real_git_binary();
    fs::write(
        shim_dir.join("git"),
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = \"log\" ]; then exit 128; fi\ndone\nexec \"{}\" \"$@\"\n",
            real_git.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(shim_dir.join("git"), fs::Permissions::from_mode(0o755))
            .expect("shim should be executable");
    }
    let shim_path = format!("{}:{}", shim_dir.display(), std::env::var("PATH").unwrap());

    // Run 1: tree indexed, history skipped.
    let (code, first, _) = run_init(repo, &store, &[], &[("PATH", &shim_path)]);
    assert_eq!(code, 0);
    assert_eq!(first["status"], "partial");
    assert_eq!(first["store_state"], "partial_history");
    assert_eq!(first["snapshot"]["head"], head.as_str());
    let stages = stage_statuses(&first);
    assert_eq!(stages["scan_tree"], "completed");
    assert_eq!(stages["scan_history"], "skipped");
    // NOTE (issue #229 test fix): this assertion previously required
    // `ingest_history` to be absent from the report. That contradicts the
    // issue's acceptance criterion — "the command reports which stages
    // completed and which did not via a machine-readable summary" — and the
    // suite's own non-Git test plus docs/cli/init.md, which name all four
    // pipeline stages. A skipped stage never runs, so asserting "skipped"
    // preserves this test's intent ("no history ingest should run when the
    // replay was skipped") while honoring the fixed report schema.
    assert_eq!(
        stages["ingest_history"], "skipped",
        "no history ingest should run when the replay was skipped"
    );

    // Run 2 with the real git: the snapshot is fresh and the semantic state
    // matches, but the store holds no Commit node for HEAD — the probe must
    // rebuild rather than report already_current.
    let (code, second, _) = run_init(repo, &store, &[], &[]);
    assert_eq!(
        code, 0,
        "second init should rebuild, not converge to a no-op"
    );
    assert_eq!(second["status"], "rebuilt");
    assert_eq!(second["store_state"], "complete");

    // Run 3: history is complete now — converges to a no-op.
    let (code, third, _) = run_init(repo, &store, &[], &[]);
    assert_eq!(code, 3);
    assert_eq!(third["status"], "already_current");
    assert_eq!(third["store_state"], "complete");
}

/// The store's snapshot must describe the tree that was scanned — including
/// its dirtiness. A dirty working tree is reported honestly (`dirty: true`);
/// reverting the edits afterwards rebuilds rather than converging to a false
/// `already_current` (the history batch's own `dirty=false` stamp is aligned
/// to the tree's snapshot before ingest).
#[test]
fn init_preserves_dirty_tree_snapshot() {
    let repo_temp = fixture_git_repo();
    let repo = repo_temp.path();
    let head = run_git(repo, &["rev-parse", "HEAD"]).trim().to_owned();
    // Dirty the tree without committing.
    fs::write(
        repo.join("src/lib.rs"),
        format!("{LIB_V2}\n// uncommitted scratch\n"),
    )
    .unwrap();

    let work_temp = tempfile::tempdir().expect("temp dir should be created");
    let store = work_temp.path().join("store");

    let (code, first, _) = run_init(repo, &store, &[], &[]);
    assert_eq!(code, 0);
    assert_eq!(first["status"], "rebuilt");
    assert_eq!(first["snapshot"]["head"], head.as_str());
    assert_eq!(
        first["snapshot"]["dirty"], true,
        "the report must honestly reflect the dirty tree: {first}"
    );

    // Revert the dirty edit: the tree is clean at the same HEAD, but the
    // store was built from the dirty tree — init must rebuild, not no-op.
    run_git(repo, &["checkout", "--", "src/lib.rs"]);
    let (code, second, _) = run_init(repo, &store, &[], &[]);
    assert_eq!(
        code, 0,
        "reverted dirty tree must rebuild, not report already_current"
    );
    assert_eq!(second["status"], "rebuilt");
    assert_eq!(second["snapshot"]["dirty"], false);

    // Now truly unchanged — converges to a no-op.
    let (code, third, _) = run_init(repo, &store, &[], &[]);
    assert_eq!(code, 3);
    assert_eq!(third["status"], "already_current");
}

/// Blending two vector spaces is refused before writing anything (issue
/// #104): exit 2 with a machine-readable JSON envelope, and the store is
/// left untouched. This exercises the refusal plumbing end-to-end with a
/// different model *name* (the early pre-write check); the write-time gate
/// additionally compares version, dimensions, and weight hash for a reused
/// name — see `init_identity_gate_compares_full_model_identity`.
#[test]
fn init_refuses_conflicting_embedding_model_before_writing() {
    let repo_temp = fixture_git_repo();
    let repo = repo_temp.path();
    let work_temp = tempfile::tempdir().expect("temp dir should be created");
    let store = work_temp.path().join("store");

    // Build the store with the default model (real embeddings).
    let (code, first, _) = run_init(repo, &store, &[], &[]);
    assert_eq!(code, 0);
    let model_name = first["semantics"]["model"]["name"]
        .as_str()
        .expect("the first bootstrap should record its embedding model")
        .to_owned();
    let records_before = first["records"]["total"].clone();

    // Re-bootstrap asking for a different model: pre-write refusal.
    let output = eg()
        .arg("init")
        .arg(repo)
        .arg("--data-dir")
        .arg(&store)
        .arg("--embed-model")
        .arg("some-other-model")
        .arg("--format")
        .arg("json")
        .output()
        .expect("eg init should execute");
    assert_eq!(
        output.status.code(),
        Some(2),
        "a conflicting embedding model must be refused, not failed"
    );
    let refusal: Value =
        serde_json::from_slice(&output.stdout).expect("the refusal should be a JSON envelope");
    assert_eq!(refusal["ok"], false);
    assert_eq!(
        refusal["error"]["code"],
        "embedding_index_identity_conflict"
    );
    assert!(
        refusal["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(&model_name),
        "the refusal should name the model that built the index, got: {refusal}"
    );

    // The store is untouched: the original model still serves semantic query.
    let (code, third, _) = run_init(repo, &store, &[], &[]);
    assert_eq!(code, 3, "the refused run must not have modified the store");
    assert_eq!(third["status"], "already_current");
    assert_eq!(third["records"]["total"], records_before);
    assert_eq!(third["semantics"]["model"]["name"], model_name.as_str());
    let semantic = query_lane("semantic", "process timeout", &store);
    assert!(
        semantic.status.success(),
        "semantic search should still work after the refusal"
    );
}

// ── AC: a failed required stage still prints a valid JSON report ─────────────

/// A failed required stage (here the data directory cannot be created)
/// prints a valid JSON bootstrap report with `status: "failed"` before
/// exiting 1 — the failure is machine-readable, not a bare error line.
///
/// NOTE (issue #229 test fix): the failure is triggered by placing the data
/// dir beneath a regular file (`ENOTDIR` on creation), which fails
/// deterministically for any user. The original fixture used a read-only
/// parent directory, which does not fail when the tests run as root. The
/// stage assertions were also corrected to the documented report schema:
/// `stages` is an array of `{name, status, detail, …}` (see
/// docs/cli/init.md and every other init test), not an object, and the
/// failure message lives in `detail`, not `message`.
#[test]
fn init_required_stage_failure_prints_json_report() {
    let temp = fixture_git_repo();
    let repo = temp.path();
    // A regular file where the data dir's parent must be: creating
    // `<file>/store` fails deterministically with ENOTDIR.
    let blocker = temp.path().join("blocker");
    fs::write(&blocker, b"not a directory").unwrap();
    let data_dir = blocker.join("store");

    let (code, report, _stderr) = run_init(repo, &data_dir, &["--no-embed"], &[]);

    assert_eq!(code, 1, "a failed required stage should exit 1");
    assert_eq!(report["status"], "failed");
    let stages = report["stages"]
        .as_array()
        .expect("stages should be an array");
    let stage = |name: &str| {
        stages
            .iter()
            .find(|stage| stage["name"] == name)
            .unwrap_or_else(|| panic!("stage {name} should be present"))
    };
    assert_eq!(stage("scan_tree")["status"], "completed");
    assert_eq!(stage("ingest_tree")["status"], "failed");
    assert!(
        !stage("ingest_tree")["detail"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the failed stage should name the cause"
    );
}
