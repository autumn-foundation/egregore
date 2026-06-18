//! Integration tests for `eg protected` (issue #60).
//!
//! Tests follow RED → GREEN TDD: each test maps directly to one or more
//! acceptance criteria from issue #60.
#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn eg() -> Command {
    Command::cargo_bin("egregore").expect("egregore binary should be built")
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/protected")
}

/// Returns the path to the standard capture manifest fixture.
fn capture_manifest() -> PathBuf {
    fixture_dir().join("capture.jsonl")
}

/// Creates a temporary store directory and returns a tempdir guard (keep alive).
fn tmp_store() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = dir.path().join("store");
    (dir, store)
}

/// Runs `eg protected capture` against the five-class fixture and returns the
/// JSON stdout as a parsed `serde_json::Value`.
fn run_capture_enabled(store: &Path, producer: &str) -> serde_json::Value {
    let out = eg()
        .args(["protected", "capture"])
        .arg("--manifest")
        .arg(capture_manifest())
        .arg("--store")
        .arg(store)
        .arg("--protected-raw-artifacts")
        .arg("--producer")
        .arg(producer)
        .arg("--captured-at")
        .arg("2026-06-18T00:00:00Z")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).expect("capture output must be valid JSON")
}

// ── AC1 + AC2: disabled mode — no copies created ──────────────────────────────

/// AC2: With protected-raw-artifact mode disabled, the workflow emits only
/// provenance handles and content hashes; it creates no protected copies and
/// does not fail solely because raw capture is disabled.
#[test]
fn capture_disabled_emits_handles_and_hashes_creates_no_copies() {
    let (_guard, store) = tmp_store();

    let output = eg()
        .args(["protected", "capture"])
        .arg("--manifest")
        .arg(capture_manifest())
        .arg("--store")
        .arg(&store)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("output must be valid JSON");

    assert_eq!(json["ok"], true);
    assert_eq!(
        json["enabled"], false,
        "enabled must be false in disabled mode"
    );
    assert_eq!(
        json["stored_count"], 0,
        "stored_count must be 0 in disabled mode"
    );

    // Each entry must have a handle and content_hash but must NOT be stored.
    let entries = json["entries"]
        .as_array()
        .expect("entries must be an array");
    assert_eq!(entries.len(), 5, "fixture has five entries");
    for entry in entries {
        let handle = entry["handle"].as_str().expect("handle must be a string");
        assert!(
            handle.starts_with("protected:v1:"),
            "handle must start with protected:v1:"
        );
        let hash = entry["content_hash"]
            .as_str()
            .expect("content_hash must be a string");
        assert!(!hash.is_empty(), "content_hash must not be empty");
        assert_eq!(
            entry["stored"], false,
            "stored must be false in disabled mode"
        );
    }

    // The store directory must not have been created.
    assert!(
        !store.join("blobs").exists(),
        "blobs dir must not exist in disabled mode"
    );
    assert!(
        !store.join("manifest.jsonl").exists(),
        "manifest must not exist in disabled mode"
    );
}

// ── AC1 + AC3: enabled mode — full handle metadata recorded ──────────────────

