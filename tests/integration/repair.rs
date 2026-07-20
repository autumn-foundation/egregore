#![allow(missing_docs)]
#![cfg(feature = "embedded-aletheiadb")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    time::{Duration, Instant},
};

use aletheia_egregore::{
    adapters::EmbeddedAletheiaSink,
    daemon::{StoreLease, runtime_dir_for_data_dir, runtime_metadata_is_stale},
    repair::{
        OwnershipVerdict, RepairAction, RepairActionResult, RepairOptions, RepairRefusalCode,
        RepairSessionResult, preflight, run_repair, run_repair_with,
    },
};
use assert_cmd::Command;
use predicates::prelude::*;

fn fixture_data_dir(tmp: &tempfile::TempDir, name: &str) -> PathBuf {
    tmp.path().join(name)
}

/// Writes stale daemon metadata (crashed state, lock file present but not held).
fn write_stale_crashed_metadata(data_dir: &Path) {
    let lease = StoreLease::acquire(data_dir).expect("lease acquire");
    drop(lease);
    let runtime_dir = runtime_dir_for_data_dir(data_dir);
    let metadata = serde_json::json!({
        "schema_version": 1,
        "pid": 99_999_u32,
        "address": "127.0.0.1:37383",
        "token": "test-token-stale",
        "data_dir": data_dir.to_string_lossy().as_ref(),
        "version": env!("CARGO_PKG_VERSION"),
        "started_at_unix_ms": 1_000_000_u64,
        "state": "crashed",
        "api_version": null,
        "transports": null,
        "token_expires_at_unix_ms": null,
        "daemons_index_url": null
    });
    let metadata_path = runtime_dir.join("egregored.json");
    fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .expect("write stale metadata");
}

/// Writes stopped daemon metadata (stopped state, lock file present but not held).
fn write_stopped_metadata(data_dir: &Path) {
    let lease = StoreLease::acquire(data_dir).expect("lease acquire");
    drop(lease);
    let runtime_dir = runtime_dir_for_data_dir(data_dir);
    let metadata = serde_json::json!({
        "schema_version": 1,
        "pid": 99_999_u32,
        "address": "127.0.0.1:37383",
        "token": "test-token-stopped",
        "data_dir": data_dir.to_string_lossy().as_ref(),
        "version": env!("CARGO_PKG_VERSION"),
        "started_at_unix_ms": 1_000_000_u64,
        "state": "stopped",
        "api_version": null,
        "transports": null,
        "token_expires_at_unix_ms": null,
        "daemons_index_url": null
    });
    let metadata_path = runtime_dir.join("egregored.json");
    fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .expect("write stopped metadata");
}

/// Writes crashed metadata declaring an unsupported daemon runtime schema, with
/// a lock file present but not held.
fn write_unknown_schema_metadata(data_dir: &Path) {
    let lease = StoreLease::acquire(data_dir).expect("lease acquire");
    drop(lease);
    let runtime_dir = runtime_dir_for_data_dir(data_dir);
    let metadata = serde_json::json!({
        "schema_version": 999,
        "pid": 99_999_u32,
        "address": "127.0.0.1:37383",
        "token": "test-token-future",
        "data_dir": data_dir.to_string_lossy().as_ref(),
        "version": env!("CARGO_PKG_VERSION"),
        "started_at_unix_ms": 1_000_000_u64,
        "state": "crashed",
        "api_version": null,
        "transports": null,
        "token_expires_at_unix_ms": null,
        "daemons_index_url": null
    });
    let metadata_path = runtime_dir.join("egregored.json");
    fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .expect("write unknown-schema metadata");
}

/// Writes crashed metadata WITHOUT a lock file (e.g. a copied store or partial
/// crash). No active owner can exist because no lock file is present.
fn write_stale_metadata_without_lock(data_dir: &Path) {
    let lease = StoreLease::acquire(data_dir).expect("lease acquire");
    drop(lease);
    let runtime_dir = runtime_dir_for_data_dir(data_dir);
    let lock_path = runtime_dir.join("egregored.lock");
    if lock_path.exists() {
        fs::remove_file(&lock_path).ok();
    }
    let metadata = serde_json::json!({
        "schema_version": 1,
        "pid": 99_999_u32,
        "address": "127.0.0.1:37383",
        "token": "test-token-nolock",
        "data_dir": data_dir.to_string_lossy().as_ref(),
        "version": env!("CARGO_PKG_VERSION"),
        "started_at_unix_ms": 1_000_000_u64,
        "state": "crashed",
        "api_version": null,
        "transports": null,
        "token_expires_at_unix_ms": null,
        "daemons_index_url": null
    });
    let metadata_path = runtime_dir.join("egregored.json");
    fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .expect("write stale metadata without lock");
}

// ---------------------------------------------------------------------------
// SPEC: AC 1 — live daemon refusal (unit)
// ---------------------------------------------------------------------------

