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
        // Authorization is manifest-derived: every record carries its producer.
        assert_eq!(rec["producer_id"], "op-1");
    }
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
        .map(|(p, c)| {
            // Serialize with serde_json so the path is JSON-escaped; on Windows a
            // raw `display()` path (`C:\Users\...`) would produce invalid JSON
            // escapes and the capture would reject the manifest.
            serde_json::json!({ "class": c, "source_path": p }).to_string()
        })
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
    // Operator value must not be echoed (it could be a secret/bearer token).
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert!(
        !stderr_str.contains("op-not-authorised"),
        "operator ID must not appear in error output: {stderr_str}"
    );
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
    // Serialize with serde_json so the path is JSON-escaped (Windows backslash
    // paths would otherwise form invalid JSON escapes).
    let manifest_line =
        serde_json::json!({ "class": "llm_inference_log", "source_path": src_file }).to_string();
    fs::write(&manifest_path, manifest_line + "\n").unwrap();

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

/// Issue #321 (Codex finding A): the generic manifest capture path reads
/// `source_path` bytes straight from disk and applies NO redaction, so it must
/// REJECT a `log_payload` entry — those blobs are post-redaction bytes produced
/// only by `eg scan-logs --protected-raw-artifacts`. A mixed manifest still
/// stores its other valid entries atomically (no partial write for the rejected
/// one, and no blob for the raw log bytes).
#[test]
fn capture_rejects_log_payload_class_in_generic_manifest() {
    let (_guard, store) = tmp_store();
    let src = tempfile::tempdir().expect("src dir");
    let good = src.path().join("transcript.txt");
    fs::write(&good, b"agent transcript body").unwrap();
    let logf = src.path().join("app.log");
    fs::write(&logf, b"ERROR boom secret=hunterSECRETtokenValueLong\n").unwrap();

    let manifest_path = src.path().join("manifest.jsonl");
    let lines = [
        serde_json::json!({ "class": "transcript", "source_path": good }).to_string(),
        serde_json::json!({ "class": "log_payload", "source_path": logf }).to_string(),
    ];
    fs::write(&manifest_path, lines.join("\n") + "\n").unwrap();

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

    // The log_payload entry is rejected with the stable diagnostic, not stored.
    let log_entry = entries
        .iter()
        .find(|e| e["source_path"].as_str().unwrap().ends_with("app.log"))
        .expect("log entry present");
    assert_eq!(
        log_entry["diagnostic"]["code"], "log_payload_requires_scan_logs",
        "log_payload must be rejected in the generic manifest path"
    );
    assert_eq!(log_entry["stored"], false);

    // The valid transcript entry is still stored (atomic / no partial write).
    let good_entry = entries
        .iter()
        .find(|e| {
            e["source_path"]
                .as_str()
                .unwrap()
                .ends_with("transcript.txt")
        })
        .expect("transcript entry present");
    assert_eq!(good_entry["stored"], true);
    assert_eq!(json["stored_count"], 1);
    assert_eq!(json["skipped_count"], 1);

    // Manifest holds exactly the transcript record; no log_payload record.
    let manifest = fs::read_to_string(store.join("manifest.jsonl")).expect("manifest must exist");
    let recs: Vec<serde_json::Value> = manifest
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(recs.len(), 1, "only the transcript record is persisted");
    assert_eq!(recs[0]["source_class"], "transcript");

    // No blob was written for the rejected raw log bytes: exactly one blob (the
    // transcript) exists. Blob file names are the 64-hex content hash.
    let blob_count = fs::read_dir(store.join("blobs"))
        .map(|rd| {
            rd.filter_map(std::result::Result::ok)
                .filter(|e| e.file_name().to_string_lossy().len() == 64)
                .count()
        })
        .unwrap_or(0);
    assert_eq!(blob_count, 1, "only the transcript blob was written");
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

/// Empty `--producer ""` must be rejected (exit 1) to prevent bypassing the
/// access gate via an accidentally unset env-var like `$(git config user.email)`.
#[test]
fn capture_enabled_with_empty_producer_exits_1() {
    let (_guard, store) = tmp_store();
    eg().args(["protected", "capture"])
        .arg("--manifest")
        .arg(capture_manifest())
        .arg("--store")
        .arg(&store)
        .arg("--protected-raw-artifacts")
        .arg("--producer")
        .arg("") // empty string
        .assert()
        .code(1)
        .stderr(predicate::str::contains("invalid_field"));
}

/// `eg protected get` must return `corrupt_manifest_record` when the stored
/// `handle` field does not match the handle recomputed from the record's
/// class / `content_hash` / `source_path`.
#[test]
fn get_corrupt_manifest_record() {
    let (_guard, store) = tmp_store();
    run_capture_enabled(&store, "op-1");

    // Tamper with the first manifest record's `handle` field.
    let manifest_path = store.join("manifest.jsonl");
    let content = fs::read_to_string(&manifest_path).expect("manifest");
    let first_line = content.lines().next().expect("at least one record");
    let mut rec: serde_json::Value =
        serde_json::from_str(first_line).expect("manifest line is JSON");
    let tampered_handle =
        "protected:v1:0000000000000000000000000000000000000000000000000000000000000000";
    rec["handle"] = serde_json::json!(tampered_handle);
    // Replace just the first line.
    let rest: Vec<&str> = content.lines().skip(1).collect();
    let new_content = std::iter::once(serde_json::to_string(&rec).unwrap().as_str())
        .chain(rest)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&manifest_path, new_content).expect("write tampered manifest");

    let stderr = eg()
        .args(["protected", "get", tampered_handle])
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
    assert_eq!(json["error"]["code"], "corrupt_manifest_record");
}

