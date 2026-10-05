use super::*;

// ---------------------------------------------------------------------------
// failure-hotspots query — `eg query failure-hotspots` (issue #254)
//
// The store-wide complement to `eg query failures <handle>`: instead of
// requiring the operator to already suspect a target, this lane aggregates
// every live agent-authored `Failure` record, resolves each to its code
// (Symbol/File) targets, and ranks the targets by DISTINCT failing-run
// count (total failure count is the documented tie-break) — the "here's
// where agents keep drowning" map.
//
// Each row cites its evidence: the target record ID + repo-relative
// path/span, and every contributing Failure record ID with its run/session
// handle. Failures that resolve to no code target are reported in an
// explicit `unresolved` section, never silently dropped. Ordering is
// deterministic and byte-stable across repeated runs on an unchanged store.
// ---------------------------------------------------------------------------

pub(crate) const FAILURE_HOTSPOTS_DISCLAIMER: &str = "Rows rank code targets (Symbol/File) \
     by the number of DISTINCT agent runs that failed against them, total failure count \
     as the tie-break, record ID as the final tie-break. Each row cites its contributing \
     Failure record IDs and their run/session handles — never a bare count. `unresolved` \
     lists live failures that resolved to no code target (`task_only`: linked only to \
     task records; `no_code_target`: linked to nothing live). Rows aggregate agent-authored \
     Failure claims (trust `agent_authored`); the lane ranks and cites, it never \
     diagnoses why a target fails.";

#[derive(Serialize)]
struct ContributingFailureJson<'a> {
    record_id: &'a str,
    /// The aggregation key: `session_id` provenance, or the failure's own
    /// record ID when it carries none.
    run: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_kind: Option<&'a str>,
}

#[derive(Serialize)]
struct UnresolvedFailureJson<'a> {
    failure_record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    reason: &'a str,
}

/// One ranked hotspot emitted as its own NDJSON line.
#[derive(Serialize)]
struct FailureHotspotRowJson<'a> {
    rank: usize,
    target_record_id: &'a str,
    target_kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    distinct_run_count: usize,
    failure_count: usize,
    failures: Vec<ContributingFailureJson<'a>>,
    trust: &'static str,
}

/// Summary envelope emitted as the first NDJSON line.
#[derive(Serialize)]
struct FailureHotspotsHeaderJson<'a> {
    ok: bool,
    lane: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<&'a str>,
    total_hotspots: usize,
    returned_hotspots: usize,
    /// Completeness signal: whether `--limit` truncated the ranking.
    truncated: bool,
    limit: usize,
    /// Failures with no code target — explicit, never silently dropped.
    unresolved: Vec<UnresolvedFailureJson<'a>>,
    disclaimer: &'static str,
}

const fn contributing_failure_json<'a>(
    f: &'a query::ContributingFailure<'a>,
) -> ContributingFailureJson<'a> {
    ContributingFailureJson {
        record_id: f.record_id,
        run: f.run.as_str(),
        session_id: f.session_id,
        observed_at: f.observed_at,
        failure_kind: f.failure_kind,
    }
}

/// Prints the stable no-match envelope and exits 2 (nothing to rank).
fn failure_hotspots_no_match(format: OutputFormat) -> ! {
    match format {
        OutputFormat::Json => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "no_match",
                    "message": "no Failure records in the store",
                }
            });
            println!("{envelope}");
        }
        OutputFormat::Text => {
            eprintln!("Error: no Failure records in the store");
        }
    }
    std::process::exit(2);
}

fn parse_temporal_bound(raw: &str, flag: &str) -> chrono::DateTime<chrono::FixedOffset> {
    chrono::DateTime::parse_from_rfc3339(raw).unwrap_or_else(|_| {
        let diag = serde_json::json!({
            "code": "malformed_timestamp",
            "flag": flag,
            "value": raw,
            "message": format!("{flag} must be an RFC 3339 instant, e.g. 2026-09-01T00:00:00Z"),
        });
        eprintln!("{diag}");
        std::process::exit(1);
    })
}

pub(crate) fn query_failure_hotspots_cmd(
    records: &[GraphRecord],
    limit: usize,
    as_of_raw: Option<&str>,
    since_raw: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // Parse the temporal selectors up front so a malformed instant fails fast
    // with a machine-readable diagnostic, before touching the aggregation.
    let as_of = as_of_raw.map(|raw| parse_temporal_bound(raw, "--as-of"));
    let since = since_raw.map(|raw| parse_temporal_bound(raw, "--since"));

    let report = match query::failure_hotspots(records, limit, as_of, since) {
        Ok(report) => report,
        Err(query::FailureHotspotsError::NoFailures) => failure_hotspots_no_match(format),
    };

    let header = FailureHotspotsHeaderJson {
        ok: true,
        lane: "failure-hotspots",
        as_of: as_of_raw,
        since: since_raw,
        total_hotspots: report.total_hotspots,
        returned_hotspots: report.returned_hotspots,
        truncated: report.truncated,
        limit,
        unresolved: report
            .unresolved
            .iter()
            .map(|u| UnresolvedFailureJson {
                failure_record_id: u.failure_record_id,
                session_id: u.session_id,
                reason: u.reason.as_str(),
            })
            .collect(),
        disclaimer: FAILURE_HOTSPOTS_DISCLAIMER,
    };

    let rows: Vec<FailureHotspotRowJson<'_>> = report
        .hotspots
        .iter()
        .map(|row| FailureHotspotRowJson {
            rank: row.rank,
            target_record_id: row.target_record_id,
            target_kind: row.target_kind,
            repo_relative_path: row.repo_relative_path,
            span: row.span,
            distinct_run_count: row.distinct_run_count,
            failure_count: row.failure_count,
            failures: row.failures.iter().map(contributing_failure_json).collect(),
            trust: row.trust,
        })
        .collect();

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&header).context("failed to serialize hotspots header")?
            );
            for row in &rows {
                println!(
                    "{}",
                    serde_json::to_string(row).context("failed to serialize hotspot row")?
                );
            }
        }
        OutputFormat::Text => {
            // Human-readable only; the exact format is unstable by contract.
            println!(
                "failure hotspots ({} total, showing {}):",
                report.total_hotspots, report.returned_hotspots
            );
            for row in &rows {
                let location = row
                    .repo_relative_path
                    .map_or_else(|| row.target_record_id.to_owned(), str::to_owned);
                let span_text = row
                    .span
                    .map_or_else(String::new, |s| format!(":{}-{}", s.start_line, s.end_line));
                println!(
                    "  {}. {location}{span_text}  [{}]  runs={} failures={}",
                    row.rank, row.target_kind, row.distinct_run_count, row.failure_count
                );
                for f in &row.failures {
                    let when = f.observed_at.unwrap_or("undated");
                    println!("     - {}  (run {}, {})", f.record_id, f.run, when);
                }
            }
            if report.truncated {
                println!(
                    "truncated: showing {} of {} hotspots (raise --limit, max {})",
                    report.returned_hotspots,
                    report.total_hotspots,
                    query::FAILURE_HOTSPOTS_MAX_LIMIT
                );
            }
            if !header.unresolved.is_empty() {
                println!("unresolved ({}):", header.unresolved.len());
                for u in &header.unresolved {
                    println!("  - {}  ({})", u.failure_record_id, u.reason);
                }
            }
        }
    }
    Ok(())
}
