use super::*;

#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn daemon(action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start {
            data_dir,
            host,
            port,
            write_queue_capacity,
        } => {
            let mut config = DaemonConfig::new(data_dir);
            config.host = host;
            config.port = port;
            config.write_queue_capacity = write_queue_capacity;
            let metadata = crate::daemon::start_background(&config)?;
            println!("daemon started at {}", metadata.address);
            Ok(())
        }
        DaemonAction::Run {
            data_dir,
            host,
            port,
            write_queue_capacity,
        } => {
            let mut config = DaemonConfig::new(data_dir);
            config.host = host;
            config.port = port;
            config.write_queue_capacity = write_queue_capacity;
            crate::daemon::run_foreground(&config)
        }
        DaemonAction::Status { data_dir } => {
            let Some(metadata) = crate::daemon::active_metadata(&data_dir)? else {
                anyhow::bail!("daemon not running for {}", data_dir.display());
            };
            println!("daemon running at {}", metadata.address);
            let client = crate::daemon::DaemonClient::new(metadata);
            let status = client.status()?;
            render_daemon_pressure(&status);
            Ok(())
        }
        DaemonAction::Stop { data_dir } => {
            crate::daemon::stop(&data_dir)?;
            println!("daemon stopped");
            Ok(())
        }
    }
}

/// Renders the daemon write-admission pressure block in human-readable form.
///
/// The HTTP `GET /v1/status` JSON remains the stable machine-readable contract;
/// this rendering is for operators reading the terminal.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn render_daemon_pressure(status: &serde_json::Value) {
    let pressure = &status["pressure"];
    let state = pressure["state"].as_str().unwrap_or("unknown");
    let depth = pressure["queue_depth"].as_u64().unwrap_or(0);
    let capacity = pressure["queue_capacity"].as_u64().unwrap_or(0);
    let rejections = pressure["total_rejections"].as_u64().unwrap_or(0);
    println!("pressure: {state} (write queue {depth}/{capacity}, {rejections} rejected)");
    match state {
        "saturated" => {
            let retry_after_ms = pressure["retry_after_ms"].as_u64().unwrap_or(500);
            println!(
                "  daemon is alive but backpressuring: wait at least {retry_after_ms} ms, then retry the same write with its original idempotency key."
            );
            println!("  do not bypass the daemon with direct embedded writes.");
        }
        "busy" => {
            println!("  daemon is admitting writes; no retry action needed.");
        }
        _ => {
            println!("  daemon is idle and accepting writes.");
        }
    }
}
