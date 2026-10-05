//! Runnable acceptance gate for issue #252: prove the five-domain context join.
//!
//! The gate drives ONLY the public `eg` binary end to end against the
//! checked-in fixture `tests/fixtures/five_domain_join` — no
//! library-internal record seeding, no hosted indexing, no remote crawling:
//!
//! 1. `eg scan` → deterministic code-graph facts (the `five_domain_probe`
//!    symbol and its file),
//! 2. `eg import-local-tasks` → one project task + acceptance criterion,
//! 3. `eg write observation` (+ one intentionally dangling citation) and
//!    `eg write failure` → agent memory,
//! 4. `eg write artifact` + `eg link-evidence` → artifact domain,
//! 5. `eg capture-tests --graph` on the fixture's REAL test run →
//!    verification evidence anchored to the probe symbol,
//! 6. one `eg query context` call over the concatenated store.
//!
//! Acceptance mapping (issue #252):
//! - AC2: a single answer has non-empty `source_facts`, `observations`,
//!   `project_state`, `artifacts`, and `verification_evidence`.
//! - AC3: every item in every populated section carries a `record_id` plus a
//!   citable handle (repo-relative file/span, target files, commit handle,
//!   source-artifact path/hash, or an evidence-link `target_record_id`).
//! - AC4: trust separation — the section ID sets are pairwise disjoint, so
//!   no agent-authored observation/task/artifact can leak into
//!   `source_facts`.
//! - AC5: the intentionally dangling citation surfaces in `unresolved`.
//! - AC6: re-running the full workflow on the unchanged fixture yields
//!   byte-identical sections across 5 consecutive runs.
//!
//! A companion test (`gate_detects_severed_artifact_edge`) replays the
//! workflow WITHOUT the `link-evidence` step and asserts the `artifacts`
//! section comes back empty — the committed RED-proof that this gate is not
//! vacuous.

#![allow(missing_docs)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;
use tempfile::TempDir;

const FIXTURE_REL: &str = "tests/fixtures/five_domain_join";
const PROBE_SYMBOL: &str = "five_domain_probe";
const REPO_ID: &str = "five-domain-join";
/// All caller-supplied timestamps / handles are pinned so repeated runs are
/// byte-identical. The scan transaction time is pinned by the fixture's own
/// `egregore.toml` (`[scan] transaction_time`), read from the fixture dir.
const PINNED_AT: &str = "2026-09-16T05:30:00Z";
const PINNED_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
/// Intentionally dangling citation: shaped like a codegraph record ID but
/// absent from every store the gate builds. AC5 requires it to surface in
/// `unresolved` rather than being silently dropped.
const DANGLING_ID: &str =
    "codegraph:v9:0000000000000000000000000000000000000000000000000000000000000000";
/// BLAKE3 of the fixture's `fix.diff` (computed 2026-09-16; the file is
/// checked in, so this never changes). `eg write artifact` stores the hash
/// as-is, so the gate pins the true value.
const PATCH_HASH: &str = "4106cd1be94c83cebd44c5ab1a16828841da7c857a92b6d85d454d9c1005235a";
const GATE_RUNS: usize = 5;
const SECTIONS: [&str; 5] = [
    "source_facts",
    "observations",
    "project_state",
    "artifacts",
    "verification_evidence",
];

/// Absolute path of the checked-in fixture crate.
fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_REL)
}

fn eg_bin() -> PathBuf {
    cargo_bin("eg")
}

fn run_eg(args: &[&str], cwd: &Path) -> String {
    let out = Command::new(eg_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn eg {args:?}: {e}"));
    assert!(
        out.status.success(),
        "eg {args:?} failed (status {}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8(out.stdout).expect("eg output is UTF-8")
}

/// First record ID in `jsonl` whose `kind` is `kind` and (optionally) whose
/// `name` is `name`.
fn find_id(jsonl: &str, kind: &str, name: Option<&str>) -> String {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| {
            v.get("kind").and_then(Value::as_str) == Some(kind)
                && name.is_none_or(|n| v.get("name").and_then(Value::as_str) == Some(n))
        })
        .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| panic!("no {kind} record (name {name:?}) in store JSONL"))
}

fn non_empty_str(v: &Value, key: &str) -> bool {
    v.get(key)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
}

