use super::*;

// ---------------------------------------------------------------------------
// transitive-callers query (issue #139)
// ---------------------------------------------------------------------------

/// One hop of a connecting call path in the transitive-callers output.
#[derive(Serialize)]
pub(crate) struct TransitivePathStepJson<'a> {
    source_record_id: &'a str,
    edge_record_id: &'a str,
    edge_label: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<&'static str>,
    target_record_id: &'a str,
}

/// One reachable row in the transitive-callers output.
#[derive(Serialize)]
pub(crate) struct TransitiveCallerRowJson<'a> {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    path_resolution: Option<&'static str>,
    path: Vec<TransitivePathStepJson<'a>>,
    trust: &'static str,
}

/// The queried target's own citable handle in the summary envelope.
#[derive(Serialize)]
pub(crate) struct TransitiveTargetJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
}

/// Dropped-frontier count at one depth beyond the bound.
#[derive(Serialize)]
pub(crate) struct TransitiveDroppedDepthJson {
    depth: usize,
    count: usize,
}

/// Depth-bound truncation diagnostic.
#[derive(Serialize)]
pub(crate) struct TransitiveTruncationJson {
    code: &'static str,
    max_depth: usize,
    dropped_frontier: Vec<TransitiveDroppedDepthJson>,
    dropped_total: usize,
}

