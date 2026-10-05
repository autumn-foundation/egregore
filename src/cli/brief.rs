use super::*;
use crate::worktree_diff::{DiffError, working_tree_diff};

/// Runs `eg brief` (issue #214): brief the agent on graph evidence scoped to
/// the working-tree diff.
///
/// Resolves the uncommitted working-tree diff at `repo_path` into `File` and
/// `Symbol` handles and returns the trust-separated briefing from
/// [`query::brief_working_set`], plus the store-freshness staleness marker
/// shared with `eg freshness` (issue #220).
///
/// Strictly read-only: the diff is computed with read-only Git plumbing
/// (`GIT_OPTIONAL_LOCKS=0`; no checkout, stash, or write) and the store is
/// only read, never re-scanned or mutated.
pub(crate) fn brief_cmd(
    repo_path: &Path,
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    repo: Option<&str>,
    staged_only: bool,
    format: OutputFormat,
) -> Result<()> {
    let records = load_query_records(graph, data_dir)?;
    let index = query::RepositoryIndex::build(&records);
    let selected = resolve_repo_scope(&index, repo);

    let diff = match working_tree_diff(repo_path, staged_only) {
        Ok(diff) => diff,
        Err(DiffError::NotAGitRepo { .. }) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "not_a_git_repo",
                    "repo_path": repo_path.to_string_lossy(),
                },
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(2);
        }
        Err(DiffError::UnbornHead) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "unborn_head",
                    "repo_path": repo_path.to_string_lossy(),
                },
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(2);
        }
        Err(err) => return Err(anyhow::anyhow!(err)),
    };

    let brief = query::brief_working_set(&records, &diff, &index, selected.as_deref());
    // Staleness marker (issue #214 AC): the store is compared against the live
    // working tree with the same assessment `eg freshness` uses, so a briefing
    // over a dirty tree carries `stale_dirty` rather than failing or silently
    // returning partial data.
    let staleness = assess_freshness(repo_path, &records, graph, data_dir, None);

    print_brief_report(&brief, &staleness, repo_path, staged_only, format)
}

/// Serializable prior-failures section: runtime vs agent-authored split.
#[derive(Serialize)]
struct BriefFailuresJson<'a> {
    target_kind: &'static str,
    runtime_failures: Vec<FailureAttemptJson<'a>>,
    agent_failures: Vec<FailureAttemptJson<'a>>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
}

/// Full `eg brief` result payload.
#[derive(Serialize)]
struct BriefResultJson<'a> {
    repo_path: String,
    staged_only: bool,
    changed_files: Vec<query::BriefChangedFile>,
    changed_symbols: Vec<query::BriefChangedSymbol<'a>>,
    unindexed: Vec<query::BriefUnindexedPath>,
    source_facts: Vec<query::BriefSourceFact<'a>>,
    prior_failures: BriefFailuresJson<'a>,
    observations: Vec<query::ContextObservation<'a>>,
    decisions: Vec<query::ContextObservation<'a>>,
    in_flight_tasks: Vec<query::ContextLinkedItem<'a>>,
    verification_evidence: Vec<query::ContextLinkedItem<'a>>,
    drift_warnings: Vec<query::BriefDriftWarning<'a>>,
    store_coverage: query::StoreCoverage,
    staleness: &'a FreshnessReport,
}

/// Prints a [`query::WorkingSetBrief`] in the requested format.
///
/// JSON is a single-line `{"ok": true, "result": …}` envelope (the
/// one-JSON-object-per-line contract from `docs/cli/query.md`). Text renders
/// one section per block; empty sections are marked `domain-absent` (the
/// domain is not in this store) vs `entity-absent` (the domain is present but
/// holds nothing for this working set), per #196.
pub(crate) fn print_brief_report(
    brief: &query::WorkingSetBrief<'_>,
    staleness: &FreshnessReport,
    repo_path: &Path,
    staged_only: bool,
    format: OutputFormat,
) -> Result<()> {
    let diagnostics: Vec<AuditDiagnostic<'_>> = brief
        .failure_context
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
    let result = BriefResultJson {
        repo_path: repo_path.to_string_lossy().into_owned(),
        staged_only,
        changed_files: brief.changed_files.clone(),
        changed_symbols: brief.changed_symbols.clone(),
        unindexed: brief.unindexed.clone(),
        source_facts: brief.source_facts.clone(),
        prior_failures: BriefFailuresJson {
            target_kind: brief.failure_context.target_kind,
            runtime_failures: brief
                .failure_context
                .runtime_failures
                .iter()
                .map(failure_attempt_json)
                .collect(),
            agent_failures: brief
                .failure_context
                .agent_failures
                .iter()
                .map(failure_attempt_json)
                .collect(),
            diagnostics,
        },
        observations: brief.observations.clone(),
        decisions: brief.decisions.clone(),
        in_flight_tasks: brief.in_flight_tasks.clone(),
        verification_evidence: brief.verification_evidence.clone(),
        drift_warnings: brief.drift_warnings.clone(),
        store_coverage: brief.store_coverage,
        staleness,
    };
    match format {
        OutputFormat::Json => {
            let envelope = serde_json::json!({ "ok": true, "result": result });
            println!("{}", serde_json::to_string(&envelope)?);
        }
        OutputFormat::Text => print_brief_text(brief, staleness, staged_only),
    }
    Ok(())
}

