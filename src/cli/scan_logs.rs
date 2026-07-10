use super::*;

use crate::log_graph::{self, LogScanError};
use crate::protected::{ProtectedPayloadClass, ProtectedStore};

/// Emits the machine-readable `{"ok":false,"error":{"code":..,"detail":..}}`
/// capture-failure envelope (issue #321) to stderr and exits with `exit_code`,
/// so every `scan-logs` protected-capture failure path shares one byte-stable
/// diagnostic shape.
fn exit_capture_error(code: &str, detail: &serde_json::Value, exit_code: i32) -> ! {
    let envelope = serde_json::json!({
        "ok": false,
        "error": { "code": code, "detail": detail }
    });
    eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
    process::exit(exit_code);
}

/// Handles `eg scan-logs <log_path> --repo-path <repo> --out <log.graph.jsonl>`.
///
/// Extracts deterministic, redaction-safe log-signature graph records from one
/// captured log file (issues #319 / #320) and writes them as JSONL. An
/// unrecognized (binary / non-UTF-8) input prints a machine-readable diagnostic
/// to stdout and exits 1 with no partial output. Exemplar-cap diagnostics, when
/// any, are printed as machine-readable JSON lines to stderr — never a silent
/// drop.
///
/// With `--protected-raw-artifacts` (issue #321) the scanned log's
/// POST-REDACTION raw bytes are additionally captured into the protected
/// artifact store as a `log_payload` blob; the graph JSONL never stores the
/// protected handle. A protected-capture I/O failure prints a machine-readable
/// `store_io_error` envelope to stderr and exits 3 with no partial manifest.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn scan_logs(
    log_path: &Path,
    repo_path: &Path,
    out: &Path,
    repo_id_override: Option<&str>,
    protected_raw_artifacts: bool,
    protected_store: Option<&Path>,
    producer: Option<&str>,
    captured_at_override: Option<&str>,
) -> Result<()> {
    // Validate the protected-capture flag group before doing any work, reusing
    // the #60 `missing_field` diagnostic shape.
    if protected_raw_artifacts {
        for (value, field, flag) in [
            (
                protected_store.is_some(),
                "protected_store",
                "--protected-store",
            ),
            (producer.is_some(), "producer", "--producer"),
        ] {
            if !value {
                exit_capture_error(
                    "missing_field",
                    &serde_json::json!({
                        "field": field,
                        "message": format!(
                            "{flag} is required when --protected-raw-artifacts is set"
                        )
                    }),
                    1,
                );
            }
        }
        if producer.is_some_and(|p| p.trim().is_empty()) {
            exit_capture_error(
                "invalid_field",
                &serde_json::json!({
                    "field": "producer",
                    "message": "--producer must not be empty when --protected-raw-artifacts is set"
                }),
                1,
            );
        }
    }

    let repository_id = identity::compute_repository_identity(repo_path, repo_id_override).id;
    let transaction_time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let scan =
        match log_graph::scan_log_records(log_path, repo_path, &repository_id, &transaction_time) {
            Ok(scan) => scan,
            Err(LogScanError::UnrecognizedFormat { detail }) => {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": { "code": "unrecognized_format", "message": detail }
                });
                println!("{}", serde_json::to_string(&envelope).unwrap_or_default());
                process::exit(1);
            }
            Err(err @ LogScanError::Read { .. }) => {
                return Err(anyhow::anyhow!(err.to_string()))
                    .with_context(|| format!("failed to scan log file {}", log_path.display()));
            }
        };

    let producer_envelope =
        log_graph::log_importer_producer(scan.source_format_version, &transaction_time);
    let mut graph = Graph::new();
    for record in scan.records {
        graph.push(record);
    }
    let graph = graph.stamp_producer(&producer_envelope);
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize log graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write log graph JSONL to {}", out.display()))?;

    // Surface exemplar-cap diagnostics to stderr (machine-readable), never silent.
    for diagnostic in &scan.diagnostics {
        eprintln!("{}", serde_json::to_string(diagnostic).unwrap_or_default());
    }

    // Protected raw-artifact capture (issue #321): store the POST-REDACTION log
    // bytes as a `log_payload` blob. The graph never holds the handle.
    if protected_raw_artifacts {
        let store_dir = protected_store.expect("validated present above");
        let producer_id = producer.expect("validated present above");
        let captured_at = captured_at_override.unwrap_or(&transaction_time);

        // Materialize redacted bytes on demand; raw bytes never leave the helper.
        let redacted = match log_graph::redacted_source_bytes(log_path) {
            Ok(b) => b,
            // The scan already succeeded above, so a read/format error here is a
            // race; report it as a machine-readable capture failure, never raw
            // bytes.
            Err(err) => {
                exit_capture_error(
                    "store_io_error",
                    &serde_json::json!({
                        "message": format!("failed to materialize redacted log bytes: {err}")
                    }),
                    3,
                );
            }
        };
        let source_rel = log_graph::source_relative_path(repo_path, log_path);
        let store = ProtectedStore::new(store_dir);
        match store.capture_bytes(
            ProtectedPayloadClass::LogPayload,
            &source_rel,
            &redacted,
            producer_id,
            env!("CARGO_PKG_VERSION"),
            captured_at,
            true,
        ) {
            Ok(report) => {
                // Report only non-sensitive metadata (handle, hash, byte count,
                // class) — never raw bytes or secrets.
                let entry = &report.entries[0];
                let envelope = serde_json::json!({
                    "ok": true,
                    "protected_capture": {
                        "handle": entry.handle,
                        "content_hash": entry.content_hash,
                        "byte_len": entry.byte_len,
                        "source_class": ProtectedPayloadClass::LogPayload.as_str(),
                        "stored": entry.stored,
                    }
                });
                println!("{}", serde_json::to_string(&envelope).expect("infallible"));
            }
            Err(e) => {
                exit_capture_error(
                    "store_io_error",
                    &serde_json::json!({
                        "message": format!(
                            "protected store I/O failed at {}: {e}",
                            store_dir.display()
                        )
                    }),
                    3,
                );
            }
        }
    }

    Ok(())
}