#[test]
fn preflight_live_daemon_refuses_with_live_daemon_active() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    let mut daemon = start_daemon_for_test(&data_dir);

    let report = preflight(&data_dir).expect("preflight should succeed");
    assert_eq!(
        report.ownership_verdict,
        OwnershipVerdict::Live,
        "live daemon must produce Live verdict"
    );
    assert!(!report.allow, "live daemon must refuse repair");
    assert!(
        report
            .refusal_reasons
            .contains(&RepairRefusalCode::LiveDaemonActive),
        "must cite live_daemon_active refusal code, got {:?}",
        report.refusal_reasons
    );
    assert!(
        report.safe_daemon_command.is_some(),
        "must name the safe daemon command path"
    );

    daemon.stop();
}

// ---------------------------------------------------------------------------
// SPEC: AC 1 — live daemon, run_repair also refuses with zero mutations
// ---------------------------------------------------------------------------

#[test]
fn run_repair_live_daemon_refuses_zero_mutations() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    let mut daemon = start_daemon_for_test(&data_dir);

    let metadata_path = runtime_dir_for_data_dir(&data_dir).join("egregored.json");
    let before_mtime = fs::metadata(&metadata_path).and_then(|m| m.modified()).ok();

    let report = run_repair(&data_dir, false, true).expect("run_repair must return a report");
    assert_eq!(
        report.result,
        RepairSessionResult::Refused,
        "live daemon must produce Refused result"
    );
    assert!(
        report
            .refusal_reasons
            .contains(&RepairRefusalCode::LiveDaemonActive),
        "must cite live_daemon_active"
    );
    assert!(
        report.changed_file_paths.is_empty(),
        "live daemon refusal must change zero files"
    );

    // Metadata file mtime must not change
    let after_mtime = fs::metadata(&metadata_path).and_then(|m| m.modified()).ok();
    assert_eq!(before_mtime, after_mtime, "metadata mtime must not change");

    daemon.stop();
}

// ---------------------------------------------------------------------------
// SPEC: AC 2 — stopped daemon metadata — preflight allows
// ---------------------------------------------------------------------------

#[test]
fn preflight_stopped_metadata_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stopped_metadata(&data_dir);

    let report = preflight(&data_dir).expect("preflight ok");
    assert_eq!(
        report.ownership_verdict,
        OwnershipVerdict::Stopped,
        "cleanly stopped daemon metadata must report the Stopped verdict, not StaleNoOwner"
    );
    assert!(
        report.allow,
        "stopped metadata must allow repair, verdict={:?}",
        report.ownership_verdict
    );
    assert!(
        report.refusal_reasons.is_empty(),
        "stopped metadata must have no refusal reasons"
    );
}

// ---------------------------------------------------------------------------
// SPEC: AC 2 — stale metadata (crashed, no active owner) — preflight allows
// ---------------------------------------------------------------------------

#[test]
fn preflight_stale_no_owner_verdict_and_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = preflight(&data_dir).expect("preflight ok");
    assert_eq!(
        report.ownership_verdict,
        OwnershipVerdict::StaleNoOwner,
        "crashed stale metadata must be StaleNoOwner"
    );
    assert!(report.allow, "stale no-owner must allow repair");
    assert!(report.refusal_reasons.is_empty());
    assert!(
        report.before_daemon_status.is_some(),
        "must surface before_daemon_status"
    );
    // Report must include data_dir, runtime_dir, and ownership_verdict
    let report_json = serde_json::to_value(&report).unwrap();
    for field in &["data_dir", "runtime_dir", "ownership_verdict"] {
        assert!(
            report_json.get(field).is_some(),
            "preflight report must contain field {field}"
        );
    }
}

// ---------------------------------------------------------------------------
// SPEC: AC 2 — run_repair on stale metadata produces complete machine-readable report
// ---------------------------------------------------------------------------

#[test]
fn run_repair_stale_no_owner_produces_machine_readable_report() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = run_repair(&data_dir, false, true).expect("repair must succeed");
    assert_eq!(
        report.result,
        RepairSessionResult::Success,
        "stale no-owner repair must succeed"
    );
    assert!(!report.started_at.is_empty(), "must have started_at");
    assert!(!report.ended_at.is_empty(), "must have ended_at");
    assert_eq!(report.ownership_verdict, OwnershipVerdict::StaleNoOwner);

    // AC 6: report must include all required fields
    let report_json = serde_json::to_value(&report).unwrap();
    for field in &[
        "data_dir",
        "runtime_dir",
        "ownership_verdict",
        "started_at",
        "ended_at",
        "result",
        "before_daemon_status",
        "attempted_actions",
        "skipped_actions",
        "changed_file_paths",
        "refusal_reasons",
    ] {
        assert!(
            report_json.get(field).is_some(),
            "report must contain field {field}"
        );
    }

    // StaleMetadataCleanup must have been attempted
    let attempted = report_json["attempted_actions"]
        .as_array()
        .expect("attempted_actions must be array");
    assert!(
        attempted
            .iter()
            .any(|a| a.as_str() == Some("stale_metadata_cleanup")),
        "stale_metadata_cleanup must be in attempted_actions"
    );
    // RecoveryReportGeneration must have been attempted
    assert!(
        attempted
            .iter()
            .any(|a| a.as_str() == Some("recovery_report_generation")),
        "recovery_report_generation must be in attempted_actions"
    );
}