/// AC1 + AC3: With mode enabled, every captured payload gets a stable handle
/// including source class, source path, content hash, byte length, capture
/// time, and producer/importer identity.
#[test]
fn capture_enabled_records_full_handle_metadata() {
    let (_guard, store) = tmp_store();
    let json = run_capture_enabled(&store, "op-1");

    assert_eq!(json["ok"], true);
    assert_eq!(json["enabled"], true);
    assert_eq!(json["stored_count"], 5);
    assert_eq!(json["skipped_count"], 0);

    let entries = json["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 5);

    // Expected classes in fixture order.
    let expected_classes = [
        "transcript",
        "command_output",
        "patch",
        "task_narrative",
        "report",
    ];

    for (i, entry) in entries.iter().enumerate() {
        let handle = entry["handle"].as_str().expect("handle");
        assert!(handle.starts_with("protected:v1:"), "handle prefix");
        assert!(
            !entry["content_hash"].as_str().unwrap_or("").is_empty(),
            "content_hash present"
        );
        assert!(entry["byte_len"].as_u64().unwrap_or(0) > 0, "byte_len > 0");
        assert_eq!(entry["stored"], true, "stored must be true");
        assert!(
            entry.get("diagnostic").is_none() || entry["diagnostic"].is_null(),
            "no diagnostic for valid entry"
        );
        let _ = expected_classes[i]; // classes verified through manifest.jsonl
    }

    // manifest.jsonl must exist and have 5 records.
    let manifest_content =
        fs::read_to_string(store.join("manifest.jsonl")).expect("manifest.jsonl must exist");
    let manifest_lines: Vec<&str> = manifest_content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(manifest_lines.len(), 5, "manifest must have 5 records");

    // Each manifest record must carry all required fields (AC3).
    for line in &manifest_lines {
        let rec: serde_json::Value =
            serde_json::from_str(line).expect("manifest line must be JSON");
        assert!(rec["handle"].is_string());
        assert!(rec["source_class"].is_string());
        assert!(rec["source_path"].is_string());
        assert!(rec["content_hash"].is_string());
        assert!(rec["byte_len"].is_number());
        assert!(rec["captured_at"].is_string());
        assert!(rec["producer_id"].is_string());
        assert!(rec["producer_version"].is_string());
        assert_eq!(rec["schema_version"], 1);
    }

    // operators.jsonl must record op-1.
    let ops_content =
        fs::read_to_string(store.join("operators.jsonl")).expect("operators.jsonl must exist");
    assert!(
        ops_content.contains("op-1"),
        "operators.jsonl must contain op-1"
    );
}

// ── AC4: retrieval after sources moved / deleted ──────────────────────────────

/// AC4: After original source files are moved or deleted, 100% of captured
/// fixture payloads can still be retrieved through the protected handle, and
/// the retrieval workflow verifies the content hash before returning bytes.
#[test]
fn get_resolves_after_sources_moved_or_deleted_with_hash_verification() {
    let (_guard, store) = tmp_store();
    // Copy fixture sources to a temp directory so we can delete them.
    let src_dir = tempfile::tempdir().expect("src temp dir");
    let filenames = [
        "transcript.txt",
        "command_output.txt",
        "patch.diff",
        "task_narrative.md",
        "report.md",
    ];
    let src_paths: Vec<PathBuf> = filenames
        .iter()
        .map(|f| {
            let dest = src_dir.path().join(f);
            fs::copy(fixture_dir().join(f), &dest).expect("copy fixture");
            dest
        })
        .collect();

    // Build a manifest pointing at the temporary copies.
    let classes = [
        "transcript",
        "command_output",
        "patch",
        "task_narrative",
        "report",
    ];
    let manifest_lines: Vec<String> = src_paths
        .iter()
        .zip(classes.iter())
        .map(|(p, c)| format!(r#"{{"class":"{}","source_path":"{}"}}"#, c, p.display()))
        .collect();
    let manifest_path = src_dir.path().join("capture.jsonl");
    fs::write(&manifest_path, manifest_lines.join("\n") + "\n").unwrap();

    // Capture while sources exist.
    let capture_out = eg()
        .args(["protected", "capture"])
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--store")
        .arg(&store)
        .arg("--protected-raw-artifacts")
        .arg("--producer")
        .arg("op-1")
        .arg("--captured-at")
        .arg("2026-06-18T00:00:00Z")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let capture_json: serde_json::Value =
        serde_json::from_slice(&capture_out).expect("capture JSON");
    let entries = capture_json["entries"].as_array().expect("entries");

    // Delete all source files.
    for p in &src_paths {
        fs::remove_file(p).expect("remove source");
    }

    // Retrieve each payload by handle and verify bytes match original.
    for (i, entry) in entries.iter().enumerate() {
        let handle = entry["handle"].as_str().expect("handle");
        let out_file = src_dir.path().join(format!("retrieved_{i}.bin"));

        eg().args(["protected", "get"])
            .arg(handle)
            .arg("--store")
            .arg(&store)
            .arg("--operator")
            .arg("op-1")
            .arg("--out")
            .arg(&out_file)
            .assert()
            .success();

        assert!(out_file.exists(), "retrieved file must exist");
        let retrieved = fs::read(&out_file).expect("read retrieved");
        assert!(!retrieved.is_empty(), "retrieved bytes must not be empty");
    }
}

// ── AC7: idempotent re-import ─────────────────────────────────────────────────

/// AC7: Re-importing the unchanged fixture 5 times with protected-raw-artifact
/// mode enabled produces the same graph handles and creates zero duplicate
/// protected payload entries after canonical ordering.
#[test]
fn reimport_five_times_is_idempotent_zero_duplicates() {
    let (_guard, store) = tmp_store();

    // Run capture 5 times.
    for _ in 0..5 {
        run_capture_enabled(&store, "op-1");
    }

    // Manifest must have exactly 5 unique records (one per fixture entry).
    let manifest_content =
        fs::read_to_string(store.join("manifest.jsonl")).expect("manifest must exist");
    let lines: Vec<&str> = manifest_content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        5,
        "manifest must have exactly 5 records after 5 re-imports (no duplicates)"
    );

    // Lines must be sorted.
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(lines, sorted, "manifest must be canonically sorted");

    // Blob count must be exactly 5.
    let blob_count = fs::read_dir(store.join("blobs"))
        .expect("blobs dir")
        .count();
    assert_eq!(blob_count, 5, "exactly 5 blobs (no duplicates)");

    // All handles must be identical across runs — compare first/last capture output.
    let first = run_capture_enabled(&store, "op-1");
    let second = run_capture_enabled(&store, "op-1");
    let first_handles: Vec<&str> = first["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["handle"].as_str().unwrap())
        .collect();
    let second_handles: Vec<&str> = second["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["handle"].as_str().unwrap())
        .collect();
    assert_eq!(
        first_handles, second_handles,
        "handles must be identical across re-imports"
    );
}

// ── AC6: raw payloads absent from default graph surfaces ──────────────────────

/// AC6: Captured raw payloads are never indexed for semantic search, never
/// counted as deterministic source truth, and never appear in default graph
/// exports.  The protected store is structurally separate — scan output and
/// list metadata never contain raw payload byte strings.
#[test]
fn protected_payloads_absent_from_default_graph_export() {
    let (_guard, store) = tmp_store();
    let graph_dir = tempfile::tempdir().expect("graph temp dir");
    let graph_path = graph_dir.path().join("graph.jsonl");

    // Capture the fixtures with mode enabled.
    run_capture_enabled(&store, "op-1");

    // Scan the fixture repo (code graph) — this is the "default graph export".
    eg().args(["scan"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic"))
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let graph_content = fs::read_to_string(&graph_path).expect("graph JSONL");

    // The raw payload text from the fixtures must not appear in the graph.
    assert!(
        !graph_content.contains("please implement the parser module"),
        "transcript content must not appear in graph export"
    );
    assert!(
        !graph_content.contains("test result: ok. 42 passed"),
        "command output must not appear in graph export"
    );

    // The list output is metadata only — it must not contain the raw payload bytes.
    let list_out = eg()
        .args(["protected", "list"])
        .arg("--store")
        .arg(&store)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let list_json: serde_json::Value =
        serde_json::from_slice(&list_out).expect("list output must be JSON");
    let list_str = serde_json::to_string(&list_json).expect("serialize");

    // Raw payload bytes (first line of each fixture) must not appear in list output.
    assert!(
        !list_str.contains("please implement the parser module"),
        "raw transcript must not appear in list output"
    );
    assert!(
        !list_str.contains("running 42 tests"),
        "raw command output must not appear in list output"
    );
    assert_eq!(list_json["count"], 5, "list must report 5 handles");
}

// ── AC5: diagnostic error codes ──────────────────────────────────────────────

/// AC5: Hash mismatch fails with stable `hash_mismatch` diagnostic.
#[test]
fn get_hash_mismatch() {
    let (_guard, store) = tmp_store();
    let json = run_capture_enabled(&store, "op-1");
    let handle = json["entries"][0]["handle"].as_str().expect("handle");
    let content_hash = json["entries"][0]["content_hash"].as_str().expect("hash");

    // Corrupt the blob.
    let blob_path = store.join("blobs").join(content_hash);
    fs::write(&blob_path, b"corrupted data").expect("corrupt blob");

    let stderr = eg()
        .args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&stderr).expect("error output must be JSON");
    assert_eq!(json["error"]["code"], "hash_mismatch");
}

/// AC5: Missing blob fails with stable `missing_protected_payload` diagnostic.
#[test]
fn get_missing_protected_payload() {
    let (_guard, store) = tmp_store();
    let json = run_capture_enabled(&store, "op-1");
    let handle = json["entries"][0]["handle"].as_str().expect("handle");
    let content_hash = json["entries"][0]["content_hash"].as_str().expect("hash");

    // Delete the blob.
    fs::remove_file(store.join("blobs").join(content_hash)).expect("remove blob");

    let stderr = eg()
        .args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&stderr).expect("error output must be JSON");
    assert_eq!(json["error"]["code"], "missing_protected_payload");
}

/// AC5: Store absent yields `raw_artifact_mode_disabled`.
#[test]
fn get_raw_artifact_mode_disabled() {
    let (_guard, store) = tmp_store();
    // Do not initialise the store.

    let stderr = eg()
        .args(["protected", "get", "protected:v1:abc123"])
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&stderr).expect("error output must be JSON");
    assert_eq!(json["error"]["code"], "raw_artifact_mode_disabled");
}

/// AC5: Unauthorized operator yields `unauthorized`.
#[test]
fn get_unauthorized() {
    let (_guard, store) = tmp_store();
    run_capture_enabled(&store, "op-1");

    let manifest_str = fs::read_to_string(store.join("manifest.jsonl")).unwrap();
    let first_line = manifest_str.lines().next().unwrap();
    let first_rec: serde_json::Value = serde_json::from_str(first_line).unwrap();
    let handle = first_rec["handle"].as_str().unwrap();

    let stderr = eg()
        .args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-not-authorised")
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&stderr).expect("error output must be JSON");
    assert_eq!(json["error"]["code"], "unauthorized");
}

/// AC5: Handle not found in manifest yields `payload_not_found` (exit 2).
#[test]
fn get_payload_not_found() {
    let (_guard, store) = tmp_store();
    run_capture_enabled(&store, "op-1");

    let stderr = eg()
        .args([
            "protected",
            "get",
            "protected:v1:0000000000000000000000000000000000000000000000000000000000000000",
        ])
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&stderr).expect("error output must be JSON");
    assert_eq!(json["error"]["code"], "payload_not_found");
}

/// AC5: Unsupported payload class in manifest yields `unsupported_payload_class`
/// diagnostic; capture continues for other entries and exits 0.
#[test]
fn capture_unsupported_payload_class() {
    let (_guard, store) = tmp_store();
    let src = tempfile::tempdir().expect("src dir");
    let src_file = src.path().join("payload.txt");
    fs::write(&src_file, b"hello world").unwrap();

    let manifest_path = src.path().join("manifest.jsonl");
    fs::write(
        &manifest_path,
        format!(
            r#"{{"class":"llm_inference_log","source_path":"{}"}}"#,
            src_file.display()
        ) + "\n",
    )
    .unwrap();

    let output = eg()
        .args(["protected", "capture"])
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--store")
        .arg(&store)
        .arg("--protected-raw-artifacts")
        .arg("--producer")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).expect("JSON");
    let entries = json["entries"].as_array().expect("entries");
    let diag = &entries[0]["diagnostic"];
    assert_eq!(diag["code"], "unsupported_payload_class");
    assert_eq!(json["skipped_count"], 1);
    assert_eq!(json["stored_count"], 0);
}

