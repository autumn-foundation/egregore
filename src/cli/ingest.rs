use super::*;

/// Fatal-refusal exit code for an ingest that would (or did) overflow the
/// configured `AletheiaDB` string-interner cap (issue #439).
///
/// `eg ingest` otherwise uses only 0 (every record ingested) and 1 (a generic
/// per-record failure, surfaced via `anyhow` through `main`). 2 has no prior
/// ingest meaning, so it is reserved here for the distinct "capacity exceeded"
/// fatal class — both the preflight refusal and a real write/persist overflow.
/// Documented in `docs/cli/ingest.md`.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) const INGEST_CAPACITY_EXIT_CODE: i32 = 2;

/// Stable machine code carried by every ingest capacity-overflow refusal.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) const INGEST_CAPACITY_EXCEEDED_CODE: &str = "ingest_capacity_exceeded";

/// Human-readable + machine-readable text shared by the preflight and runtime
/// capacity refusals: names the upstream cap and the workarounds.
#[cfg(feature = "embedded-aletheiadb")]
const INGEST_CAPACITY_MESSAGE: &str = "ingest would overflow AletheiaDB's process-global string-interner cap of \
     10000000 entries (the cap Egregore configures on every embedded store via \
     PersistenceConfig.max_interned_strings); the interner is process-global and \
     read once at open, so the budget is shared by every store this process \
     opens";

#[cfg(feature = "embedded-aletheiadb")]
const INGEST_CAPACITY_WORKAROUND: &str = "split the graph into smaller per-crate / per-subsystem ingests, or query \
     the JSONL directly with the `--graph` query path (which needs no embedded \
     store)";

/// Prints the machine-readable capacity envelope on stdout, a one-line human
/// summary on stderr, and exits with [`INGEST_CAPACITY_EXIT_CODE`].
///
/// The preflight case (before the store is opened) carries
/// `estimated_distinct_strings`; the runtime case (a real write/persist
/// overflow) carries `records_written`. Both share the stable `code`, `limit`,
/// `record_count`, `message`, and `workaround` fields.
#[cfg(feature = "embedded-aletheiadb")]
fn refuse_ingest_capacity(
    data_dir: &Path,
    record_count: usize,
    estimated_distinct_strings: Option<u64>,
    records_written: Option<usize>,
    detail: Option<&str>,
) -> ! {
    let mut error = serde_json::json!({
        "code": INGEST_CAPACITY_EXCEEDED_CODE,
        "limit": MAX_INTERNED_STRINGS,
        "record_count": record_count,
        "message": INGEST_CAPACITY_MESSAGE,
        "workaround": INGEST_CAPACITY_WORKAROUND,
        "data_dir": data_dir.display().to_string(),
    });
    let map = error
        .as_object_mut()
        .expect("capacity error envelope is a JSON object");
    if let Some(estimate) = estimated_distinct_strings {
        map.insert("estimated_distinct_strings".to_owned(), estimate.into());
    }
    if let Some(written) = records_written {
        map.insert("records_written".to_owned(), written.into());
    }
    if let Some(detail) = detail {
        map.insert("detail".to_owned(), detail.into());
    }
    let envelope = serde_json::json!({ "ok": false, "error": error });
    println!("{envelope}");
    match estimated_distinct_strings {
        Some(estimate) => eprintln!(
            "ingest refused: estimated {estimate} distinct interned strings \
             meets AletheiaDB's {MAX_INTERNED_STRINGS} cap; {INGEST_CAPACITY_WORKAROUND} \
             (or re-run with --force to bypass the estimate)"
        ),
        None => eprintln!(
            "ingest aborted: AletheiaDB reached its {MAX_INTERNED_STRINGS} \
             string-interner cap during write/persist; {INGEST_CAPACITY_WORKAROUND}"
        ),
    }
    std::process::exit(INGEST_CAPACITY_EXIT_CODE);
}