// ---------------------------------------------------------------------------
// SPEC: AC 3 — no metadata at all → stopped verdict, allows repair
// ---------------------------------------------------------------------------

#[test]
fn preflight_no_metadata_returns_stopped_and_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();

    let report = preflight(&data_dir).expect("preflight ok");
    assert_eq!(report.ownership_verdict, OwnershipVerdict::Stopped);
    assert!(report.allow);
    assert!(report.refusal_reasons.is_empty());
}

// ---------------------------------------------------------------------------
// SPEC: AC 4 — dry-run proves zero mutations while returning correct verdict
// ---------------------------------------------------------------------------

#[test]
fn dry_run_returns_correct_verdict_with_zero_mutations() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let metadata_path = runtime_dir_for_data_dir(&data_dir).join("egregored.json");
    assert!(metadata_path.exists(), "metadata must exist before dry-run");

    let report = run_repair(&data_dir, true, false).expect("dry-run must succeed");
    assert_eq!(
        report.result,
        RepairSessionResult::DryRun,
        "dry-run must return DryRun result"
    );
    // Verdict must match what real run would use
    assert_eq!(
        report.ownership_verdict,
        OwnershipVerdict::StaleNoOwner,
        "dry-run must return same verdict as real run"
    );
    // Zero mutations: metadata file must still exist
    assert!(
        metadata_path.exists(),
        "dry-run must not remove metadata file"
    );
    assert!(
        report.changed_file_paths.is_empty(),
        "dry-run must report zero changed files"
    );
}

#[test]
fn dry_run_live_daemon_still_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    let mut daemon = start_daemon_for_test(&data_dir);

    let report = run_repair(&data_dir, true, false).expect("run_repair must return report");
    assert_eq!(
        report.result,
        RepairSessionResult::Refused,
        "dry-run with live daemon must still refuse"
    );
    assert!(
        report
            .refusal_reasons
            .contains(&RepairRefusalCode::LiveDaemonActive)
    );
    assert!(report.changed_file_paths.is_empty());

    daemon.stop();
}

// ---------------------------------------------------------------------------
// SPEC: AC 4 — confirmation required without --confirm or --dry-run
// ---------------------------------------------------------------------------

#[test]
fn run_repair_without_confirm_requires_confirmation() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = run_repair(&data_dir, false, false).expect("must return report");
    assert_eq!(
        report.result,
        RepairSessionResult::Refused,
        "without --confirm must refuse"
    );
    assert!(
        report
            .refusal_reasons
            .contains(&RepairRefusalCode::ConfirmationRequired),
        "must cite confirmation_required"
    );
    // Metadata must not have been removed
    assert!(
        runtime_dir_for_data_dir(&data_dir)
            .join("egregored.json")
            .exists(),
        "metadata must be untouched without confirm"
    );
}

// ---------------------------------------------------------------------------
// SPEC: AC 5 — only supported operations appear in attempted_actions
// ---------------------------------------------------------------------------

#[test]
fn repair_only_attempts_supported_operations() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = run_repair(&data_dir, false, true).expect("repair ok");
    let report_json = serde_json::to_value(&report).unwrap();
    let attempted = report_json["attempted_actions"]
        .as_array()
        .expect("attempted_actions array");

    let supported = ["stale_metadata_cleanup", "recovery_report_generation"];
    for action in attempted {
        let action_str = action.as_str().expect("action must be string");
        assert!(
            supported.contains(&action_str),
            "action {action_str:?} is not a supported repair operation"
        );
    }
}

// ---------------------------------------------------------------------------
// SPEC: AC 6 — repair output includes redaction-safe report, no invented facts
// ---------------------------------------------------------------------------

#[test]
fn repair_report_does_not_invent_source_facts_or_agent_observations() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = run_repair(&data_dir, false, true).expect("repair ok");
    let report_json = serde_json::to_value(&report).unwrap();

    // Trust boundary: no invented source facts or agent observations
    for disallowed in &[
        "observations",
        "source_code",
        "tasks_completed",
        "agent_memory",
    ] {
        assert!(
            report_json.get(disallowed).is_none(),
            "report must not carry {disallowed}"
        );
    }
}

