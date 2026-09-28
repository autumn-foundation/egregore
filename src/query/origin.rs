//! Symbol origin tracing (issue #159): trace a code symbol to the commit that
//! introduced it, then deterministically link that commit to project-graph PR
//! / issue / review records via exact `merge_commit_sha` equality.
//!
//! Design notes:
//! - The introducing commit is the first `Introduced` event of the symbol's
//!   lifeline ([`super::lifeline::symbol_lifeline`]); reintroductions never
//!   move the origin.
//! - The project link is deterministic commit-SHA byte equality ONLY. No
//!   fuzzy title/body matching: a PR whose `merge_commit_sha` merely shares a
//!   prefix with the introducing commit does not match.
//! - Project facts are never temporally filtered: the `--at` / `--as-of` pin
//!   scopes the *code* history the origin is computed over, while GitHub
//!   import records are store-level state matched as-is.
//! - This module is transport-agnostic and offline: it reads the already-
//!   ingested record slice and performs zero network calls. CLI rendering
//!   (JSON sections, error envelopes, exit codes) lives in
//!   `crate::cli::origin`.

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use super::lifeline::{LifelineError, symbol_lifeline};
use crate::github::records::{IMPORTER_ID, SOURCE_KIND_ISSUE};
use crate::ir::{GraphRecord, NodeKind, SourceSpan};

/// Closed error vocabulary for origin resolution.
#[derive(thiserror::Error, Debug, Clone, Eq, PartialEq)]
pub enum OriginError {
    /// The symbol query matched no symbol in the requested view.
    #[error("symbol not found in the requested view: {query}")]
    UnknownSymbol {
        /// The query string.
        query: String,
    },
    /// The symbol query matched more than one distinct symbol.
    #[error("ambiguous symbol name '{query}' matches multiple symbols")]
    AmbiguousSymbol {
        /// The query string.
        query: String,
        /// The unique record IDs matching the query.
        candidates: Vec<String>,
    },
    /// The symbol resolved but has no commit-linked history, so there is no
    /// introducing commit to cite (e.g. a snapshot-only store).
    #[error("symbol '{query}' has no commit-linked history in the requested view")]
    NoHistory {
        /// The query string.
        query: String,
    },
    /// The `--at` commit prefix matches no commit in the store.
    #[error("commit not present in the store: {commit_prefix}")]
    MissingCommit {
        /// The `--at` argument as given.
        commit_prefix: String,
    },
    /// The `--at` commit prefix matches more than one commit.
    #[error("ambiguous commit prefix '{commit_prefix}'")]
    AmbiguousCommitPrefix {
        /// The `--at` argument as given.
        commit_prefix: String,
        /// The full SHAs the prefix matched.
        matches: Vec<String>,
    },
    /// The `--as-of` argument is not a valid RFC 3339 timestamp.
    #[error("malformed --as-of timestamp: {as_of}")]
    InvalidAsOf {
        /// The `--as-of` argument as given.
        as_of: String,
    },
}

/// Temporal pin for origin resolution: which past code state the origin is
/// computed over. `None` resolves against the full (current HEAD) state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginPin<'a> {
    /// No pin: resolve against the full store.
    None,
    /// Resolve against the ancestry-closed state at this commit SHA or
    /// unique prefix.
    AtCommit(&'a str),
    /// Resolve against commits whose valid time is at or before this RFC
    /// 3339 instant.
    AsOf(&'a str),
}

/// One project-graph record whose `merge_commit_sha` equals the introducing
/// commit (exact byte equality).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginProjectLink<'a> {
    /// The matched project record.
    pub record: &'a GraphRecord,
}

