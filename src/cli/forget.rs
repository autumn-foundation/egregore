use super::*;

/// Implements `eg forget` (issue #231): logical, auditable retraction of one
/// persisted record from every transaction-time-current read surface.
///
/// Reads the current store view, resolves the retraction through the pure
/// [`crate::forget`] logic, persists the generated retraction-event node and
/// tombstone through the adapter, and prints a machine-readable JSON envelope.
/// Failures print a JSON envelope to stderr and exit 1 (refused or malformed)
/// or 2 (handle not found).
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn forget_cmd(
    handle: &str,
    data_dir: &Path,
    reason: String,
    retracted_by: String,
    transaction_time: Option<String>,
) -> Result<()> {
    validate_existing_embedded_store(data_dir)?;
    let mut sink = EmbeddedAletheiaSink::open(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;

    let req = crate::forget::ForgetRequest {
        handle: handle.to_owned(),
        reason,
        retracted_by,
        transaction_time,
    };
    let outcome = match crate::forget::retract_from_records(&records, &req) {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("{}", error.to_json());
            std::process::exit(error.exit_code());
        }
    };

    let (action, event) = match outcome {
        crate::forget::ForgetOutcome::Retracted {
            event,
            records: generated,
        } => {
            let report = ingest_records(&generated, &mut sink);
            if !report.is_success() {
                for failure in &report.failures {
                    eprintln!("{}: {}", failure.record_id, failure.message);
                }
                anyhow::bail!("failed to write retraction records to store");
            }
            sink.persist_indexes().with_context(|| {
                format!("failed to persist embedded store {}", data_dir.display())
            })?;
            ("retracted", event)
        }
        crate::forget::ForgetOutcome::AlreadyRetracted { event } => ("already_retracted", event),
    };

    let envelope = serde_json::json!({
        "ok": true,
        "action": action,
        "retraction": event,
    });
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}
