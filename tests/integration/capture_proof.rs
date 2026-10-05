//! SPEC-PROOF-RED-GREEN tests for `eg capture-proof` (issue #69).
//!
//! Capturing a local Verus proof run as citable, deterministic
//! verification-domain records: one `ProofResult` node (the normalized proof
//! claim) plus one `CommandRun` node (the captured verifier command whose exit
//! status and output support the claim, linked from the proof via an
//! `evidence_links` `HAS_EVIDENCE` citation).
//!
//! The fixtures are a stub `verus` shell script plus seeded Rust/Verus proof
//! files — the real Verus is never installed and nothing here touches the
//! network. A missing or non-executable verifier produces a stable diagnostic,
//! never a passing proof record.

#![allow(missing_docs)]

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command;
use serde_json::Value;

use aletheia_egregore::ir::{
    EdgeLabel, GraphRecord, NodeKind, PROJECT_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION,
    project_stable_id, verification_stable_id,
};
use aletheia_egregore::proof_capture::{
    CaptureConfig, MALFORMED_VERUS_OUTPUT_CODE, PROOF_TARGET_AMBIGUOUS_CODE,
    PROOF_TARGET_MISSING_CODE, PROOF_TARGET_STALE_CODE, ProofStatus, ProofTargetError,
    UNRECOGNIZED_VERUS_OUTPUT_CODE, UNSUPPORTED_VERIFIER_VERSION_CODE,
    VERIFIER_BINARY_MISSING_CODE, VERIFIER_BINARY_NOT_EXECUTABLE_CODE,
    VERUS_EXIT_CODE_MISMATCH_CODE, VerifierBinaryError, VerusParse, VerusRun, VerusSummary,
    capture_proof, classify_proof_run, parse_verus_output, pre_execution_diagnostic,
    probe_verifier_version, resolve_proof_target, validate_verifier_binary,
};
use aletheia_egregore::query::{
    NotReadyReason, TaskEvidenceGateOutcome, TrustClass, TrustIndex, task_evidence_gate_report,
};
use aletheia_egregore::redaction::parse_redaction_markers;

// ── Fixtures ────────────────────────────────────────────────────────────────

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/capture_proof")
}

fn fixture(name: &str) -> PathBuf {
    fixtures().join(name)
}

/// A tempdir-seeded copy of the fixture tree: the CLI/tests must never mutate
/// the checked-in fixtures (e.g. chmod experiments).
fn seeded_workdir() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    for entry in fs::read_dir(fixtures()).expect("fixtures should list") {
        let entry = entry.expect("fixture entry");
        let dest = temp.path().join(entry.file_name());
        fs::copy(entry.path(), &dest).expect("fixture should copy");
    }
    // Preserve the stub executability that `fs::copy` keeps on unix anyway;
    // belt-and-braces for filesystems that drop modes.
    for name in ["verus-stub", "verus-no-version"] {
        let p = temp.path().join(name);
        let mut perms = fs::metadata(&p).expect("stub should exist").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&p, perms).expect("chmod should work");
    }
    temp
}

const SESSION: &str = "sess-69";
const COMMIT: &str = "deadbeef12345678";
const EXECUTED_AT: &str = "2026-09-30T12:00:00Z";

fn capture_config(workdir: &Path, target_name: &str, stub_name: &str) -> CaptureConfig {
    let target = workdir.join(target_name);
    CaptureConfig {
        verus_bin: workdir.join(stub_name),
        verus_args: Vec::new(),
        target: target.clone(),
        target_display: target.to_string_lossy().into_owned(),
        session_id: SESSION.to_owned(),
        commit: COMMIT.to_owned(),
        executed_at: EXECUTED_AT.to_owned(),
        verifier_name: "verus".to_owned(),
        verifier_version_override: None,
        repo: Some("egregore-fixture".to_owned()),
        timeout: Duration::from_secs(30),
        probe_timeout: Duration::from_secs(10),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn proof_node(records: &[GraphRecord]) -> &GraphRecord {
    records
        .iter()
        .find(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::ProofResult,
                    ..
                }
            )
        })
        .expect("a ProofResult node should be emitted")
}

fn command_node(records: &[GraphRecord]) -> &GraphRecord {
    records
        .iter()
        .find(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::CommandRun,
                    ..
                }
            )
        })
        .expect("a CommandRun node should be emitted")
}

fn node_status(record: &GraphRecord) -> Option<String> {
    match record {
        GraphRecord::Node { status, .. } => status.clone(),
        _ => None,
    }
}

/// The normalized proof-summary JSON out of the `ProofResult`'s `stdout_handle`.
fn normalized_payload(records: &[GraphRecord]) -> Value {
    let node = proof_node(records);
    let inline = match node {
        GraphRecord::Node { stdout_handle, .. } => stdout_handle
            .as_ref()
            .expect("ProofResult should carry a stdout_handle")
            .inline
            .clone()
            .expect("normalized summary should be inline (well under the ceiling)"),
        _ => unreachable!(),
    };
    serde_json::from_str(&inline).expect("normalized summary should be valid JSON")
}

fn diagnostic_code(record: &GraphRecord) -> Option<String> {
    match record {
        GraphRecord::Node { symbol_kind, .. } => symbol_kind.clone(),
        _ => None,
    }
}

fn diagnostics(records: &[GraphRecord]) -> Vec<&GraphRecord> {
    records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::Diagnostic,
                    ..
                }
            )
        })
        .collect()
}

fn verification_link_id_of(record: &GraphRecord) -> Option<String> {
    match record {
        GraphRecord::Node {
            verification_link_id,
            ..
        } => verification_link_id.clone(),
        _ => None,
    }
}

