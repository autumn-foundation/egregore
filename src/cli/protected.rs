use super::*;

// ── eg protected ──────────────────────────────────────────────────────────────

/// Implements `eg protected get`: retrieves the verified payload to `out` (a
/// file) or stdout without buffering the whole payload in memory.
///
/// The bytes are ALWAYS staged to a temp and only released to the destination
/// after a fully verified copy — for `--out` to a uniquely named, exclusively
/// created temp in the destination directory (never follows a symlink) that is
/// renamed into place, and for stdout to an ANONYMOUS (unlinked) temp that is
/// rewound and streamed out.  This preserves verify-before-release for both
/// destinations (a failed get never truncates a `--out` file and never emits
/// unverified bytes to stdout).  The `--out` temp is removed on every path,
/// including the `process::exit` error paths; the anonymous stdout temp leaves
/// no named entry to leak and is reclaimed by the OS on process exit.
/// Exits the process on any failure with the documented JSON envelope/exit code.
pub(crate) fn protected_get_cmd(handle: &str, store: &Path, operator: &str, out: Option<&Path>) {
    use crate::protected::{GetStreamError, ProtectedStore};
    let ps = ProtectedStore::new(store);

    let exit_get_error = |e: crate::protected::GetError| -> ! {
        let is_not_found = e.code() == "payload_not_found";
        eprintln!("{}", e.to_json()); // to_json() is not Display; format is deliberate
        process::exit(if is_not_found { 2 } else { 1 });
    };
    let exit_output_error = |code: &str, message: String| -> ! {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "detail": { "message": message } }
        });
        eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
        process::exit(1);
    };
    // Error code + human label for the destination.
    let (err_code, dest_label): (&str, String) = out.map_or_else(
        || ("stdout_write_error", "stdout".to_owned()),
        |p| ("output_write_error", p.display().to_string()),
    );

    // Surfaces the RETRIEVAL diagnostic first when temp creation fails — verify
    // into a discard sink so a store/auth/malformed handle is reported as such
    // rather than masked by a staging error.  Returns only when retrieval would
    // have succeeded; otherwise exits with the get error.
    let stage_failed = |create_err: std::io::Error| -> ! {
        let mut sink = std::io::sink();
        match ps.get_to_writer(handle, operator, &mut sink) {
            Err(GetStreamError::Get(e)) => exit_get_error(e),
            // Retrieval succeeded (sink writes never fail), so the failure is
            // genuinely the staging destination.
            _ => exit_output_error(
                err_code,
                format!("failed to stage bytes for {dest_label}: {create_err}"),
            ),
        }
    };

    if let Some(out_path) = out {
        // --out: stage a NAMED temp in the destination directory so the final
        // release is an in-directory atomic rename onto the destination.  Keep
        // the verified `NamedTempFile` (and its open descriptor) BOUND through
        // the release step: do not convert it to a bare path and reopen, which
        // would let another local process swap the staging entry between
        // verification and release.  `process::exit` skips the destructor, so
        // the temp is removed explicitly (via `close()`/`PersistError`).
        let stage_dir = out_path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf);
        let mut tmp = match tempfile::Builder::new()
            .prefix(".eg-")
            .suffix(".partial")
            .tempfile_in(&stage_dir)
        {
            Ok(t) => t,
            Err(create_err) => stage_failed(create_err),
        };
        match ps.get_to_writer(handle, operator, tmp.as_file_mut()) {
            Ok(_) => {
                // Atomically persist the verified temp onto the destination
                // (rename of the SAME file object, replacing an existing file).
                if let Err(e) = tmp.persist(out_path) {
                    let _ = e.file.close(); // remove the staged temp
                    exit_output_error(
                        err_code,
                        format!("failed to write bytes to {dest_label}: {}", e.error),
                    );
                }
            }
            Err(GetStreamError::Get(e)) => {
                let _ = tmp.close();
                exit_get_error(e);
            }
            Err(GetStreamError::Output(e)) => {
                let _ = tmp.close();
                exit_output_error(
                    err_code,
                    format!("failed to write bytes to {dest_label}: {e}"),
                );
            }
        }
    } else {
        // stdout: stage into an ANONYMOUS temp file (unlinked at creation) so a
        // crash or kill never leaves a `.eg-*.partial` entry behind in the
        // system temp dir.  Verify-before-release still holds — nothing reaches
        // stdout until the full verified copy lands in the temp, which is then
        // rewound and streamed out.  The anonymous inode is reclaimed by the OS
        // on process exit, so no explicit cleanup is needed on the exit paths.
        let mut tmp = match tempfile::tempfile() {
            Ok(t) => t,
            Err(create_err) => stage_failed(create_err),
        };
        match ps.get_to_writer(handle, operator, &mut tmp) {
            Ok(_) => {
                let result = (|| -> std::io::Result<()> {
                    use std::io::Seek as _;
                    tmp.seek(std::io::SeekFrom::Start(0))?;
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    std::io::copy(&mut tmp, &mut lock)?;
                    Ok(())
                })();
                if let Err(e) = result {
                    exit_output_error(
                        err_code,
                        format!("failed to write bytes to {dest_label}: {e}"),
                    );
                }
            }
            Err(GetStreamError::Get(e)) => exit_get_error(e),
            Err(GetStreamError::Output(e)) => exit_output_error(
                err_code,
                format!("failed to write bytes to {dest_label}: {e}"),
            ),
        }
    }
}

