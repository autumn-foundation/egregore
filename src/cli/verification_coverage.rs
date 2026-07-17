use super::*;

// ---------------------------------------------------------------------------
// verification-coverage lane (issue #109)
// ---------------------------------------------------------------------------

/// One crediting verification link on a covered-symbol row.
#[derive(Serialize)]
pub(crate) struct CoverageEvidenceJson<'a> {
    record_id: &'a str,
    verification_kind: &'a str,
    edge_label: &'a str,
    /// `symbol` for a symbol-direct link, `file` for a file-level link.
    link_level: &'a str,
}

/// One covered-symbol row.
#[derive(Serialize)]
pub(crate) struct CoveredItemJson<'a> {
    record_id: &'a str,
    kind: &'a str,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    /// Crediting verification links, sorted and de-duplicated.
    verification: Vec<CoverageEvidenceJson<'a>>,
}

/// One uncovered-symbol row.
#[derive(Serialize)]
pub(crate) struct UncoveredItemJson<'a> {
    record_id: &'a str,
    kind: &'a str,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
}

/// Deterministic tallies in the verification-coverage response.
#[derive(Serialize)]
pub(crate) struct VerificationCoverageCountsJson {
    symbols_in_scope: usize,
    covered: usize,
    uncovered: usize,
    verification_records_in_store: usize,
    verification_code_links_in_store: usize,
    covered_truncated: bool,
    uncovered_truncated: bool,
}

/// One stable machine-readable diagnostic in the response.
#[derive(Serialize)]
pub(crate) struct VerificationCoverageDiagnosticJson<'a> {
    code: &'a str,
    detail: &'a str,
}

/// Top-level verification-coverage response envelope.
#[derive(Serialize)]
pub(crate) struct VerificationCoverageResponse<'a> {
    ok: bool,
    language: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
    /// `verification_links_recorded` when the store records at least one
    /// verification→code link; `verification_facts_unavailable` (explicit
    /// capability-absent verdict) otherwise — never "everything is uncovered".
    capability: &'static str,
    /// Per-response soundness boundary: presence/absence only.
    disclaimer: &'static str,
    covered: Vec<CoveredItemJson<'a>>,
    uncovered: Vec<UncoveredItemJson<'a>>,
    counts: VerificationCoverageCountsJson,
    diagnostics: Vec<VerificationCoverageDiagnosticJson<'a>>,
}

pub(crate) const VERIFICATION_COVERAGE_DISCLAIMER: &str = "Rows report presence or absence of recorded verification evidence in this store only. \
     Absence is a prioritization signal, never proof that code is untested, unverified in \
     reality, unsafe, or broken; presence is a recorded link, never proof of correctness or \
     that a test or proof passed.";