#[test]
fn repair_report_contains_stable_refusal_codes_as_strings() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = run_repair(&data_dir, false, false).expect("report ok");
    let report_json = serde_json::to_value(&report).unwrap();
    let refusals = report_json["refusal_reasons"]
        .as_array()
        .expect("refusal_reasons must be array");
    for r in refusals {
        assert!(
            r.is_string(),
            "each refusal reason must be a stable string code, got: {r:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// SPEC: AC 7 — successful repair leaves daemon start working
// ---------------------------------------------------------------------------

#[test]
fn successful_repair_leaves_daemon_start_working() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = run_repair(&data_dir, false, true).expect("repair must succeed");
    assert_eq!(report.result, RepairSessionResult::Success);

    // After repair: stale metadata must be removed
    let metadata_path = runtime_dir_for_data_dir(&data_dir).join("egregored.json");
    assert!(
        !metadata_path.exists(),
        "successful repair must remove stale metadata"
    );

    // After repair: daemon start must succeed in 100% of fixture runs
    // (use CLI binary — start_background calls current_exe() which is the test binary in tests)
    let status = ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"))
        .arg("daemon")
        .arg("start")
        .arg("--data-dir")
        .arg(&data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to execute daemon start");
    assert!(status.success(), "daemon start failed");

    // Verify the daemon is actually running
    assert!(
        aletheia_egregore::daemon::active_metadata(&data_dir)
            .unwrap_or(None)
            .is_some(),
        "daemon must be active after repair + start"
    );

    // Cleanup
    ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"))
        .arg("daemon")
        .arg("stop")
        .arg("--data-dir")
        .arg(&data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok();
    wait_until_stopped(&data_dir);
}

// ---------------------------------------------------------------------------
// SPEC (issue #72 AC "healthy stopped store"): a healthy stopped store must
// report "no repair needed" and MUST NOT create, modify, or delete any runtime
// file — not even under --confirm. This is the AC8 regression: the prior impl
// ran stale_metadata_cleanup for the Stopped verdict too, deleting a healthy
// stopped store's egregored.json.
//
// RED: written against the existing run_repair API. With the prior impl this
// FAILS at runtime because the confirmed path removes egregored.json for a
// Stopped verdict.
// ---------------------------------------------------------------------------

#[test]
fn confirmed_repair_on_healthy_stopped_store_does_not_mutate() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    // Healthy stopped store: metadata with state `stopped`, lock present but not
    // held (no active owner).
    write_stopped_metadata(&data_dir);

    let metadata_path = runtime_dir_for_data_dir(&data_dir).join("egregored.json");
    let before_bytes = fs::read(&metadata_path).expect("stopped metadata must exist");

    let report = run_repair(&data_dir, false, true).expect("run_repair must return a report");

    // The healthy stopped store must be left byte-for-byte untouched.
    assert_eq!(
        report.result,
        RepairSessionResult::NoRepairNeeded,
        "healthy stopped store must report no_repair_needed, not a mutating success"
    );
    assert!(
        metadata_path.exists(),
        "confirmed repair must NOT delete a healthy stopped store's metadata"
    );
    let after_bytes = fs::read(&metadata_path).expect("metadata must still exist");
    assert_eq!(
        before_bytes, after_bytes,
        "confirmed repair must not modify a healthy stopped store's metadata"
    );
    assert!(
        report.changed_file_paths.is_empty(),
        "healthy stopped store repair must change zero files, got {:?}",
        report.changed_file_paths
    );
    assert!(
        report.manifest.is_empty(),
        "healthy stopped store repair must record no manifest actions"
    );
    // No recovery report or manifest may be created for a healthy stopped store.
    let runtime_dir = runtime_dir_for_data_dir(&data_dir);
    assert!(
        !runtime_dir.join("repair-report.json").exists(),
        "healthy stopped store repair must not write a recovery report"
    );
    assert!(
        !runtime_dir.join("repair-manifest.json").exists(),
        "healthy stopped store repair must not write a repair manifest"
    );
}

#[test]
fn dry_run_on_healthy_stopped_store_reports_no_repair_needed() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stopped_metadata(&data_dir);

    let metadata_path = runtime_dir_for_data_dir(&data_dir).join("egregored.json");
    let before = fs::read(&metadata_path).unwrap();

    let report = run_repair(&data_dir, true, false).expect("dry-run must return a report");
    assert_eq!(
        report.result,
        RepairSessionResult::NoRepairNeeded,
        "dry-run on a healthy stopped store must report no_repair_needed"
    );
    assert_eq!(
        fs::read(&metadata_path).unwrap(),
        before,
        "dry-run on a healthy stopped store must not touch metadata"
    );
    // The healthy stopped store re-verifies clean.
    assert_eq!(
        report.after_ownership_verdict,
        Some(OwnershipVerdict::Stopped)
    );
}

#[test]
fn preflight_stopped_store_reports_no_repair_needed() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stopped_metadata(&data_dir);

    let report = preflight(&data_dir).expect("preflight ok");
    assert_eq!(report.ownership_verdict, OwnershipVerdict::Stopped);
    assert!(report.allow, "stopped store is allowed (not refused)");
    assert!(
        !report.repair_needed,
        "a healthy stopped store needs no repair"
    );
}

#[test]
fn preflight_stale_store_reports_repair_needed() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = preflight(&data_dir).expect("preflight ok");
    assert_eq!(report.ownership_verdict, OwnershipVerdict::StaleNoOwner);
    assert!(report.allow);
    assert!(
        report.repair_needed,
        "stale metadata with no owner needs repair"
    );
}

// ---------------------------------------------------------------------------
// SPEC (issue #72 AC "quarantining or removing"): apply mode supports moving
// stale metadata ASIDE (recoverable) instead of deleting it, and writes a
// redaction-safe manifest with before/after hashes.
// ---------------------------------------------------------------------------

