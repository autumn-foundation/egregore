use super::*;

// ---------------------------------------------------------------------------
// covering-tests query (issue #126)
// ---------------------------------------------------------------------------

/// One covering test row in the output.
#[derive(Serialize)]
pub(crate) struct CoveringTestRowJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    hop: usize,
    /// `"direct"` when the test calls the symbol itself (hop 1),
    /// `"transitive"` when it reaches it through other callers (hop > 1).
    coverage: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_resolution: Option<&'static str>,
    path: Vec<TransitivePathStepJson<'a>>,
    trust: &'static str,
}

/// Summary envelope emitted as the first NDJSON line.
#[derive(Serialize)]
pub(crate) struct CoveringTestsHeaderJson<'a> {
    ok: bool,
    handle: &'a str,
    target: TransitiveTargetJson<'a>,
    direction: &'static str,
    edge_labels: [&'static str; 1],
    max_depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    at_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<&'a str>,
    total_covering_tests: usize,
    direct_tests: usize,
    transitive_tests: usize,
    /// Corpus the current-state view read (issue #427):
    /// `head_anchored`/`union`/`commit_pinned`/`single_snapshot`.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: `default`/`explicit_flag`/`selector`.
    corpus_mode_source: &'static str,
    /// One-line human description of what the corpus includes.
    corpus_disclaimer: &'static str,
    disclaimer: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncation: Option<TransitiveTruncationJson>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
}

pub(crate) const COVERING_TESTS_DISCLAIMER: &str = "Rows are reachability LEADS: a test with a CALLS path to the \
     target may exercise it. They are not proof the test covers the symbol, that the test will \
     fail, or that the edit is unsafe; absence of a covering test is not proof the symbol is \
     untested by other means (integration tests, manual QA).";

pub(crate) fn covering_test_row_json<'a>(
    row: &query::CoveringTestRow<'a>,
) -> Option<CoveringTestRowJson<'a>> {
    let GraphRecord::Node {
        id,
        kind,
        schema_version,
        name,
        repo_relative_path,
        span,
        temporal,
        valid_time,
        ..
    } = row.record
    else {
        return None;
    };
    Some(CoveringTestRowJson {
        record_id: id,
        schema_version: *schema_version,
        name: name.as_deref(),
        kind: kind.as_str(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        valid_time: valid_time
            .as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        hop: row.hop,
        coverage: if row.is_direct() {
            "direct"
        } else {
            "transitive"
        },
        path_resolution: row.path_resolution.map(CallResolution::as_str),
        path: row
            .path
            .iter()
            .map(|s| TransitivePathStepJson {
                source_record_id: s.source_record_id,
                edge_record_id: s.edge_record_id,
                edge_label: s.edge_label,
                resolution: s.resolution.map(CallResolution::as_str),
                target_record_id: s.target_record_id,
            })
            .collect(),
        trust: "reachability_lead",
    })
}

/// How the shared covering-tests computation failed: the typed payload, the
/// CLI exit code the verb must use, and which stream the payload goes to.
pub(crate) struct CoveringTestsFailure {
    pub payload: serde_json::Value,
    pub exit_code: i32,
    pub to_stdout: bool,
}

const fn ct_failure(
    payload: serde_json::Value,
    exit_code: i32,
    to_stdout: bool,
) -> CoveringTestsFailure {
    CoveringTestsFailure {
        payload,
        exit_code,
        to_stdout,
    }
}

fn unsupported_handle(handle: &str, message: String) -> CoveringTestsFailure {
    let payload = failure_handle_payload(&query::FailureHandleError::Unsupported {
        handle: handle.to_owned(),
        message,
    });
    ct_failure(payload, 1, false)
}

/// Serializes a [`query::FailureHandleError`] as a normalized typed payload:
/// the `snake_case` `code` is always top-level so both the CLI's stderr
/// diagnostics and the daemon's `error.code` extraction see the same code.
fn failure_handle_payload(err: &query::FailureHandleError) -> serde_json::Value {
    match err {
        query::FailureHandleError::Ambiguous { handle, candidates } => serde_json::json!({
            "code": "ambiguous_handle",
            "handle": handle,
            "candidates": candidates,
        }),
        query::FailureHandleError::Unsupported { handle, message } => serde_json::json!({
            "code": "unsupported_handle",
            "handle": handle,
            "message": message,
        }),
    }
}

