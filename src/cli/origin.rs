//! `eg query origin <SYMBOL>` (issue #159): trace a code symbol to the commit
//! that introduced it, plus the project-graph PR / issue / review records
//! whose `merge_commit_sha` equals that commit.
//!
//! The link is deterministic commit-SHA byte equality ONLY — never fuzzy
//! title/body matching. Code facts (the introducing commit) and project facts
//! (PR/issue/review rows) render in trust-separated sections, mirroring
//! `eg query context`'s contract. The lane resolves entirely offline against
//! the already-ingested store: zero live GitHub calls.
//!
//! Exit codes: 0 on a traced origin; 1 on malformed input (ambiguous symbol,
//! ambiguous commit prefix, malformed `--as-of`); 2 when nothing matched
//! (`no_match`, `no_history`, `missing_commit`).

use std::borrow::Cow;
use std::collections::HashSet;

use super::*;

use crate::query::{OriginError, OriginPin, OriginProjectLink, OriginReport};

/// The single-answer JSON body for `eg query origin` (issue #159).
#[derive(Debug, Serialize)]
struct OriginAnswer<'a> {
    ok: bool,
    symbol_name: &'a str,
    code: OriginCodeSection<'a>,
    project: OriginProjectSection<'a>,
}

/// Trust-separated code facts: the symbol and its introducing commit.
/// Everything here is deterministic extractor output (`source_derived`).
#[derive(Debug, Serialize)]
struct OriginCodeSection<'a> {
    trust: crate::query::TrustClass,
    symbol_record_id: &'a str,
    introducing_commit: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    introducing_commit_record_id: Option<&'a str>,
    introducing_valid_time: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temporal_selector: Option<TemporalSelectorView<'a>>,
}

/// The temporal pin echoed back so a pinned answer can never be mistaken
/// for a current-HEAD one.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum TemporalSelectorView<'a> {
    At(&'a str),
    AsOf(&'a str),
}

/// Trust-separated project facts: the GitHub-imported records whose
/// `merge_commit_sha` equals the introducing commit (`project_state`).
#[derive(Debug, Serialize)]
struct OriginProjectSection<'a> {
    /// `"present"` when the store carries any GitHub-imported records,
    /// `"absent"` otherwise.
    github_import: &'static str,
    /// The deterministic link rule, stated on every answer.
    link_rule: &'static str,
    pull_requests: Vec<OriginProjectRow<'a>>,
    issues: Vec<OriginProjectRow<'a>>,
    reviews: Vec<OriginProjectRow<'a>>,
    /// Present only when `github_import` is `"absent"`: the explicit
    /// `github_import_absent` degradation note (issue #159 AC7).
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

/// One linked project record. `merge_commit_sha` is echoed so the equality
/// the link rests on is visible in the answer itself.
#[derive(Debug, Serialize)]
struct OriginProjectRow<'a> {
    record_id: &'a str,
    trust: crate::query::TrustClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    merge_commit_sha: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    merged_at: Option<&'a str>,
}

/// Prints a machine-readable origin diagnostic and exits. JSON envelopes go
/// to stdout (the `no_match` contract, issue #159 AC6); text diagnostics go
/// to stderr.
fn fail_origin(
    code: &str,
    message: &str,
    detail: Option<serde_json::Value>,
    format: OutputFormat,
    exit_code: i32,
) -> ! {
    match format {
        OutputFormat::Json => {
            let mut error = serde_json::json!({ "code": code, "message": message });
            if let Some(detail) = detail {
                for (key, value) in detail.as_object().expect("detail is an object") {
                    error[key] = value.clone();
                }
            }
            let envelope = serde_json::json!({ "ok": false, "error": error });
            println!("{envelope}");
        }
        OutputFormat::Text => eprintln!("Error [{code}]: {message}"),
    }
    std::process::exit(exit_code);
}

/// The answer's corpus: the store minus records under active
/// repository-eviction tombstones (issue #472), mirroring `query resolve`.
/// Zero-cost borrow when nothing is evicted.
fn live_records<'a>(
    records: &'a [GraphRecord],
    evicted: &HashSet<String>,
) -> Cow<'a, [GraphRecord]> {
    if evicted.is_empty() {
        Cow::Borrowed(records)
    } else {
        Cow::Owned(
            records
                .iter()
                .filter(|record| !evicted.contains(record.id()))
                .cloned()
                .collect(),
        )
    }
}