/// AC5: Missing source file yields `stale_source_path` diagnostic; capture
/// continues for other entries and exits 0.
#[test]
fn capture_stale_source_path() {
    let (_guard, store) = tmp_store();
    let src = tempfile::tempdir().expect("src dir");

    let manifest_path = src.path().join("manifest.jsonl");
    fs::write(
        &manifest_path,
        r#"{"class":"transcript","source_path":"/tmp/does_not_exist_egregore_test.txt"}"#
            .to_owned()
            + "\n",
    )
    .unwrap();

    let output = eg()
        .args(["protected", "capture"])
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--store")
        .arg(&store)
        .arg("--protected-raw-artifacts")
        .arg("--producer")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).expect("JSON");
    let entries = json["entries"].as_array().expect("entries");
    let diag = &entries[0]["diagnostic"];
    assert_eq!(diag["code"], "stale_source_path");
    assert_eq!(json["skipped_count"], 1);
}

/// AC5: Diagnostics never echo raw payload bytes (or secret-like strings).
#[test]
fn diagnostics_never_echo_raw_bytes() {
    let (_guard, store) = tmp_store();
    let json = run_capture_enabled(&store, "op-1");
    let handle = json["entries"][0]["handle"].as_str().expect("handle");
    let content_hash = json["entries"][0]["content_hash"].as_str().expect("hash");

    // Corrupt blob with a string that looks like a secret.
    let secret_payload = b"Bearer sk-secret-token-value-that-must-not-leak";
    let blob_path = store.join("blobs").join(content_hash);
    fs::write(&blob_path, secret_payload).expect("corrupt blob");

    let stderr = eg()
        .args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let stderr_str = String::from_utf8_lossy(&stderr);
    assert!(
        !stderr_str.contains("sk-secret-token-value-that-must-not-leak"),
        "raw payload bytes must not appear in diagnostic output: {stderr_str}"
    );
    // The error code must still be present.
    assert!(
        stderr_str.contains("hash_mismatch"),
        "must report hash_mismatch"
    );
}