/// Dispatches `eg protected <subcommand>` (issue #60).
pub(crate) fn protected_cmd(subcommand: ProtectedSubcommand) -> Result<()> {
    use crate::protected::ProtectedStore;
    match subcommand {
        ProtectedSubcommand::Capture {
            manifest,
            store,
            protected_raw_artifacts,
            producer,
            captured_at,
        } => protected_capture_cmd(
            &manifest,
            &store,
            protected_raw_artifacts,
            producer.as_deref(),
            captured_at.as_deref(),
        ),
        ProtectedSubcommand::Get {
            handle,
            store,
            operator,
            out,
        } => {
            protected_get_cmd(&handle, &store, &operator, out.as_deref());
            Ok(())
        }
        ProtectedSubcommand::List { store } => {
            let ps = ProtectedStore::new(&store);
            let handles = match ps.list() {
                Ok(h) => h,
                Err(e) => {
                    let envelope = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": "store_io_error",
                            "detail": {
                                "message": format!(
                                    "failed to read protected store at {}: {e}",
                                    store.display()
                                )
                            }
                        }
                    });
                    eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
                    process::exit(1);
                }
            };
            let envelope = serde_json::json!({
                "ok": true,
                "count": handles.len(),
                "handles": handles,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&envelope)
                    .context("failed to serialise list response")?
            );
            Ok(())
        }
    }
}

/// Implements `eg protected capture`.
#[allow(clippy::too_many_lines)]
pub(crate) fn protected_capture_cmd(
    manifest_path: &Path,
    store_path: &Path,
    enabled: bool,
    producer: Option<&str>,
    captured_at_override: Option<&str>,
) -> Result<()> {
    use crate::protected::{CaptureEntry, ProtectedStore};

    // Validate: enabled mode requires a non-empty --producer.
    if enabled && producer.is_none() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "missing_field",
                "detail": {
                    "field": "producer",
                    "message": "--producer is required when --protected-raw-artifacts is set"
                }
            }
        });
        eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
        process::exit(1);
    }
    if enabled && producer.is_some_and(|p| p.trim().is_empty()) {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "invalid_field",
                "detail": {
                    "field": "producer",
                    "message": "--producer must not be empty when --protected-raw-artifacts is set"
                }
            }
        });
        eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
        process::exit(1);
    }

    // Read manifest — emit JSON envelope on failure so automation can distinguish
    // manifest errors from other stderr output.
    //
    // Emits the `manifest_read_error` envelope and exits 1.  Defined as a
    // closure so the regular-file/size guard and the read error path share one
    // emission site.
    let emit_manifest_error = |message: String| -> ! {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "manifest_read_error",
                "detail": { "message": message }
            }
        });
        eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
        process::exit(1);
    };

    // Read the manifest bound to a single no-follow, regular-file, size-capped
    // descriptor.  Opening once and reading that descriptor (rather than
    // stat-then-reopen) closes the TOCTOU window: a `--manifest` in a writable
    // location cannot be swapped for a FIFO, device, symlink, or much larger
    // file between a check and the read, so capture cannot be made to block or
    // allocate unbounded memory before emitting the JSON diagnostic.
    let manifest_content = match crate::protected::read_capped_regular_file(
        manifest_path,
        crate::protected::MAX_STORE_FILE_BYTES,
    ) {
        Ok(c) => c,
        Err(e) => emit_manifest_error(format!(
            "failed to read capture manifest at {}: {e}",
            manifest_path.display()
        )),
    };
    let mut entries: Vec<CaptureEntry> = Vec::new();
    for (i, line) in manifest_content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: CaptureEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(e) => {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": "invalid_manifest",
                        "detail": {
                            "line": i + 1,
                            "message": format!(
                                "manifest line {}: failed to parse JSON: {e}",
                                i + 1
                            )
                        }
                    }
                });
                eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
                process::exit(1);
            }
        };
        entries.push(entry);
    }

    let producer_id = producer.unwrap_or("preview");
    let producer_version = env!("CARGO_PKG_VERSION");
    let ts: String;
    let captured_at = if let Some(ov) = captured_at_override {
        ov
    } else {
        ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        &ts
    };

    let ps = ProtectedStore::new(store_path);
    let report = match ps.capture(
        &entries,
        producer_id,
        producer_version,
        captured_at,
        enabled,
    ) {
        Ok(r) => r,
        Err(e) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "store_io_error",
                    "detail": {
                        "message": format!(
                            "protected store I/O failed at {}: {e}",
                            store_path.display()
                        )
                    }
                }
            });
            eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
            process::exit(1);
        }
    };

    let envelope = serde_json::json!({
        "ok": true,
        "enabled": report.enabled,
        "stored_count": report.stored_count,
        "skipped_count": report.skipped_count,
        "entries": report.entries,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&envelope).context("failed to serialise capture response")?
    );
    Ok(())
}