fn evidence_links_of(record: &GraphRecord) -> Vec<aletheia_egregore::ir::EvidenceLink> {
    match record {
        GraphRecord::Node { evidence_links, .. } => evidence_links.clone().unwrap_or_default(),
        _ => Vec::new(),
    }
}

// ── 1. Proof-target resolution ──────────────────────────────────────────────

#[test]
fn missing_proof_target_yields_stable_diagnostic_code() {
    let err = resolve_proof_target(Path::new("/nonexistent/proof.rs"), "proof.rs")
        .expect_err("missing target should fail");
    assert!(matches!(err, ProofTargetError::Missing));
    assert_eq!(err.code(), PROOF_TARGET_MISSING_CODE);
}

#[test]
fn directory_proof_target_is_ambiguous() {
    let temp = tempfile::tempdir().expect("temp dir");
    let err = resolve_proof_target(temp.path(), "dir").expect_err("dir target should fail");
    assert!(matches!(err, ProofTargetError::Ambiguous));
    assert_eq!(err.code(), PROOF_TARGET_AMBIGUOUS_CODE);
}

#[test]
fn dangling_symlink_proof_target_is_stale() {
    let temp = tempfile::tempdir().expect("temp dir");
    let link = temp.path().join("stale.rs");
    std::os::unix::fs::symlink(temp.path().join("gone.rs"), &link).expect("symlink");
    let err = resolve_proof_target(&link, "stale.rs").expect_err("dangling link should fail");
    assert!(matches!(err, ProofTargetError::Stale));
    assert_eq!(err.code(), PROOF_TARGET_STALE_CODE);
}

#[test]
fn existing_proof_target_resolves_with_hash() {
    let target = resolve_proof_target(&fixture("proof_pass.rs"), "proof_pass.rs")
        .expect("fixture target should resolve");
    let raw = fs::read(fixture("proof_pass.rs")).expect("fixture should read");
    assert_eq!(target.bytes, raw);
    assert_eq!(target.hash, blake3::hash(&raw).to_hex().to_string());
}

// ── 2. Verifier binary validation ───────────────────────────────────────────

#[test]
fn missing_verifier_binary_is_a_stable_diagnostic() {
    let err = validate_verifier_binary(Path::new("/nonexistent/verus"))
        .expect_err("missing binary should fail");
    assert!(matches!(err, VerifierBinaryError::Missing));
    assert_eq!(err.code(), VERIFIER_BINARY_MISSING_CODE);
}

#[test]
fn nonexecutable_verifier_binary_is_refused() {
    let workdir = seeded_workdir();
    let candidate = workdir.path().join("verus-nonexec");
    let mut perms = fs::metadata(&candidate)
        .expect("fixture copy")
        .permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&candidate, perms).expect("chmod");
    let err = validate_verifier_binary(&candidate).expect_err("non-executable should fail");
    assert!(matches!(err, VerifierBinaryError::NotExecutable));
    assert_eq!(err.code(), VERIFIER_BINARY_NOT_EXECUTABLE_CODE);
}

#[test]
fn executable_stub_passes_validation() {
    let workdir = seeded_workdir();
    validate_verifier_binary(&workdir.path().join("verus-stub"))
        .expect("executable stub should validate");
}

// ── 3. Version probe ────────────────────────────────────────────────────────

#[test]
fn version_probe_reads_stub_version() {
    let workdir = seeded_workdir();
    let version =
        probe_verifier_version(&workdir.path().join("verus-stub"), Duration::from_secs(10))
            .expect("stub --version should parse");
    assert_eq!(version, "0.4.0");
}

#[test]
fn version_probe_rejects_unrecognizable_version() {
    let workdir = seeded_workdir();
    let err = probe_verifier_version(
        &workdir.path().join("verus-no-version"),
        Duration::from_secs(10),
    )
    .expect_err("garbage version should fail");
    assert!(matches!(
        err,
        aletheia_egregore::proof_capture::VersionProbeError::Unrecognized
    ));
}

#[test]
fn version_probe_missing_binary_is_stable() {
    let err = probe_verifier_version(Path::new("/nonexistent/verus"), Duration::from_secs(10))
        .expect_err("missing binary probe should fail");
    assert!(matches!(
        err,
        aletheia_egregore::proof_capture::VersionProbeError::SpawnFailed
            | aletheia_egregore::proof_capture::VersionProbeError::Missing
    ));
}

// ── 4. Output parsing ───────────────────────────────────────────────────────

#[test]
fn parse_recognizes_passing_shape() {
    let parse = parse_verus_output(
        "verifying file proof_pass.rs\nverification results:: 2 verified, 0 errors\n",
        "",
    );
    match parse {
        VerusParse::Recognized {
            summary,
            error_lines,
        } => {
            assert_eq!(summary.verified, 2);
            assert_eq!(summary.errors, 0);
            assert_eq!(error_lines, 0);
        }
        other => panic!("expected Recognized, got {other:?}"),
    }
}

#[test]
fn parse_recognizes_failing_shape_with_error_lines() {
    let parse = parse_verus_output(
        "error: postcondition failed\nverification results:: 1 verified, 1 errors\n",
        "",
    );
    match parse {
        VerusParse::Recognized {
            summary,
            error_lines,
        } => {
            assert_eq!(summary.verified, 1);
            assert_eq!(summary.errors, 1);
            assert_eq!(error_lines, 1);
        }
        other => panic!("expected Recognized, got {other:?}"),
    }
}

#[test]
fn parse_flags_unknown_shape() {
    let parse = parse_verus_output("qzxw blorple fnord 42\n", "");
    assert!(matches!(parse, VerusParse::Unrecognized { error_lines: 0 }));
}

#[test]
fn parse_flags_malformed_summary() {
    let parse = parse_verus_output("verification results:: many verified, few errors\n", "");
    assert!(matches!(parse, VerusParse::MalformedSummary));
}