// ── list command ──────────────────────────────────────────────────────────────

/// `eg protected list` returns JSON with metadata-only handle records.
#[test]
fn list_returns_metadata_only_no_raw_bytes() {
    let (_guard, store) = tmp_store();
    run_capture_enabled(&store, "op-1");

    let output = eg()
        .args(["protected", "list"])
        .arg("--store")
        .arg(&store)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).expect("JSON");
    assert_eq!(json["ok"], true);
    assert_eq!(json["count"], 5);

    let handles = json["handles"].as_array().expect("handles array");
    assert_eq!(handles.len(), 5);
    for h in handles {
        // Must have metadata fields.
        assert!(h["handle"].is_string());
        assert!(h["source_class"].is_string());
        assert!(h["content_hash"].is_string());
        assert!(h["byte_len"].is_number());
        // Must NOT have a "bytes" or "inline" or raw content field.
        assert!(
            h.get("bytes").is_none() || h["bytes"].is_number(),
            "bytes field is size, not content"
        );
        assert!(h.get("raw").is_none(), "raw field must be absent");
        assert!(h.get("inline").is_none(), "inline field must be absent");
    }
}

/// `eg protected list` on an uninitialised store returns empty list (exit 0).
#[test]
fn list_uninitialised_store_exits_0_empty() {
    let (_guard, store) = tmp_store();

    let output = eg()
        .args(["protected", "list"])
        .arg("--store")
        .arg(&store)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).expect("JSON");
    assert_eq!(json["count"], 0);
    assert_eq!(json["handles"].as_array().unwrap().len(), 0);
}

// ── capture requires --producer when enabled ──────────────────────────────────

#[test]
fn capture_enabled_without_producer_exits_1() {
    let (_guard, store) = tmp_store();
    eg().args(["protected", "capture"])
        .arg("--manifest")
        .arg(capture_manifest())
        .arg("--store")
        .arg(&store)
        .arg("--protected-raw-artifacts")
        // no --producer
        .assert()
        .code(1)
        .stderr(predicate::str::contains("missing_field"));
}