/// Builds the full covering-tests response value (header plus row values),
/// shared by the CLI verb and the daemon verb (issue #126).
///
/// Returns the typed failure (payload, CLI exit code, output stream) on
/// every documented error path so both transports honor the same contract:
/// exit 0 with an explicit empty row set when the symbol has no covering
/// tests; exit 1 on malformed/ambiguous/unsupported handles; exit 2 when the
/// handle resolves to no live record or `--at`/`--as-of` names no commit.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn covering_tests_response_value(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    max_depth: usize,
    at: Option<&str>,
    as_of: Option<&str>,
) -> Result<(serde_json::Value, Vec<serde_json::Value>), CoveringTestsFailure> {
    // ── corpus-mode selection (issue #427) ────────────────────────────────────
    // Same default as the transitive lanes: HEAD-anchored when a source
    // snapshot exists, single-snapshot otherwise; --at/--as-of pins one
    // commit. This verb takes no --at-head/--all-history flags.
    let has_snapshot = query::store_has_source_snapshot(records);
    let (corpus_mode, corpus_mode_source) = match query::resolve_corpus_mode(
        at.is_some() || as_of.is_some(),
        false,
        false,
        has_snapshot,
    ) {
        Ok(pair) => pair,
        Err(message) => {
            let payload = serde_json::json!({
                "ok": false,
                "error": { "code": "unsupported_combination", "message": message },
            });
            return Err(ct_failure(payload, 1, true));
        }
    };

    // ── temporal narrowing: one commit's snapshot view ───────────────────────
    let mut at_commit: Option<String> = None;
    let filtered: Option<Vec<GraphRecord>> =
        if matches!(corpus_mode, query::CorpusMode::CommitPinned) {
            let sha =
                match try_resolve_transitive_commit_view(records, index, repo_scope, at, as_of) {
                    Ok(sha) => sha,
                    Err(failure) => {
                        return Err(ct_failure(
                            failure.payload,
                            failure.exit_code,
                            failure.to_stdout,
                        ));
                    }
                };
            let view: Vec<GraphRecord> = records
                .iter()
                .filter(|r| match r {
                    GraphRecord::Node {
                        temporal: Some(t), ..
                    }
                    | GraphRecord::Edge {
                        temporal: Some(t), ..
                    } => t.git_commit == sha,
                    _ => false,
                })
                .cloned()
                .collect();
            at_commit = Some(sha);
            Some(view)
        } else if matches!(corpus_mode, query::CorpusMode::HeadAnchored) {
            let non_current = query::non_head_current_record_ids(records, index);
            let view: Vec<GraphRecord> = records
                .iter()
                .filter(|r| !non_current.contains(r.id()))
                .cloned()
                .collect();
            Some(view)
        } else {
            None
        };
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    // ── handle resolution (symbol record ID or exact symbol name only) ────────
    let target = match query::resolve_failure_handle(records, handle, index, repo_scope) {
        Ok(t) => t,
        Err(
            err @ (query::FailureHandleError::Ambiguous { .. }
            | query::FailureHandleError::Unsupported { .. }),
        ) => {
            return Err(ct_failure(failure_handle_payload(&err), 1, false));
        }
    };

    if matches!(
        target.kind,
        query::FailureTargetKind::Task | query::FailureTargetKind::Source
    ) {
        return Err(unsupported_handle(
            handle,
            format!(
                "handle resolved to a {} target; covering-tests accepts only symbol handles",
                target.kind.as_str()
            ),
        ));
    }
    if matches!(target.kind, query::FailureTargetKind::File) {
        return Err(unsupported_handle(
            handle,
            "handle resolved to a file; covering-tests accepts only symbol handles".to_owned(),
        ));
    }
    if let Some(kind) = query::covering_tests_non_symbol_anchor_kind(records, &target) {
        return Err(unsupported_handle(
            handle,
            format!(
                "handle resolved to a {kind:?} node; covering-tests accepts only symbol handles"
            ),
        ));
    }

    if target.is_empty() {
        let code = if target.stale {
            "stale_handle"
        } else {
            "no_match"
        };
        let payload = serde_json::json!({
            "ok": false,
            "error": { "code": code, "handle": handle },
        });
        return Err(ct_failure(payload, 2, true));
    }

    // A name matching more than one live symbol is ambiguous: walking the
    // union would bleed an unrelated same-name symbol's tests into the
    // target's covering set.
    if target.anchor_ids.len() > 1 {
        let err = query::FailureHandleError::Ambiguous {
            handle: handle.to_owned(),
            candidates: target.anchor_ids.iter().cloned().collect(),
        };
        return Err(ct_failure(failure_handle_payload(&err), 1, false));
    }
    let anchor_id = target
        .anchor_ids
        .iter()
        .next()
        .expect("non-empty target has an anchor");

    let Some(ctx) = query::covering_tests(records, anchor_id, max_depth) else {
        let payload = serde_json::json!({
            "ok": false,
            "error": { "code": "no_match", "handle": handle },
        });
        return Err(ct_failure(payload, 2, true));
    };

    // ── diagnostics: traversal diagnostics + redaction gate over reached rows ─
    let mut diagnostics: Vec<AuditDiagnostic<'_>> = ctx
        .diagnostics
        .iter()
        .map(|d| AuditDiagnostic {
            code: &d.code,
            source_record_id: &d.source_record_id,
            target_handle: &d.target_handle,
            relation: &d.relation,
            target_domain: &d.target_domain,
        })
        .collect();
    for row in &ctx.rows {
        protected_payload_diagnostics(row.record, &mut diagnostics);
    }
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.source_record_id.cmp(b.source_record_id))
            .then_with(|| a.target_handle.cmp(b.target_handle))
            .then_with(|| a.relation.cmp(b.relation))
    });
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
    });

    let rows_json: Vec<CoveringTestRowJson<'_>> =
        ctx.rows.iter().filter_map(covering_test_row_json).collect();

    let GraphRecord::Node {
        id: target_id,
        kind: target_kind,
        schema_version: target_schema_version,
        name: target_name,
        repo_relative_path: target_path,
        span: target_span,
        ..
    } = ctx.anchor
    else {
        let payload = serde_json::json!({
            "ok": false,
            "error": { "code": "internal", "message": "resolved anchor is not a node record" },
        });
        return Err(ct_failure(payload, 2, true));
    };

    let header = CoveringTestsHeaderJson {
        ok: true,
        handle,
        target: TransitiveTargetJson {
            record_id: target_id,
            schema_version: *target_schema_version,
            name: target_name.as_deref(),
            kind: target_kind.as_str(),
            repo_relative_path: target_path.as_deref(),
            span: *target_span,
        },
        direction: "inbound",
        edge_labels: ["CALLS"],
        max_depth: ctx.max_depth,
        at_commit: at_commit.as_deref(),
        as_of,
        total_covering_tests: rows_json.len(),
        direct_tests: ctx.direct_count(),
        transitive_tests: ctx.transitive_count(),
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer: corpus_mode.disclaimer(),
        disclaimer: COVERING_TESTS_DISCLAIMER,
        truncation: ctx.truncation.as_ref().map(|t| TransitiveTruncationJson {
            code: "max_depth_truncated",
            max_depth: t.max_depth,
            dropped_frontier: t
                .dropped_frontier
                .iter()
                .map(|d| TransitiveDroppedDepthJson {
                    depth: d.depth,
                    count: d.count,
                })
                .collect(),
            dropped_total: t.dropped_total,
        }),
        diagnostics,
    };

    let header_value = serde_json::to_value(&header).expect("covering-tests header serializes");
    let row_values: Vec<serde_json::Value> = rows_json
        .iter()
        .map(|r| serde_json::to_value(r).expect("covering-tests row serializes"))
        .collect();
    Ok((header_value, row_values))
}