/// Maps an embedded open failure on the ingest write path into a CLI error.
///
/// A write-lease contention refusal (issue #200) additionally prints the
/// structured `{"ok": false, "error": {...}}` envelope on stdout so agents can
/// machine-parse the `store_contended` contract — the write was refused before
/// any record was persisted, and the remedy is to route concurrent writers
/// through the daemon or retry after the current writer releases the store.
/// Other failures keep the existing human-readable context.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn embedded_write_open_error(data_dir: &Path, error: AdapterError) -> anyhow::Error {
    if let AdapterError::Contended { message, .. } = &error {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": STORE_CONTENDED_CODE,
                "message": message,
                "data_dir": data_dir.display().to_string(),
                "remedy": "route concurrent writers through the daemon (`eg daemon start`, \
                           then re-run with `--adapter daemon`), or retry after the current \
                           writer releases the store",
            },
        });
        println!("{envelope}");
        return anyhow::anyhow!("{error}");
    }
    anyhow::Error::new(error).context(format!(
        "failed to open embedded store {}",
        data_dir.display()
    ))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn ingest(
    graph: &Path,
    adapter: IngestAdapter,
    data_dir: Option<&Path>,
    agent_id: &str,
    session_id: &str,
    idempotency_key: Option<&str>,
    #[cfg(feature = "embeddings")] embed: bool,
    #[cfg(feature = "embedded-aletheiadb")] force: bool,
) -> Result<()> {
    #[cfg(not(feature = "embedded-aletheiadb"))]
    let _ = (data_dir, agent_id, session_id, idempotency_key);

    #[cfg(feature = "embeddings")]
    if embed && adapter != IngestAdapter::Embedded {
        anyhow::bail!("--embed requires --adapter embedded");
    }

    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    #[cfg_attr(not(feature = "embeddings"), allow(unused_mut))]
    let mut records = records_from_jsonl(&jsonl).context("failed to parse graph JSONL")?;

    let report = match adapter {
        IngestAdapter::DryRun => {
            let mut sink = DryRunSink::default();
            ingest_records(&records, &mut sink)
        }
        #[cfg(feature = "embedded-aletheiadb")]
        IngestAdapter::Embedded => {
            let data_dir = data_dir.map_or_else(|| PathBuf::from(".egregore"), Path::to_path_buf);
            // Capacity preflight (issue #439): refuse fast BEFORE opening the
            // store when a graph is estimated to overflow the configured
            // string-interner cap, rather than writing for a long time and
            // failing at persist. On AletheiaDB 0.1.1 this was the only safe
            // defense — an overflow made the background persistence thread
            // hot-loop forever. 0.2.0 raised the cap 100x AND removed that retry
            // loop, so this is now an early, better-diagnosed refusal rather
            // than hang avoidance. Skipped under `--force`; a real overflow
            // during write/persist is still fatal below.
            if let Err(refusal) = check_ingest_capacity(&records, force) {
                let PreflightRefusal {
                    estimate,
                    record_count,
                    ..
                } = refusal;
                refuse_ingest_capacity(&data_dir, record_count, Some(estimate), None, None);
            }
            #[cfg(feature = "embeddings")]
            let mut sink = if embed {
                let (vectors, dimensions, model) = generate_embeddings(&records)?;
                let sink =
                    EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, dimensions)
                        .map_err(|error| embedded_write_open_error(&data_dir, error))?;
                // Refuse before writing anything when the index was built by a
                // different model (issue #104): embedding into it again would
                // blend two vector spaces that no ranking can compare.
                refuse_conflicting_index_identity(&sink, &model)?;
                // Record which model produced the queryable vector index so
                // `eg query semantic` can prove the query embedder shares the
                // index's vector space instead of ranking silently across
                // incompatible ones. Keyed on a fixed ID, so a repeated ingest
                // supersedes rather than accumulating a second identity.
                records.push(crate::embeddings::embedding_index_identity_record(&model));
                sink
            } else {
                EmbeddedAletheiaSink::open(&data_dir)
                    .map_err(|error| embedded_write_open_error(&data_dir, error))?
            };
            #[cfg(not(feature = "embeddings"))]
            let mut sink = EmbeddedAletheiaSink::open(&data_dir)
                .map_err(|error| embedded_write_open_error(&data_dir, error))?;
            let report = ingest_records(&records, &mut sink);
            // A capacity overflow surfaced as a per-record write failure is
            // fatal (never a generic exit-1 failure): a partial store whose
            // interner is at the cap cannot be persisted.
            if let Some(failure) = report.failures.iter().find(|failure| {
                crate::adapters::is_string_interner_capacity_error(&failure.message)
            }) {
                refuse_ingest_capacity(
                    &data_dir,
                    records.len(),
                    None,
                    Some(report.succeeded),
                    Some(&failure.message),
                );
            }
            if report.is_success() {
                match sink.persist_indexes() {
                    Ok(()) => {}
                    Err(AdapterError::CapacityExceeded { detail, .. }) => {
                        refuse_ingest_capacity(
                            &data_dir,
                            records.len(),
                            None,
                            Some(report.succeeded),
                            Some(&detail),
                        );
                    }
                    Err(error) => {
                        return Err(anyhow::Error::new(error).context(format!(
                            "failed to persist embedded store {}",
                            data_dir.display()
                        )));
                    }
                }
            }
            report
        }
        #[cfg(feature = "embedded-aletheiadb")]
        IngestAdapter::Daemon => {
            let data_dir = data_dir.map_or_else(|| PathBuf::from(".egregore"), Path::to_path_buf);
            let idempotency_key =
                idempotency_key.context("--idempotency-key is required for --adapter daemon")?;
            let client = DaemonClient::from_data_dir(&data_dir)
                .with_context(|| format!("failed to load daemon for {}", data_dir.display()))?;
            let response =
                client.ingest_records(&records, agent_id, session_id, idempotency_key)?;
            println!("attempted: {}", response.attempted);
            println!("succeeded: {}", response.succeeded);
            println!("failed: {}", response.failed);
            println!("idempotent: {}", response.idempotent);
            if response.failed == 0 {
                return Ok(());
            }
            for failure in &response.failures {
                eprintln!("{}: {}", failure.record_id, failure.message);
            }
            anyhow::bail!("ingest failed for {} records", response.failed);
        }
    };

    println!("attempted: {}", report.attempted);
    println!("succeeded: {}", report.succeeded);
    println!("failed: {}", report.failed);

    if report.is_success() {
        Ok(())
    } else {
        for failure in &report.failures {
            eprintln!("{}: {}", failure.record_id, failure.message);
        }
        anyhow::bail!("ingest failed for {} records", report.failed);
    }
}