/// The origin answer.
///
/// Code facts plus the deterministically linked project records, kept in
/// trust-separated groups (mirroring `eg query context`'s contract that
/// code facts and project facts never share a section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginReport<'a> {
    /// The resolved symbol's stable record ID.
    pub symbol_record_id: String,
    /// The resolved symbol's display name.
    pub symbol_name: String,
    /// The introducing commit's full SHA.
    pub introducing_commit: String,
    /// The `Commit` node ID for the introducing commit, when the store
    /// carries one.
    pub introducing_commit_record_id: Option<String>,
    /// The introducing commit's valid time (RFC 3339).
    pub introducing_valid_time: String,
    /// The symbol's repo-relative path at introduction.
    pub repo_relative_path: Option<String>,
    /// The symbol's source span at introduction.
    pub span: Option<SourceSpan>,
    /// Whether any GitHub-imported records exist in the store at all. When
    /// false, the project sections are empty by construction and the caller
    /// must render the `github_import_absent` note (issue #159 AC7).
    pub github_import_present: bool,
    /// Matched PR records, sorted by record ID.
    pub pull_requests: Vec<OriginProjectLink<'a>>,
    /// Matched issue records, sorted by record ID.
    pub issues: Vec<OriginProjectLink<'a>>,
    /// Matched review records, sorted by record ID.
    pub reviews: Vec<OriginProjectLink<'a>>,
}

/// Resolve a `--at` commit prefix to a full SHA over every temporal-bearing
/// record's `git_commit` (the issue #160 temporal contract: case-sensitive
/// prefix match).
fn resolve_commit_prefix<'a>(
    records: &'a [GraphRecord],
    prefix: &str,
) -> Result<&'a str, OriginError> {
    let mut matches: BTreeSet<&'a str> = BTreeSet::new();
    for record in records {
        let commit = match record {
            GraphRecord::Node {
                temporal: Some(t), ..
            }
            | GraphRecord::Edge {
                temporal: Some(t), ..
            } => t.git_commit.as_str(),
            _ => continue,
        };
        if commit.starts_with(prefix) {
            matches.insert(commit);
        }
    }
    match matches.len() {
        0 => Err(OriginError::MissingCommit {
            commit_prefix: prefix.to_owned(),
        }),
        1 => Ok(matches.into_iter().next().expect("exactly one match")),
        _ => Err(OriginError::AmbiguousCommitPrefix {
            commit_prefix: prefix.to_owned(),
            matches: matches.into_iter().map(str::to_owned).collect(),
        }),
    }
}

/// The ancestry-closed commit set for an `--at` pin: the anchor plus every
/// commit reachable through `git_parent_commits` links. Walking the temporal
/// parent links (rather than requiring `Commit` nodes) keeps shallow or
/// partial stores honest.
fn ancestor_closure<'a>(records: &'a [GraphRecord], anchor: &'a str) -> BTreeSet<&'a str> {
    let mut parents: BTreeMap<&'a str, Vec<&'a str>> = BTreeMap::new();
    for record in records {
        let temporal = match record {
            GraphRecord::Node {
                temporal: Some(t), ..
            }
            | GraphRecord::Edge {
                temporal: Some(t), ..
            } => t,
            _ => continue,
        };
        parents
            .entry(temporal.git_commit.as_str())
            .or_default()
            .extend(temporal.git_parent_commits.iter().map(String::as_str));
    }
    let mut seen: BTreeSet<&'a str> = BTreeSet::new();
    let mut stack = vec![anchor];
    while let Some(sha) = stack.pop() {
        if seen.insert(sha) {
            if let Some(ps) = parents.get(sha) {
                stack.extend(ps.iter().copied());
            }
        }
    }
    seen
}

/// The commit set for an `--as-of` pin: every `Commit` node's SHA whose
/// valid time is at or before the instant (the issue #160 valid-time axis).
fn as_of_commit_set<'a>(
    records: &'a [GraphRecord],
    cutoff: &DateTime<chrono::FixedOffset>,
) -> BTreeSet<&'a str> {
    let mut in_scope: BTreeSet<&'a str> = BTreeSet::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Commit,
            name: Some(sha),
            temporal: Some(t),
            ..
        } = record
        {
            if let Ok(valid_time) = DateTime::parse_from_rfc3339(t.valid_time.as_str()) {
                if valid_time <= *cutoff {
                    in_scope.insert(sha.as_str());
                }
            }
        }
    }
    in_scope
}

