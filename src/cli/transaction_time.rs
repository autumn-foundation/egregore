use super::*;

// ---------------------------------------------------------------------------
// query symbol --tx-as-of <instant> (issue #66)
// ---------------------------------------------------------------------------

/// A single transaction-time query result row.
///
/// Carries only redaction-safe handles (AC8): record ID, schema version, trust
/// and domain class, valid-time fields, the transaction-time handle, and a
/// citable source handle (`repo_relative_path` + `span`). No summaries, bodies,
/// or raw payloads are emitted.
#[derive(Serialize)]
pub(crate) struct TxSymbolRow<'a> {
    record_id: &'a str,
    schema_version: u32,
    name: &'a str,
    kind: &'static str,
    domain: &'a str,
    trust_class: &'static str,
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time_source: Option<&'a str>,
    transaction_time: &'a str,
    /// Stable `Repository` record ID owning this row; absent when the store
    /// carries no repository topology for the record (legacy graphs).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    /// Human-usable repository identity handle (e.g. `owner/name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    extraction_completeness: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<Vec<DiagnosticRef<'a>>>,
}

/// Response envelope for `eg query symbol --tx-as-of`.
#[derive(Serialize)]
pub(crate) struct TxSymbolEnvelope<'a> {
    ok: bool,
    verb: &'static str,
    name: &'a str,
    tx_as_of: &'a str,
    // Always serialized (as `null` when no valid-time axis) so the JSON schema
    // matches the daemon path and the documented envelope.
    as_of: Option<&'a str>,
    snapshot: &'a str,
    records: Vec<TxSymbolRow<'a>>,
    diagnostics: Vec<query::TxDiagnostic>,
    page: TxPage,
}

#[derive(Serialize)]
pub(crate) struct TxPage {
    cursor: Option<()>,
    has_more: bool,
    returned: usize,
}

/// Builds a redaction-safe result row from a selected Symbol record.
pub(crate) fn tx_symbol_row<'a>(
    record: &'a GraphRecord,
    index: &'a query::RepositoryIndex,
    all_records: &'a [GraphRecord],
    deleted: &std::collections::BTreeSet<&str>,
) -> Option<TxSymbolRow<'a>> {
    let GraphRecord::Node {
        id,
        kind: NodeKind::Symbol,
        schema_version,
        name,
        repo_relative_path,
        span,
        temporal,
        valid_time,
        valid_time_source,
        domain,
        ..
    } = record
    else {
        return None;
    };
    let (completeness, _) = repo_relative_path
        .as_deref()
        .map_or(("complete", None), |path| {
            get_file_diagnostics(all_records, path, deleted)
        });
    // Prefer the explicit node `domain` field; otherwise fall back to the
    // kind-derived domain (a `'static str`).
    let domain_str = domain
        .as_deref()
        .unwrap_or_else(|| crate::schema_version::domain_for_node_kind("Symbol"));
    let repository_id = index.owner_of(id);
    Some(TxSymbolRow {
        record_id: id,
        schema_version: *schema_version,
        name: name.as_deref().unwrap_or(""),
        kind: "Symbol",
        domain: domain_str,
        trust_class: trust_class_for(record),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        valid_time: temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref()),
        valid_time_source: temporal
            .as_ref()
            .and_then(|t| t.valid_time_source.as_deref())
            .or(valid_time_source.as_deref()),
        transaction_time: query::record_transaction_time(record).unwrap_or(""),
        repository_id,
        repository: repository_id.and_then(|repo| index.display_of(repo)),
        extraction_completeness: completeness,
        diagnostics: None,
    })
}