/// Runs `eg query tests`: prints the NDJSON header + rows (or the text
/// rendering) and exits with the documented machine-readable diagnostics.
#[allow(clippy::too_many_arguments)]
pub(crate) fn query_tests_cmd(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    max_depth: usize,
    at: Option<&str>,
    as_of: Option<&str>,
    format: OutputFormat,
) {
    let (header_value, row_values) = match covering_tests_response_value(
        records, handle, index, repo_scope, max_depth, at, as_of,
    ) {
        Ok(pair) => pair,
        Err(failure) => {
            let text =
                serde_json::to_string(&failure.payload).expect("covering-tests failure serializes");
            if failure.to_stdout {
                println!("{text}");
            } else {
                eprintln!("{text}");
            }
            std::process::exit(failure.exit_code);
        }
    };

    match format {
        OutputFormat::Json => {
            println!("{header_value}");
            for row in &row_values {
                println!("{row}");
            }
        }
        OutputFormat::Text => {
            // Human-readable only; the exact format is unstable by contract.
            let target_name = header_value["target"]["name"].as_str().unwrap_or("?");
            let total = header_value["total_covering_tests"].as_u64().unwrap_or(0);
            let direct = header_value["direct_tests"].as_u64().unwrap_or(0);
            println!(
                "covering tests of {target_name}: {total} ({direct} direct) — reachability leads, not proof of coverage"
            );
            for row in &row_values {
                let name = row["name"].as_str().unwrap_or("?");
                let path = row["repo_relative_path"].as_str().unwrap_or("?");
                let line = row["span"]["start_line"].as_u64().unwrap_or(0);
                let coverage = row["coverage"].as_str().unwrap_or("?");
                let hop = row["hop"].as_u64().unwrap_or(0);
                println!("{name} {path}:{line} {coverage} hop={hop}");
            }
            if let Some(trunc) = header_value.get("truncation") {
                println!(
                    "truncated at max_depth={}: {} reachable caller(s) beyond the bound",
                    trunc["max_depth"], trunc["dropped_total"]
                );
            }
        }
    }
}