/// Marks an empty section `domain-absent` vs `entity-absent` (#196): when the
/// section's domain is absent from the store the emptiness means "domain not
/// in this store"; when the domain is present it means "no records for this
/// working set".
const fn empty_mark(section_empty: bool, domain_present: bool) -> &'static str {
    if !section_empty {
        ""
    } else if domain_present {
        "entity-absent"
    } else {
        "domain-absent"
    }
}

fn print_brief_text(
    brief: &query::WorkingSetBrief<'_>,
    staleness: &FreshnessReport,
    staged_only: bool,
) {
    print_working_set_rows(brief, staged_only);
    print_evidence_sections(brief);
    print_state_sections(brief, staleness);
}

/// Header plus the working-set rows: changed files, changed symbols,
/// unindexed paths, and source facts.
fn print_working_set_rows(brief: &query::WorkingSetBrief<'_>, staged_only: bool) {
    println!(
        "working set{}: {} changed files, {} changed symbols, {} unindexed paths",
        if staged_only { " (staged only)" } else { "" },
        brief.changed_files.len(),
        brief.changed_symbols.len(),
        brief.unindexed.len(),
    );
    for file in &brief.changed_files {
        println!(
            "  {} {} ({}, {} hunks, {} lines)",
            file.status,
            file.path,
            file.record_id.as_deref().unwrap_or("unresolved"),
            file.hunk_count,
            file.changed_lines,
        );
    }
    for symbol in &brief.changed_symbols {
        let span = symbol.span.map_or_else(
            || "no span".to_owned(),
            |s| format!("{}-{}", s.start_line, s.end_line),
        );
        println!(
            "  symbol {} {} @ {}:{}",
            symbol.record_id, symbol.name, symbol.path, span,
        );
    }
    for unindexed in &brief.unindexed {
        println!("  unindexed {} {}", unindexed.status, unindexed.path);
    }

    println!("source facts ({}):", brief.source_facts.len());
    for fact in &brief.source_facts {
        println!(
            "  {} {} {}",
            fact.record_id,
            fact.kind,
            fact.path.unwrap_or(""),
        );
    }
}

/// Prior failures, observations, and decisions with their #196
/// domain-absent/entity-absent marks.
fn print_evidence_sections(brief: &query::WorkingSetBrief<'_>) {
    let failures = &brief.failure_context;
    println!(
        "prior failures: {} runtime, {} agent {}",
        failures.runtime_failures.len(),
        failures.agent_failures.len(),
        empty_mark(
            failures.has_no_failures(),
            brief.store_coverage.agent_memory || brief.store_coverage.verification,
        ),
    );

    println!(
        "observations ({}): {}",
        brief.observations.len(),
        empty_mark(
            brief.observations.is_empty(),
            brief.store_coverage.agent_memory
        ),
    );
    for obs in &brief.observations {
        println!(
            "  {} {}",
            obs.record_id,
            obs.provenance_handle.as_deref().unwrap_or(""),
        );
    }
    println!(
        "decisions ({}): {}",
        brief.decisions.len(),
        empty_mark(
            brief.decisions.is_empty(),
            brief.store_coverage.agent_memory
        ),
    );
    for decision in &brief.decisions {
        println!(
            "  {} {}",
            decision.record_id,
            decision.provenance_handle.as_deref().unwrap_or(""),
        );
    }
}

/// In-flight tasks, verification evidence, drift warnings, store coverage,
/// and the staleness marker.
fn print_state_sections(brief: &query::WorkingSetBrief<'_>, staleness: &FreshnessReport) {
    println!(
        "in-flight tasks ({}): {}",
        brief.in_flight_tasks.len(),
        empty_mark(
            brief.in_flight_tasks.is_empty(),
            brief.store_coverage.project
        ),
    );
    for task in &brief.in_flight_tasks {
        println!(
            "  {} {} {}",
            task.record_id,
            task.kind,
            task.status.unwrap_or(""),
        );
    }
    println!(
        "verification evidence ({}): {}",
        brief.verification_evidence.len(),
        empty_mark(
            brief.verification_evidence.is_empty(),
            brief.store_coverage.verification,
        ),
    );
    println!(
        "drift warnings ({}): {}",
        brief.drift_warnings.len(),
        empty_mark(
            brief.drift_warnings.is_empty(),
            brief.store_coverage.code_graph
        ),
    );
    for warning in &brief.drift_warnings {
        println!(
            "  {} -> {} score {:.4}",
            warning.record_id, warning.target_record_id, warning.score,
        );
    }
    let coverage = &brief.store_coverage;
    println!(
        "store coverage: code_graph={} agent_memory={} project={} artifact={} verification={}",
        coverage.code_graph,
        coverage.agent_memory,
        coverage.project,
        coverage.artifact,
        coverage.verification,
    );
    println!("staleness: {} — {}", staleness.freshness, staleness.message,);
}
