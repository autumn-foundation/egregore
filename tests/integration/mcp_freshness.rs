//! MCP freshness stamping (issue #220).
//!
//! The MCP server's tools (`inspect_store`, `symbol_context`, `task_evidence`,
//! plus the dedicated `store_freshness`) stamp a non-fatal, machine-readable
//! `freshness` object onto successful JSON responses, reusing the store
//! freshness contract from #186 (`fresh` / `stale_head` / `stale_dirty` /
//! `unknown` verdict codes, stored source-snapshot identity, read-only
//! working-tree comparison) — no new vocabulary.
//!
//! Fixture matrix (success metric): for each of clean-tree-at-HEAD, committed
//! change moving HEAD, uncommitted edit, non-Git store, and pre-stamping store,
//! every tool's `freshness.verdict` agrees with `eg freshness` for the same
//! store, with zero false `fresh` on stale/dirty stores.

#![allow(missing_docs)]
#![cfg(feature = "embedded-aletheiadb")]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use aletheia_egregore::{
    GraphRecord, NodeKind, PROJECT_SCHEMA_VERSION,
    freshness::Freshness,
    mcp::{
        EgregoreMcpServer, StoreFreshnessArgs, stamp_freshness_on_payload, tool_freshness_stamp,
        tool_inspect_store_from_records, tool_symbol_context_from_records,
        tool_task_evidence_from_records,
    },
    project_stable_id,
};
use assert_cmd::Command as AssertCommand;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Git + scan helpers (mirrors tests/integration/freshness.rs)
// ---------------------------------------------------------------------------

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        out.status.success(),
        "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_init(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "test@example.invalid"]);
    git(repo, ["config", "user.name", "Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);
}

fn write_lib(repo: &Path, body: &str) {
    let src = repo.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("lib.rs"), body).unwrap();
}

fn commit_all(repo: &Path, msg: &str) {
    git(repo, ["add", "."]);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", msg])
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        out.status.success(),
        "git commit failed\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_head(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .expect("git rev-parse should execute");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_porcelain(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .stdin(Stdio::null())
        .output()
        .expect("git status should execute");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn eg() -> AssertCommand {
    AssertCommand::cargo_bin("egregore").expect("egregore binary should build")
}

/// A Git working tree plus a separate `work` dir for store artifacts, so the
/// tree's clean/dirty state reflects only the test's own edits.
struct Fixture {
    repo: tempfile::TempDir,
    work: tempfile::TempDir,
}

impl Fixture {
    /// Fresh Git repo with one committed `src/lib.rs` (`pub fn hello()`).
    fn committed() -> Self {
        let repo = tempfile::tempdir().unwrap();
        git_init(repo.path());
        write_lib(repo.path(), "pub fn hello() {}\n");
        commit_all(repo.path(), "initial");
        Self {
            repo,
            work: tempfile::tempdir().unwrap(),
        }
    }

    /// Plain (non-Git) directory with one `src/lib.rs`.
    fn non_git() -> Self {
        let repo = tempfile::tempdir().unwrap();
        write_lib(repo.path(), "pub fn hello() {}\n");
        Self {
            repo,
            work: tempfile::tempdir().unwrap(),
        }
    }

    fn repo(&self) -> &Path {
        self.repo.path()
    }

    fn graph(&self) -> PathBuf {
        self.work.path().join("graph.jsonl")
    }

    fn data_dir(&self) -> PathBuf {
        self.work.path().join(".egregore")
    }

    /// `eg scan <repo> --out work/graph.jsonl`.
    fn scan(&self) {
        eg().args(["scan"])
            .arg(self.repo())
            .arg("--out")
            .arg(self.graph())
            .assert()
            .success();
    }

    /// Records read back from the scanned graph JSONL.
    fn records(&self) -> Vec<GraphRecord> {
        read_records(&self.graph())
    }

    /// `eg freshness <repo> --graph work/graph.jsonl --format json`.
    fn cli_freshness(&self) -> Value {
        let out = eg()
            .args(["freshness"])
            .arg(self.repo())
            .arg("--graph")
            .arg(self.graph())
            .args(["--format", "json"])
            .assert()
            .success();
        serde_json::from_slice(&out.get_output().stdout).expect("freshness JSON should parse")
    }
}

fn read_records(graph: &Path) -> Vec<GraphRecord> {
    fs::read_to_string(graph)
        .expect("graph.jsonl should read")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<GraphRecord>(line)
                .expect("every graph.jsonl line should parse as a GraphRecord")
        })
        .collect::<Vec<_>>()
}

