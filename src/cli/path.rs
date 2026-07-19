use super::*;

// ---------------------------------------------------------------------------
// path query — `eg query path <A> <B>` (issue #225)
//
// Directed shortest-call-path witness between two named symbols over resolved
// `CALLS` edges, with a citable per-hop trail (record IDs + repo-relative
// file/span handles). Mirrors the `transitive-callers` (#139) lane's handle
// resolution, temporal selectors, ambiguity handling, and NDJSON envelope +
// row output shape; the BFS itself lives privately in `query::path`.
// ---------------------------------------------------------------------------

/// A resolved endpoint's citable handle (used both in the summary envelope and
/// per-hop, so every hop carries from/to `record_id`/`name`/`kind`/`path`/`span`
/// per AC2).
#[derive(Serialize)]
pub(crate) struct PathEndpointJson<'a> {
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
}

/// One hop of the witness path emitted as its own NDJSON line.
#[derive(Serialize)]
pub(crate) struct PathHopJson<'a> {
    index: usize,
    from: PathEndpointJson<'a>,
    to: PathEndpointJson<'a>,
    edge_record_id: &'a str,
    edge_label: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<&'a str>,
    trust: &'static str,
}

/// Summary envelope emitted as the first NDJSON line.
#[derive(Serialize)]
pub(crate) struct PathHeaderJson<'a> {
    ok: bool,
    from_handle: &'a str,
    to_handle: &'a str,
    from: PathEndpointJson<'a>,
    to: PathEndpointJson<'a>,
    direction: &'static str,
    edge_labels: [&'static str; 1],
    resolution_scope: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    at_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<&'a str>,
    verdict: &'static str,
    path_found: bool,
    hops: usize,
    /// Corpus the current-state view read (issue #427):
    /// `head_anchored`/`union`/`commit_pinned`/`single_snapshot`.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: `default`/`explicit_flag`/`selector`.
    corpus_mode_source: &'static str,
    /// One-line human description of what the corpus includes.
    corpus_disclaimer: &'static str,
    disclaimer: &'static str,
}

pub(crate) const PATH_DISCLAIMER: &str = "The witness path uses only `resolved` CALLS edges: ambiguous and unresolved CALLS edges \
     (issues #152/#134), CALLS edges outside the resolution contract, and all \
     REFERENCES/MENTIONS/IMPORTS/IMPLEMENTS/containment edges are excluded. The path is a \
     reachability LEAD over the extracted call graph — it proves A names a resolved call chain \
     to B, never that control flow reaches B at runtime; a no_path verdict is not proof of \
     non-reachability (dynamic dispatch, macro-generated calls, and cross-crate calls are \
     outside the extraction contract).";

/// Builds an endpoint handle JSON from a graph node record. Returns `None` when
/// the record is not a node (defensive; witness nodes are always symbols).
fn endpoint_json(record: &GraphRecord) -> Option<PathEndpointJson<'_>> {
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
    } = record
    else {
        return None;
    };
    Some(PathEndpointJson {
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
    })
}

/// Emits one deterministic machine-readable handle-error line to stderr,
/// folding in the `endpoint` field ("from" / "to") that names which side
/// failed. The externally-tagged `FailureHandleError` body (`Ambiguous` /
/// `Unsupported` with its `handle`/`candidates`/`message`) is preserved
/// verbatim as a sibling key, and `endpoint` is added at the top level so a
/// failure is attributable even when the same handle is supplied for both
/// endpoints. Reuses the `endpoint` field name of the `no_match`/no-path
/// envelope for a consistent contract.
fn emit_endpoint_handle_error(err: &query::FailureHandleError, endpoint: &str) {
    let mut value = serde_json::to_value(err).expect("handle error serializes");
    if let serde_json::Value::Object(map) = &mut value {
        map.insert(
            "endpoint".to_owned(),
            serde_json::Value::String(endpoint.to_owned()),
        );
    }
    eprintln!(
        "{}",
        serde_json::to_string(&value).expect("handle error serializes")
    );
}