/// Prints a `{ "ok": false, "error": { code, message } }` envelope to stdout.
pub(crate) fn print_tx_error(code: &str, message: &str) -> Result<()> {
    let envelope = serde_json::json!({
        "ok": false,
        "error": { "code": code, "message": message }
    });
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn query_symbol_tx_as_of(
    records: &[GraphRecord],
    name: &str,
    tx_as_of: &str,
    as_of: Option<&str>,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
) -> Result<()> {
    // Repository scope applies to the record set BEFORE temporal resolution,
    // not to the row list afterwards: a forked repository's descendant commits
    // must not drive this repository's removal or supersession logic
    // (issue #67). Store-wide transaction bounds stay global so out-of-range
    // diagnostics keep reflecting the whole store.
    let scoped_records: Vec<GraphRecord> = selected_repo
        .map(|repo| {
            records
                .iter()
                .filter(|r| {
                    matches!(r, GraphRecord::Node { .. }) && index.owner_of(r.id()) == Some(repo)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    // The CLI loads the entire `--graph` file, so the unscoped store-wide
    // range is just the full record set: pass `None` to let the resolver
    // derive it.
    let (effective_records, store_bounds) = if selected_repo.is_some() {
        (
            scoped_records.as_slice(),
            query::store_transaction_bounds(records),
        )
    } else {
        (records, None)
    };
    match query::symbol_as_of_transaction_time(
        effective_records,
        name,
        tx_as_of,
        as_of,
        store_bounds,
    ) {
        Err(err) => {
            print_tx_error(&err.code, &err.message)?;
            std::process::exit(1);
        }
        Ok(result) => {
            let deleted = current_deleted_ids(records);
            let rows: Vec<TxSymbolRow<'_>> = result
                .records
                .iter()
                .filter_map(|r| tx_symbol_row(r, index, records, &deleted))
                .collect();
            let envelope = TxSymbolEnvelope {
                ok: true,
                verb: "symbol",
                name,
                tx_as_of,
                as_of,
                // The view's transaction-time handle is the requested instant.
                snapshot: tx_as_of,
                page: TxPage {
                    cursor: None,
                    has_more: false,
                    returned: rows.len(),
                },
                records: rows,
                diagnostics: result.diagnostics,
            };
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => {
                    for row in &envelope.records {
                        let path = row.repo_relative_path.unwrap_or("(unknown)");
                        let line = row.span.map_or(0, |s| s.start_line);
                        println!(
                            "{} (Symbol) @ {path}:{line} tx={}",
                            row.name, row.transaction_time
                        );
                    }
                    for diag in &envelope.diagnostics {
                        println!("# {}: {}", diag.code, diag.message);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn query_symbol_tx_via_daemon(
    name: &str,
    data_dir: &Path,
    tx_as_of: &str,
    as_of: Option<&str>,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // Validate timestamps client-side first so a malformed instant produces the
    // same `invalid_timestamp` envelope as the non-daemon path, before connecting.
    if let Err(e) = chrono::DateTime::parse_from_rfc3339(tx_as_of) {
        print_tx_error(
            "invalid_timestamp",
            &format!("invalid --tx-as-of timestamp '{tx_as_of}': {e}"),
        )?;
        std::process::exit(1);
    }
    if let Some(vt) = as_of
        && let Err(e) = chrono::DateTime::parse_from_rfc3339(vt)
    {
        print_tx_error(
            "invalid_timestamp",
            &format!("invalid --as-of timestamp '{vt}': {e}"),
        )?;
        std::process::exit(1);
    }

    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let mut as_of_obj = serde_json::Map::new();
    as_of_obj.insert("transaction_time".to_owned(), serde_json::json!(tx_as_of));
    if let Some(vt) = as_of {
        as_of_obj.insert("valid_time".to_owned(), serde_json::json!(vt));
    }
    let as_of_value = serde_json::Value::Object(as_of_obj);
    // Translate any daemon-side rejection into the documented machine-readable
    // CLI error envelope on stdout instead of an anyhow string on stderr.
    let mut verb_params = serde_json::json!({ "name": name });
    if let Some(repo) = repo {
        verb_params["repo"] = serde_json::json!(repo);
    }
    let result = match client.query_verb_raw_with_as_of(
        "symbol_by_name",
        &verb_params,
        Some(&as_of_value),
    ) {
        Ok(r) => r,
        Err(e) => {
            let e = surface_daemon_selector_rejection(e, repo);
            print_tx_error("daemon_query_error", &e.to_string())?;
            std::process::exit(1);
        }
    };
    // `query_verb_raw_with_as_of` returns only the daemon `result` object, whose
    // record rows already carry the tx handles. Reconstruct the same CLI
    // `--tx-as-of` envelope the non-daemon path emits (top-level `ok`, `verb`,
    // `name`, `as_of`, `snapshot`) so JSON consumers see one shape regardless of
    // `--daemon`.
    let empty_records = serde_json::json!([]);
    let records = result.get("records").unwrap_or(&empty_records);
    let empty_diags = serde_json::json!([]);
    let diagnostics = result.get("diagnostics").unwrap_or(&empty_diags);
    let default_page = serde_json::json!({ "cursor": null, "has_more": false, "returned": 0 });
    let page = result.get("page").unwrap_or(&default_page);
    let envelope = serde_json::json!({
        "ok": true,
        "verb": "symbol",
        "name": name,
        "tx_as_of": tx_as_of,
        "as_of": as_of,
        "snapshot": tx_as_of,
        "records": records,
        "diagnostics": diagnostics,
        "page": page,
    });
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string(&envelope)?);
        }
        OutputFormat::Text => {
            if let Some(records) = records.as_array() {
                for rec in records {
                    let name = rec.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let path = rec
                        .get("repo_relative_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(unknown)");
                    let tx = rec
                        .get("transaction_time")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    println!("{name} (Symbol) @ {path} tx={tx}");
                }
            }
            // Mirror the non-daemon text path: surface diagnostics so the
            // no-silent-fallback signal is not lost for empty diagnostic-bearing
            // results (e.g. before_first_transaction, no_named_symbol).
            if let Some(diags) = diagnostics.as_array() {
                for diag in diags {
                    let code = diag.get("code").and_then(|v| v.as_str()).unwrap_or("");
                    let message = diag.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    println!("# {code}: {message}");
                }
            }
        }
    }
    Ok(())
}