/// Minimal task node so `task_evidence` returns a successful payload.
fn with_task(mut records: Vec<GraphRecord>) -> (Vec<GraphRecord>, String) {
    let task_id = project_stable_id(&["task", "t1"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Implement thing".to_owned()),
        "Task t1".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut title,
        ..
    } = task
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *title = Some("Implement thing".to_owned());
    }
    records.push(task);
    (records, task_id)
}

/// Stamps `freshness` onto each tool's successful payload, the way the MCP tool
/// methods do.
fn stamped_tool_payloads(records: &[GraphRecord], repo: &Path) -> Vec<(&'static str, Value)> {
    let (records, task_id) = with_task(records.to_vec());

    let mut inspect = tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z");
    stamp_freshness_on_payload(&mut inspect, &records, repo, None);

    let mut symbol = tool_symbol_context_from_records(&records, "hello");
    stamp_freshness_on_payload(&mut symbol, &records, repo, None);

    let mut task = tool_task_evidence_from_records(&records, &task_id);
    stamp_freshness_on_payload(&mut task, &records, repo, None);

    vec![
        ("inspect_store", inspect),
        ("symbol_context", symbol),
        ("task_evidence", task),
    ]
}

fn verdict_of(payload: &Value) -> &str {
    payload["freshness"]["verdict"]
        .as_str()
        .expect("freshness.verdict must be a string")
}

// ---------------------------------------------------------------------------
// AC1: every tool's successful response carries a freshness object reusing the
// #186 verdict vocabulary (fresh / stale_head / stale_dirty / unknown)
// ---------------------------------------------------------------------------

#[test]
fn inspect_store_success_carries_freshness_object() {
    let fx = Fixture::committed();
    fx.scan();
    let records = fx.records();

    let mut payload = tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z");
    assert!(payload["ok"].as_bool().unwrap_or(false));
    stamp_freshness_on_payload(&mut payload, &records, fx.repo(), None);

    let freshness = &payload["freshness"];
    assert!(
        freshness.is_object(),
        "must carry a freshness object; got {payload}"
    );
    let verdict = verdict_of(&payload);
    assert!(
        [
            Freshness::Fresh.code(),
            Freshness::StaleHead.code(),
            Freshness::StaleDirty.code(),
            Freshness::Unknown.code(),
        ]
        .contains(&verdict),
        "verdict must reuse the #186 vocabulary; got {verdict}"
    );
}

#[test]
fn symbol_context_success_carries_freshness_object() {
    let fx = Fixture::committed();
    fx.scan();
    let records = fx.records();

    let mut payload = tool_symbol_context_from_records(&records, "hello");
    assert!(
        payload["ok"].as_bool().unwrap_or(false),
        "symbol_context should succeed for a scanned symbol; got {payload}"
    );
    stamp_freshness_on_payload(&mut payload, &records, fx.repo(), None);

    assert!(
        payload["freshness"].is_object(),
        "must carry a freshness object; got {payload}"
    );
    assert_eq!(verdict_of(&payload), Freshness::Fresh.code());
}

#[test]
fn task_evidence_success_carries_freshness_object() {
    let fx = Fixture::committed();
    fx.scan();
    let (records, task_id) = with_task(fx.records());

    let mut payload = tool_task_evidence_from_records(&records, &task_id);
    assert!(
        payload["ok"].as_bool().unwrap_or(false),
        "task_evidence should succeed for the fixture task; got {payload}"
    );
    stamp_freshness_on_payload(&mut payload, &records, fx.repo(), None);

    assert!(
        payload["freshness"].is_object(),
        "must carry a freshness object; got {payload}"
    );
    assert_eq!(verdict_of(&payload), Freshness::Fresh.code());
}

#[test]
fn error_payloads_carry_no_freshness_object() {
    // Freshness is a trust signal on answers, never on errors: a failed lookup
    // stays a plain error and the stamp is a no-op.
    let fx = Fixture::committed();
    fx.scan();
    let records = fx.records();

    let mut payload = tool_symbol_context_from_records(&records, "definitely_not_a_symbol");
    assert!(!payload["ok"].as_bool().unwrap_or(true));
    stamp_freshness_on_payload(&mut payload, &records, fx.repo(), None);
    assert!(
        payload.get("freshness").is_none(),
        "error payloads must not gain a freshness object; got {payload}"
    );
}

// ---------------------------------------------------------------------------
// AC2: the object reports the verdict, the stored snapshot identity, and the
// working-tree state it was compared against
// ---------------------------------------------------------------------------