fn project_row<'a>(
    link: OriginProjectLink<'a>,
    trust: &query::TrustIndex<'_>,
) -> OriginProjectRow<'a> {
    let GraphRecord::Node {
        id,
        name,
        title,
        merge_commit_sha,
        merged_at,
        ..
    } = link.record
    else {
        unreachable!("origin project links are always Node records");
    };
    OriginProjectRow {
        record_id: id.as_str(),
        trust: trust.classify(link.record),
        name: name.as_deref(),
        title: title.as_deref(),
        merge_commit_sha: merge_commit_sha.as_deref().unwrap_or_default(),
        merged_at: merged_at.as_deref(),
    }
}

/// Implements `eg query origin` (issue #159).
/// Maps an [`OriginError`] to the `(code, message, detail, exit_code)` error
/// envelope. Split from [`query_origin_cmd`] to keep the dispatch readable.
fn origin_error_envelope(
    error: OriginError,
    pin: OriginPin<'_>,
) -> (&'static str, String, Option<serde_json::Value>, i32) {
    match error {
        OriginError::UnknownSymbol { query } => {
            let message = match pin {
                OriginPin::None => format!("symbol not found in the graph: {query}"),
                OriginPin::AtCommit(commit) => format!(
                    "symbol '{query}' is not present as of commit {commit}: \
                     introduced later, or never"
                ),
                OriginPin::AsOf(instant) => format!(
                    "symbol '{query}' is not present as of {instant}: \
                     introduced later, or never"
                ),
            };
            (
                "no_match",
                message,
                Some(serde_json::json!({ "symbol_name": query })),
                2,
            )
        }
        OriginError::AmbiguousSymbol { query, candidates } => (
            "ambiguous_symbol",
            format!(
                "the name '{query}' matches more than one distinct symbol; \
                 re-run with the exact record id"
            ),
            Some(serde_json::json!({
                "symbol_name": query,
                "candidates": candidates,
            })),
            1,
        ),
        OriginError::NoHistory { query } => (
            "no_history",
            format!(
                "symbol '{query}' matched but has no commit-linked history: \
                 the store carries no commits introducing it"
            ),
            Some(serde_json::json!({ "symbol_name": query })),
            2,
        ),
        OriginError::MissingCommit { commit_prefix } => (
            "missing_commit",
            format!("commit '{commit_prefix}' is not present in the store"),
            Some(serde_json::json!({ "commit": commit_prefix })),
            2,
        ),
        OriginError::AmbiguousCommitPrefix {
            commit_prefix,
            matches,
        } => (
            "ambiguous_commit_prefix",
            format!(
                "commit prefix '{commit_prefix}' is ambiguous: matches {} commits",
                matches.len()
            ),
            Some(serde_json::json!({
                "commit_prefix": commit_prefix,
                "matches": matches,
            })),
            1,
        ),
        OriginError::InvalidAsOf { as_of } => (
            "invalid_as_of",
            format!("--as-of '{as_of}' is not a valid RFC 3339 timestamp"),
            Some(serde_json::json!({ "as_of": as_of })),
            1,
        ),
    }
}