/// Resolves the `--at` commit selector (full SHA or unique prefix, repo-scoped)
/// and filters `records` to that single-commit snapshot: every record whose
/// temporal metadata matches the resolved commit, plus non-temporal records
/// that are not tombstoned (Repository / current-tree records the surface still
/// needs). An unknown or ambiguous selector prints a stable machine-readable
/// diagnostic and exits 1.
pub(crate) fn verification_coverage_snapshot_at_commit(
    records: Vec<GraphRecord>,
    selector: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
) -> Vec<GraphRecord> {
    let record_in_repo_scope = |record: &GraphRecord| -> bool {
        let Some(repo_id) = repo_scope else {
            return true;
        };
        match record {
            GraphRecord::Node { id, .. } => index.owner_of(id) == Some(repo_id),
            GraphRecord::Edge { source, target, .. } => {
                index.owner_of(source) == Some(repo_id) || index.owner_of(target) == Some(repo_id)
            }
            GraphRecord::Tombstone { .. } => false,
        }
    };

    let matching: std::collections::BTreeSet<&str> = records
        .iter()
        .filter(|r| record_in_repo_scope(r))
        .filter_map(|r| match r {
            GraphRecord::Node {
                temporal: Some(t), ..
            }
            | GraphRecord::Edge {
                temporal: Some(t), ..
            } if t.git_commit.starts_with(selector) => Some(t.git_commit.as_str()),
            _ => None,
        })
        .collect();
    let resolved = match matching.len() {
        0 => {
            let diag = serde_json::json!({
                "code": "unknown_commit",
                "commit": selector,
                "message": "no record in the selected store slice carries this commit",
            });
            eprintln!("{diag}");
            std::process::exit(1);
        }
        1 => (*matching.iter().next().unwrap()).to_owned(),
        count => {
            let diag = serde_json::json!({
                "code": "ambiguous_commit",
                "commit": selector,
                "count": count,
                "message": "commit prefix matches more than one commit",
            });
            eprintln!("{diag}");
            std::process::exit(1);
        }
    };

    let tombstoned: std::collections::BTreeSet<String> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.clone())
            } else {
                None
            }
        })
        .collect();

    records
        .into_iter()
        .filter(|r| match r {
            GraphRecord::Node {
                temporal: Some(t), ..
            }
            | GraphRecord::Edge {
                temporal: Some(t), ..
            } => t.git_commit == resolved,
            GraphRecord::Node { id, .. } | GraphRecord::Edge { id, .. } => !tombstoned.contains(id),
            GraphRecord::Tombstone { .. } => false,
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
pub(crate) fn query_verification_coverage_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    scope: Option<&str>,
    limit: Option<usize>,
    format: OutputFormat,
) -> Result<()> {
    let report = query::verification_coverage(records, index, repo_scope, scope, limit);

    // A supplied scope handle that matched no in-store code item is a usage
    // error (exit 2): `scope_not_found` for a path-shaped handle, `no_match`
    // for a name/id-shaped one.
    let scope_error_code = match report.scope_outcome {
        query::ScopeOutcome::NotFoundPath => Some("scope_not_found"),
        query::ScopeOutcome::NoMatch => Some("no_match"),
        query::ScopeOutcome::NoScope | query::ScopeOutcome::Matched => None,
    };
    if let Some(code) = scope_error_code {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": code,
                "scope": scope,
                "message": "no public code item in the selected store slice matches this scope handle",
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let capability = if report.capability_present {
        "verification_links_recorded"
    } else {
        "verification_facts_unavailable"
    };

    match format {
        OutputFormat::Json => {
            let response = VerificationCoverageResponse {
                ok: true,
                language: "Rust",
                repo_scope,
                scope,
                capability,
                disclaimer: VERIFICATION_COVERAGE_DISCLAIMER,
                covered: report
                    .covered
                    .iter()
                    .map(|item| CoveredItemJson {
                        record_id: item.record_id,
                        kind: &item.kind,
                        path: &item.path,
                        repo_relative_path: item.repo_relative_path,
                        span: item.span,
                        verification: item
                            .verification
                            .iter()
                            .map(|ev| CoverageEvidenceJson {
                                record_id: ev.record_id,
                                verification_kind: ev.verification_kind,
                                edge_label: ev.edge_label,
                                link_level: ev.link_level,
                            })
                            .collect(),
                    })
                    .collect(),
                uncovered: report
                    .uncovered
                    .iter()
                    .map(|item| UncoveredItemJson {
                        record_id: item.record_id,
                        kind: &item.kind,
                        path: &item.path,
                        repo_relative_path: item.repo_relative_path,
                        span: item.span,
                        schema_version: item.schema_version,
                        valid_time: item.valid_time,
                        git_commit: item.git_commit,
                    })
                    .collect(),
                counts: VerificationCoverageCountsJson {
                    symbols_in_scope: report.counts.symbols_in_scope,
                    covered: report.counts.covered,
                    uncovered: report.counts.uncovered,
                    verification_records_in_store: report.counts.verification_records_in_store,
                    verification_code_links_in_store: report
                        .counts
                        .verification_code_links_in_store,
                    covered_truncated: report.counts.covered_truncated,
                    uncovered_truncated: report.counts.uncovered_truncated,
                },
                diagnostics: report
                    .diagnostics
                    .iter()
                    .map(|d| VerificationCoverageDiagnosticJson {
                        code: d.code,
                        detail: &d.detail,
                    })
                    .collect(),
            };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize verification-coverage report")?;
            println!("{output}");
        }
        OutputFormat::Text => {
            println!(
                "Verification coverage of the public API surface (recorded evidence only — \
                 never proof of correctness). Capability: {capability}."
            );
            if !report.capability_present {
                println!(
                    "# capability {capability}: no verification→code links recorded; the \
                     covered/uncovered partition is unavailable (absence is a prioritization \
                     signal, not proof code is untested or unsafe)."
                );
            }
            let citation = |path: Option<&str>, span: Option<SourceSpan>| match (path, span) {
                (Some(path), Some(span)) => {
                    format!(" @ {path}:{}-{}", span.start_line, span.end_line)
                }
                (Some(path), None) => format!(" @ {path}"),
                (None, _) => String::new(),
            };
            println!("covered:");
            for item in &report.covered {
                let citation = citation(item.repo_relative_path, item.span);
                for ev in &item.verification {
                    println!(
                        "- {} [{}]{} via {}({}) <- {} [{}]",
                        item.path,
                        item.kind,
                        citation,
                        ev.edge_label,
                        ev.link_level,
                        ev.record_id,
                        ev.verification_kind
                    );
                }
            }
            println!("uncovered:");
            for item in &report.uncovered {
                let citation = citation(item.repo_relative_path, item.span);
                println!(
                    "- {} [{}]{} ({})",
                    item.path, item.kind, citation, item.record_id
                );
            }
            println!(
                "counts: symbols_in_scope={} covered={} uncovered={} \
                 verification_records_in_store={} verification_code_links_in_store={}",
                report.counts.symbols_in_scope,
                report.counts.covered,
                report.counts.uncovered,
                report.counts.verification_records_in_store,
                report.counts.verification_code_links_in_store
            );
            for d in &report.diagnostics {
                println!("diagnostic: {}: {}", d.code, d.detail);
            }
        }
    }
    Ok(())
}