#[test]
fn freshness_object_reports_stored_identity_and_working_tree_state() {
    let fx = Fixture::committed();
    fx.scan();
    let records = fx.records();
    let head = git_head(fx.repo());

    let stamp = tool_freshness_stamp(&records, fx.repo(), None);
    let freshness = &stamp;

    assert_eq!(freshness["verdict"].as_str(), Some(Freshness::Fresh.code()));
    assert_eq!(freshness["fresh"].as_bool(), Some(true));
    // Stored source-snapshot identity the answer was derived from.
    assert_eq!(
        freshness["stored_snapshot"]["head"]["state"].as_str(),
        Some("commit")
    );
    assert_eq!(
        freshness["stored_snapshot"]["head"]["sha"].as_str(),
        Some(head.as_str()),
        "stored HEAD SHA must match the scanned commit"
    );
    assert_eq!(freshness["stored_snapshot"]["dirty"].as_bool(), Some(false));
    // Working-tree state it was compared against.
    assert_eq!(freshness["current_head"]["state"].as_str(), Some("commit"));
    assert_eq!(
        freshness["current_head"]["sha"].as_str(),
        Some(head.as_str())
    );
    assert_eq!(freshness["current_dirty"].as_bool(), Some(false));
    assert!(
        freshness["repository_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "must name the repository the verdict was computed for"
    );
    assert_eq!(
        freshness["repo_path"].as_str(),
        Some(fx.repo().to_str().unwrap()),
        "must name the working tree it was compared against"
    );
    assert!(
        freshness["message"].as_str().is_some_and(|s| !s.is_empty()),
        "must carry the human-readable explanation from the #186 contract"
    );
}

#[test]
fn moved_head_reports_stale_head_with_both_shas() {
    let fx = Fixture::committed();
    fx.scan();
    let stored_head = git_head(fx.repo());
    // Committed change moves HEAD after the scan.
    write_lib(fx.repo(), "pub fn hello() {}\npub fn world() {}\n");
    commit_all(fx.repo(), "second");
    let current_head = git_head(fx.repo());
    assert_ne!(stored_head, current_head);

    let records = fx.records();
    let stamp = tool_freshness_stamp(&records, fx.repo(), None);

    assert_eq!(stamp["verdict"].as_str(), Some(Freshness::StaleHead.code()));
    assert_eq!(stamp["fresh"].as_bool(), Some(false));
    assert_eq!(
        stamp["stored_snapshot"]["head"]["sha"].as_str(),
        Some(stored_head.as_str())
    );
    assert_eq!(
        stamp["current_head"]["sha"].as_str(),
        Some(current_head.as_str())
    );
}

#[test]
fn uncommitted_edit_reports_stale_dirty() {
    let fx = Fixture::committed();
    fx.scan();
    let head = git_head(fx.repo());
    // Uncommitted edit: HEAD matches the stored snapshot but the tree is dirty.
    write_lib(fx.repo(), "pub fn hello() {}\n// uncommitted\n");

    let records = fx.records();
    let stamp = tool_freshness_stamp(&records, fx.repo(), None);

    assert_eq!(
        stamp["verdict"].as_str(),
        Some(Freshness::StaleDirty.code())
    );
    assert_eq!(stamp["fresh"].as_bool(), Some(false));
    assert_eq!(
        stamp["stored_snapshot"]["head"]["sha"].as_str(),
        Some(head.as_str())
    );
    assert_eq!(stamp["current_head"]["sha"].as_str(), Some(head.as_str()));
    assert_eq!(stamp["current_dirty"].as_bool(), Some(true));
}

// ---------------------------------------------------------------------------
// AC7: pre-stamping stores and missing Git context report `unknown` — never a
// false `fresh` — with an explicit marker
// ---------------------------------------------------------------------------

/// Records with the `source_snapshot` stripped from every Repository node: a
/// store that predates snapshot stamping.
fn strip_snapshots(records: &[GraphRecord]) -> Vec<GraphRecord> {
    read_records_from_values(
        &records
            .iter()
            .map(|r| {
                let mut v = serde_json::to_value(r).expect("record should serialize");
                if v["kind"] == "Repository" {
                    v.as_object_mut()
                        .expect("node should be an object")
                        .remove("source_snapshot");
                }
                v
            })
            .collect::<Vec<_>>(),
    )
}

fn read_records_from_values(values: &[Value]) -> Vec<GraphRecord> {
    values
        .iter()
        .map(|v| serde_json::from_value::<GraphRecord>(v.clone()).expect("record should parse"))
        .collect()
}

#[test]
fn pre_stamping_store_reports_unknown_with_explicit_marker() {
    let fx = Fixture::committed();
    fx.scan();
    let records = strip_snapshots(&fx.records());

    let stamp = tool_freshness_stamp(&records, fx.repo(), None);

    assert_eq!(stamp["verdict"].as_str(), Some(Freshness::Unknown.code()));
    assert_eq!(stamp["fresh"].as_bool(), Some(false));
    assert_eq!(
        stamp["stored_snapshot"],
        serde_json::json!({"state": "pre_stamping"}),
        "a store that predates stamping must carry the explicit pre_stamping marker"
    );
}

#[test]
fn non_git_store_reports_unknown_with_no_git_markers() {
    let fx = Fixture::non_git();
    fx.scan();
    let records = fx.records();

    let stamp = tool_freshness_stamp(&records, fx.repo(), None);

    assert_eq!(stamp["verdict"].as_str(), Some(Freshness::Unknown.code()));
    assert_eq!(stamp["fresh"].as_bool(), Some(false));
    assert_eq!(
        stamp["stored_snapshot"]["head"]["state"].as_str(),
        Some("no_git")
    );
    assert_eq!(stamp["current_head"]["state"].as_str(), Some("no_git"));
}

// ---------------------------------------------------------------------------
// AC3: dedicated `store_freshness` tool — same verdict, no symbol/task argument
// ---------------------------------------------------------------------------

#[test]
fn store_freshness_tool_is_registered() {
    let tool = EgregoreMcpServer::store_freshness_tool_attr();
    assert_eq!(tool.name, "store_freshness");
    assert!(
        tool.description.as_ref().is_some_and(|d| !d.is_empty()),
        "store_freshness must have a non-empty description"
    );
}

#[test]
fn store_freshness_tool_returns_verdict_without_symbol_or_task_argument() {
    // End-to-end through a real daemon: ingest the fixture graph, start the
    // daemon, and call the actual tool method with no symbol/task argument.
    let fx = Fixture::committed();
    fx.scan();
    let data_dir = fx.data_dir();

    eg().args(["ingest"])
        .arg(fx.graph())
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let mut daemon = start_daemon(&data_dir);
    let server = EgregoreMcpServer::new(PathBuf::from(".egregore-nonexistent-default"));
    let args = StoreFreshnessArgs {
        repo_path: Some(fx.repo().to_str().unwrap().to_owned()),
        data_dir: Some(data_dir.to_str().unwrap().to_owned()),
    };
    let raw = server.store_freshness(rmcp::handler::server::wrapper::Parameters(args));
    daemon.kill();

    let payload: Value = serde_json::from_str(&raw).expect("tool must return JSON");
    assert!(
        payload["ok"].as_bool().unwrap_or(false),
        "store_freshness should succeed; got {payload}"
    );
    let freshness = &payload["freshness"];
    assert!(
        freshness.is_object(),
        "must carry a freshness object; got {payload}"
    );
    assert_eq!(
        freshness["verdict"].as_str(),
        Some(Freshness::Fresh.code()),
        "clean tree at HEAD must read fresh; got {payload}"
    );
    // The tool's verdict must agree with the direct stamp over the same store.
    let direct = tool_freshness_stamp(&fx.records(), fx.repo(), Some(&data_dir));
    assert_eq!(
        freshness["verdict"], direct["verdict"],
        "tool verdict must match the direct stamp"
    );
}

// ---------------------------------------------------------------------------
// AC4: freshness is a trust signal, never suppression — the full payload stays
// ---------------------------------------------------------------------------

#[test]
fn non_fresh_verdict_preserves_full_answer_payload() {
    let fx = Fixture::committed();
    fx.scan();
    write_lib(fx.repo(), "pub fn hello() {}\n// uncommitted edit\n");
    let records = fx.records();

    let mut payload = tool_symbol_context_from_records(&records, "hello");
    assert!(payload["ok"].as_bool().unwrap_or(false));
    let facts_before = payload["source_facts"].as_array().map(Vec::len);
    stamp_freshness_on_payload(&mut payload, &records, fx.repo(), None);

    assert_eq!(
        payload["freshness"]["verdict"].as_str(),
        Some(Freshness::StaleDirty.code())
    );
    assert!(
        payload["source_facts"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "a stale_dirty verdict must not suppress the answer payload; got {payload}"
    );
    assert_eq!(
        payload["source_facts"].as_array().map(Vec::len),
        facts_before,
        "stamping must not add or remove answer rows"
    );
    assert!(
        payload.get("observations").is_some(),
        "all sections must survive stamping"
    );
}

// ---------------------------------------------------------------------------
// AC5: the freshness path is strictly read-only and offline
// ---------------------------------------------------------------------------

/// Content hashes of every file under `roots`, keyed by relative path.
fn hash_tree(roots: &[&Path]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for root in roots {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = fs::read_dir(&dir).unwrap_or_else(|_| {
                panic!("should read dir {}", dir.display());
            });
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    let bytes = fs::read(&path).unwrap_or_default();
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    std::hash::Hasher::write(&mut hasher, &bytes);
                    out.insert(
                        format!("{}:{rel}", root.display()),
                        format!("{:x}", std::hash::Hasher::finish(&hasher)),
                    );
                }
            }
        }
    }
    out
}