/// Resolves one endpoint handle to a single live symbol record ID, mirroring
/// `transitive-callers` handle semantics. Exits the process with the documented
/// machine-readable diagnostics on any failure (`endpoint` names which side —
/// `"from"` / `"to"` — for the caller's convenience).
fn resolve_path_endpoint(
    records: &[GraphRecord],
    handle: &str,
    endpoint: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
) -> String {
    let target = match query::resolve_failure_handle(records, handle, index, repo_scope) {
        Ok(t) => t,
        Err(
            err @ (query::FailureHandleError::Ambiguous { .. }
            | query::FailureHandleError::Unsupported { .. }),
        ) => {
            emit_endpoint_handle_error(&err, endpoint);
            std::process::exit(1);
        }
    };

    if matches!(
        target.kind,
        query::FailureTargetKind::Task | query::FailureTargetKind::Source
    ) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "{endpoint} handle resolved to a {} target; query path accepts only symbol handles",
                target.kind.as_str()
            ),
        };
        emit_endpoint_handle_error(&err, endpoint);
        std::process::exit(1);
    }
    if matches!(target.kind, query::FailureTargetKind::File) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "{endpoint} handle resolved to a file; query path accepts only symbol handles"
            ),
        };
        emit_endpoint_handle_error(&err, endpoint);
        std::process::exit(1);
    }
    if let Some(kind) = query::transitive_callers_non_symbol_anchor_kind(records, &target) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "{endpoint} handle resolved to a {kind:?} node; query path accepts only symbol handles"
            ),
        };
        emit_endpoint_handle_error(&err, endpoint);
        std::process::exit(1);
    }

    if target.is_empty() {
        let code = if target.stale {
            "stale_handle"
        } else {
            "no_match"
        };
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "endpoint": endpoint, "handle": handle },
        });
        println!(
            "{}",
            serde_json::to_string(&envelope).expect("envelope serializes")
        );
        std::process::exit(2);
    }

    // A name matching more than one live symbol is ambiguous: a witness path
    // over the union would silently pick one same-name symbol. All candidate
    // record IDs are reported so the caller can re-run with one of them.
    if target.anchor_ids.len() > 1 {
        let err = query::FailureHandleError::Ambiguous {
            handle: handle.to_owned(),
            candidates: target.anchor_ids.iter().cloned().collect(),
        };
        emit_endpoint_handle_error(&err, endpoint);
        std::process::exit(1);
    }

    target
        .anchor_ids
        .iter()
        .next()
        .expect("non-empty target has an anchor")
        .clone()
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::fn_params_excessive_bools
)]
pub(crate) fn query_path_cmd(
    records: &[GraphRecord],
    from_handle: &str,
    to_handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    at: Option<&str>,
    as_of: Option<&str>,
    at_head: bool,
    all_history: bool,
    format: OutputFormat,
) -> Result<()> {
    // ── corpus-mode selection (issue #427) ────────────────────────────────────
    // Current-state code lanes default to HEAD-anchoring (records current at
    // each repository's stamped `source_snapshot` HEAD) when a snapshot exists;
    // `--all-history` opts into the union of all commit snapshots and `--at-head`
    // makes the default explicit. `--at`/`--as-of` still pin a single commit.
    let has_snapshot = query::store_has_source_snapshot(records);
    let (corpus_mode, corpus_mode_source) = match query::resolve_corpus_mode(
        at.is_some() || as_of.is_some(),
        at_head,
        all_history,
        has_snapshot,
    ) {
        Ok(pair) => pair,
        Err(message) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": { "code": "unsupported_combination", "message": message },
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(1);
        }
    };

    // ── temporal narrowing: one commit's snapshot view (mirrors #139) ─────────
    let mut at_commit: Option<String> = None;
    let filtered: Option<Vec<GraphRecord>> =
        if matches!(corpus_mode, query::CorpusMode::CommitPinned) {
            let sha = resolve_transitive_commit_view(records, index, repo_scope, at, as_of)?;
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
            // Head-anchor: drop every record not current at its owning repository's
            // stamped HEAD BEFORE the directed BFS, so a call path over an edge
            // removed at HEAD yields `no_path` under the default while
            // `--all-history` still finds it. The deterministic parent-pointer
            // selection over the surviving edges is unchanged. Snapshot-less
            // stores never reach here. See `query::non_head_current_record_ids`.
            let non_current = query::non_head_current_record_ids(records, index);
            let view: Vec<GraphRecord> = records
                .iter()
                .filter(|r| !non_current.contains(r.id()))
                .cloned()
                .collect();
            Some(view)
        } else {
            // Union / SingleSnapshot: keep the full record set; the BFS applies
            // its own tombstone/latest-edge-version liveness.
            None
        };
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    // ── resolve both endpoints (each may exit 1 / exit 2 independently) ───────
    let from_id = resolve_path_endpoint(records, from_handle, "from", index, repo_scope);
    let to_id = resolve_path_endpoint(records, to_handle, "to", index, repo_scope);

    let Some(ctx) = query::call_path(records, &from_id, &to_id) else {
        // Both endpoints resolved to a live anchor above, so this is
        // unreachable in practice; fail closed with a no_match envelope.
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": "no_match", "from_handle": from_handle, "to_handle": to_handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    };

    let from_json =
        endpoint_json(ctx.from).ok_or_else(|| anyhow::anyhow!("resolved from is not a node"))?;
    let to_json =
        endpoint_json(ctx.to).ok_or_else(|| anyhow::anyhow!("resolved to is not a node"))?;

    let verdict = if ctx.path_found {
        "path_found"
    } else {
        "no_path"
    };
    let header = PathHeaderJson {
        ok: true,
        from_handle,
        to_handle,
        from: from_json,
        to: to_json,
        direction: "outbound",
        edge_labels: ["CALLS"],
        resolution_scope: "resolved",
        at_commit: at_commit.as_deref(),
        as_of,
        verdict,
        path_found: ctx.path_found,
        hops: ctx.steps.len(),
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer: corpus_mode.disclaimer(),
        disclaimer: PATH_DISCLAIMER,
    };

    // Node lookup for per-hop from/to citable handles.
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

    let hop_rows: Vec<PathHopJson<'_>> = ctx
        .steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let from = by_id
                .get(step.source_record_id)
                .and_then(|r| endpoint_json(r));
            let to = by_id
                .get(step.target_record_id)
                .and_then(|r| endpoint_json(r));
            (i, step, from, to)
        })
        .filter_map(|(i, step, from, to)| {
            Some(PathHopJson {
                index: i + 1,
                from: from?,
                to: to?,
                edge_record_id: step.edge_record_id,
                edge_label: step.edge_label,
                resolution: step.resolution.map(CallResolution::as_str),
                confidence: step.confidence,
                trust: "reachability_lead",
            })
        })
        .collect();

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&header).context("failed to serialize path header")?
            );
            for hop in &hop_rows {
                println!(
                    "{}",
                    serde_json::to_string(hop).context("failed to serialize path hop")?
                );
            }
        }
        OutputFormat::Text => {
            // Human-readable only; the exact format is unstable by contract.
            let from_name = header.from.name.unwrap_or("?");
            let to_name = header.to.name.unwrap_or("?");
            if ctx.path_found {
                println!(
                    "path {from_name} -> {to_name}: {} hop(s) over resolved CALLS edges — reachability lead, not proof of runtime control flow",
                    ctx.steps.len()
                );
                for hop in &hop_rows {
                    let from = hop.from.name.unwrap_or("?");
                    let to = hop.to.name.unwrap_or("?");
                    let location = hop.to.repo_relative_path.map_or_else(String::new, |p| {
                        hop.to
                            .span
                            .map_or_else(|| format!(" {p}"), |s| format!(" {p}:{}", s.start_line))
                    });
                    println!("  {}. {from} -CALLS-> {to}{location}", hop.index);
                }
            } else {
                println!(
                    "no resolved CALLS path from {from_name} to {to_name} — not proof of non-reachability"
                );
            }
        }
    }

    if ctx.path_found {
        Ok(())
    } else {
        // AC4: an explicit machine-readable no-path verdict exits 2.
        std::process::exit(2);
    }
}