// ── 5. Classification matrix ────────────────────────────────────────────────

fn fake_run(exit_code: Option<i64>, timed_out: bool) -> VerusRun {
    VerusRun {
        argv: vec!["verus".to_owned(), "proof.rs".to_owned()],
        exit_code,
        timed_out,
        stdout: Vec::new(),
        stderr: Vec::new(),
        started_at: EXECUTED_AT.to_owned(),
        finished_at: EXECUTED_AT.to_owned(),
    }
}

#[test]
fn classification_matrix_is_fail_closed() {
    let recognized = |verified, errors, error_lines| VerusParse::Recognized {
        summary: VerusSummary { verified, errors },
        error_lines,
    };

    // Clean pass.
    let c = classify_proof_run(&fake_run(Some(0), false), &recognized(2, 0, 0));
    assert_eq!(c.status, ProofStatus::Pass);
    assert_eq!(c.diagnostic_code, None);

    // Errors reported -> fail, no diagnostic needed.
    let c = classify_proof_run(&fake_run(Some(1), false), &recognized(1, 1, 1));
    assert_eq!(c.status, ProofStatus::Fail);
    assert_eq!(c.diagnostic_code, None);

    // Timeout dominates everything.
    let c = classify_proof_run(&fake_run(None, true), &recognized(2, 0, 0));
    assert_eq!(c.status, ProofStatus::Timeout);

    // Clean summary but nonzero exit -> error, never pass.
    let c = classify_proof_run(&fake_run(Some(7), false), &recognized(2, 0, 0));
    assert_eq!(c.status, ProofStatus::Error);
    assert_eq!(c.diagnostic_code, Some(VERUS_EXIT_CODE_MISMATCH_CODE));

    // Malformed summary -> error, never pass.
    let c = classify_proof_run(&fake_run(Some(0), false), &VerusParse::MalformedSummary);
    assert_eq!(c.status, ProofStatus::Error);
    assert_eq!(c.diagnostic_code, Some(MALFORMED_VERUS_OUTPUT_CODE));

    // Unknown shape, nonzero exit, no error lines -> error, never silent success.
    let c = classify_proof_run(
        &fake_run(Some(3), false),
        &VerusParse::Unrecognized { error_lines: 0 },
    );
    assert_eq!(c.status, ProofStatus::Error);
    assert_eq!(c.diagnostic_code, Some(UNRECOGNIZED_VERUS_OUTPUT_CODE));

    // Unknown shape but error lines + nonzero exit -> fail (fail-closed) with diagnostic.
    let c = classify_proof_run(
        &fake_run(Some(1), false),
        &VerusParse::Unrecognized { error_lines: 2 },
    );
    assert_eq!(c.status, ProofStatus::Fail);
    assert_eq!(c.diagnostic_code, Some(UNRECOGNIZED_VERUS_OUTPUT_CODE));

    // Exit 0 with NO recognizable output -> error, never a silent pass.
    let c = classify_proof_run(
        &fake_run(Some(0), false),
        &VerusParse::Unrecognized { error_lines: 0 },
    );
    assert_eq!(c.status, ProofStatus::Error);
}

// ── 6. Full capture: passing proof (AC1, AC3, AC5) ──────────────────────────

#[test]
#[allow(clippy::too_many_lines)] // End-to-end assertion sequence by design.
fn seeded_workflow_captures_passing_proof() {
    let workdir = seeded_workdir();
    let config = capture_config(workdir.path(), "proof_pass.rs", "verus-stub");
    let outcome = capture_proof(&config).expect("passing capture should succeed");

    assert_eq!(outcome.status, ProofStatus::Pass);
    assert_eq!(outcome.diagnostic_code, None);
    // Exactly the CommandRun + the ProofResult, in canonical order.
    assert_eq!(outcome.records.len(), 2);
    assert!(matches!(
        outcome.records[0],
        GraphRecord::Node {
            kind: NodeKind::CommandRun,
            ..
        }
    ));
    assert!(matches!(
        outcome.records[1],
        GraphRecord::Node {
            kind: NodeKind::ProofResult,
            ..
        }
    ));

    // Stable record IDs per docs/schema/verification.md §3.
    assert_eq!(
        outcome.proof_record_id,
        verification_stable_id(&[
            "proof_result",
            SESSION,
            COMMIT,
            config.target_display.as_str()
        ])
    );
    assert_eq!(
        outcome.command_record_id,
        verification_stable_id(&[
            "command_run",
            SESSION,
            COMMIT,
            config.target_display.as_str()
        ])
    );

    // AC3: every required field is present on the ProofResult.
    let proof = proof_node(&outcome.records);
    match proof {
        GraphRecord::Node {
            id,
            schema_version,
            domain,
            verification_kind,
            status,
            exit_code,
            executed_at,
            source_artifact_path,
            source_artifact_hash,
            producer,
            temporal,
            ..
        } => {
            assert_eq!(id, &outcome.proof_record_id);
            assert_eq!(*schema_version, VERIFICATION_SCHEMA_VERSION);
            assert_eq!(domain.as_deref(), Some("verification"));
            assert_eq!(verification_kind.as_deref(), Some("proof_result"));
            assert_eq!(status.as_deref(), Some("pass"));
            assert_eq!(*exit_code, Some(0));
            assert_eq!(executed_at.as_deref(), Some(EXECUTED_AT));
            assert_eq!(
                source_artifact_path.as_deref(),
                Some(config.target_display.as_str())
            );
            let raw = fs::read(&config.target).expect("target should read");
            assert_eq!(
                source_artifact_hash.as_deref(),
                Some(blake3::hash(&raw).to_hex().to_string()).as_deref()
            );
            // The producer stamps the caller-supplied executed_at, never a
            // wall clock.
            assert_eq!(
                producer.as_ref().expect("producer").producer_started_at,
                EXECUTED_AT
            );
            // Commit handle on the temporal block.
            assert_eq!(temporal.as_ref().expect("temporal").git_commit, COMMIT);
        }
        _ => unreachable!(),
    }

    // Verifier name + version are captured when available (normalized summary).
    let payload = normalized_payload(&outcome.records);
    assert_eq!(payload["verifier"], Value::String("verus".to_owned()));
    assert_eq!(
        payload["verifier_version"],
        Value::String("0.4.0".to_owned())
    );
    assert_eq!(payload["status"], Value::String("pass".to_owned()));
    assert_eq!(payload["verified"], Value::Number(2.into()));
    assert_eq!(payload["errors"], Value::Number(0.into()));

    // The trust classifier derives the verification trust class (AC3, AC12: no
    // new trust class introduced).
    let classifier = TrustIndex::build(&outcome.records);
    assert_eq!(classifier.classify(proof), TrustClass::VerificationEvidence);

    // AC5: the proof links to the captured verifier command whose exit status
    // and output support the claim — and the summary prose is derived from
    // that evidence, not invented.
    let links = proof
        .evidence_links()
        .expect("proof should link its command");
    assert_eq!(links.len(), 1);
    assert_eq!(
        links[0].target_record_id.as_deref(),
        Some(outcome.command_record_id.as_str())
    );
    assert_eq!(links[0].target_domain, "verification");
    assert_eq!(links[0].relation, "HAS_EVIDENCE");
    let command = command_node(&outcome.records);
    assert_eq!(node_status(command).as_deref(), Some("pass"));
    match command {
        GraphRecord::Node { exit_code, .. } => assert_eq!(*exit_code, Some(0)),
        _ => unreachable!(),
    }
}

