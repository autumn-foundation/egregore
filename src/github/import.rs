//! GitHub import orchestrator: fetch → redact → transform → idempotent handoff.
//!
//! Ties the REST client, the pure record transform, and the idempotency state
//! file into the `eg import github <owner>/<repo>` workflow specified in
//! `docs/schema/import-github.md` and `docs/cli/github-import.md`.
//!
//! v1 scope for issue #46 promotes the reserved `Review` kind: issues and PRs
//! emit `Task` + `ExternalLink`; issue comments, PR review summaries, and PR
//! review comments emit `project.Review` with `REFERENCES_TASK` (and, for
//! file-anchored review comments, `TOUCHES_FILE`) edges. Re-imports are
//! conditional (`If-None-Match`) and emit only changed resources.

use std::{path::Path, time::Duration};

use serde_json::Value;

use crate::{
    adapters::records_from_jsonl,
    github::{
        auth,
        client::{Client, FetchOutcome},
        error::{GithubError, GithubResult},
        model,
        records::{self, CommitIndex, Context, Emitted, FileIndex},
        state::{self, State},
    },
    ir::{Graph, GraphRecord, NodeKind, PROJECT_SCHEMA_VERSION, project_stable_id},
    redaction::{REDACTION_POLICY_VERSION, redact_value},
};

/// Options controlling one import run.
pub struct ImportOptions<'a> {
    /// `<owner>/<repo>` to import.
    pub source_repo: &'a str,
    /// API base URL (override for tests; defaults to `api.github.com`).
    pub api_base: String,
    /// Optional `--token-file` path.
    pub token_file: Option<&'a Path>,
    /// Optional seeded code-graph JSONL for `TOUCHES_FILE` resolution.
    pub code_graph: Option<&'a Path>,
    /// Fixed RFC 3339 transaction time for deterministic output; `None` = now.
    pub transaction_time: Option<String>,
    /// When `true`, never sleep on backoff (tests).
    pub no_backoff: bool,
}

/// Summary of a completed import run (the per-run stderr line, AC + metric).
pub struct RunSummary {
    /// Total HTTP requests issued.
    pub requests: u64,
    /// `X-RateLimit-Remaining` at finish, if observed.
    pub quota_remaining: Option<u64>,
    /// Wall-clock seconds elapsed.
    pub elapsed_secs: f64,
    /// Number of project-graph records written (excluding the handoff record).
    pub emitted_records: usize,
}

/// Result of a completed import.
pub struct ImportOutcome {
    /// Canonically ordered handoff JSONL.
    pub jsonl: String,
    /// Updated idempotency state.
    pub state: State,
    /// Per-run summary.
    pub summary: RunSummary,
}

