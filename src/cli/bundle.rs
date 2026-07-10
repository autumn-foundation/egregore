use super::*;

/// Dispatches `eg bundle <subcommand>` (issue #68).
#[allow(clippy::too_many_lines)]
pub(crate) fn bundle_cmd(subcommand: BundleSubcommand) -> Result<()> {
    match subcommand {
        BundleSubcommand::Export {
            root_selector,
            graph,
            data_dir,
            out,
        } => {
            // For an embedded store, read from a throwaway read-only copy
            let store_copy = data_dir
                .as_ref()
                .map(|dir| match readonly_audit_store(dir) {
                    Ok(pair) => pair,
                    Err(error) => {
                        eprintln!("{error}");
                        std::process::exit(2);
                    }
                });
            let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());

            let records = match load_query_records(graph.as_deref(), effective_data_dir) {
                Ok(records) => records,
                Err(error) => {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "ok": false,
                            "error": {
                                "code": "load_failed",
                                "message": error.to_string()
                            }
                        })
                    );
                    std::process::exit(2);
                }
            };

            let version = env!("CARGO_PKG_VERSION");
            let bundle = match crate::bundle::export_bundle(&records, &root_selector, version) {
                Ok(b) => b,
                Err(error) => {
                    let (exit_code, error_code) = match &error {
                        crate::error::CodegraphError::InvalidArgument { .. } => {
                            (2, "invalid_argument")
                        }
                        _ => (1, "export_failed"),
                    };
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "ok": false,
                            "error": {
                                "code": error_code,
                                "message": error.to_string()
                            }
                        })
                    );
                    std::process::exit(exit_code);
                }
            };

            let json = match serde_json::to_string_pretty(&bundle) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "ok": false,
                            "error": {
                                "code": "serialization_failed",
                                "message": format!("failed to serialize bundle: {e}")
                            }
                        })
                    );
                    std::process::exit(2);
                }
            };
            if let Err(e) = fs::write(&out, &json) {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": "file_write_failed",
                            "message": format!("failed to write bundle to {}: {e}", out.display())
                        }
                    })
                );
                std::process::exit(2);
            }
            Ok(())
        }
        BundleSubcommand::Verify { path, format } => {
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(error) => {
                    let msg = format!("failed to read bundle file: {error}");
                    match format {
                        OutputFormat::Json => {
                            eprintln!(
                                "{}",
                                serde_json::json!({
                                    "ok": false,
                                    "error": {
                                        "code": "file_read_failed",
                                        "message": msg
                                    }
                                })
                            );
                        }
                        OutputFormat::Text => {
                            eprintln!("{msg}");
                        }
                    }
                    std::process::exit(2);
                }
            };
            let bundle: crate::bundle::EvidenceBundle = match serde_json::from_str(&content) {
                Ok(b) => b,
                Err(error) => {
                    let msg = format!("failed to parse bundle JSON: {error}");
                    match format {
                        OutputFormat::Json => {
                            eprintln!(
                                "{}",
                                serde_json::json!({
                                    "ok": false,
                                    "error": {
                                        "code": "parse_failed",
                                        "message": msg
                                    }
                                })
                            );
                        }
                        OutputFormat::Text => {
                            eprintln!("{msg}");
                        }
                    }
                    std::process::exit(2);
                }
            };

            let report = crate::bundle::verify_bundle(&bundle);

            let output = match format {
                OutputFormat::Json => serde_json::to_string_pretty(&report)
                    .map_err(|e| anyhow::anyhow!("failed to serialize report: {e}"))?,
                OutputFormat::Text => {
                    format!(
                        "Verification Verdict: {}\n\n- Integrity: {} ({})\n- Coverage: {} ({})\n- Safety: {} ({})\n",
                        if report.ok { "PASS" } else { "FAIL" },
                        if report.integrity.passed {
                            "PASS"
                        } else {
                            "FAIL"
                        },
                        report.integrity.detail,
                        if report.coverage.passed {
                            "PASS"
                        } else {
                            "FAIL"
                        },
                        report.coverage.detail,
                        if report.safety.passed { "PASS" } else { "FAIL" },
                        report.safety.detail,
                    )
                }
            };
            println!("{output}");

            if !report.ok {
                std::process::exit(1);
            }
            Ok(())
        }
        BundleSubcommand::Inspect { path } => {
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(error) => {
                    eprintln!("failed to read bundle file: {error}");
                    std::process::exit(2);
                }
            };
            let bundle: crate::bundle::EvidenceBundle = match serde_json::from_str(&content) {
                Ok(b) => b,
                Err(error) => {
                    eprintln!("failed to parse bundle JSON: {error}");
                    std::process::exit(2);
                }
            };

            let m = &bundle.manifest;
            println!("Evidence Bundle Manifest:");
            println!("  Root Selector: {}", m.root_selector);
            println!("  Source Query: {}", m.source_query);
            println!("  Repository Identity: {}", m.repository_identity);
            println!("  Egregore Version: {}", m.egregore_version);
            if let Some(s) = &m.snapshot {
                match s {
                    SnapshotHead::Commit { sha } => println!("  Snapshot HEAD Commit: {sha}"),
                    SnapshotHead::NoGit => println!("  Snapshot HEAD: no git"),
                    SnapshotHead::UnbornHead => println!("  Snapshot HEAD: unborn"),
                }
            }
            println!("  Omitted Records: {}", m.omitted_record_counts);
            println!("  Included Records by Trust Class:");
            for (tc, count) in &m.included_record_counts {
                println!("    {tc}: {count}");
            }
            println!("  Root Record IDs Selected: {:?}", m.root_record_ids);
            if !bundle.unresolved_links.is_empty() {
                println!("  Diagnostics (Unresolved Links):");
                for link in &bundle.unresolved_links {
                    let src_id = &link.source_id;
                    let tgt_handle = &link.target_handle;
                    let rel = &link.relation;
                    println!("    - Link from {src_id} to missing {tgt_handle} via {rel}");
                }
            }
            Ok(())
        }
    }
}
