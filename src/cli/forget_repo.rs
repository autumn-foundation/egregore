use super::*;

/// Implements `eg forget-repo <selector>` (issue #248): logically evict every
/// record belonging to ONE repository from a shared multi-repo embedded store,
/// across every domain, leaving co-resident repositories byte-identical.
///
/// Dry-run is the DEFAULT (no `--confirm`): the command is strictly read-only,
/// never takes the write lease, reads a throwaway snapshot copy, and prints the
/// eviction PLAN. `--confirm` opens the real store (write lease → inherits the
/// `store_contended` refusal), writes one eviction event plus one tombstone per
/// attributed record, and prints the same envelope shape with an `evicted`
/// action. Selector-resolution failures print a machine-readable JSON envelope
/// on stderr and exit 2 (unknown / ambiguous); malformed request fields exit 1.
/// See `docs/cli/forget-repo.md`.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn forget_repo_cmd(
    selector: &str,
    data_dir: &Path,
    reason: String,
    evicted_by: String,
    transaction_time: Option<String>,
    confirm: bool,
) -> Result<()> {
    let req = crate::repo_evict::EvictionRequest {
        selector: selector.to_owned(),
        reason,
        evicted_by,
        transaction_time,
    };

    if !confirm {
        // Dry-run: strictly read-only. Read a throwaway snapshot copy (the same
        // mechanism `eg inspect --data-dir` uses) so no write lease is taken and
        // the store is left byte-for-byte untouched. The repository identity node
        // survives eviction, so the current-state view still resolves an
        // already-evicted repository for the no-op report.
        let records = load_records_from_data_dir_readonly(data_dir)?;
        let resolution_records = load_records_from_db_history_readonly(data_dir)?;
        let plan = match crate::repo_evict::plan_eviction(&records, &resolution_records, &req) {
            Ok(plan) => plan,
            Err(error) => {
                eprintln!("{}", error.to_json());
                std::process::exit(error.exit_code());
            }
        };
        let action = if plan.already_evicted {
            "already_evicted"
        } else if plan.repair {
            // A prior eviction event exists but attributed records are live
            // again; a `--confirm` would re-issue tombstones without a second
            // event. Disclose it honestly rather than a false `already_evicted`.
            "repair_needed"
        } else {
            "dry_run"
        };
        println!("{}", serde_json::to_string(&plan.to_envelope(action))?);
        return Ok(());
    }

    // `--confirm`: open the real store with the write lease (inherits the
    // `store_contended` refusal when the daemon holds it). The current-state view
    // suffices: the repository identity node survives eviction, so an
    // already-evicted repository still resolves for the idempotent no-op.
    validate_existing_embedded_store(data_dir)?;
    let mut sink = EmbeddedAletheiaSink::open(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
    // History-inclusive view: after a prior eviction the identity node is
    // tombstoned out of `records`, but survives here (eviction tombstones stripped
    // inside `plan_eviction`) so an already-evicted selector still resolves for the
    // idempotent no-op.
    let resolution_records = sink
        .read_all_records_including_superseded()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;

    let plan = match crate::repo_evict::plan_eviction(&records, &resolution_records, &req) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("{}", error.to_json());
            std::process::exit(error.exit_code());
        }
    };

    let action = if plan.already_evicted {
        "already_evicted"
    } else {
        let generated = crate::repo_evict::eviction_records(&plan);
        let report = ingest_records(&generated, &mut sink);
        if !report.is_success() {
            for failure in &report.failures {
                eprintln!("{}: {}", failure.record_id, failure.message);
            }
            anyhow::bail!("failed to write eviction records to store");
        }
        sink.persist_indexes()
            .with_context(|| format!("failed to persist embedded store {}", data_dir.display()))?;
        // A repair re-issues tombstones for records that a prior eviction event
        // named but that are live again; no second event is written. Report it
        // distinctly from a fresh eviction.
        if plan.repair { "repaired" } else { "evicted" }
    };

    println!("{}", serde_json::to_string(&plan.to_envelope(action))?);
    Ok(())
}