/// AC3 predicate: every populated-section item must carry a record ID plus a
/// citable handle — a repo-relative file/span, target files, a commit handle,
/// a source-artifact path/hash, or an evidence-link `target_record_id`.
fn has_citable_handle(item: &Value) -> bool {
    non_empty_str(item, "repo_relative_path")
        || item.get("span").is_some_and(|s| !s.is_null())
        || item
            .get("target_files")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        || non_empty_str(item, "git_commit")
        || non_empty_str(item, "base_commit")
        || non_empty_str(item, "head_sha")
        || non_empty_str(item, "source_artifact_path")
        || non_empty_str(item, "source_artifact_hash")
        || item
            .get("evidence_links")
            .and_then(Value::as_array)
            .is_some_and(|links| links.iter().any(|l| non_empty_str(l, "target_record_id")))
}

fn assert_sections_citable(envelope: &Value) {
    for section in SECTIONS {
        let items = envelope
            .get(section)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("answer missing section `{section}`"));
        assert!(
            !items.is_empty(),
            "AC2: section `{section}` must be non-empty in the joined answer"
        );
        for item in items {
            let rid = item
                .get("record_id")
                .and_then(Value::as_str)
                .unwrap_or("<missing>");
            assert!(
                !rid.is_empty() && rid != "<missing>",
                "AC3: item in `{section}` lacks a record_id: {}",
                serde_json::to_string(item).unwrap_or_default(),
            );
            assert!(
                has_citable_handle(item),
                "AC3: item `{rid}` in `{section}` lacks a citable handle: {}",
                serde_json::to_string(item).unwrap_or_default(),
            );
        }
    }
}

/// AC4: trust separation — no record ID may appear in two sections, so an
/// agent-authored observation/task/artifact can never masquerade as a source
/// fact.
fn assert_sections_disjoint(envelope: &Value) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for section in SECTIONS {
        let items = envelope
            .get(section)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("answer missing section `{section}`"));
        for item in items {
            let rid = item
                .get("record_id")
                .and_then(Value::as_str)
                .expect("record_id checked by assert_sections_citable");
            assert!(
                seen.insert(rid),
                "AC4: record `{rid}` appears in more than one section \
                 (trust separation violated)"
            );
        }
    }
}

/// AC5: the intentionally dangling citation must surface in `unresolved`.
fn assert_dangling_unresolved(envelope: &Value) {
    let unresolved = envelope
        .get("unresolved")
        .and_then(Value::as_array)
        .expect("answer must carry an `unresolved` section");
    assert!(
        unresolved
            .iter()
            .any(|u| { u.get("target_handle").and_then(Value::as_str) == Some(DANGLING_ID) }),
        "AC5: dangling citation `{DANGLING_ID}` missing from `unresolved`: {}",
        serde_json::to_string(unresolved).unwrap_or_default(),
    );
}

/// Canonical, comparable rendering of the answer's joined sections for AC6.
///
/// AC6 requires byte-stable *section membership and ordering* — the ordered
/// `record_id` list of each section (sections are record-ID sorted by the
/// query) plus the ordered dangling-target handles from `unresolved`.
/// Full-row bytes are deliberately NOT compared: rows may carry volatile
/// run metadata (e.g. the `TestRun`'s BLAKE3 of the raw libtest stream,
/// whose `exec_time` field varies run to run) while membership and order
/// stay canonical.
fn canonical_answer(envelope: &Value) -> String {
    let mut parts = Vec::with_capacity(SECTIONS.len() + 1);
    for section in SECTIONS {
        let ids: Vec<&str> = envelope
            .get(section)
            .and_then(Value::as_array)
            .expect("section present (checked by assert_sections_citable)")
            .iter()
            .map(|item| {
                item.get("record_id")
                    .and_then(Value::as_str)
                    .expect("record_id checked by assert_sections_citable")
            })
            .collect();
        parts.push(format!("{section}:{}", ids.join(",")));
    }
    let dangling: Vec<&str> = envelope
        .get("unresolved")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|u| u.get("target_handle").and_then(Value::as_str))
        .collect();
    parts.push(format!("unresolved:{}", dangling.join(",")));
    parts.join("\n")
}