// ── 7. Full capture: failing proof (AC1, AC4) ────────────────────────────────

#[test]
fn seeded_workflow_captures_failing_proof() {
    let workdir = seeded_workdir();
    let config = capture_config(workdir.path(), "proof_fail.rs", "verus-stub");
    let outcome = capture_proof(&config).expect("failing capture should succeed");

    assert_eq!(outcome.status, ProofStatus::Fail);
    let proof = proof_node(&outcome.records);
    assert_eq!(node_status(proof).as_deref(), Some("fail"));
    let payload = normalized_payload(&outcome.records);
    assert_eq!(payload["status"], Value::String("fail".to_owned()));
    assert_eq!(payload["errors"], Value::Number(1.into()));

    // The failed proof still links its command evidence (AC5 holds for every
    // status: the claim is always backed by the captured command).
    let links = proof
        .evidence_links()
        .expect("failed proof should link its command");
    assert_eq!(
        links[0].target_record_id.as_deref(),
        Some(outcome.command_record_id.as_str())
    );

    // AC4: the recorded status is Failing, never Passing. (`verification_outcome`
    // is crate-internal; the public surface is the status string itself.)
    assert_ne!(node_status(proof).as_deref(), Some("pass"));
    assert_ne!(
        node_status(command_node(&outcome.records)).as_deref(),
        Some("pass")
    );
}

// ── 8. Timeout (AC4) ────────────────────────────────────────────────────────

#[test]
fn hung_verifier_times_out_with_stable_status() {
    let workdir = seeded_workdir();
    let mut config = capture_config(workdir.path(), "proof_hang.rs", "verus-stub");
    config.timeout = Duration::from_secs(2);
    let outcome = capture_proof(&config).expect("timeout capture should succeed");

    assert_eq!(outcome.status, ProofStatus::Timeout);
    let proof = proof_node(&outcome.records);
    assert_eq!(node_status(proof).as_deref(), Some("timeout"));
    let command = command_node(&outcome.records);
    assert_eq!(node_status(command).as_deref(), Some("timeout"));
    // A killed child has no exit code.
    match command {
        GraphRecord::Node { exit_code, .. } => assert_eq!(*exit_code, None),
        _ => unreachable!(),
    }
    assert_ne!(
        node_status(proof).as_deref(),
        Some("pass"),
        "a timed-out proof is Failing, never Passing"
    );
}

// ── 9. Verifier-error statuses (AC4, AC8) ───────────────────────────────────

#[test]
fn unknown_output_shape_is_verifier_error_never_silent_success() {
    let workdir = seeded_workdir();
    let config = capture_config(workdir.path(), "proof_unknown.rs", "verus-stub");
    let outcome = capture_proof(&config).expect("unknown-shape capture should succeed");

    assert_eq!(outcome.status, ProofStatus::Error);
    assert_eq!(
        outcome.diagnostic_code,
        Some(UNRECOGNIZED_VERUS_OUTPUT_CODE)
    );
    let diags = diagnostics(&outcome.records);
    assert_eq!(diags.len(), 1);
    assert_eq!(
        diagnostic_code(diags[0]).as_deref(),
        Some(UNRECOGNIZED_VERUS_OUTPUT_CODE)
    );
    // The diagnostic carries source handles: target path + hash.
    match diags[0] {
        GraphRecord::Node {
            source_artifact_path,
            source_artifact_hash,
            ..
        } => {
            assert_eq!(
                source_artifact_path.as_deref(),
                Some(config.target_display.as_str())
            );
            assert!(
                source_artifact_hash
                    .as_deref()
                    .is_some_and(|h| !h.is_empty())
            );
        }
        _ => unreachable!(),
    }
    assert_ne!(
        node_status(proof_node(&outcome.records)).as_deref(),
        Some("pass"),
        "unrecognized verifier output is Failing, never Passing"
    );
}