/// Summary envelope emitted as the first NDJSON line.
#[derive(Serialize)]
pub(crate) struct TransitiveCallersHeaderJson<'a> {
    ok: bool,
    handle: &'a str,
    target: TransitiveTargetJson<'a>,
    direction: &'static str,
    edge_labels: [&'static str; 2],
    max_depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    at_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<&'a str>,
    total_reachable: usize,
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

pub(crate) const TRANSITIVE_CALLERS_DISCLAIMER: &str = "Rows are reachability LEADS: a call path exists in the graph. They are not proof that any \
     reachable symbol will break, that a test will fail, or that the edit is unsafe; absence of \
     a path is not proof of unreachability.";

pub(crate) fn transitive_row_json<'a>(
    row: &query::TransitiveCallerRow<'a>,
) -> Option<TransitiveCallerRowJson<'a>> {
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
    Some(TransitiveCallerRowJson {
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

/// Resolves the `--at`/`--as-of` selector against the store's `Commit` nodes
/// and returns the selected commit SHA. Exits with the documented
/// machine-readable diagnostics on failure.
pub(crate) fn resolve_transitive_commit_view(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    at: Option<&str>,
    as_of: Option<&str>,
) -> Result<String> {
    let mut commits: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Commit,
            name: Some(sha),
            temporal,
            ..
        } = r
        {
            // Repository scoping mirrors `range_deltas` (issue #118): in a
            // shared multi-repository store the temporal view must resolve
            // within the selected repository, or `--as-of` could select
            // another repository's newest commit (emptying the scoped view)
            // and an `--at` prefix could be ambiguous solely because of
            // commits outside the selected repository.
            if repo_scope.is_some_and(|scope| index.owner_of(id) != Some(scope)) {
                continue;
            }
            commits
                .entry(sha.as_str())
                .or_insert_with(|| temporal.as_ref().map(|t| t.valid_time.as_str()));
        }
    }
    if commits.is_empty() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "empty_history",
                "message": "--at/--as-of requires a history store with Commit records (run scan-history)",
            },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }
    if let Some(prefix) = at {
        let needle = prefix.to_lowercase();
        let matches: Vec<&str> = commits
            .keys()
            .copied()
            .filter(|sha| sha.to_lowercase().starts_with(&needle))
            .collect();
        return match matches.len() {
            0 => {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": { "code": "missing_commit", "commit_prefix": prefix },
                });
                println!("{}", serde_json::to_string(&envelope)?);
                std::process::exit(2);
            }
            1 => Ok(matches[0].to_owned()),
            _ => {
                let diag = serde_json::json!({
                    "code": "ambiguous_commit_prefix",
                    "commit_prefix": prefix,
                    "matches": matches,
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
        };
    }
    let as_of = as_of.expect("caller passes exactly one of --at / --as-of");
    let Ok(as_of_dt) = chrono::DateTime::parse_from_rfc3339(as_of) else {
        let diag = serde_json::json!({
            "code": "invalid_as_of_timestamp",
            "as_of": as_of,
            "message": "--as-of must be an RFC 3339 instant",
        });
        eprintln!("{diag}");
        std::process::exit(1);
    };
    // Most recent commit at or before the instant; ascending-SHA iteration
    // with a strict `>` comparison makes ties resolve to the smallest SHA.
    let mut best: Option<(&str, chrono::DateTime<chrono::FixedOffset>)> = None;
    for (sha, vt) in &commits {
        let Some(vt) = vt else { continue };
        let Ok(vt) = chrono::DateTime::parse_from_rfc3339(vt) else {
            continue;
        };
        if vt > as_of_dt {
            continue;
        }
        if best.as_ref().is_none_or(|(_, bvt)| vt > *bvt) {
            best = Some((sha, vt));
        }
    }
    if let Some((sha, _)) = best {
        Ok(sha.to_owned())
    } else {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": "no_commit_at_or_before", "as_of": as_of },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::fn_params_excessive_bools
)]
pub(crate) fn query_transitive_callers_cmd(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    max_depth: usize,
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

    // ── temporal narrowing: one commit's snapshot view (issue #139 AC6) ───────
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
            // stamped HEAD BEFORE the walk, so a caller/call-edge removed at HEAD
            // does not appear. Snapshot-less stores never reach here (the default
            // resolves to SingleSnapshot). See `query::non_head_current_record_ids`.
            let non_current = query::non_head_current_record_ids(records, index);
            let view: Vec<GraphRecord> = records
                .iter()
                .filter(|r| !non_current.contains(r.id()))
                .cloned()
                .collect();
            Some(view)
        } else {
            // Union / SingleSnapshot: keep the full record set; the walk applies
            // its own keep-last-per-id / latest-edge-version liveness.
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
            eprintln!("{}", serde_json::to_string(&err)?);
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
                "handle resolved to a {} target; transitive-callers accepts only symbol handles",
                target.kind.as_str()
            ),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }
    if matches!(target.kind, query::FailureTargetKind::File) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: "handle resolved to a file; transitive-callers accepts only symbol handles \
                      (use `eg query change-impact` for file-level blast radius)"
                .to_owned(),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }
    if let Some(kind) = query::transitive_callers_non_symbol_anchor_kind(records, &target) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {kind:?} node; transitive-callers accepts only symbol handles"
            ),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
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
            "error": { "code": code, "handle": handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    // A name matching more than one live symbol is ambiguous for this verb:
    // walking the union would bleed an unrelated same-name symbol's edges
    // into the target's reachability (issue #139 AC2/AC6). All candidate
    // record IDs are reported so the caller can re-run with one of them.
    if target.anchor_ids.len() > 1 {
        let err = query::FailureHandleError::Ambiguous {
            handle: handle.to_owned(),
            candidates: target.anchor_ids.iter().cloned().collect(),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }
    let anchor_id = target
        .anchor_ids
        .iter()
        .next()
        .expect("non-empty target has an anchor");

    let Some(ctx) = query::transitive_callers(records, anchor_id, max_depth) else {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": "no_match", "handle": handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
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

    let rows_json: Vec<TransitiveCallerRowJson<'_>> =
        ctx.rows.iter().filter_map(transitive_row_json).collect();

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
        anyhow::bail!("resolved anchor is not a node record");
    };

    let header = TransitiveCallersHeaderJson {
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
        edge_labels: ["CALLS", "REFERENCES"],
        max_depth: ctx.max_depth,
        at_commit: at_commit.as_deref(),
        as_of,
        total_reachable: rows_json.len(),
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer: corpus_mode.disclaimer(),
        disclaimer: TRANSITIVE_CALLERS_DISCLAIMER,
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

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&header)
                    .context("failed to serialize transitive-callers header")?
            );
            for row in &rows_json {
                println!(
                    "{}",
                    serde_json::to_string(row)
                        .context("failed to serialize transitive-callers row")?
                );
            }
        }
        OutputFormat::Text => {
            // Human-readable only; the exact format is unstable by contract.
            let mut names: BTreeMap<&str, &str> = BTreeMap::new();
            names.insert(target_id.as_str(), target_name.as_deref().unwrap_or("?"));
            for row in &ctx.rows {
                if let GraphRecord::Node {
                    id, name: Some(n), ..
                } = row.record
                {
                    names.insert(id.as_str(), n.as_str());
                }
            }
            println!(
                "transitive callers of {} ({target_id}) max_depth={} total={} — reachability leads, not proof of breakage",
                target_name.as_deref().unwrap_or("?"),
                ctx.max_depth,
                rows_json.len(),
            );
            for row in &rows_json {
                let mut chain = String::new();
                for step in &row.path {
                    let from = names.get(step.source_record_id).copied().unwrap_or("?");
                    chain.push_str(from);
                    chain.push_str(" -");
                    chain.push_str(step.edge_label);
                    chain.push_str("-> ");
                }
                chain.push_str(names.get(target_id.as_str()).copied().unwrap_or("?"));
                let location = row.repo_relative_path.map_or_else(String::new, |p| {
                    row.span
                        .map_or_else(|| format!(" {p}"), |s| format!(" {p}:{}", s.start_line))
                });
                let res = row
                    .path_resolution
                    .map_or_else(String::new, |r| format!(" [resolution={r}]"));
                println!(
                    "{}{location} hop={} via {chain}{res}",
                    row.name.unwrap_or("?"),
                    row.hop,
                );
            }
            if let Some(t) = &ctx.truncation {
                println!(
                    "truncated at max_depth={}: {} reachable node(s) beyond the bound",
                    t.max_depth, t.dropped_total
                );
            }
        }
    }
    Ok(())
}