#[test]
fn quarantine_apply_moves_metadata_aside_and_is_recoverable() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    // Stale, crashed metadata with no active owner — the one repairable case.
    write_stale_crashed_metadata(&data_dir);

    let runtime_dir = runtime_dir_for_data_dir(&data_dir);
    let metadata_path = runtime_dir.join("egregored.json");
    let original_bytes = fs::read(&metadata_path).unwrap();

    let report = run_repair_with(
        &data_dir,
        &RepairOptions {
            confirm: true,
            quarantine: true,
            ..RepairOptions::default()
        },
    )
    .expect("quarantine repair must succeed");

    assert_eq!(report.result, RepairSessionResult::Success);
    // Original location must be cleared.
    assert!(
        !metadata_path.exists(),
        "quarantine must move the metadata out of its original path"
    );

    // The manifest must record a quarantine action with a recoverable dest and
    // content-preserving before==after hashes.
    let entry = report
        .manifest
        .iter()
        .find(|e| e.action == RepairAction::StaleMetadataQuarantine)
        .expect("manifest must carry a quarantine entry");
    assert_eq!(entry.result, RepairActionResult::Applied);
    assert_eq!(
        entry.before_metadata_hash, entry.after_metadata_hash,
        "a quarantine move preserves content, so before/after hashes must match"
    );
    let dest = entry
        .quarantine_path
        .clone()
        .expect("quarantine entry must carry the recoverable destination path");

    // The moved bytes must be byte-for-byte recoverable at the destination.
    assert!(dest.exists(), "quarantined file must exist at the dest");
    assert_eq!(
        fs::read(&dest).unwrap(),
        original_bytes,
        "quarantine must preserve the metadata bytes exactly (recoverable)"
    );
    // The store re-verifies clean.
    assert_eq!(
        report.after_ownership_verdict,
        Some(OwnershipVerdict::Stopped)
    );
}

#[test]
fn apply_writes_repair_manifest_with_required_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = run_repair_with(
        &data_dir,
        &RepairOptions {
            confirm: true,
            ..RepairOptions::default()
        },
    )
    .expect("repair ok");
    assert_eq!(report.result, RepairSessionResult::Success);

    // The manifest file must exist on disk.
    let manifest_path = runtime_dir_for_data_dir(&data_dir).join("repair-manifest.json");
    assert!(
        manifest_path.exists(),
        "confirmed apply must write repair-manifest.json"
    );

    // Each manifest entry must carry every AC-required field.
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let entries = manifest.as_array().expect("manifest is a JSON array");
    assert!(
        !entries.is_empty(),
        "manifest must record at least one action"
    );
    let cleanup = entries
        .iter()
        .find(|e| e["action"] == "stale_metadata_cleanup")
        .expect("removal manifest entry must exist");
    for field in &["action", "action_time", "result", "original_path"] {
        assert!(
            cleanup.get(field).is_some(),
            "manifest entry must carry field {field}"
        );
    }
    assert_eq!(cleanup["result"], "applied");
    // Removal records a before-hash but no after-hash (the file is gone).
    assert!(
        cleanup["before_metadata_hash"].is_string(),
        "removal must record the before hash"
    );
    assert!(
        cleanup.get("after_metadata_hash").is_none(),
        "removal leaves no file, so after_metadata_hash is omitted"
    );
}

#[test]
fn dry_run_manifest_preview_is_planned_and_no_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = run_repair(&data_dir, true, false).expect("dry-run ok");
    assert_eq!(report.result, RepairSessionResult::DryRun);
    assert!(
        !report.manifest.is_empty(),
        "dry-run must preview a manifest"
    );
    for entry in &report.manifest {
        assert_eq!(
            entry.result,
            RepairActionResult::Planned,
            "dry-run manifest rows must be planned, not applied"
        );
    }
    // No manifest file may be written in dry-run.
    assert!(
        !runtime_dir_for_data_dir(&data_dir)
            .join("repair-manifest.json")
            .exists(),
        "dry-run must not write a manifest file"
    );
}

// ---------------------------------------------------------------------------
// SPEC (issue #72 AC "5x determinism"): repeating the same dry-run fixture
// returns byte-identical status, ordering, diagnostics, and manifest preview.
// The action time is pinned so wall-clock stamps cannot break byte-identity.
// ---------------------------------------------------------------------------

#[test]
fn dry_run_is_byte_identical_across_five_cli_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let mut outputs: Vec<Vec<u8>> = Vec::new();
    for _ in 0..5 {
        let out = Command::cargo_bin("egregore")
            .unwrap()
            .arg("repair")
            .arg("run")
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("--dry-run")
            .arg("--transaction-time")
            .arg("2026-01-02T03:04:05Z")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        outputs.push(out);
    }
    for (i, out) in outputs.iter().enumerate().skip(1) {
        assert_eq!(
            &outputs[0], out,
            "dry-run stdout must be byte-identical across runs (run {i} differed)"
        );
    }
    // The pinned time must actually appear (proves timestamps are deterministic).
    let text = String::from_utf8(outputs[0].clone()).unwrap();
    assert!(
        text.contains("2026-01-02T03:04:05Z"),
        "pinned transaction time must drive the deterministic output"
    );
}

