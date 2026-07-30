use super::*;

// ---------------------------------------------------------------------------
// dependency-cycles query (issue #138)
// ---------------------------------------------------------------------------

/// One citable record contributing a dependency edge on a cycle.
#[derive(Serialize)]
pub(crate) struct CycleEvidenceJson<'a> {
    /// Wire relation label the dependency was derived from ("CALLS" / "IMPORTS").
    relation: &'static str,
    /// Stable record ID of the contributing CALLS edge or Import node.
    record_id: &'a str,
    /// Call resolution status ("resolved") on CALLS-derived evidence; absent
    /// on import-derived evidence. Ambiguous, unresolved, and unlabeled
    /// cross-file edges never appear here — they are excluded from cycle
    /// detection and tallied in `counts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<&'static str>,
}

/// One directed file-level dependency edge closing part of a cycle.
#[derive(Serialize)]
pub(crate) struct CycleEdgeJson<'a> {
    from: &'a str,
    to: &'a str,
    /// Sorted unique relation labels contributing to this edge.
    relations: Vec<&'static str>,
    /// Contributing records, sorted by (relation, record ID), capped.
    evidence: Vec<CycleEvidenceJson<'a>>,
    /// Total contributing records before the evidence cap.
    evidence_total: usize,
}

/// One file participating in a dependency cycle.
#[derive(Serialize)]
pub(crate) struct CycleMemberJson<'a> {
    record_id: &'a str,
    repo_relative_path: &'a str,
}

/// One canonical dependency cycle.
#[derive(Serialize)]
pub(crate) struct DependencyCycleJson<'a> {
    /// Number of distinct member files.
    length: usize,
    /// Human-readable closing path, e.g. `a.rs -> b.rs -> a.rs`.
    path: String,
    /// Ordered members, rotated to start at the lexicographically smallest.
    members: Vec<CycleMemberJson<'a>>,
    /// `edges[i]` connects `members[i]` to `members[(i + 1) % length]`.
    edges: Vec<CycleEdgeJson<'a>>,
}

/// Resolved scope echo for the scoped (pre-refactor check) form.
#[derive(Serialize)]
pub(crate) struct CycleScopeJson<'a> {
    handle: &'a str,
    target_type: &'a str,
    target_ids: Vec<&'a str>,
}

/// Dependency-graph and edge-class tallies.
#[derive(Serialize)]
pub(crate) struct CycleCountsJson {
    files: usize,
    dependency_edges: usize,
    cycles_total: usize,
    cycles_returned: usize,
    calls_resolved: usize,
    calls_ambiguous_excluded: usize,
    calls_unresolved_excluded: usize,
    calls_unlabeled_excluded: usize,
    imports_resolved: usize,
    imports_ambiguous_excluded: usize,
    imports_external: usize,
    imports_non_rust_excluded: usize,
}

/// One stable machine-readable diagnostic in the cycles response.
#[derive(Serialize)]
pub(crate) struct CycleDiagnosticJson<'a> {
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    record_id: Option<&'a str>,
    detail: &'a str,
}

/// Top-level dependency-cycles response envelope.
#[derive(Serialize)]
pub(crate) struct CyclesResponse<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<CycleScopeJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    /// The documented ambiguity policy in machine-checkable form.
    edge_policy: &'static str,
    disclaimer: &'static str,
    cycles: Vec<DependencyCycleJson<'a>>,
    counts: CycleCountsJson,
    diagnostics: Vec<CycleDiagnosticJson<'a>>,
    /// Corpus the current-state view read (issue #427):
    /// `union` over a scan-history store, `single_snapshot` over a plain scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
}

pub(crate) const CYCLES_EDGE_POLICY: &str = "resolved CALLS edges and imports name-resolving to exactly one in-repo defining file \
     form dependency edges; ambiguous, unresolved, and unlabeled cross-file CALLS edges and \
     ambiguous imports are excluded from cycle detection and tallied in counts. Import name \
     resolution is Rust-only in this slice: non-Rust imports are excluded and tallied.";

pub(crate) const CYCLES_DISCLAIMER: &str = "Cycles are derived from extracted, resolution-labeled graph edges. Absence of a \
     reported cycle is not proof the modules are acyclic at runtime (excluded ambiguous \
     edges are tallied in counts); presence is a structural lead to inspect before a refactor.";

/// Renders one cycle's closing path, e.g. `a.rs -> b.rs -> a.rs`.
pub(crate) fn cycle_display_path(cycle: &query::DependencyCycle<'_>) -> String {
    let mut parts: Vec<&str> = cycle.members.iter().map(|m| m.repo_relative_path).collect();
    if let Some(&first) = parts.first() {
        parts.push(first);
    }
    parts.join(" -> ")
}