/// Trace a symbol to its introducing commit and link the project records.
///
/// The `records` slice is the answer's corpus: callers suppress
/// repository-eviction tombstone targets (issue #472) BEFORE calling, the
/// same way the other query lanes do. Ordinary `forget` tombstones keep the
/// issue #231 temporal exemption inside the lifeline computation.
///
/// # Errors
///
/// Returns [`OriginError`] for an unknown/ambiguous symbol, a symbol with no
/// commit-linked history, or a malformed/unresolvable temporal pin.
pub fn symbol_origin<'a>(
    records: &'a [GraphRecord],
    query: &str,
    repo_id: Option<&str>,
    pin: OriginPin<'_>,
) -> Result<OriginReport<'a>, OriginError> {
    // Resolve the temporal pin BEFORE the symbol lookup: a bad pin is an
    // input error regardless of whether the symbol exists.
    let pin_set: Option<BTreeSet<&'a str>> = match pin {
        OriginPin::None => None,
        OriginPin::AtCommit(prefix) => {
            let anchor = resolve_commit_prefix(records, prefix)?;
            Some(ancestor_closure(records, anchor))
        }
        OriginPin::AsOf(as_of) => {
            let cutoff =
                DateTime::parse_from_rfc3339(as_of).map_err(|_| OriginError::InvalidAsOf {
                    as_of: as_of.to_owned(),
                })?;
            Some(as_of_commit_set(records, &cutoff))
        }
    };

    let events = symbol_lifeline(records, query, repo_id).map_err(|error| match error {
        LifelineError::UnknownSymbol { query } => OriginError::UnknownSymbol { query },
        LifelineError::AmbiguousSymbol { query, candidates } => {
            OriginError::AmbiguousSymbol { query, candidates }
        }
    })?;
    // By lifeline construction the first event is always `Introduced`; an
    // empty event list means the symbol resolved but has no commit-linked
    // history (the `no_history` lane contract, mirroring `query lifeline`).
    let Some(introduced) = events.first() else {
        return Err(OriginError::NoHistory {
            query: query.to_owned(),
        });
    };
    debug_assert_eq!(introduced.event_type, super::LifelineEventKind::Introduced);
    if let Some(set) = &pin_set {
        if !set.contains(introduced.commit.as_str()) {
            // The symbol exists in the store but was introduced after the
            // pinned state: as of the pin, it did not exist.
            return Err(OriginError::UnknownSymbol {
                query: query.to_owned(),
            });
        }
    }
    let introducing_commit = introduced.commit.clone();

    let introducing_commit_record_id = records.iter().find_map(|record| match record {
        GraphRecord::Node {
            id,
            kind: NodeKind::Commit,
            name: Some(sha),
            ..
        } if sha == &introducing_commit => Some(id.clone()),
        _ => None,
    });

    // Deterministic project link: exact `merge_commit_sha` byte equality
    // against the introducing commit. Only PR-derived `Task` records carry
    // the field (importer construction, issue #333); the generic rule also
    // classifies any other record kind that carries it, so the three project
    // sections stay truthful without kind-specific special cases.
    let github_import_present = records.iter().any(|record| match record {
        GraphRecord::Node {
            importer_id: Some(importer),
            ..
        } => importer == IMPORTER_ID,
        _ => false,
    });
    let mut pull_requests = Vec::new();
    let mut issues = Vec::new();
    let mut reviews = Vec::new();
    for record in records {
        let GraphRecord::Node {
            kind,
            source_kind,
            merge_commit_sha: Some(sha),
            ..
        } = record
        else {
            continue;
        };
        if sha != &introducing_commit {
            continue;
        }
        let link = OriginProjectLink { record };
        match kind {
            NodeKind::Review => reviews.push(link),
            NodeKind::Task if source_kind.as_deref() == Some(SOURCE_KIND_ISSUE) => {
                issues.push(link);
            }
            // `merge_commit_sha` is PR-only by importer construction; any
            // other carrier falls in the PR section by the same token.
            _ => pull_requests.push(link),
        }
    }
    for section in [&mut pull_requests, &mut issues, &mut reviews] {
        section.sort_by(|a: &OriginProjectLink<'_>, b: &OriginProjectLink<'_>| {
            a.record.id().cmp(b.record.id())
        });
    }

    Ok(OriginReport {
        symbol_record_id: introduced.record_id.clone(),
        symbol_name: query.to_owned(),
        introducing_commit,
        introducing_commit_record_id,
        introducing_valid_time: introduced.valid_time.clone(),
        repo_relative_path: introduced.repo_relative_path.clone(),
        span: introduced.span,
        github_import_present,
        pull_requests,
        issues,
        reviews,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::TemporalMetadata;

    fn temporal(commit: &str, parents: &[&str], valid_time: &str) -> TemporalMetadata {
        TemporalMetadata {
            git_commit: commit.to_owned(),
            git_parent_commits: parents.iter().map(|s| (*s).to_owned()).collect(),
            valid_time: valid_time.to_owned(),
            author_time: None,
            observed_at: valid_time.to_owned(),
            valid_time_source: None,
        }
    }

    fn commit(id: &str, sha: &str, parents: &[&str], valid_time: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Commit,
            None,
            None,
            Some(sha.to_owned()),
            format!("commit {sha}"),
        )
        .with_temporal(temporal(sha, parents, valid_time))
    }

    fn symbol(id: &str, name: &str, sha: &str, parents: &[&str], valid_time: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some(name.to_owned()),
            format!("fn {name}"),
        )
        // Symbol snapshots carry their commit's parent links, like real
        // `scan-history` output.
        .with_temporal(temporal(sha, parents, valid_time))
    }

    /// A project `Task` node carrying the promoted flat fields. The real
    /// importer stamps these; the test builds them at the JSON level, the
    /// same way the integration fixtures do.
    fn pr_task(id: &str, merge_commit_sha: Option<&str>, source_kind: &str) -> GraphRecord {
        let mut value = serde_json::to_value(GraphRecord::node(
            id.to_owned(),
            NodeKind::Task,
            None,
            None,
            Some("PR #10".to_owned()),
            "task".to_owned(),
        ))
        .unwrap();
        let obj = value.as_object_mut().unwrap();
        obj.insert(
            "importer_id".to_owned(),
            serde_json::Value::String(IMPORTER_ID.to_owned()),
        );
        obj.insert(
            "source_kind".to_owned(),
            serde_json::Value::String(source_kind.to_owned()),
        );
        if let Some(sha) = merge_commit_sha {
            obj.insert(
                "merge_commit_sha".to_owned(),
                serde_json::Value::String(sha.to_owned()),
            );
        }
        serde_json::from_value(value).unwrap()
    }

    fn history() -> Vec<GraphRecord> {
        vec![
            commit("commit:1", "c1", &[], "2026-01-01T00:00:00Z"),
            commit("commit:2", "c2", &["c1"], "2026-01-02T00:00:00Z"),
            commit("commit:3", "c3", &["c2"], "2026-01-03T00:00:00Z"),
            symbol(
                "symbol:answer",
                "answer",
                "c2",
                &["c1"],
                "2026-01-02T00:00:00Z",
            ),
            symbol(
                "symbol:answer",
                "answer",
                "c3",
                &["c2"],
                "2026-01-03T00:00:00Z",
            ),
        ]
    }

    #[test]
    fn origin_is_first_introduction_not_first_seen_snapshot() {
        let records = history();
        let report = symbol_origin(&records, "answer", None, OriginPin::None).unwrap();
        assert_eq!(report.introducing_commit, "c2");
        assert_eq!(report.introducing_valid_time, "2026-01-02T00:00:00Z");
        assert_eq!(report.symbol_record_id, "symbol:answer");
        assert_eq!(
            report.introducing_commit_record_id,
            Some("commit:2".to_owned()),
            "the Commit node for the introducing SHA is cited"
        );
    }

    #[test]
    fn project_link_requires_exact_sha_equality() {
        let mut records = history();
        records.push(pr_task("project:v1:task:10", Some("c2"), "github_pr"));
        records.push(pr_task("project:v1:task:11", Some("c2-extra"), "github_pr"));
        records.push(pr_task("project:v1:task:12", Some("c3"), "github_pr"));
        records.push(pr_task("project:v1:task:13", None, "github_pr"));
        let report = symbol_origin(&records, "answer", None, OriginPin::None).unwrap();
        assert!(report.github_import_present);
        let ids: Vec<&str> = report.pull_requests.iter().map(|l| l.record.id()).collect();
        assert_eq!(
            ids,
            vec!["project:v1:task:10"],
            "only the byte-exact merge_commit_sha matches"
        );
    }

    #[test]
    fn at_pin_before_introduction_is_unknown_symbol() {
        let records = history();
        let error = symbol_origin(&records, "answer", None, OriginPin::AtCommit("c1")).unwrap_err();
        assert_eq!(
            error,
            OriginError::UnknownSymbol {
                query: "answer".to_owned()
            }
        );
    }

    #[test]
    fn at_pin_at_introduction_resolves() {
        let records = history();
        let report = symbol_origin(&records, "answer", None, OriginPin::AtCommit("c2")).unwrap();
        assert_eq!(report.introducing_commit, "c2");
    }

    #[test]
    fn at_pin_after_introduction_keeps_first_introduction() {
        let records = history();
        let report = symbol_origin(&records, "answer", None, OriginPin::AtCommit("c3")).unwrap();
        assert_eq!(
            report.introducing_commit, "c2",
            "origin is the first introduction, not the pin"
        );
    }

    #[test]
    fn at_unknown_prefix_is_missing_commit() {
        let records = history();
        let error =
            symbol_origin(&records, "answer", None, OriginPin::AtCommit("deadbeef")).unwrap_err();
        assert_eq!(
            error,
            OriginError::MissingCommit {
                commit_prefix: "deadbeef".to_owned()
            }
        );
    }

    #[test]
    fn as_of_before_introduction_is_unknown_symbol() {
        let records = history();
        let error = symbol_origin(
            &records,
            "answer",
            None,
            OriginPin::AsOf("2026-01-01T12:00:00Z"),
        )
        .unwrap_err();
        assert_eq!(
            error,
            OriginError::UnknownSymbol {
                query: "answer".to_owned()
            }
        );
    }

    #[test]
    fn as_of_after_introduction_resolves() {
        let records = history();
        let report = symbol_origin(
            &records,
            "answer",
            None,
            OriginPin::AsOf("2026-01-03T12:00:00Z"),
        )
        .unwrap();
        assert_eq!(report.introducing_commit, "c2");
    }

    #[test]
    fn malformed_as_of_is_invalid() {
        let records = history();
        let error =
            symbol_origin(&records, "answer", None, OriginPin::AsOf("not-a-time")).unwrap_err();
        assert_eq!(
            error,
            OriginError::InvalidAsOf {
                as_of: "not-a-time".to_owned()
            }
        );
    }

    #[test]
    fn unknown_symbol_errors() {
        let records = history();
        let error = symbol_origin(&records, "missing", None, OriginPin::None).unwrap_err();
        assert_eq!(
            error,
            OriginError::UnknownSymbol {
                query: "missing".to_owned()
            }
        );
    }

    #[test]
    fn symbol_without_history_errors_no_history() {
        let records = vec![GraphRecord::node(
            "symbol:answer".to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some("answer".to_owned()),
            "fn answer".to_owned(),
        )];
        let error = symbol_origin(&records, "answer", None, OriginPin::None).unwrap_err();
        assert_eq!(
            error,
            OriginError::NoHistory {
                query: "answer".to_owned()
            }
        );
    }

    #[test]
    fn suppressed_records_are_invisible_to_the_core() {
        // Eviction suppression happens BEFORE the core is called (the CLI
        // pre-filters, mirroring `query resolve`): the core answers over
        // exactly the slice it is given, so a suppressed-only import reads
        // as absent here.
        let mut records = history();
        records.push(pr_task("project:v1:task:10", Some("c2"), "github_pr"));
        let visible: Vec<GraphRecord> = records
            .into_iter()
            .filter(|r| r.id() != "project:v1:task:10")
            .collect();
        let report = symbol_origin(&visible, "answer", None, OriginPin::None).unwrap();
        assert_eq!(report.introducing_commit, "c2");
        assert!(report.pull_requests.is_empty());
        assert!(!report.github_import_present);
    }
}
