//! `eg capture-proof` — execute a local Verus proof run and capture it as
//! citable verification-domain records (issue #69).
//!
//! Unlike the capture-only siblings, this workflow EXECUTES the verifier
//! binary. The operator must have a real `verus` installed; the tests use a
//! stub script pointed at via `--verus-bin` and never install anything.
//!
//! Exit codes: 0 captured (any proof status — a failing PROOF is still a
//! successful CAPTURE); 1 usage error; 2 pre-execution failure (diagnostic
//! written to `--out`, no proof record); 3 protected-store I/O failure.

use super::*;

use crate::proof_capture::{CaptureConfig, capture_proof};
use crate::protected::{ProtectedPayloadClass, ProtectedStore};

/// Exit code for a pre-execution failure: the verifier never ran, so the only
/// record is a stable diagnostic.
const EXIT_PRE_EXECUTION: i32 = 2;
/// Exit code for a protected-store I/O failure (mirrors `capture-tests`).
const EXIT_PROTECTED_IO: i32 = 3;

/// Arguments for [`capture_proof_cmd`], bundled to keep the dispatch arm readable.
pub(crate) struct CaptureProofArgs<'a> {
    pub verus_bin: &'a Path,
    pub verus_arg: &'a [String],
    pub target: &'a Path,
    pub out: &'a Path,
    pub session_id: &'a str,
    pub commit: &'a str,
    pub executed_at: &'a str,
    pub verifier_version: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub timeout_secs: u64,
    pub probe_timeout_secs: u64,
    pub protected_raw_artifacts: bool,
    pub protected_store: Option<&'a Path>,
    pub producer: Option<&'a str>,
}