// ---------------------------------------------------------------------------
// SPEC (issue #72 AC "text and JSON output"): --format text renders the same
// facts human-readably; default stays JSON.
// ---------------------------------------------------------------------------

#[test]
fn preflight_format_text_renders_verdict() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    Command::cargo_bin("egregore")
        .unwrap()
        .arg("repair")
        .arg("preflight")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "ownership_verdict: stale_no_owner",
        ))
        .stdout(predicate::str::contains("repair_needed: true"));
}

#[test]
fn run_format_text_default_stays_json() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    // No --format: default must be JSON (parseable object).
    let output = Command::cargo_bin("egregore")
        .unwrap()
        .arg("repair")
        .arg("run")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--dry-run")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_slice(&output).expect("default output is JSON");
    assert_eq!(parsed["result"], "dry_run");

    // --format text: human-readable, not JSON.
    Command::cargo_bin("egregore")
        .unwrap()
        .arg("repair")
        .arg("run")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--dry-run")
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .stdout(predicate::str::contains("repair run"))
        .stdout(predicate::str::contains("result: dry_run"));
}

// ---------------------------------------------------------------------------
// SPEC (issue #72 AC "before/after re-verification"): after a mutating apply the
// detection pass is re-run and the report states the post-repair state honestly.
// ---------------------------------------------------------------------------

#[test]
fn confirmed_repair_reverifies_store_is_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = run_repair(&data_dir, false, true).expect("repair ok");
    assert_eq!(report.result, RepairSessionResult::Success);
    // Re-verification: the store is now a clean Stopped store.
    assert_eq!(
        report.after_ownership_verdict,
        Some(OwnershipVerdict::Stopped),
        "after a successful repair the store must re-verify as Stopped (clean)"
    );
    let after = report
        .after_inspect_summary
        .expect("success carries an after inspect summary");
    assert!(!after.metadata_exists, "stale metadata must be gone");
    assert!(!after.lock_held, "no external owner remains");
}

// ---------------------------------------------------------------------------
// SPEC (issue #72 AC "no bearer tokens"): repair output and the on-disk manifest
// must never leak the daemon token from the runtime metadata.
// ---------------------------------------------------------------------------

#[test]
fn repair_output_and_manifest_never_leak_token() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    // This fixture writes a token of "test-token-stale".
    write_stale_crashed_metadata(&data_dir);
    let token = "test-token-stale";

    let report = run_repair_with(
        &data_dir,
        &RepairOptions {
            confirm: true,
            ..RepairOptions::default()
        },
    )
    .expect("repair ok");

    let report_json = serde_json::to_string(&report).unwrap();
    assert!(
        !report_json.contains(token),
        "repair report must never carry the bearer token"
    );

    let manifest_bytes =
        fs::read(runtime_dir_for_data_dir(&data_dir).join("repair-manifest.json")).unwrap();
    assert!(
        !String::from_utf8_lossy(&manifest_bytes).contains(token),
        "repair manifest must never carry the bearer token"
    );
    let recovery_bytes =
        fs::read(runtime_dir_for_data_dir(&data_dir).join("repair-report.json")).unwrap();
    assert!(
        !String::from_utf8_lossy(&recovery_bytes).contains(token),
        "recovery report must never carry the bearer token"
    );
}

// ---------------------------------------------------------------------------
// SPEC: AC 8 — embedded ingest blocked when stale crashed metadata exists
// ---------------------------------------------------------------------------

#[test]
fn embedded_open_blocked_by_stale_crashed_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    assert!(
        runtime_metadata_is_stale(&data_dir).unwrap(),
        "metadata must be stale for this test"
    );

    let result = EmbeddedAletheiaSink::open(&data_dir);
    assert!(
        result.is_err(),
        "embedded open must fail when stale crashed metadata exists"
    );
    let err_msg = result.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        err_msg.contains("repair") || err_msg.contains("stale"),
        "error must mention repair or stale: {err_msg}"
    );
}

#[test]
fn embedded_open_not_blocked_after_repair_removes_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    run_repair(&data_dir, false, true).expect("repair ok");

    let result = EmbeddedAletheiaSink::open(&data_dir);
    assert!(
        result.is_ok(),
        "embedded open must succeed after repair clears stale metadata: {:?}",
        result.err().map(|e| e.to_string())
    );
}

#[test]
fn embedded_open_not_blocked_by_stopped_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stopped_metadata(&data_dir);

    let result = EmbeddedAletheiaSink::open(&data_dir);
    assert!(
        result.is_ok(),
        "stopped metadata must not block embedded open: {:?}",
        result.err().map(|e| e.to_string())
    );
}

// ---------------------------------------------------------------------------
// SPEC: CLI — repair preflight command
// ---------------------------------------------------------------------------