#[test]
fn malformed_summary_is_verifier_error() {
    let workdir = seeded_workdir();
    let config = capture_config(workdir.path(), "proof_malformed.rs", "verus-stub");
    let outcome = capture_proof(&config).expect("malformed capture should succeed");
    assert_eq!(outcome.status, ProofStatus::Error);
    assert_eq!(outcome.diagnostic_code, Some(MALFORMED_VERUS_OUTPUT_CODE));
}

#[test]
fn nonzero_exit_with_clean_summary_is_verifier_error() {
    let workdir = seeded_workdir();
    let config = capture_config(workdir.path(), "proof_exit_mismatch.rs", "verus-stub");
    let outcome = capture_proof(&config).expect("mismatch capture should succeed");
    assert_eq!(outcome.status, ProofStatus::Error);
    assert_eq!(outcome.diagnostic_code, Some(VERUS_EXIT_CODE_MISMATCH_CODE));
    assert_ne!(
        node_status(proof_node(&outcome.records)).as_deref(),
        Some("pass"),
        "exit-code mismatch is Failing, never Passing"
    );
}

#[test]
fn unsupported_verifier_version_is_diagnostic_only() {
    let workdir = seeded_workdir();
    let config = capture_config(workdir.path(), "proof_pass.rs", "verus-no-version");
    let failure = capture_proof(&config).expect_err("bad version should fail pre-execution");
    assert_eq!(failure.code, UNSUPPORTED_VERIFIER_VERSION_CODE);
    // Diagnostic-only: never a passing record.
    assert_eq!(
        diagnostic_code(&failure.diagnostic).as_deref(),
        Some(UNSUPPORTED_VERIFIER_VERSION_CODE)
    );
}

// ── 10. Pre-execution failures: diagnostic instead of a proof record (AC2) ──

#[test]
fn missing_verifier_binary_yields_diagnostic_not_proof() {
    let workdir = seeded_workdir();
    let mut config = capture_config(workdir.path(), "proof_pass.rs", "verus-stub");
    config.verus_bin = workdir.path().join("does-not-exist");
    let failure = capture_proof(&config).expect_err("missing binary should fail");
    assert_eq!(failure.code, VERIFIER_BINARY_MISSING_CODE);
    assert_eq!(
        diagnostic_code(&failure.diagnostic).as_deref(),
        Some(VERIFIER_BINARY_MISSING_CODE)
    );
}

#[test]
fn nonexecutable_verifier_yields_diagnostic_not_proof() {
    let workdir = seeded_workdir();
    let mut config = capture_config(workdir.path(), "proof_pass.rs", "verus-stub");
    let candidate = workdir.path().join("verus-nonexec");
    let mut perms = fs::metadata(&candidate).expect("copy").permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&candidate, perms).expect("chmod");
    config.verus_bin = candidate;
    let failure = capture_proof(&config).expect_err("non-executable should fail");
    assert_eq!(failure.code, VERIFIER_BINARY_NOT_EXECUTABLE_CODE);
}

#[test]
fn missing_proof_target_yields_diagnostic_not_proof() {
    let workdir = seeded_workdir();
    let mut config = capture_config(workdir.path(), "proof_pass.rs", "verus-stub");
    config.target = workdir.path().join("gone.rs");
    config.target_display = config.target.to_string_lossy().into_owned();
    let failure = capture_proof(&config).expect_err("missing target should fail");
    assert_eq!(failure.code, PROOF_TARGET_MISSING_CODE);
}

#[test]
fn pre_execution_diagnostic_ids_are_stable() {
    let d1 = pre_execution_diagnostic(
        SESSION,
        COMMIT,
        "proof.rs",
        VERIFIER_BINARY_MISSING_CODE,
        "no such file",
        Some("/bin/verus"),
        None,
    );
    let d2 = pre_execution_diagnostic(
        SESSION,
        COMMIT,
        "proof.rs",
        VERIFIER_BINARY_MISSING_CODE,
        "no such file",
        Some("/bin/verus"),
        None,
    );
    assert_eq!(d1.id(), d2.id(), "diagnostic IDs carry no wall clock");
    assert_eq!(
        diagnostic_code(&d1).as_deref(),
        Some(VERIFIER_BINARY_MISSING_CODE)
    );
}

// ── 11. Redaction (AC7) ─────────────────────────────────────────────────────

#[test]
fn verifier_output_secrets_are_redacted_before_persistence() {
    let workdir = seeded_workdir();
    let config = capture_config(workdir.path(), "proof_pass.rs", "verus-stub");
    let outcome = capture_proof(&config).expect("capture should succeed");

    // The stub leaks AKIAIOSFODNN7EXAMPLE on stderr on every run: it must not
    // survive into the graph.
    let jsonl = outcome
        .records
        .iter()
        .map(|r| serde_json::to_string(r).expect("serialize"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !jsonl.contains("AKIAIOSFODNN7EXAMPLE"),
        "raw secret must never enter the graph JSONL"
    );

    // ...but the redaction is auditable: a well-formed marker is present and
    // the daemon-visible policy version is stamped.
    let command = command_node(&outcome.records);
    let stderr_inline = match command {
        GraphRecord::Node { stderr_handle, .. } => stderr_handle
            .as_ref()
            .expect("CommandRun should carry a stderr_handle")
            .inline
            .clone()
            .expect("stderr should be inline (small)"),
        _ => unreachable!(),
    };
    let markers = parse_redaction_markers(&stderr_inline);
    assert_eq!(markers.len(), 1, "exactly one redaction marker expected");
    assert_eq!(markers[0].0.as_str(), "cloud_credential");
    assert!(outcome.secrets_redacted >= 1);

    for record in &outcome.records {
        match record {
            GraphRecord::Node {
                redaction_policy_version,
                ..
            } => assert_eq!(
                redaction_policy_version.as_deref(),
                Some("v1"),
                "redaction policy version must be stamped"
            ),
            _ => panic!("expected node records only"),
        }
    }
}

#[test]
// `${1:-}` in the stub below is POSIX shell parameter expansion, not a Rust
// format placeholder.
#[allow(clippy::literal_string_with_formatting_args)]
fn oversized_verifier_output_is_handle_only() {
    // A stub emitting >16 KiB must be represented by handles/hashes/counts,
    // never inline.
    let workdir = seeded_workdir();
    let big = workdir.path().join("verus-big");
    fs::write(
        &big,
        "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ]; then echo \"verus 9.9.9\"; exit 0; fi\npython3 -c \"print('x' * 20000)\"\nprintf 'verification results:: 1 verified, 0 errors\\n'\n",
    )
    .expect("write big stub");
    let mut perms = fs::metadata(&big).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&big, perms).expect("chmod");
    let config = capture_config(workdir.path(), "proof_pass.rs", "verus-big");
    let outcome = capture_proof(&config).expect("big-output capture should succeed");
    assert_eq!(outcome.status, ProofStatus::Pass);
    let command = command_node(&outcome.records);
    match command {
        GraphRecord::Node { stdout_handle, .. } => {
            let handle = stdout_handle.as_ref().expect("stdout_handle");
            assert!(
                handle.inline.is_none(),
                "output above the 16 KiB ceiling must not be inline"
            );
            assert!(handle.bytes > 16 * 1024);
            assert!(!handle.hash.is_empty());
        }
        _ => unreachable!(),
    }
}