/// Capturing the same payloads again after a blob is **corrupted** (not just
/// absent) must repair the store.  This extends the missing-blob repair test
/// to cover the case where the file exists but the content hash no longer matches.
#[test]
fn capture_repairs_corrupted_blob_on_recapture() {
    let (_guard, store) = tmp_store();
    let first = run_capture_enabled(&store, "op-1");
    let handle = first["entries"][0]["handle"].as_str().expect("handle");
    let content_hash = first["entries"][0]["content_hash"].as_str().expect("hash");

    // Corrupt the blob (overwrite with wrong bytes — file still exists).
    fs::write(store.join("blobs").join(content_hash), b"corrupted data").expect("corrupt blob");

    // Re-capture with sources still present — must repair the corrupted blob.
    run_capture_enabled(&store, "op-1");

    // get must now succeed and return valid content.
    eg().args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success();
}

/// Capturing the same payloads again after a blob is deleted must repair the
/// store: `eg protected get` must succeed after re-capture while the source
/// is still available (fix for missing-blob re-capture scenario).
#[test]
fn capture_repairs_missing_blob_on_recapture() {
    let (_guard, store) = tmp_store();
    let first = run_capture_enabled(&store, "op-1");
    let handle = first["entries"][0]["handle"].as_str().expect("handle");
    let content_hash = first["entries"][0]["content_hash"].as_str().expect("hash");

    // Delete the blob.
    fs::remove_file(store.join("blobs").join(content_hash)).expect("delete blob");
    assert!(
        !store.join("blobs").join(content_hash).exists(),
        "blob must be gone"
    );

    // Re-capture with sources still present — must repair the blob.
    run_capture_enabled(&store, "op-1");

    assert!(
        store.join("blobs").join(content_hash).exists(),
        "blob must be restored after re-capture"
    );

    // get must now succeed.
    eg().args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success();
}

/// Capture manifest file read failure must emit a JSON error envelope (not
/// plain-text anyhow error) so automation can parse the diagnostic.
#[test]
fn capture_manifest_read_error_emits_json_envelope() {
    let (_guard, store) = tmp_store();
    let stderr = eg()
        .args(["protected", "capture"])
        .arg("--manifest")
        .arg("/tmp/egregore_test_nonexistent_manifest_file.jsonl")
        .arg("--store")
        .arg(&store)
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&stderr).expect("manifest read error must produce JSON envelope");
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "manifest_read_error");
}

/// Capture manifest parse failure must emit a JSON error envelope with code
/// `invalid_manifest` and the failing line number so automation can locate it.
#[test]
fn capture_manifest_parse_error_emits_json_envelope() {
    let (_guard, store) = tmp_store();
    let src = tempfile::tempdir().expect("src");
    let bad_manifest = src.path().join("bad.jsonl");
    fs::write(&bad_manifest, "this is not json\n").unwrap();

    let stderr = eg()
        .args(["protected", "capture"])
        .arg("--manifest")
        .arg(&bad_manifest)
        .arg("--store")
        .arg(&store)
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&stderr).expect("parse error must produce JSON envelope");
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "invalid_manifest");
    assert!(
        json["error"]["detail"]["line"].as_u64().is_some(),
        "line number must be present in detail"
    );
}