/// Routes `eg query tests --daemon` through the running daemon's
/// `tests_for_symbol` verb, re-emitting the daemon's typed errors as the
/// machine-readable envelopes the `--graph` path prints.
#[cfg(feature = "embedded-aletheiadb")]
#[allow(clippy::too_many_lines)] // mirrors the --graph path's error taxonomy arm-for-arm
pub(crate) fn query_tests_via_daemon(
    handle: &str,
    data_dir: &std::path::Path,
    max_depth: usize,
    at: Option<&str>,
    as_of: Option<&str>,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let mut params = serde_json::json!({
        "handle": handle,
        "max_depth": max_depth,
    });
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    if let Some(at) = at {
        params["at"] = serde_json::json!(at);
    }
    if let Some(as_of) = as_of {
        params["as_of"] = serde_json::json!(as_of);
    }
    match client.query_verb_raw("tests_for_symbol", &params, None) {
        Ok(result) => {
            let tests = &result["tests"];
            match format {
                OutputFormat::Json => {
                    let header = serde_json::json!({
                        "ok": true,
                        "handle": handle,
                        "target": tests["target"],
                        "direction": tests["direction"],
                        "edge_labels": tests["edge_labels"],
                        "max_depth": tests["max_depth"],
                        "total_covering_tests": tests["total_covering_tests"],
                        "direct_tests": tests["direct_tests"],
                        "transitive_tests": tests["transitive_tests"],
                        "disclaimer": tests["disclaimer"],
                        "truncation": tests["truncation"],
                        "diagnostics": tests["diagnostics"],
                    });
                    println!(
                        "{}",
                        serde_json::to_string(&header)
                            .context("failed to serialize tests_for_symbol header")?
                    );
                    let no_rows: &[serde_json::Value] = &[];
                    for row in result["records"].as_array().map_or(no_rows, Vec::as_slice) {
                        println!(
                            "{}",
                            serde_json::to_string(row)
                                .context("failed to serialize tests_for_symbol row")?
                        );
                    }
                }
                OutputFormat::Text => {
                    let total = tests["total_covering_tests"].as_u64().unwrap_or(0);
                    let direct = tests["direct_tests"].as_u64().unwrap_or(0);
                    let target_name = tests["target"]["name"].as_str().unwrap_or("?");
                    println!(
                        "covering tests of {target_name}: {total} ({direct} direct) — reachability leads, not proof of coverage"
                    );
                    let no_rows: &[serde_json::Value] = &[];
                    for row in result["records"].as_array().map_or(no_rows, Vec::as_slice) {
                        let name = row["name"].as_str().unwrap_or("?");
                        let path = row["repo_relative_path"].as_str().unwrap_or("?");
                        let line = row["span"]["start_line"].as_u64().unwrap_or(0);
                        let coverage = row["coverage"].as_str().unwrap_or("?");
                        let hop = row["hop"].as_u64().unwrap_or(0);
                        println!("{name} {path}:{line} {coverage} hop={hop}");
                    }
                }
            }
            Ok(())
        }
        Err(e) => {
            // Re-emit the daemon's typed error as the machine-readable
            // envelope the `--graph` path prints, keeping the cold path's
            // exit-code contract (2 = not found, 1 = ambiguous / malformed).
            if let Some(rejection) = e.downcast_ref::<crate::daemon::DaemonQueryRejection>() {
                let mut error = serde_json::json!({
                    "code": rejection.code,
                    "handle": handle,
                    "message": rejection.message,
                });
                if let Some(candidates) = &rejection.candidates {
                    error["candidates"] = serde_json::json!(candidates);
                }
                let exit_code = match rejection.code.as_str() {
                    "no_match"
                    | "stale_handle"
                    | "missing_commit"
                    | "no_commit_at_or_before"
                    | "empty_history" => 2,
                    _ => 1,
                };
                let text =
                    serde_json::to_string(&error).context("failed to serialize daemon error")?;
                if exit_code == 2 {
                    println!("{text}");
                } else {
                    eprintln!("{text}");
                }
                std::process::exit(exit_code);
            }
            Err(e)
        }
    }
}