// ── 12. Determinism (AC9) ───────────────────────────────────────────────────

/// Canonical JSON for cross-run comparison: masks the documented volatile
/// metadata (`started_at`/`finished_at` wall clock on the `CommandRun`) while
/// keeping every canonical field — including `executed_at`, which is
/// caller-supplied and therefore part of the contract.
fn canonical_json(records: &[GraphRecord]) -> String {
    let masked: Vec<Value> = records
        .iter()
        .map(|record| {
            let mut value = serde_json::to_value(record).expect("record should serialize");
            if let Value::Object(ref mut map) = value
                && map.get("kind").and_then(Value::as_str) == Some("CommandRun")
            {
                for key in ["started_at", "finished_at"] {
                    map.insert(key.to_owned(), Value::String("<masked>".to_owned()));
                }
            }
            value
        })
        .collect();
    serde_json::to_string_pretty(&masked).expect("canonical JSON")
}

#[test]
fn five_identical_runs_are_byte_identical_up_to_documented_volatile_metadata() {
    let workdir = seeded_workdir();
    let mut canonicals = BTreeSet::new();
    for _ in 0..5 {
        let config = capture_config(workdir.path(), "proof_pass.rs", "verus-stub");
        let outcome = capture_proof(&config).expect("repeat capture should succeed");
        canonicals.insert(canonical_json(&outcome.records));
    }
    assert_eq!(
        canonicals.len(),
        1,
        "five identical captures must produce one canonical output"
    );
}

// ── 13. Task evidence gate (AC4, AC6) ───────────────────────────────────────

fn task_node_for_gate(task_id: &str) -> GraphRecord {
    let mut rec = GraphRecord::node(
        task_id.to_owned(),
        NodeKind::Task,
        None,
        None,
        Some("proof-gate-task".to_owned()),
        "Task: close on verus proof".to_owned(),
    );
    if let GraphRecord::Node { title, status, .. } = &mut rec {
        *title = Some("close on verus proof".to_owned());
        *status = Some("open".to_owned());
    }
    rec
}