fn run_cargo_test(fixture: &Path, target_dir: &Path, run_json: &Path) {
    // The libtest JSON event stream (`--format json`) is nightly-only; the
    // linked libtest is baked in at compile time, so the fixture itself must
    // be built by nightly. This matches the documented `eg capture-tests`
    // workflow (docs/cli/capture-tests.md), which already requires
    // `-Z unstable-options`. RUSTUP_TOOLCHAIN overrides the worktree's
    // rust-toolchain.toml pin for this nested fixture build only.
    let out = Command::new("cargo")
        .arg("test")
        .arg("--manifest-path")
        .arg(fixture.join("Cargo.toml"))
        .arg("--")
        .arg("-Z")
        .arg("unstable-options")
        .arg("--format")
        .arg("json")
        .current_dir(fixture)
        .env("RUSTUP_TOOLCHAIN", "nightly")
        .env("CARGO_TARGET_DIR", target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn cargo test: {e}"));
    assert!(
        out.status.success(),
        "fixture cargo test failed:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    fs::write(run_json, &out.stdout).expect("write libtest stream");
}

/// Build one full five-domain store with public `eg` commands only and
/// return the parsed `eg query context` answer envelope.
///
/// When `link_artifact` is false the `link-evidence` step is skipped, which
/// severs the artifact domain from the join (used by the RED-proof test).
#[allow(clippy::too_many_lines)] // Linear six-step workflow; splitting would scatter the documented golden path.
fn build_answer(fixture: &Path, work: &Path, target_dir: &Path, link_artifact: bool) -> Value {
    let code = work.join("code.jsonl");
    let tasks = work.join("tasks.jsonl");
    let artifact = work.join("artifact.jsonl");
    let links = work.join("links.jsonl");
    let testrun = work.join("testrun.jsonl");
    let observation = work.join("observation.jsonl");
    let failure = work.join("failure.jsonl");
    let store = work.join("store.jsonl");
    let run_json = work.join("run.json");

    let s = |p: &Path| p.to_str().expect("work paths are UTF-8").to_owned();

    // 1. Code facts. The fixture's egregore.toml pins [scan] transaction_time.
    let code_s = s(&code);
    run_eg(
        &[
            "scan",
            s(fixture).as_str(),
            "--repo-id-override",
            REPO_ID,
            "--out",
            code_s.as_str(),
        ],
        fixture,
    );
    let code_jsonl = fs::read_to_string(&code).expect("read code.jsonl");
    let symbol_id = find_id(&code_jsonl, "Symbol", Some(PROBE_SYMBOL));

    // 2. Project state.
    let tasks_s = s(&tasks);
    run_eg(
        &[
            "import-local-tasks",
            s(&fixture.join("tasks")).as_str(),
            "--repo-root",
            s(fixture).as_str(),
            "--transaction-time",
            PINNED_AT,
            "--out",
            tasks_s.as_str(),
        ],
        fixture,
    );
    let task_id = find_id(
        &fs::read_to_string(&tasks).expect("read tasks.jsonl"),
        "Task",
        None,
    );

    // 3. Agent memory: one observation citing the probe symbol plus one
    // intentionally dangling citation, and one failure referencing the task.
    let observation_s = s(&observation);
    run_eg(
        &[
            "write",
            "observation",
            "--agent-id",
            "gate-agent",
            "--agent-kind",
            "other",
            "--session-id",
            "gate-session-1",
            "--observed-at",
            PINNED_AT,
            "--source-handle",
            "gate-fixture:five_domain_probe",
            "--text",
            "five_domain_probe returns the join sentinel; keep it stable.",
            "--confidence",
            "0.9",
            "--evidence-target",
            symbol_id.as_str(),
            "--evidence-target",
            DANGLING_ID,
            "--out",
            observation_s.as_str(),
        ],
        fixture,
    );
    let failure_s = s(&failure);
    run_eg(
        &[
            "write",
            "failure",
            "--agent-id",
            "gate-agent",
            "--agent-kind",
            "human",
            "--session-id",
            "gate-session-1",
            "--observed-at",
            PINNED_AT,
            "--source-handle",
            "gate-fixture:five_domain_probe",
            "--failure-kind",
            "assumption_rejected",
            "--text",
            "Assumed the probe needed no test; the captured run below rejects that.",
            "--failed-on",
            symbol_id.as_str(),
            "--references-task",
            task_id.as_str(),
            "--out",
            failure_s.as_str(),
        ],
        fixture,
    );

    // 4. Artifact domain, linked to the code graph (optional for RED-proof).
    let artifact_s = s(&artifact);
    let patch_file = s(&fixture.join("fix.diff"));
    run_eg(
        &[
            "write",
            "artifact",
            "--agent-id",
            "gate-agent",
            "--agent-kind",
            "other",
            "--session-id",
            "gate-session-1",
            "--observed-at",
            PINNED_AT,
            "--patch-file",
            patch_file.as_str(),
            "--source-artifact-path",
            patch_file.as_str(),
            "--source-artifact-hash",
            PATCH_HASH,
            "--validation-summary",
            "Fixture patch: comment-only change to five_domain_probe; applies cleanly.",
            "--target-file",
            "src/lib.rs",
            "--out",
            artifact_s.as_str(),
        ],
        fixture,
    );
    if link_artifact {
        let links_s = s(&links);
        run_eg(
            &[
                "link-evidence",
                "--code-graph",
                code_s.as_str(),
                "--evidence",
                artifact_s.as_str(),
                "--out",
                links_s.as_str(),
            ],
            fixture,
        );
    }

    // 5. Verification evidence: the fixture's REAL test run, anchored to the
    // probe symbol through --graph resolution (no fabricated streams).
    run_cargo_test(fixture, target_dir, &run_json);
    let testrun_s = s(&testrun);
    run_eg(
        &[
            "capture-tests",
            "--input",
            s(&run_json).as_str(),
            "--out",
            testrun_s.as_str(),
            "--session-id",
            "gate-session-1",
            "--commit",
            PINNED_COMMIT,
            "--suite",
            "five-domain-join",
            "--command",
            "cargo test",
            "--exit-code",
            "0",
            "--executed-at",
            PINNED_AT,
            "--graph",
            code_s.as_str(),
        ],
        fixture,
    );

    // 6. One store, one question.
    let mut store_bytes = Vec::new();
    for part in [&code, &tasks, &artifact, &testrun, &observation, &failure] {
        store_bytes.extend_from_slice(&fs::read(part).expect("read store part"));
    }
    if link_artifact {
        store_bytes.extend_from_slice(&fs::read(&links).expect("read links.jsonl"));
    }
    fs::write(&store, store_bytes).expect("write store.jsonl");

    let stdout = run_eg(
        &[
            "query",
            "context",
            PROBE_SYMBOL,
            "--graph",
            s(&store).as_str(),
        ],
        fixture,
    );
    let envelope: Value = serde_json::from_str(&stdout).expect("query context is JSON");
    assert_eq!(
        envelope.get("ok"),
        Some(&Value::Bool(true)),
        "query context must succeed: {stdout}"
    );
    envelope
}

/// Issue #252 acceptance gate: one fixture/store composes all five domains
/// through public `eg` commands, and the joined answer is byte-stable across
/// 5 consecutive runs.
#[test]
fn five_domain_context_join_gate() {
    let fixture = fixture_dir();
    assert!(
        fixture.is_dir(),
        "fixture dir {} must be checked in",
        fixture.display()
    );
    // Shared cargo target dir across the 5 runs: the fixture never changes,
    // so rebuilds are fingerprint no-ops after the first compile.
    let cargo_target = TempDir::new().expect("cargo target tempdir");

    let mut canonical: Option<String> = None;
    for run in 1..=GATE_RUNS {
        let work = TempDir::new().expect("work tempdir");
        let envelope = build_answer(
            &fixture,
            work.path(),
            cargo_target.path(),
            true, // link_artifact
        );
        assert_sections_citable(&envelope);
        assert_sections_disjoint(&envelope);
        assert_dangling_unresolved(&envelope);
        let rendered = canonical_answer(&envelope);
        if let Some(prev) = &canonical {
            assert_eq!(
                rendered, *prev,
                "AC6: run {run} of {GATE_RUNS} differs — the join must be byte-stable"
            );
        } else {
            canonical = Some(rendered);
        }
    }
}

/// RED-proof for the gate: without the `link-evidence` step the artifact
/// domain is severed from the join and the `artifacts` section comes back
/// empty, so the AC2 assertion (not the workflow) is what fails.
#[test]
fn gate_detects_severed_artifact_edge() {
    let fixture = fixture_dir();
    let work = TempDir::new().expect("work tempdir");
    let cargo_target = TempDir::new().expect("cargo target tempdir");
    let envelope = build_answer(&fixture, work.path(), cargo_target.path(), false);
    let artifacts = envelope
        .get("artifacts")
        .and_then(Value::as_array)
        .expect("answer carries an `artifacts` section");
    assert!(
        artifacts.is_empty(),
        "severing link-evidence must empty the artifacts section, got: {}",
        serde_json::to_string(artifacts).unwrap_or_default(),
    );
}