/// Runs one import. The caller is responsible for writing `jsonl`/`state` to
/// disk; this keeps the engine pure and testable (the CLI wrapper persists).
///
/// # Errors
///
/// Returns a typed [`GithubError`] naming the failing source class on auth,
/// repo-availability, rate-limit, or fetch failure.
#[allow(clippy::too_many_lines)]
pub fn run_import(opts: &ImportOptions<'_>, prior_state: State) -> GithubResult<ImportOutcome> {
    let started = std::time::Instant::now();
    let transaction_time = opts
        .transaction_time
        .clone()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

    let token = auth::resolve_token(opts.token_file);
    let client = {
        let c = Client::new(opts.api_base.clone(), token);
        if opts.no_backoff {
            c.with_sleep(Box::new(|_d: Duration| {}))
        } else {
            c
        }
    };

    // Repo probe (auth state machine). Failures here may suppress state writes.
    client.probe_repo(opts.source_repo)?;

    let (file_index, commit_index) = load_indexes(opts.code_graph)?;
    let mut state = prior_state;
    let mut graph = Graph::new();
    let mut emitted_count = 0usize;

    let ctx = Context {
        source_repo: opts.source_repo,
        transaction_time: &transaction_time,
        redact: &redact_value,
        file_index: &file_index,
        commit_index: &commit_index,
    };

    // ── Issues (Task + ExternalLink) ────────────────────────────────────────────
    let issues_path = format!("/repos/{}/issues?state=all&per_page=100", opts.source_repo);
    if let FetchOutcome::Modified { items, etags } =
        client.fetch_paginated("issues", &issues_path, &state.etags)?
    {
        state.etags.extend(etags);
        let mut watermark = state.last_seen_updated_at.issues.clone();
        for item in &items {
            let Ok(issue) = serde_json::from_value::<model::Issue>(item.clone()) else {
                continue;
            };
            // The issues endpoint includes PRs; discard those (handled by pulls).
            if issue.pull_request.is_some() {
                continue;
            }
            advance_watermark(&mut watermark, &issue.updated_at);
            let key = format!("issue:{}", issue.number);
            let hash = state::issue_hash(&issue);
            if state.is_unchanged(&key, &hash) {
                continue;
            }
            state.record_hash(key, hash);
            push_emitted(
                &mut graph,
                &mut emitted_count,
                records::issue_records(&ctx, &issue),
            );
        }
        state.last_seen_updated_at.issues = watermark;
    }

    // ── Pull requests (Task + ExternalLink) ─────────────────────────────────────
    let pulls_path = format!("/repos/{}/pulls?state=all&per_page=100", opts.source_repo);
    let mut changed_pr_numbers: Vec<u64> = Vec::new();
    let mut pulls_changed = false;
    if let FetchOutcome::Modified { items, etags } =
        client.fetch_paginated("pulls", &pulls_path, &state.etags)?
    {
        pulls_changed = true;
        state.etags.extend(etags);
        let mut watermark = state.last_seen_updated_at.pulls.clone();
        for item in &items {
            let Ok(pr) = serde_json::from_value::<model::PullRequest>(item.clone()) else {
                continue;
            };
            advance_watermark(&mut watermark, &pr.updated_at);
            changed_pr_numbers.push(pr.number);
            let key = format!("pr:{}", pr.number);
            // The MERGED_AS resolution outcome against the seeded code graph
            // participates in the change hash (#333, Codex round-4): a changed
            // seed graph that now resolves this PR's merge_commit_sha must
            // re-emit the merge edge even though the PR payload is unchanged.
            let merge_marker = records::merge_resolution_marker(ctx.commit_index, &pr);
            let hash = state::pull_hash(&pr, &merge_marker);
            if state.is_unchanged(&key, &hash) {
                continue;
            }
            state.record_hash(key, hash);
            push_emitted(
                &mut graph,
                &mut emitted_count,
                records::pull_records(&ctx, &pr),
            );
        }
        state.last_seen_updated_at.pulls = watermark;
    }

    // ── Labels (list hash only; label changes also ride on issue/PR hashes) ─────
    let labels_path = format!("/repos/{}/labels?per_page=100", opts.source_repo);
    if let FetchOutcome::Modified { items, etags } =
        client.fetch_paginated("labels", &labels_path, &state.etags)?
    {
        state.etags.extend(etags);
        let labels: Vec<model::Label> = items
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        state.label_list_hash = Some(state::label_list_hash(&labels));
    }

    // ── Issue comments → project.Review (issue_comment) ─────────────────────────
    let ic_path = format!("/repos/{}/issues/comments?per_page=100", opts.source_repo);
    if let FetchOutcome::Modified { items, etags } =
        client.fetch_paginated("issue_comments", &ic_path, &state.etags)?
    {
        state.etags.extend(etags);
        for item in &items {
            let Ok(c) = serde_json::from_value::<model::IssueComment>(item.clone()) else {
                continue;
            };
            let key = format!("issue_comment:{}", c.id);
            let hash = blake3_hash_value(item);
            if state.is_unchanged(&key, &hash) {
                continue;
            }
            state.record_hash(key, hash);
            push_emitted(
                &mut graph,
                &mut emitted_count,
                records::issue_comment_records(&ctx, &c),
            );
        }
    }

    // ── PR review comments → project.Review (pr_review_comment) ──────────────────
    let prc_path = format!("/repos/{}/pulls/comments?per_page=100", opts.source_repo);
    if let FetchOutcome::Modified { items, etags } =
        client.fetch_paginated("pr_review_comments", &prc_path, &state.etags)?
    {
        state.etags.extend(etags);
        for item in &items {
            let Ok(c) = serde_json::from_value::<model::ReviewComment>(item.clone()) else {
                continue;
            };
            let key = format!("pr_review_comment:{}", c.id);
            let hash = blake3_hash_value(item);
            if state.is_unchanged(&key, &hash) {
                continue;
            }
            state.record_hash(key, hash);
            push_emitted(
                &mut graph,
                &mut emitted_count,
                records::review_comment_records(&ctx, &c),
            );
        }
    }

    // ── PR review summaries → project.Review (pr_review), per changed PR ─────────
    // Per §5, per-PR review fetches fire only when the pulls list changed.
    if pulls_changed {
        changed_pr_numbers.sort_unstable();
        changed_pr_numbers.dedup();
        for number in changed_pr_numbers {
            let path = format!(
                "/repos/{}/pulls/{number}/reviews?per_page=100",
                opts.source_repo
            );
            if let FetchOutcome::Modified { items, etags } =
                client.fetch_paginated("pr_reviews", &path, &state.etags)?
            {
                state.etags.extend(etags);
                for item in &items {
                    let Ok(r) = serde_json::from_value::<model::Review>(item.clone()) else {
                        continue;
                    };
                    let key = format!("pr_review:{number}:{}", r.id);
                    let hash = blake3_hash_value(item);
                    if state.is_unchanged(&key, &hash) {
                        continue;
                    }
                    state.record_hash(key, hash);
                    push_emitted(
                        &mut graph,
                        &mut emitted_count,
                        records::pr_review_records(&ctx, number, &r),
                    );
                }
            }
        }
    }

    // ── Handoff metadata record (always emitted) ────────────────────────────────
    graph.push(handoff_record(
        opts.source_repo,
        &transaction_time,
        emitted_count,
    ));

    // Finalise state bookkeeping.
    state.last_run_at_unix_ms = now_unix_ms();
    state.api_base_url.clone_from(&opts.api_base);
    opts.source_repo.clone_into(&mut state.source_repo);

    let jsonl = graph.to_jsonl().map_err(|e| GithubError::Io {
        detail: format!("serialize handoff: {e}"),
    })?;

    let summary = RunSummary {
        requests: client.request_count(),
        quota_remaining: client.quota_remaining(),
        elapsed_secs: started.elapsed().as_secs_f64(),
        emitted_records: emitted_count,
    };

    Ok(ImportOutcome {
        jsonl,
        state,
        summary,
    })
}