fn ac_node_for_gate(ac_id: &str, task_id: &str) -> GraphRecord {
    let mut rec = GraphRecord::node(
        ac_id.to_owned(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("proof-gate-ac".to_owned()),
        "acceptance criterion under test".to_owned(),
    );
    if let GraphRecord::Node {
        parent_task_id,
        ordinal,
        text,
        status,
        ..
    } = &mut rec
    {
        *parent_task_id = Some(task_id.to_owned());
        *ordinal = Some(1);
        *text = Some("verus proves the target".to_owned());
        *status = Some("in_progress".to_owned());
    }
    rec
}

fn ready_gate_report(
    records: &[GraphRecord],
    task_id: &str,
) -> aletheia_egregore::query::TaskEvidenceGateReport {
    match task_evidence_gate_report(records, task_id) {
        TaskEvidenceGateOutcome::Ready(report) => report,
        TaskEvidenceGateOutcome::NoSuchTask => panic!("expected a gate verdict, got NoSuchTask"),
    }
}

#[test]
fn passing_proof_closes_criterion_failing_proof_blocks() {
    let gate_case = |target: &str| {
        let workdir = seeded_workdir();
        let config = capture_config(workdir.path(), target, "verus-stub");
        let outcome = capture_proof(&config).expect("capture should succeed");
        // AC6: the operator links the acceptance criterion to the captured
        // CommandRun — the verifier command whose exit status and output the
        // criterion is closed on. The gate follows CLOSES_ACCEPTANCE_CRITERION.
        let task_id = project_stable_id(&["task", SESSION, target]);
        let ac_id = project_stable_id(&["ac", SESSION, target]);
        let command = command_node(&outcome.records).clone();
        let records = vec![
            task_node_for_gate(&task_id),
            ac_node_for_gate(&ac_id, &task_id),
            command,
            GraphRecord::edge(
                EdgeLabel::ClosesAcceptanceCriterion,
                ac_id.clone(),
                outcome.command_record_id,
                None,
                "acceptance criterion closed by verus proof run".to_owned(),
            ),
        ];
        (workdir, records, task_id)
    };

    let (_workdir_pass, pass_records, pass_task) = gate_case("proof_pass.rs");
    let pass_report = ready_gate_report(&pass_records, &pass_task);
    assert!(
        pass_report.ready,
        "a passing proof command must satisfy the task evidence gate"
    );
    assert_eq!(pass_report.criteria.len(), 1);
    let row = &pass_report.criteria[0];
    assert!(
        row.satisfied,
        "passing proof evidence satisfies the criterion"
    );
    assert_eq!(row.not_ready_reason, None);
    assert_eq!(row.evidence.len(), 1);
    assert!(row.evidence[0].pass);

    let (_workdir_fail, fail_records, fail_task) = gate_case("proof_fail.rs");
    let fail_report = ready_gate_report(&fail_records, &fail_task);
    assert!(
        !fail_report.ready,
        "a failing proof command must NOT satisfy the task evidence gate"
    );
    let row = &fail_report.criteria[0];
    assert!(!row.satisfied);
    assert_eq!(
        row.not_ready_reason,
        Some(NotReadyReason::FailingEvidence),
        "a failing proof command blocks closure with FailingEvidence"
    );
    // No fallback to stale/passing evidence: the proof itself is the claim.
    assert!(
        !row.evidence.is_empty(),
        "the failing evidence must be visible in the gate report"
    );
}

// ── 14. CLI end-to-end (AC1, AC2, AC8, AC12) ─────────────────────────────────

fn eg() -> Command {
    Command::cargo_bin("eg").expect("eg binary should be built")
}

fn cli_capture(
    workdir: &Path,
    target: &str,
    stub: &str,
    out_name: &str,
) -> (assert_cmd::assert::Assert, PathBuf) {
    let out_path = workdir.join(out_name);
    let assertion = eg()
        .args([
            "capture-proof",
            "--target",
            workdir.join(target).to_str().unwrap(),
            "--verus-bin",
            workdir.join(stub).to_str().unwrap(),
            "--session",
            SESSION,
            "--commit",
            COMMIT,
            "--executed-at",
            EXECUTED_AT,
            "--out",
            out_path.to_str().unwrap(),
        ])
        .assert();
    (assertion, out_path)
}

fn read_graph_records(path: &Path) -> Vec<GraphRecord> {
    let jsonl = fs::read_to_string(path).expect("CLI should write the graph file");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .expect("every line should be a GraphRecord");
    // No secret bytes anywhere in the emitted file.
    assert!(!jsonl.contains("AKIAIOSFODNN7EXAMPLE"));
    records
}

#[test]
fn cli_capture_proof_end_to_end() {
    let workdir = seeded_workdir();
    let (assertion, out_path) = cli_capture(
        workdir.path(),
        "proof_pass.rs",
        "verus-stub",
        "proof.graph.jsonl",
    );
    assertion.success();
    let records = read_graph_records(&out_path);
    assert_eq!(records.len(), 2, "proof + command records");
    assert_eq!(
        node_status(proof_node(&records)).as_deref(),
        Some("pass"),
        "the CLI capture must report the passing proof"
    );
}

#[test]
fn cli_reports_pre_execution_failure_with_exit_2() {
    let workdir = seeded_workdir();
    let (assertion, out_path) = cli_capture(
        workdir.path(),
        "proof_pass.rs",
        "does-not-exist",
        "diag.graph.jsonl",
    );
    assertion.code(2);
    let records = read_graph_records(&out_path);
    assert_eq!(records.len(), 1);
    assert_eq!(
        diagnostic_code(&records[0]).as_deref(),
        Some(VERIFIER_BINARY_MISSING_CODE)
    );
}

#[test]
fn cli_reports_failing_proof_with_exit_0() {
    // A failing PROOF is still a successful CAPTURE: the operator proved the
    // claim is false. Exit 0; the status lives in the records.
    let workdir = seeded_workdir();
    let (assertion, out_path) = cli_capture(
        workdir.path(),
        "proof_fail.rs",
        "verus-stub",
        "fail.graph.jsonl",
    );
    assertion.success();
    let records = read_graph_records(&out_path);
    assert_eq!(node_status(proof_node(&records)).as_deref(), Some("fail"));
}

// ── 15. Embedded ingest read-back (AC10) ────────────────────────────────────

/// Builds a project/agent-memory-domain node by mutating the typed fields
/// directly (the codebase's established pattern for tests of cross-domain
/// validation; production proof capture only ever emits verification-domain
/// nodes).
fn typed_node(
    id: String,
    kind: NodeKind,
    domain: &str,
    schema_version: u32,
    mutate: impl FnOnce(&mut GraphRecord),
) -> GraphRecord {
    let mut rec = GraphRecord::node(id, kind, None, None, None, "test node".to_owned());
    if let GraphRecord::Node {
        domain: d,
        schema_version: sv,
        ..
    } = &mut rec
    {
        *d = Some(domain.to_owned());
        *sv = schema_version;
    }
    mutate(&mut rec);
    rec
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
#[allow(clippy::too_many_lines)] // End-to-end assertion sequence by design.
fn embedded_ingest_reads_back_proof_and_linked_criterion() {
    use aletheia_egregore::adapters::{
        DanglingCitationPolicy, EmbeddedAletheiaSink, ingest_records_with_policy,
    };

    let workdir = seeded_workdir();
    let config = capture_config(workdir.path(), "proof_pass.rs", "verus-stub");
    let outcome = capture_proof(&config).expect("capture should succeed");

    // An operator links their acceptance criterion through the project domain:
    // `verified` + verification_link_id tells the daemon to synthesize the
    // CLOSES_ACCEPTANCE_CRITERION edge to the captured verifier command.
    let task_id = project_stable_id(&["task", SESSION]);
    let ac_id = project_stable_id(&["ac", SESSION]);
    let ext_id = project_stable_id(&["ext", SESSION]);
    let mut records = outcome.records.clone();
    records.push(typed_node(
        ext_id.clone(),
        NodeKind::ExternalLink,
        "project",
        PROJECT_SCHEMA_VERSION,
        |rec| {
            if let GraphRecord::Node {
                system,
                url,
                system_native_id,
                discovered_at,
                ..
            } = rec
            {
                *system = Some("manual".to_owned());
                *url = Some("https://example.invalid/issue/69".to_owned());
                *system_native_id = Some("69".to_owned());
                *discovered_at = Some(EXECUTED_AT.to_owned());
            }
        },
    ));
    records.push(typed_node(
        task_id.clone(),
        NodeKind::Task,
        "project",
        PROJECT_SCHEMA_VERSION,
        |rec| {
            if let GraphRecord::Node {
                title,
                status,
                source_kind,
                source_external_link_id,
                assignees,
                labels,
                priority,
                ..
            } = rec
            {
                *title = Some("Prove the lemma".to_owned());
                *status = Some("open".to_owned());
                *source_kind = Some("manual".to_owned());
                *source_external_link_id = Some(ext_id.clone());
                *assignees = Some(vec!["operator".to_owned()]);
                *labels = Some(Vec::new());
                *priority = Some("normal".to_owned());
            }
        },
    ));
    records.push(typed_node(
        ac_id.clone(),
        NodeKind::AcceptanceCriterion,
        "project",
        PROJECT_SCHEMA_VERSION,
        |rec| {
            if let GraphRecord::Node {
                parent_task_id,
                ordinal,
                text,
                status,
                verification_link_id,
                ..
            } = rec
            {
                *parent_task_id = Some(task_id.clone());
                *ordinal = Some(1);
                *text = Some("verus proves the target with no errors".to_owned());
                *status = Some("verified".to_owned());
                *verification_link_id = Some(outcome.command_record_id.clone());
            }
        },
    ));
    // ...and notes an observation validated by the proof record.
    let observation_id = format!("agent-memory:v1:obs-{SESSION}");
    records.push(typed_node(
        observation_id.clone(),
        NodeKind::Observation,
        "agent_memory",
        1,
        |rec| {
            if let GraphRecord::Node {
                agent_id,
                agent_kind,
                session_id,
                observed_at,
                ingested_at,
                confidence,
                text,
                summary,
                evidence_links,
                ..
            } = rec
            {
                *agent_id = Some("test-agent".to_owned());
                *agent_kind = Some("test".to_owned());
                *session_id = Some(SESSION.to_owned());
                *observed_at = Some(EXECUTED_AT.to_owned());
                *ingested_at = Some(EXECUTED_AT.to_owned());
                *confidence = Some("1.0".to_owned());
                *text = Some("verus proved the target".to_owned());
                *summary = "Observation: verus proved the target".to_owned();
                *evidence_links = Some(vec![aletheia_egregore::ir::EvidenceLink {
                    target_record_id: Some(outcome.proof_record_id.clone()),
                    target_domain: "verification".to_owned(),
                    relation: "VALIDATED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                }]);
            }
        },
    ));

    let data_dir = workdir.path().join("store");
    let report = ingest_records_with_policy(
        &records,
        &mut EmbeddedAletheiaSink::open(&data_dir).expect("store should open"),
        DanglingCitationPolicy::RejectBatch,
    );
    assert!(
        report.is_success(),
        "proof records plus linked criterion must ingest, got {report:?}"
    );

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("store should reopen");
    for id in [
        &outcome.proof_record_id,
        &outcome.command_record_id,
        &ac_id,
        &observation_id,
    ] {
        assert!(
            sink.read_back(id).expect("read_back should work").is_some(),
            "record {id} should read back with the same public handle"
        );
    }
    // The linked criterion points at the same command handle the capture
    // emitted. (The daemon's ingest path additionally synthesizes a
    // CLOSES_ACCEPTANCE_CRITERION edge from `verification_link_id`; that
    // synthesis lives in `daemon::validate_project_domain_records` and is
    // covered by the daemon's own tests. The embedded adapter path tested
    // here guarantees the link itself round-trips intact.)
    let ac = sink
        .read_back(&ac_id)
        .expect("read_back")
        .expect("ac should exist");
    assert_eq!(
        verification_link_id_of(&ac).as_deref(),
        Some(outcome.command_record_id.as_str())
    );
    let proof = sink
        .read_back(&outcome.proof_record_id)
        .expect("read_back")
        .expect("proof should exist");
    let proof_links = evidence_links_of(&proof);
    assert!(
        proof_links
            .iter()
            .any(|l| l.target_record_id.as_deref() == Some(outcome.command_record_id.as_str())),
        "proof record's HAS_EVIDENCE link should target the captured command"
    );
}

// ── 16. Trust-class contract (AC12) ─────────────────────────────────────────

#[test]
fn proof_records_introduce_no_new_kinds_or_trust_classes() {
    let workdir = seeded_workdir();
    let config = capture_config(workdir.path(), "proof_pass.rs", "verus-stub");
    let outcome = capture_proof(&config).expect("capture should succeed");
    for record in &outcome.records {
        match record {
            GraphRecord::Node { kind, .. } => {
                assert!(
                    matches!(kind, NodeKind::CommandRun | NodeKind::ProofResult),
                    "capture must emit only the reserved verification-domain kinds, got {kind:?}"
                );
            }
            other => panic!("capture must emit nodes only, got {other:?}"),
        }
    }
    // The only trust class on these records is the existing
    // deterministic-but-runtime-derived verification class — nothing new.
    let classifier = TrustIndex::build(&outcome.records);
    for record in &outcome.records {
        assert_eq!(
            classifier.classify(record),
            TrustClass::VerificationEvidence,
            "proof records must classify as existing verification evidence"
        );
    }
}