#[test]
fn cli_repair_preflight_no_metadata_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();

    Command::cargo_bin("egregore")
        .unwrap()
        .arg("repair")
        .arg("preflight")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("ownership_verdict"));
}

#[test]
fn cli_repair_preflight_stale_metadata_returns_stale_no_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let output = Command::cargo_bin("egregore")
        .unwrap()
        .arg("repair")
        .arg("preflight")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let report: serde_json::Value =
        serde_json::from_slice(&output).expect("preflight output must be valid JSON");
    assert_eq!(report["ownership_verdict"], "stale_no_owner");
    assert_eq!(report["allow"], true);
}

#[test]
fn cli_repair_preflight_live_daemon_returns_live_verdict() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    let mut daemon = start_daemon_for_test(&data_dir);

    let output = Command::cargo_bin("egregore")
        .unwrap()
        .arg("repair")
        .arg("preflight")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let report: serde_json::Value =
        serde_json::from_slice(&output).expect("preflight output must be valid JSON");
    assert_eq!(report["ownership_verdict"], "live");
    assert_eq!(report["allow"], false);
    let refusals = report["refusal_reasons"].as_array().unwrap();
    assert!(
        refusals
            .iter()
            .any(|r| r.as_str() == Some("live_daemon_active")),
        "must cite live_daemon_active"
    );
    assert!(
        report["safe_daemon_command"].as_str().is_some(),
        "must name safe daemon command"
    );

    daemon.stop();
}

// ---------------------------------------------------------------------------
// SPEC: CLI — repair run command
// ---------------------------------------------------------------------------

#[test]
fn cli_repair_run_without_confirm_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let output = Command::cargo_bin("egregore")
        .unwrap()
        .arg("repair")
        .arg("run")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["result"], "refused");
    let refusals = report["refusal_reasons"].as_array().unwrap();
    assert!(
        refusals
            .iter()
            .any(|r| r.as_str() == Some("confirmation_required")),
        "must cite confirmation_required"
    );
}

#[test]
fn cli_repair_run_dry_run_zero_mutations() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let metadata_path = runtime_dir_for_data_dir(&data_dir).join("egregored.json");
    assert!(metadata_path.exists());

    let output = Command::cargo_bin("egregore")
        .unwrap()
        .arg("repair")
        .arg("run")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--dry-run")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["result"], "dry_run");
    assert!(metadata_path.exists(), "dry-run must not modify metadata");
    let changed = report["changed_file_paths"].as_array().unwrap();
    assert!(changed.is_empty(), "dry-run must report zero changed files");
}

#[test]
fn cli_repair_run_confirm_removes_stale_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let metadata_path = runtime_dir_for_data_dir(&data_dir).join("egregored.json");
    assert!(metadata_path.exists());

    let output = Command::cargo_bin("egregore")
        .unwrap()
        .arg("repair")
        .arg("run")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--confirm")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["result"], "success");
    assert!(
        !metadata_path.exists(),
        "confirmed repair must remove stale metadata"
    );
    // changed_file_paths must include the removed metadata path
    let changed = report["changed_file_paths"].as_array().unwrap();
    assert!(
        !changed.is_empty(),
        "confirmed repair must report changed files"
    );
}

// ---------------------------------------------------------------------------
// SPEC: AC 9 — trust boundary: preflight is idempotent, no invented facts
// ---------------------------------------------------------------------------

#[test]
fn preflight_is_idempotent_across_multiple_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report1 = preflight(&data_dir).unwrap();
    let report2 = preflight(&data_dir).unwrap();
    assert_eq!(
        report1.ownership_verdict, report2.ownership_verdict,
        "preflight must be idempotent"
    );
    assert_eq!(report1.allow, report2.allow);
    // Metadata must still exist (preflight is zero-mutation)
    assert!(
        runtime_dir_for_data_dir(&data_dir)
            .join("egregored.json")
            .exists(),
        "preflight must not modify metadata"
    );
}

// ---------------------------------------------------------------------------
// Review fix: refuse unknown daemon runtime schema (do not delete newer state)
// ---------------------------------------------------------------------------

#[test]
fn preflight_unknown_schema_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_unknown_schema_metadata(&data_dir);

    let report = preflight(&data_dir).expect("preflight ok");
    assert!(
        !report.allow,
        "unknown daemon schema must refuse repair, verdict={:?}",
        report.ownership_verdict
    );
    assert!(
        report
            .refusal_reasons
            .contains(&RepairRefusalCode::UnsupportedRuntimeSchema),
        "must cite unsupported_runtime_schema, got {:?}",
        report.refusal_reasons
    );
}

#[test]
fn run_repair_unknown_schema_refuses_and_preserves_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_unknown_schema_metadata(&data_dir);

    let report = run_repair(&data_dir, false, true).expect("must return report");
    assert_eq!(
        report.result,
        RepairSessionResult::Refused,
        "unknown schema must refuse even with --confirm"
    );
    assert!(
        report
            .refusal_reasons
            .contains(&RepairRefusalCode::UnsupportedRuntimeSchema)
    );
    assert!(
        report.changed_file_paths.is_empty(),
        "unknown-schema refusal must change zero files"
    );
    assert!(
        runtime_dir_for_data_dir(&data_dir)
            .join("egregored.json")
            .exists(),
        "newer-schema metadata must not be deleted by repair"
    );
}