/// Pushes every record of an [`Emitted`] batch into `graph`, counting them.
fn push_emitted(graph: &mut Graph, count: &mut usize, emitted: Emitted) {
    for rec in emitted.records {
        *count += 1;
        graph.push(rec);
    }
}

/// Builds the top-level handoff metadata record (a project `Diagnostic` node).
fn handoff_record(source_repo: &str, transaction_time: &str, emitted: usize) -> GraphRecord {
    let id = project_stable_id(&[
        "project",
        "Diagnostic",
        records::IMPORTER_ID,
        source_repo,
        "github_import_handoff",
    ]);
    let summary = format!(
        "[github_import_handoff] repo={source_repo} emitted={emitted} at={transaction_time}"
    );
    let mut rec = GraphRecord::node(id.clone(), NodeKind::Diagnostic, None, None, None, summary);
    if let GraphRecord::Node {
        schema_version,
        domain,
        entity_id,
        valid_time,
        valid_time_source,
        transaction_time: tt,
        importer_id,
        importer_version,
        redaction_policy_version,
        ..
    } = &mut rec
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *domain = Some(records::DOMAIN.to_owned());
        *entity_id = Some(id);
        *valid_time = Some(transaction_time.to_owned());
        *valid_time_source = Some("github_updated_at".to_owned());
        *tt = Some(transaction_time.to_owned());
        *importer_id = Some(records::IMPORTER_ID.to_owned());
        *importer_version = Some(records::IMPORTER_VERSION.to_owned());
        *redaction_policy_version = Some(REDACTION_POLICY_VERSION.to_owned());
    }
    rec
}

/// Loads both the `repo_relative_path -> [file_id]` and `commit_sha ->
/// [commit_id]` indexes from a code-graph JSONL in a single read + parse pass.
///
/// A `File` node carries its path in `repo_relative_path`; a `Commit` node
/// carries its SHA in the `name` field (see `commit_record` in `history.rs`).
/// A SHA claimed by more than one `Commit` record is ambiguous and is diagnosed
/// rather than linked by [`records::pull_records`] (#333).
fn load_indexes(code_graph: Option<&Path>) -> GithubResult<(FileIndex, CommitIndex)> {
    let mut files = FileIndex::new();
    let mut commits = CommitIndex::new();
    let Some(path) = code_graph else {
        return Ok((files, commits));
    };
    let jsonl = std::fs::read_to_string(path).map_err(|e| GithubError::Io {
        detail: format!("read code-graph {}: {e}", path.display()),
    })?;
    let records = records_from_jsonl(&jsonl).map_err(|e| GithubError::Io {
        detail: format!("parse code-graph: {e}"),
    })?;
    for rec in &records {
        match rec {
            GraphRecord::Node {
                id,
                kind: NodeKind::File,
                repo_relative_path: Some(p),
                ..
            } => files.entry(p.clone()).or_default().push(id.clone()),
            GraphRecord::Node {
                id,
                kind: NodeKind::Commit,
                name: Some(sha),
                ..
            } => commits.entry(sha.clone()).or_default().push(id.clone()),
            _ => {}
        }
    }
    Ok((files, commits))
}

/// Advances `watermark` to `candidate` when it is lexically greater (RFC 3339
/// timestamps sort chronologically).
fn advance_watermark(watermark: &mut Option<String>, candidate: &str) {
    if candidate.is_empty() {
        return;
    }
    match watermark {
        Some(w) if w.as_str() >= candidate => {}
        _ => *watermark = Some(candidate.to_owned()),
    }
}

/// BLAKE3 hex of the canonical JSON of a value (change-detection for reviews).
fn blake3_hash_value(value: &Value) -> String {
    let canon = serde_json::to_string(value).unwrap_or_default();
    blake3::hash(canon.as_bytes()).to_hex().to_string()
}

fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermark_advances_only_forward() {
        let mut w = Some("2026-01-02T00:00:00Z".to_owned());
        advance_watermark(&mut w, "2026-01-01T00:00:00Z");
        assert_eq!(w.as_deref(), Some("2026-01-02T00:00:00Z"));
        advance_watermark(&mut w, "2026-01-03T00:00:00Z");
        assert_eq!(w.as_deref(), Some("2026-01-03T00:00:00Z"));
    }

    #[test]
    fn handoff_record_is_project_diagnostic() {
        let rec = handoff_record("o/r", "2026-01-01T00:00:00Z", 7);
        match rec {
            GraphRecord::Node { kind, domain, .. } => {
                assert_eq!(kind, NodeKind::Diagnostic);
                assert_eq!(domain.as_deref(), Some("project"));
            }
            _ => panic!("expected node"),
        }
    }
}