/// Emits the machine-readable `{"code":..,"field":..}` usage diagnostic to
/// stderr and exits 1, matching the typed evidence-writer envelope shape.
fn exit_usage_error(code: &str, field: &str) -> ! {
    eprintln!(r#"{{"code":"{code}", "field":"{field}"}}"#);
    process::exit(1);
}

/// Emits the machine-readable capture-failure envelope to stderr and exits with
/// `exit_code`, mirroring the `capture-tests` protected-capture failure shape.
fn exit_capture_error(code: &str, detail: &serde_json::Value, exit_code: i32) -> ! {
    let envelope = serde_json::json!({
        "ok": false,
        "error": { "code": code, "detail": detail }
    });
    eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
    process::exit(exit_code);
}

/// Writes a diagnostic-only graph to `out`, prints the machine-readable error
/// envelope to stdout, and exits 2. Never a `ProofResult`.
fn write_diagnostic_and_exit(out: &Path, diagnostic: GraphRecord, code: &str) -> ! {
    let mut graph = Graph::new();
    let record_id = diagnostic.id().to_owned();
    graph.push(diagnostic);
    match graph.to_jsonl() {
        Ok(jsonl) => {
            if let Err(e) = fs::write(out, jsonl) {
                exit_capture_error(
                    "store_io_error",
                    &serde_json::json!({ "message": format!("failed to write diagnostic JSONL to {}: {e}", out.display()) }),
                    EXIT_PROTECTED_IO,
                );
            }
        }
        Err(e) => {
            exit_capture_error(
                "store_io_error",
                &serde_json::json!({ "message": format!("failed to serialize diagnostic JSONL: {e}") }),
                EXIT_PROTECTED_IO,
            );
        }
    }
    let envelope = serde_json::json!({
        "ok": false,
        "error": { "code": code, "diagnostic_id": record_id }
    });
    println!("{}", serde_json::to_string(&envelope).expect("infallible"));
    process::exit(EXIT_PRE_EXECUTION);
}

/// Resolves a bare verifier name against PATH; paths containing a separator
/// are used as given.
fn resolve_verifier_bin(raw: &Path) -> PathBuf {
    if raw.components().count() > 1 {
        return raw.to_path_buf();
    }
    let file_name = raw.as_os_str();
    for dir in
        std::env::var_os("PATH").map_or(Vec::new(), |paths| std::env::split_paths(&paths).collect())
    {
        let candidate = dir.join(file_name);
        if candidate.is_file() {
            return candidate;
        }
    }
    raw.to_path_buf()
}

/// Handles `eg capture-proof`.
pub(crate) fn capture_proof_cmd(args: &CaptureProofArgs) -> Result<()> {
    // ── Usage validation (exit 1) ────────────────────────────────────────────
    if chrono::DateTime::parse_from_rfc3339(args.executed_at).is_err() {
        exit_usage_error("invalid_field", "executed_at");
    }
    if args.timeout_secs == 0 {
        exit_usage_error("invalid_field", "timeout_secs");
    }
    if args.protected_raw_artifacts {
        if args.protected_store.is_none() {
            exit_usage_error("missing_field", "protected_store");
        }
        if args.producer.is_none() {
            exit_usage_error("missing_field", "producer");
        }
        if args.producer.is_some_and(|p| p.trim().is_empty()) {
            exit_usage_error("invalid_field", "producer");
        }
    }

    let target_display = args.target.to_string_lossy().into_owned();
    let config = CaptureConfig {
        verus_bin: resolve_verifier_bin(args.verus_bin),
        verus_args: args.verus_arg.to_vec(),
        target: args.target.to_path_buf(),
        target_display,
        session_id: args.session_id.to_owned(),
        commit: args.commit.to_owned(),
        executed_at: args.executed_at.to_owned(),
        verifier_name: "verus".to_owned(),
        verifier_version_override: args.verifier_version.map(str::to_owned),
        repo: args.repo.map(str::to_owned),
        timeout: std::time::Duration::from_secs(args.timeout_secs),
        probe_timeout: std::time::Duration::from_secs(args.probe_timeout_secs),
    };

    // ── Execute + capture ────────────────────────────────────────────────────
    let outcome = match capture_proof(&config) {
        Ok(outcome) => outcome,
        Err(failure) => write_diagnostic_and_exit(args.out, *failure.diagnostic, failure.code),
    };

    // ── Write the record batch ───────────────────────────────────────────────
    let mut graph = Graph::new();
    for record in &outcome.records {
        graph.push(record.clone());
    }
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize proof-capture JSONL")?;
    fs::write(args.out, &jsonl).with_context(|| {
        format!(
            "failed to write proof-capture JSONL to {}",
            args.out.display()
        )
    })?;

    let envelope = serde_json::json!({
        "ok": true,
        "proof_record_id": outcome.proof_record_id,
        "command_record_id": outcome.command_record_id,
        "proof_status": outcome.status.as_str(),
        "records": outcome.records.len(),
        "secrets_redacted": outcome.secrets_redacted,
        "diagnostic_code": outcome.diagnostic_code,
    });
    println!("{}", serde_json::to_string(&envelope).expect("infallible"));

    // ── Protected raw-artifact capture ───────────────────────────────────────
    // The graph holds REDACTED verifier output; the protected store keeps the
    // raw stdout/stderr bytes for audit, under access control.
    if args.protected_raw_artifacts {
        let store_dir = args.protected_store.expect("validated present above");
        let producer_id = args.producer.expect("validated present above");
        let store = ProtectedStore::new(store_dir);
        let mut handles = Vec::new();
        for (stream, bytes) in [
            ("stdout", outcome.raw_stdout.as_slice()),
            ("stderr", outcome.raw_stderr.as_slice()),
        ] {
            match store.capture_bytes(
                ProtectedPayloadClass::CommandOutput,
                &format!("verus {stream}: {}", config.target_display),
                bytes,
                producer_id,
                env!("CARGO_PKG_VERSION"),
                args.executed_at,
                true,
            ) {
                Ok(report) => handles.push(report.entries[0].handle.clone()),
                Err(e) => {
                    exit_capture_error(
                        "store_io_error",
                        &serde_json::json!({
                            "message": format!(
                                "protected store I/O failed at {}: {e}",
                                store_dir.display()
                            )
                        }),
                        EXIT_PROTECTED_IO,
                    );
                }
            }
        }
        let envelope = serde_json::json!({
            "ok": true,
            "protected_capture": { "handles": handles },
        });
        println!("{}", serde_json::to_string(&envelope).expect("infallible"));
    }

    // A failing PROOF is still a successful CAPTURE: exit 0, the status lives
    // in the records. The operator proves the claim false — that is evidence.
    Ok(())
}