pub(crate) fn query_origin_cmd(
    records: &[GraphRecord],
    symbol: &str,
    repo_id: Option<&str>,
    at: Option<&str>,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let pin = match (at, as_of) {
        (Some(commit), None) => OriginPin::AtCommit(commit),
        (None, Some(instant)) => OriginPin::AsOf(instant),
        (None, None) => OriginPin::None,
        // Clap's conflicts_with rejects --at with --as-of at parse time.
        _ => unreachable!("exactly one of --at / --as-of must be set"),
    };

    // Issue #472: targets of active repository-eviction tombstones are
    // suppressed from this lane. The borrow is free when nothing is
    // evicted; the filtered copy otherwise.
    let evicted = crate::repo_evict::active_eviction_tombstoned_ids(records);
    let live = live_records(records, &evicted);
    let records: &[GraphRecord] = &live;

    let report: OriginReport<'_> = match query::symbol_origin(records, symbol, repo_id, pin) {
        Ok(report) => report,
        Err(error) => {
            let (code, message, detail, exit_code) = origin_error_envelope(error, pin);
            fail_origin(code, &message, detail, format, exit_code);
        }
    };

    let trust = query::TrustIndex::build(records);
    let temporal_selector = match pin {
        OriginPin::None => None,
        OriginPin::AtCommit(commit) => Some(TemporalSelectorView::At(commit)),
        OriginPin::AsOf(instant) => Some(TemporalSelectorView::AsOf(instant)),
    };
    let code = OriginCodeSection {
        // The introducing-commit row is deterministic extractor output.
        trust: crate::query::TrustClass::SourceDerived,
        symbol_record_id: report.symbol_record_id.as_str(),
        introducing_commit: report.introducing_commit.as_str(),
        introducing_commit_record_id: report.introducing_commit_record_id.as_deref(),
        introducing_valid_time: report.introducing_valid_time.as_str(),
        repo_relative_path: report.repo_relative_path.as_deref(),
        span: report.span,
        temporal_selector,
    };
    let (github_import, note) = if report.github_import_present {
        ("present", None)
    } else {
        // Issue #159 AC7: code history without a GitHub import degrades to a
        // commit-only answer with an explicit note — never an error, and
        // never implying no PR exists.
        (
            "absent",
            Some(
                "github_import_absent: this store carries no GitHub-imported \
                 records, so the project sections are empty; that does not \
                 mean no PR introduced the symbol",
            ),
        )
    };
    let project = OriginProjectSection {
        github_import,
        link_rule: "merge_commit_sha equality (exact byte match against the \
                    introducing commit); no fuzzy title/body matching",
        pull_requests: report
            .pull_requests
            .iter()
            .map(|link| project_row(*link, &trust))
            .collect(),
        issues: report
            .issues
            .iter()
            .map(|link| project_row(*link, &trust))
            .collect(),
        reviews: report
            .reviews
            .iter()
            .map(|link| project_row(*link, &trust))
            .collect(),
        note,
    };
    let answer = OriginAnswer {
        ok: true,
        symbol_name: report.symbol_name.as_str(),
        code,
        project,
    };

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&answer)?);
        }
        OutputFormat::Text => {
            println!("{}", origin_text_line(&answer));
        }
    }
    Ok(())
}

/// One human-readable summary for `--format text`. Not script-stable: parse
/// the JSON form instead.
fn origin_text_line(answer: &OriginAnswer<'_>) -> String {
    use std::fmt::Write as _;
    let sanitize = crate::embeddings::sanitized_handle;
    let mut line = format!(
        "origin `{}`: introduced at {} ({})",
        sanitize(answer.symbol_name),
        answer.code.introducing_commit,
        answer.code.introducing_valid_time,
    );
    if let Some(path) = answer.code.repo_relative_path {
        let _ = write!(line, " @ {}", sanitize(path));
    }
    if answer.project.github_import == "absent" {
        line.push_str("; GitHub import absent (commit-only answer)");
    } else {
        let prs: Vec<String> = answer
            .project
            .pull_requests
            .iter()
            .map(|pr| {
                format!(
                    "{} ({})",
                    pr.name.map_or_else(|| pr.record_id.to_owned(), sanitize),
                    pr.record_id
                )
            })
            .collect();
        if prs.is_empty() {
            line.push_str("; no PR with a matching merge_commit_sha in the GitHub import");
        } else {
            let _ = write!(line, "; PRs: {}", prs.join(", "));
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            None,
            None,
            Some(id.to_owned()),
            format!("node {id}"),
        )
    }

    #[test]
    fn live_records_borrows_when_nothing_evicted() {
        let records = vec![node("a"), node("b")];
        let evicted = HashSet::new();
        let live = live_records(&records, &evicted);
        assert!(matches!(live, Cow::Borrowed(_)));
        assert_eq!(live.len(), 2);
    }

    #[test]
    fn live_records_filters_evicted_ids() {
        let records = vec![node("a"), node("b"), node("c")];
        let evicted: HashSet<String> = std::iter::once("b".to_owned()).collect();
        let live = live_records(&records, &evicted);
        let ids: Vec<&str> = live.iter().map(GraphRecord::id).collect();
        assert_eq!(ids, vec!["a", "c"]);
    }
}