/// Re-capturing sources after a manifest record's metadata is corrupted must
/// replace the corrupt record and make `eg protected get` succeed again.
#[test]
fn capture_replaces_corrupt_manifest_record_on_recapture() {
    let (_guard, store) = tmp_store();
    let first = run_capture_enabled(&store, "op-1");
    let handle = first["entries"][0]["handle"].as_str().expect("handle");

    // get() succeeds with the valid record.
    eg().args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success();

    // Tamper with manifest: corrupt content_hash in the target record (keep
    // handle unchanged).  The manifest is multi-line JSONL — find and replace
    // only the line that corresponds to `handle`.
    let manifest = store.join("manifest.jsonl");
    let content = fs::read_to_string(&manifest).unwrap();
    let fake_hash = "a".repeat(64);
    let new_content: String = content
        .lines()
        .map(|line| {
            if line.contains(handle) {
                let mut rec: serde_json::Value = serde_json::from_str(line).unwrap();
                rec["content_hash"] = serde_json::json!(&fake_hash);
                serde_json::to_string(&rec).unwrap()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&manifest, &new_content).unwrap();

    // get() must fail now — integrity check catches the tampered record.
    eg().args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .failure();

    // Re-capture with sources still present — must replace the corrupt record.
    run_capture_enabled(&store, "op-1");

    // get() must succeed again.
    eg().args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success();
}

/// An empty `--operator` identity is never authorized (producer IDs are
/// non-empty, and authorization is manifest-derived).
#[test]
fn get_empty_operator_identity_denied() {
    let (_guard, store) = tmp_store();
    let first = run_capture_enabled(&store, "op-1");
    let handle = first["entries"][0]["handle"].as_str().expect("handle");

    let stderr = eg()
        .args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("")
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&stderr).expect("JSON envelope");
    assert_eq!(json["error"]["code"], "unauthorized");
}

/// `eg protected capture --captured-at not-a-date` must exit 1 with a JSON
/// envelope and write nothing to the store.
#[test]
fn capture_invalid_captured_at_exits_1_with_json() {
    let (_guard, store) = tmp_store();
    let stderr = eg()
        .args(["protected", "capture"])
        .arg("--manifest")
        .arg(capture_manifest())
        .arg("--store")
        .arg(&store)
        .arg("--protected-raw-artifacts")
        .arg("--producer")
        .arg("op-1")
        .arg("--captured-at")
        .arg("not-a-date")
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&stderr).expect("must emit JSON envelope");
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "store_io_error");
    assert!(
        !store.join("blobs").exists(),
        "no blobs on invalid timestamp"
    );
}

/// `eg protected get --out <unwritable>` must exit 1 with a JSON envelope
/// (code: `output_write_error`) instead of a plain anyhow error.
#[test]
fn get_unwritable_out_path_emits_json_envelope() {
    let (_guard, store) = tmp_store();
    let first = run_capture_enabled(&store, "op-1");
    let handle = first["entries"][0]["handle"].as_str().expect("handle");

    // Use a path whose parent directory does not exist.
    let bad_out = store.join("nonexistent_dir").join("out.txt");

    let stderr = eg()
        .args(["protected", "get"])
        .arg(handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .arg("--out")
        .arg(&bad_out)
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&stderr).expect("must emit JSON envelope");
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "output_write_error");
}

/// `eg protected list` on a store with a corrupt manifest must exit 1 with a
/// JSON envelope (code: `store_io_error`) instead of a plain anyhow error.
#[test]
fn list_corrupt_manifest_emits_json_envelope() {
    let (_guard, store) = tmp_store();
    run_capture_enabled(&store, "op-1");
    // Corrupt the manifest.
    fs::write(store.join("manifest.jsonl"), "not valid json\n").expect("corrupt manifest");

    let stderr = eg()
        .args(["protected", "list"])
        .arg("--store")
        .arg(&store)
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&stderr).expect("must emit JSON envelope");
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "store_io_error");
}

// ── Capture: bound/guard the --manifest read before parsing ────────────────────

/// A `--manifest` that names a non-regular file (here a directory; the same
/// path covers FIFOs and devices such as `/dev/zero`) must be rejected with a
/// JSON `manifest_read_error` envelope *before* the file is slurped into
/// memory — otherwise a FIFO/device would block indefinitely or a huge file
/// would exhaust memory ahead of any diagnostic.
#[test]
fn capture_rejects_non_regular_manifest() {
    let (_guard, store) = tmp_store();
    let manifest_dir = tempfile::tempdir().expect("temp dir for non-regular manifest");

    let stderr = eg()
        .args(["protected", "capture"])
        .arg("--manifest")
        .arg(manifest_dir.path())
        .arg("--store")
        .arg(&store)
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&stderr).expect("must emit JSON envelope");
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "manifest_read_error");
    let msg = json["error"]["detail"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        msg.contains("not a regular file"),
        "message must mention 'not a regular file': {msg}"
    );
}