#[test]
fn freshness_stamping_creates_modifies_or_deletes_nothing() {
    let fx = Fixture::committed();
    fx.scan();
    let records = fx.records();

    let before_files = hash_tree(&[fx.repo(), fx.work.path()]);
    let before_porcelain = git_porcelain(fx.repo());

    // Every freshness-stamped path, repeatedly.
    for _ in 0..3 {
        let stamp = tool_freshness_stamp(&records, fx.repo(), None);
        let payloads = stamped_tool_payloads(&records, fx.repo());
        let mut inspect = tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z");
        stamp_freshness_on_payload(&mut inspect, &records, fx.repo(), Some(&fx.data_dir()));
        let _ = (&stamp, &payloads);
    }

    let after_files = hash_tree(&[fx.repo(), fx.work.path()]);
    assert_eq!(
        before_files, after_files,
        "freshness stamping must not create, modify, or delete any file"
    );
    assert_eq!(
        before_porcelain,
        git_porcelain(fx.repo()),
        "the git probe must not refresh the index or otherwise mutate the repo"
    );
}

// ---------------------------------------------------------------------------
// AC6: deterministic output — byte-identical across repeated calls
// ---------------------------------------------------------------------------

#[test]
fn freshness_stamp_is_byte_identical_across_repeated_calls() {
    let fx = Fixture::committed();
    fx.scan();
    let records = fx.records();

    let first = serde_json::to_string(&tool_freshness_stamp(&records, fx.repo(), None))
        .expect("stamp should serialize");
    let second = serde_json::to_string(&tool_freshness_stamp(&records, fx.repo(), None))
        .expect("stamp should serialize");
    assert_eq!(
        first, second,
        "freshness must be deterministic for a fixed store + tree"
    );

    // And per-tool payloads stay byte-identical too.
    for (name, _) in stamped_tool_payloads(&records, fx.repo()) {
        let a = {
            let mut p = match name {
                "inspect_store" => {
                    tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z")
                }
                "symbol_context" => tool_symbol_context_from_records(&records, "hello"),
                _ => {
                    let (recs, task_id) = with_task(records.clone());
                    tool_task_evidence_from_records(&recs, &task_id)
                }
            };
            let recs = records.clone();
            stamp_freshness_on_payload(&mut p, &recs, fx.repo(), None);
            serde_json::to_string(&p).unwrap()
        };
        let b = {
            let mut p = match name {
                "inspect_store" => {
                    tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z")
                }
                "symbol_context" => tool_symbol_context_from_records(&records, "hello"),
                _ => {
                    let (recs, task_id) = with_task(records.clone());
                    tool_task_evidence_from_records(&recs, &task_id)
                }
            };
            let recs = records.clone();
            stamp_freshness_on_payload(&mut p, &recs, fx.repo(), None);
            serde_json::to_string(&p).unwrap()
        };
        assert_eq!(a, b, "{name} payload must be byte-identical across calls");
    }
}