// ---------------------------------------------------------------------------
// Review fix: preflight/dry-run must not create the lock file (zero mutation)
// ---------------------------------------------------------------------------

#[test]
fn preflight_does_not_create_lock_file() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_metadata_without_lock(&data_dir);

    let lock_path = runtime_dir_for_data_dir(&data_dir).join("egregored.lock");
    assert!(!lock_path.exists(), "precondition: no lock file");

    let report = preflight(&data_dir).expect("preflight ok");
    assert_eq!(report.ownership_verdict, OwnershipVerdict::StaleNoOwner);
    assert!(report.allow);
    assert!(
        !lock_path.exists(),
        "zero-mutation preflight must not create the lock file"
    );

    // Dry-run must likewise not create the lock file.
    let dry = run_repair(&data_dir, true, false).expect("dry-run ok");
    assert_eq!(dry.result, RepairSessionResult::DryRun);
    assert!(
        !lock_path.exists(),
        "zero-mutation dry-run must not create the lock file"
    );
}

// ---------------------------------------------------------------------------
// Review fix: confirmed repair writes the recovery report and reports lock state
// ---------------------------------------------------------------------------

#[test]
fn confirmed_repair_writes_recovery_report_and_reports_unlocked() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let report = run_repair(&data_dir, false, true).expect("repair ok");
    assert_eq!(report.result, RepairSessionResult::Success);

    // The recovery report file the CLI/docs promise must actually exist.
    let report_path = runtime_dir_for_data_dir(&data_dir).join("repair-report.json");
    assert!(
        report_path.exists(),
        "confirmed repair must write the recovery report to the runtime dir"
    );

    // After cleanup there is no external owner: lock_held must be false.
    let after = report
        .after_inspect_summary
        .expect("success must carry after_inspect_summary");
    assert!(
        !after.lock_held,
        "post-cleanup inspect summary must report lock_held=false, not a false active-lock signal"
    );
    assert!(
        !after.metadata_exists,
        "metadata must be gone after cleanup"
    );
}

// ---------------------------------------------------------------------------
// Review fix: embedding ingest path is also gated by stale daemon metadata
// ---------------------------------------------------------------------------

#[cfg(feature = "embeddings")]
#[test]
fn open_with_embeddings_blocked_by_stale_crashed_metadata() {
    use aletheia_egregore::embeddings::EmbeddingVectorMap;

    let tmp = tempfile::tempdir().unwrap();
    let data_dir = fixture_data_dir(&tmp, "store");
    fs::create_dir_all(&data_dir).unwrap();
    write_stale_crashed_metadata(&data_dir);

    let result =
        EmbeddedAletheiaSink::open_with_embeddings(&data_dir, EmbeddingVectorMap::new(), 1);
    assert!(
        result.is_err(),
        "embedded --embed open must also be blocked by stale crashed metadata"
    );
    let err_msg = result.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        err_msg.contains("repair") || err_msg.contains("stale"),
        "error must mention repair or stale: {err_msg}"
    );

    // After repair, the embedding open path is unblocked.
    run_repair(&data_dir, false, true).expect("repair ok");
    let ok = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, EmbeddingVectorMap::new(), 1);
    assert!(
        ok.is_ok(),
        "embedding open must succeed after repair clears stale metadata: {:?}",
        ok.err().map(|e| e.to_string())
    );
}

// ---------------------------------------------------------------------------
// Daemon test lifecycle helpers
// ---------------------------------------------------------------------------

struct RunningDaemon {
    child: Option<std::process::Child>,
    data_dir: PathBuf,
}

impl RunningDaemon {
    fn stop(&mut self) {
        ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"))
            .arg("daemon")
            .arg("stop")
            .arg("--data-dir")
            .arg(&self.data_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
        wait_until_stopped(&self.data_dir);
        if let Some(mut child) = self.child.take() {
            let start = std::time::Instant::now();
            let mut exited = false;
            while start.elapsed() < std::time::Duration::from_secs(2) {
                if let Ok(Some(_)) = child.try_wait() {
                    exited = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if !exited {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.stop();
        }
    }
}

fn start_daemon_for_test(data_dir: &Path) -> RunningDaemon {
    fs::create_dir_all(data_dir).unwrap();
    let child = ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"))
        .arg("daemon")
        .arg("run")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg("0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon must spawn");

    // Wait until daemon is healthy
    let start = Instant::now();
    loop {
        if aletheia_egregore::daemon::active_metadata(data_dir)
            .unwrap_or(None)
            .is_some()
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon did not start within timeout"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    RunningDaemon {
        child: Some(child),
        data_dir: data_dir.to_path_buf(),
    }
}

fn wait_until_stopped(data_dir: &Path) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if aletheia_egregore::daemon::active_metadata(data_dir)
            .unwrap_or(None)
            .is_none()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
