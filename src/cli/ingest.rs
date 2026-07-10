use super::*;

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

pub(crate) fn ingest(
    graph: &Path,
    adapter: IngestAdapter,
    data_dir: Option<&Path>,
    agent_id: &str,
    session_id: &str,
    idempotency_key: Option<&str>,
    #[cfg(feature = "embeddings")] embed: bool,
) -> Result<()> {
    #[cfg(not(feature = "embedded-aletheiadb"))]
    let _ = (data_dir, agent_id, session_id, idempotency_key);

    #[cfg(feature = "embeddings")]
    if embed && adapter != IngestAdapter::Embedded {
        anyhow::bail!("--embed requires --adapter embedded");
    }

    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    let records = records_from_jsonl(&jsonl).context("failed to parse graph JSONL")?;

    let report = match adapter {
        IngestAdapter::DryRun => {
            let mut sink = DryRunSink::default();
            ingest_records(&records, &mut sink)
        }
        #[cfg(feature = "embedded-aletheiadb")]
        IngestAdapter::Embedded => {
            let data_dir = data_dir.map_or_else(|| PathBuf::from(".egregore"), Path::to_path_buf);
            #[cfg(feature = "embeddings")]
            let mut sink = if embed {
                let (vectors, dimensions) = generate_embeddings(&records)?;
                EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, dimensions)
                    .map_err(|error| embedded_write_open_error(&data_dir, error))?
            } else {
                EmbeddedAletheiaSink::open(&data_dir)
                    .map_err(|error| embedded_write_open_error(&data_dir, error))?
            };
            #[cfg(not(feature = "embeddings"))]
            let mut sink = EmbeddedAletheiaSink::open(&data_dir)
                .map_err(|error| embedded_write_open_error(&data_dir, error))?;
            let report = ingest_records(&records, &mut sink);
            if report.is_success() {
                sink.persist_indexes().with_context(|| {
                    format!("failed to persist embedded store {}", data_dir.display())
                })?;
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