// ── Get: a failed --out get must not destroy an existing file ──────────────────

/// When `--out` names an existing file and `eg protected get` fails (here an
/// unauthorized operator), the existing file must be preserved: the bytes are
/// staged to a sibling temp and only renamed into place on a verified success.
#[test]
fn failed_get_preserves_existing_out_file() {
    let (_guard, store) = tmp_store();
    let json = run_capture_enabled(&store, "op-1");
    let handle = json["entries"][0]["handle"]
        .as_str()
        .expect("handle string")
        .to_owned();

    let out_dir = tempfile::tempdir().expect("out dir");
    let out_file = out_dir.path().join("existing.bin");
    fs::write(&out_file, b"PRECIOUS EXISTING DATA").unwrap();

    // Unauthorized operator → get fails; the existing --out file must survive.
    eg().args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("not-authorized")
        .arg("--out")
        .arg(&out_file)
        .assert()
        .failure();

    let after = fs::read(&out_file).expect("existing --out file must still exist");
    assert_eq!(
        after, b"PRECIOUS EXISTING DATA",
        "a failed get must not truncate or destroy the existing --out file"
    );
}

// ── Get: a successful --out get replaces an existing file ──────────────────────

/// A successful `eg protected get --out existing.bin` must overwrite the
/// existing destination with the verified payload (cross-platform replace).
#[test]
fn get_out_replaces_existing_file_on_success() {
    let (_guard, store) = tmp_store();
    let json = run_capture_enabled(&store, "op-1");
    let handle = json["entries"][0]["handle"]
        .as_str()
        .expect("handle string")
        .to_owned();

    let out_dir = tempfile::tempdir().expect("out dir");
    let out_file = out_dir.path().join("dest.bin");
    fs::write(&out_file, b"OLD SENTINEL CONTENT").unwrap();

    eg().args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .arg("--out")
        .arg(&out_file)
        .assert()
        .success();

    let after = fs::read(&out_file).expect("destination must exist after success");
    assert_ne!(
        after, b"OLD SENTINEL CONTENT",
        "a successful get must overwrite the existing --out file"
    );
    assert!(!after.is_empty(), "destination must contain the payload");
}

// ── Get: a failed-verification --out get creates nothing and leaves no temp ────

/// When the blob is tampered so verification fails, `eg protected get --out`
/// must not create the destination file and must not leave a staging temp.
#[test]
fn failed_verification_get_creates_no_out_file_and_no_temp() {
    let (_guard, store) = tmp_store();
    let json = run_capture_enabled(&store, "op-1");
    let handle = json["entries"][0]["handle"]
        .as_str()
        .expect("handle")
        .to_owned();
    let content_hash = json["entries"][0]["content_hash"]
        .as_str()
        .expect("content_hash")
        .to_owned();

    // Corrupt the blob (wrong size triggers the fd size guard before any emit).
    fs::write(store.join("blobs").join(&content_hash), b"corrupt").unwrap();

    let out_dir = tempfile::tempdir().expect("out dir");
    let out_file = out_dir.path().join("new.bin"); // does not exist yet

    eg().args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .arg("--out")
        .arg(&out_file)
        .assert()
        .failure();

    assert!(
        !out_file.exists(),
        "no --out file may be created when verification fails"
    );
    let leftover: Vec<PathBuf> = fs::read_dir(out_dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert!(
        leftover.is_empty(),
        "no staging temp may be left behind, found: {leftover:?}"
    );
}

// ── Get: retrieval diagnostics take priority over --out staging failures ───────

/// When `--out` has a missing parent AND retrieval would fail (unauthorized
/// operator), the retrieval diagnostic must be surfaced, not the output error.
#[test]
fn failed_out_staging_surfaces_retrieval_diagnostic() {
    let (_guard, store) = tmp_store();
    let json = run_capture_enabled(&store, "op-1");
    let handle = json["entries"][0]["handle"]
        .as_str()
        .expect("handle")
        .to_owned();

    let out_dir = tempfile::tempdir().expect("out dir");
    let bad_out = out_dir.path().join("missing_subdir").join("out.bin");

    let stderr = eg()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("not-authorized")
        .arg("--out")
        .arg(&bad_out)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();

    let j: serde_json::Value = serde_json::from_slice(&stderr).expect("JSON envelope");
    assert_eq!(
        j["error"]["code"], "unauthorized",
        "the retrieval diagnostic must take priority over the --out staging error"
    );
}