#[allow(clippy::too_many_lines)]
pub(crate) fn query_cycles_cmd(
    records: &[GraphRecord],
    scope_handle: Option<&str>,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    at_head: bool,
    all_history: bool,
    format: OutputFormat,
) -> Result<()> {
    // Corpus-mode selection (issue #456): head-anchor by default over a
    // scan-history store; `--all-history` opts into the union. Pre-filter drops
    // off-HEAD records BEFORE cycle detection.
    let (corpus_mode, corpus_mode_source, filtered) =
        resolve_current_state_corpus(records, index, false, at_head, all_history)?;
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    // Resolve the optional scope handle through the shared handle-resolution
    // contract (mirrors `query change-impact`): unsupported/ambiguous handles
    // exit 1 with machine-readable stderr JSON; a handle resolving to nothing
    // live exits 2 with a `no_match` / `stale_handle` stdout envelope.
    let scope_target = match scope_handle {
        None => None,
        Some(handle) => {
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
                        "handle resolved to a {} target; cycles accepts only code symbol or file handles",
                        target.kind.as_str()
                    ),
                };
                eprintln!("{}", serde_json::to_string(&err)?);
                std::process::exit(1);
            }
            if let Some(kind) = query::change_impact_unsupported_anchor_kind(records, &target) {
                let err = query::FailureHandleError::Unsupported {
                    handle: handle.to_owned(),
                    message: format!(
                        "handle resolved to a {kind:?} node; cycles accepts only code symbol or file handles"
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
            Some(target)
        }
    };

    let ctx = query::dependency_cycles(records, index, repo_scope, scope_target.as_ref());

    let corpus_disclaimer = corpus_mode.disclaimer().to_owned();

    match format {
        OutputFormat::Json => {
            let response = CyclesResponse {
                ok: true,
                scope: scope_target.as_ref().map(|target| CycleScopeJson {
                    handle: &target.handle,
                    target_type: match target.kind {
                        query::FailureTargetKind::File => "file",
                        _ => "symbol",
                    },
                    target_ids: target.anchor_ids.iter().map(String::as_str).collect(),
                }),
                repo_scope,
                edge_policy: CYCLES_EDGE_POLICY,
                disclaimer: CYCLES_DISCLAIMER,
                cycles: ctx
                    .cycles
                    .iter()
                    .map(|cycle| DependencyCycleJson {
                        length: cycle.members.len(),
                        path: cycle_display_path(cycle),
                        members: cycle
                            .members
                            .iter()
                            .map(|m| CycleMemberJson {
                                record_id: m.record_id,
                                repo_relative_path: m.repo_relative_path,
                            })
                            .collect(),
                        edges: cycle
                            .edges
                            .iter()
                            .map(|edge| CycleEdgeJson {
                                from: edge.from,
                                to: edge.to,
                                relations: edge.relations.clone(),
                                evidence: edge
                                    .evidence
                                    .iter()
                                    .map(|ev| CycleEvidenceJson {
                                        relation: ev.relation,
                                        record_id: ev.record_id,
                                        resolution: ev.resolution.map(CallResolution::as_str),
                                    })
                                    .collect(),
                                evidence_total: edge.evidence_total,
                            })
                            .collect(),
                    })
                    .collect(),
                counts: CycleCountsJson {
                    files: ctx.counts.files,
                    dependency_edges: ctx.counts.dependency_edges,
                    cycles_total: ctx.cycles_total,
                    cycles_returned: ctx.cycles.len(),
                    calls_resolved: ctx.counts.calls_resolved,
                    calls_ambiguous_excluded: ctx.counts.calls_ambiguous_excluded,
                    calls_unresolved_excluded: ctx.counts.calls_unresolved_excluded,
                    calls_unlabeled_excluded: ctx.counts.calls_unlabeled_excluded,
                    imports_resolved: ctx.counts.imports_resolved,
                    imports_ambiguous_excluded: ctx.counts.imports_ambiguous_excluded,
                    imports_external: ctx.counts.imports_external,
                    imports_non_rust_excluded: ctx.counts.imports_non_rust_excluded,
                },
                diagnostics: ctx
                    .diagnostics
                    .iter()
                    .map(|d| CycleDiagnosticJson {
                        code: d.code,
                        record_id: d.record_id.as_deref(),
                        detail: &d.detail,
                    })
                    .collect(),
                corpus_mode: corpus_mode.as_str(),
                corpus_mode_source: corpus_mode_source.as_str(),
                corpus_disclaimer,
            };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize dependency cycles")?;
            println!("{output}");
        }
        OutputFormat::Text => {
            if ctx.cycles.is_empty() {
                println!("no dependency cycles detected");
            } else {
                println!("{} dependency cycle(s) detected", ctx.cycles.len());
                for (i, cycle) in ctx.cycles.iter().enumerate() {
                    println!(
                        "cycle {} (length {}): {}",
                        i + 1,
                        cycle.members.len(),
                        cycle_display_path(cycle)
                    );
                }
            }
            for d in &ctx.diagnostics {
                println!("[{}] {}", d.code, d.detail);
            }
            println!("corpus: {}", corpus_mode.as_str());
        }
    }
    Ok(())
}