// ---------------------------------------------------------------------------
// Success metric: the 5-state fixture matrix — every tool's verdict agrees with
// `eg freshness` for the same store, with zero false `fresh`
// ---------------------------------------------------------------------------

/// Drives the store into one matrix state, returning the expected verdict.
fn drive_state(fx: &Fixture, state: &str) -> &'static str {
    match state {
        "clean" => Freshness::Fresh.code(),
        "moved_head" => {
            write_lib(fx.repo(), "pub fn hello() {}\npub fn world() {}\n");
            commit_all(fx.repo(), "second");
            Freshness::StaleHead.code()
        }
        "dirty" => {
            write_lib(fx.repo(), "pub fn hello() {}\n// uncommitted\n");
            Freshness::StaleDirty.code()
        }
        "non_git" | "pre_stamping" => Freshness::Unknown.code(),
        _ => panic!("unknown matrix state {state}"),
    }
}

#[test]
fn mcp_verdicts_match_cli_verdict_across_fixture_matrix() {
    for state in ["clean", "moved_head", "dirty", "non_git", "pre_stamping"] {
        let fx = if state == "non_git" {
            Fixture::non_git()
        } else {
            Fixture::committed()
        };
        fx.scan();
        let expected = drive_state(&fx, state);
        let records = if state == "pre_stamping" {
            strip_snapshots(&fx.records())
        } else {
            fx.records()
        };

        // CLI verdict for the same store.
        let cli = if state == "pre_stamping" {
            // The CLI reads the same stripped store: write it to a scratch graph.
            let stripped_graph = fx.work.path().join("stripped.jsonl");
            let jsonl = records
                .iter()
                .map(|r| serde_json::to_string(r).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(&stripped_graph, jsonl).unwrap();
            let out = eg()
                .args(["freshness"])
                .arg(fx.repo())
                .arg("--graph")
                .arg(&stripped_graph)
                .args(["--format", "json"])
                .assert()
                .success();
            serde_json::from_slice::<Value>(&out.get_output().stdout).unwrap()
        } else {
            fx.cli_freshness()
        };
        let cli_verdict = cli["freshness"].as_str().expect("CLI freshness code");
        assert_eq!(
            cli_verdict, expected,
            "CLI fixture bug: state {state} should read {expected}, got {cli_verdict}"
        );

        // Every tool's stamped verdict agrees with the CLI.
        for (name, payload) in stamped_tool_payloads(&records, fx.repo()) {
            let verdict = verdict_of(&payload);
            assert_eq!(
                verdict, cli_verdict,
                "state {state}: {name} verdict {verdict} must match CLI verdict {cli_verdict}"
            );
        }

        // The dedicated stamp agrees too.
        let stamp = tool_freshness_stamp(&records, fx.repo(), None);
        assert_eq!(
            stamp["verdict"].as_str(),
            Some(cli_verdict),
            "state {state}: store stamp must match CLI verdict {cli_verdict}"
        );

        // Zero false `fresh`: any non-fresh state must never read fresh.
        if expected != Freshness::Fresh.code() {
            assert_ne!(
                cli_verdict,
                Freshness::Fresh.code(),
                "state {state}: false fresh from CLI"
            );
            let stamp = tool_freshness_stamp(&records, fx.repo(), None);
            assert_ne!(
                stamp["verdict"].as_str(),
                Some(Freshness::Fresh.code()),
                "state {state}: false fresh from MCP stamp"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon helpers for the end-to-end store_freshness test
// ---------------------------------------------------------------------------

fn runtime_dir(data_dir: &Path) -> PathBuf {
    data_dir.file_name().map_or_else(
        || data_dir.join(".egregore-runtime"),
        |file_name| {
            let mut runtime_name = file_name.to_os_string();
            runtime_name.push(".egregore-runtime");
            data_dir.with_file_name(runtime_name)
        },
    )
}

fn read_daemon_state(data_dir: &Path) -> Option<String> {
    let metadata_path = runtime_dir(data_dir).join("egregored.json");
    let contents = fs::read_to_string(metadata_path).ok()?;
    serde_json::from_str::<Value>(&contents)
        .ok()?
        .get("state")?
        .as_str()
        .map(ToOwned::to_owned)
}

struct RunningDaemon {
    child: Child,
    data_dir: PathBuf,
}

impl RunningDaemon {
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Best-effort stop so no daemon is left behind on failure paths.
        let _ = AssertCommand::cargo_bin("egregore")
            .unwrap()
            .args(["daemon", "stop", "--data-dir"])
            .arg(&self.data_dir)
            .output();
    }
}

fn start_daemon(data_dir: &Path) -> RunningDaemon {
    fs::create_dir_all(data_dir).expect("should create data dir");
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("egregore"));
    command
        .arg("daemon")
        .arg("run")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("daemon should spawn");
    let start = Instant::now();
    loop {
        if read_daemon_state(data_dir).as_deref() == Some("running") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "daemon should reach running state for {}",
            data_dir.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
    RunningDaemon {
        child,
        data_dir: data_dir.to_path_buf(),
    }
}
