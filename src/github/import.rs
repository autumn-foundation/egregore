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
    // Run-level dedup of ExternalIdentity nodes to one-per-login (issue #335).
    let mut seen_identities: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // Seed-graph fingerprint gating for the `/pulls` conditional request (#333,
    // Codex round-5). PR merge-link resolution depends on the local seed graph,
    // which GitHub's `/pulls` `ETag` cannot see, so a cached `ETag` can return 304
    // and short-circuit PR processing before the round-4 `pull_hash` marker ever
    // runs. When the current fingerprint differs from the stored one (including a
    // `None`/"unknown" stored value from a pre-fingerprint state file, and every
    // none→some / some→different / some→none transition), the `/pulls` `ETag` is
    // suppressed below so GitHub returns a full 200 and merge links recompute; an
    // unchanged fingerprint keeps the 304 fast path. The same fingerprint gate is
    // extended to the commit-anchored review endpoints (`/pulls/comments` and
    // `/pulls/{n}/reviews`) for REVIEWS_COMMIT recomputation (#334). Only
    // `/issues/comments` (exempt issue_comment reviews) and other endpoints keep
    // their conditional fast path unconditionally.
    let code_graph_fingerprint = state::code_graph_fingerprint(&commit_index);
    let seed_graph_changed =
        state.code_graph_fingerprint.as_deref() != Some(code_graph_fingerprint.as_str());

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
                &mut seen_identities,
                records::issue_records(&ctx, &issue),
            );
        }
        state.last_seen_updated_at.issues = watermark;
    }

    // ── Pull requests (Task + ExternalLink) ─────────────────────────────────────
    let pulls_path = format!("/repos/{}/pulls?state=all&per_page=100", opts.source_repo);
    let mut changed_pr_numbers: Vec<u64> = Vec::new();
    let mut pulls_changed = false;
    // A changed seed graph suppresses the `/pulls` conditional `ETag` so GitHub
    // returns a full 200 and merge links are recomputed even when the PR payload
    // is byte-identical. An unchanged seed keeps every stored page `ETag` (304
    // fast path). Only the `/pulls` page `ETags` are dropped; all other endpoints
    // continue to use `state.etags`.
    let pulls_prior_etags = if seed_graph_changed {
        etags_without_prefix(&state.etags, &format!("{pulls_path}?page="))
    } else {
        state.etags.clone()
    };
    if let FetchOutcome::Modified { items, etags } =
        client.fetch_paginated("pulls", &pulls_path, &pulls_prior_etags)?
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
            // The record id of the merge artifact this run would emit (edge,
            // diagnostic, or none). Since the marker is folded into `hash`, an
            // unchanged hash guarantees an unchanged artifact id, so tombstoning
            // is only ever needed on the reprocess path below (#333, round-6).
            let current_artifact =
                records::merge_artifact_id(ctx.commit_index, opts.source_repo, &pr);
            // The current set of REQUESTED_REVIEW_FROM edge ids this PR emits, for
            // the requested-reviewer supersession diff (#335, Codex P1). Folded
            // into `pull_hash`, so an unchanged hash guarantees an unchanged set.
            let current_request_edges = records::requested_review_edge_ids(opts.source_repo, &pr);
            // The current set of github_team_review_request_unexpanded Diagnostic
            // ids this PR emits, for the requested-team supersession diff (#335,
            // Codex P2 — the exact sibling of the reviewer-edge case above). Folded
            // into `pull_hash` (via requested_teams), so an unchanged hash
            // guarantees an unchanged set.
            let current_team_diagnostics =
                records::team_review_diagnostic_ids(opts.source_repo, &pr);
            if state.is_unchanged(&key, &hash) {
                // Backfill the tracked artifact id without emitting anything: a
                // no-op on a store this build already wrote, but it populates a
                // pre-round-6 (or legacy) store so a LATER outcome change can
                // still retract this artifact. Safe because the unchanged hash
                // proves `current_artifact` equals what was emitted before.
                state.set_merge_artifact(key.clone(), current_artifact);
                // Backfill the prior request set likewise (#335, Codex P1) so a
                // legacy store gains the tracking without a re-emit; the unchanged
                // hash proves the request set is unchanged too.
                state.set_request_edges(key.clone(), current_request_edges);
                // Backfill the prior team-diagnostic set likewise (#335, Codex P2)
                // so a legacy store gains the tracking without a re-emit; the
                // unchanged hash proves the team set is unchanged too.
                state.set_team_diagnostics(key, current_team_diagnostics);
                continue;
            }
            // Retract a superseded merge artifact whose outcome changed on this
            // re-import (#333, Codex round-6): the importer is otherwise purely
            // additive, so without this the prior edge/diagnostic would linger
            // live alongside the new one in a persistent store. Emit the
            // tombstone (keyed on the prior record's id) before the fresh
            // outcome; `graph.to_jsonl` sorts, so relative order is immaterial.
            if let Some(prior) = state.prior_merge_artifact(&key).map(str::to_owned)
                && Some(prior.as_str()) != current_artifact.as_deref()
            {
                push_emitted(
                    &mut graph,
                    &mut emitted_count,
                    &mut seen_identities,
                    Emitted {
                        records: vec![records::merge_artifact_tombstone(pr.number, &prior)],
                        link_diagnostics: 0,
                    },
                );
            }
            // Retract each REQUESTED_REVIEW_FROM edge whose reviewer was removed
            // from the PR's requested set since the last run (#335, Codex P1):
            // the importer is otherwise purely additive, so without this a
            // removed reviewer's edge lingers live in a persistent store and
            // downstream queries still report them as "requested". Only the edge
            // is tombstoned — never the global ExternalIdentity node.
            let removed_request_edges: Vec<String> = state
                .prior_request_edges(&key)
                .iter()
                .filter(|prior| !current_request_edges.iter().any(|c| c == *prior))
                .cloned()
                .collect();
            for prior in &removed_request_edges {
                push_emitted(
                    &mut graph,
                    &mut emitted_count,
                    &mut seen_identities,
                    Emitted {
                        records: vec![records::request_review_edge_tombstone(pr.number, prior)],
                        link_diagnostics: 0,
                    },
                );
            }
            // Retract each github_team_review_request_unexpanded diagnostic whose
            // team was removed from the PR's requested-team set since the last run
            // (#335, Codex P2): the exact sibling of the reviewer-edge case above.
            // Without this a removed team's diagnostic lingers live in a persistent
            // store and current-state queries still report the removed team's
            // review request. Only the diagnostic is tombstoned — never a reviewer
            // edge, an identity node, or a REVIEWED_BY edge.
            let removed_team_diagnostics: Vec<String> = state
                .prior_team_diagnostics(&key)
                .iter()
                .filter(|prior| !current_team_diagnostics.iter().any(|c| c == *prior))
                .cloned()
                .collect();
            for prior in &removed_team_diagnostics {
                push_emitted(
                    &mut graph,
                    &mut emitted_count,
                    &mut seen_identities,
                    Emitted {
                        records: vec![records::team_review_diagnostic_tombstone(pr.number, prior)],
                        link_diagnostics: 0,
                    },
                );
            }
            state.record_hash(key.clone(), hash);
            state.set_merge_artifact(key.clone(), current_artifact);
            state.set_request_edges(key.clone(), current_request_edges);
            state.set_team_diagnostics(key, current_team_diagnostics);
            push_emitted(
                &mut graph,
                &mut emitted_count,
                &mut seen_identities,
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
                &mut seen_identities,
                records::issue_comment_records(&ctx, &c),
            );
        }
    }

    // ── PR review comments → project.Review (pr_review_comment) ──────────────────
    // Commit-anchored (#334): a changed seed graph suppresses this endpoint's
    // conditional `ETag` too, so REVIEWS_COMMIT anchors are recomputed even when
    // the comment payload is byte-identical. GitHub's `ETag` cannot observe the
    // local seed, exactly as for `/pulls` (#333, round-5).
    let prc_path = format!("/repos/{}/pulls/comments?per_page=100", opts.source_repo);
    let prc_prior_etags = if seed_graph_changed {
        etags_without_prefix(&state.etags, &format!("{prc_path}?page="))
    } else {
        state.etags.clone()
    };
    if let FetchOutcome::Modified { items, etags } =
        client.fetch_paginated("pr_review_comments", &prc_path, &prc_prior_etags)?
    {
        state.etags.extend(etags);
        for item in &items {
            let Ok(c) = serde_json::from_value::<model::ReviewComment>(item.clone()) else {
                continue;
            };
            let key = format!("pr_review_comment:{}", c.id);
            // Anchor identity (#334) only applies when the parent PR number
            // parses, matching review_comment_records (which emits nothing
            // otherwise).
            let native = model::trailing_number(&c.pull_request_url)
                .map(|n| format!("pr_review_comment:{n}:{}", c.id));
            // Fold the REVIEWS_COMMIT resolution outcome into the change hash so
            // a seed graph that newly resolves this comment's commit_id re-emits
            // the anchor even when the comment payload is unchanged (#334).
            let marker = records::review_commit_marker(
                ctx.commit_index,
                "pr_review_comment",
                c.commit_id.as_deref(),
            );
            let hash = state::review_hash(&blake3_hash_value(item), &marker);
            let current_artifact = native.as_deref().and_then(|n| {
                records::review_artifact_id(
                    ctx.commit_index,
                    opts.source_repo,
                    n,
                    "pr_review_comment",
                    c.commit_id.as_deref(),
                )
            });
            if state.is_unchanged(&key, &hash) {
                // Backfill the tracked artifact id without emitting (idempotent
                // on a store this build already wrote; populates a legacy store
                // so a LATER outcome change can still retract this artifact).
                state.set_review_artifact(key, current_artifact);
                continue;
            }
            // Retract a superseded anchor artifact whose outcome changed on this
            // re-import (#334): the importer is otherwise purely additive.
            if let Some(prior) = state.prior_review_artifact(&key).map(str::to_owned)
                && Some(prior.as_str()) != current_artifact.as_deref()
                && let Some(native) = native.as_deref()
            {
                push_emitted(
                    &mut graph,
                    &mut emitted_count,
                    &mut seen_identities,
                    Emitted {
                        records: vec![records::review_artifact_tombstone(native, &prior)],
                        link_diagnostics: 0,
                    },
                );
            }
            state.record_hash(key.clone(), hash);
            state.set_review_artifact(key, current_artifact);
            push_emitted(
                &mut graph,
                &mut emitted_count,
                &mut seen_identities,
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
            // Commit-anchored (#334): suppress this per-PR reviews `ETag` on a
            // changed seed graph so REVIEWS_COMMIT anchors recompute even when
            // the review payload is byte-identical.
            let reviews_prior_etags = if seed_graph_changed {
                etags_without_prefix(&state.etags, &format!("{path}?page="))
            } else {
                state.etags.clone()
            };
            if let FetchOutcome::Modified { items, etags } =
                client.fetch_paginated("pr_reviews", &path, &reviews_prior_etags)?
            {
                state.etags.extend(etags);
                for item in &items {
                    let Ok(r) = serde_json::from_value::<model::Review>(item.clone()) else {
                        continue;
                    };
                    let key = format!("pr_review:{number}:{}", r.id);
                    let native = format!("pr_review:{number}:{}", r.id);
                    // Fold the REVIEWS_COMMIT resolution outcome into the change
                    // hash (#334): a seed graph that newly resolves this review's
                    // commit_id re-emits the anchor even when the payload is
                    // unchanged.
                    let marker = records::review_commit_marker(
                        ctx.commit_index,
                        "pr_review",
                        r.commit_id.as_deref(),
                    );
                    let hash = state::review_hash(&blake3_hash_value(item), &marker);
                    let current_artifact = records::review_artifact_id(
                        ctx.commit_index,
                        opts.source_repo,
                        &native,
                        "pr_review",
                        r.commit_id.as_deref(),
                    );
                    if state.is_unchanged(&key, &hash) {
                        state.set_review_artifact(key, current_artifact);
                        continue;
                    }
                    // Retract a superseded anchor artifact whose outcome changed
                    // on this re-import (#334); otherwise purely additive.
                    if let Some(prior) = state.prior_review_artifact(&key).map(str::to_owned)
                        && Some(prior.as_str()) != current_artifact.as_deref()
                    {
                        push_emitted(
                            &mut graph,
                            &mut emitted_count,
                            &mut seen_identities,
                            Emitted {
                                records: vec![records::review_artifact_tombstone(&native, &prior)],
                                link_diagnostics: 0,
                            },
                        );
                    }
                    state.record_hash(key.clone(), hash);
                    state.set_review_artifact(key, current_artifact);
                    push_emitted(
                        &mut graph,
                        &mut emitted_count,
                        &mut seen_identities,
                        records::pr_review_records(&ctx, number, &r),
                    );
                }
            }

            // ── PR review-state transitions → ReviewStateTransition (issue #336) ─
            // Gated on the SAME `if pulls_changed` trigger as the per-PR review
            // summaries above. The timeline records the append-only history a
            // dismissal would otherwise erase: `review_state` stays last-write-
            // wins, while each transition event is preserved. The stream is
            // filtered to the closed review-state-transition kinds; every other
            // timeline event kind is skipped. Timeline events depend on no seed
            // graph, so — unlike the reviews endpoint — this fetch always uses the
            // stored `ETags` directly (no seed-graph suppression).
            let timeline_path = format!(
                "/repos/{}/issues/{number}/timeline?per_page=100",
                opts.source_repo
            );
            if let FetchOutcome::Modified { items, etags } =
                client.fetch_paginated("timeline", &timeline_path, &state.etags)?
            {
                state.etags.extend(etags);
                for item in &items {
                    // Filter to the closed transition kinds; an unknown event
                    // kind is skipped silently (out of scope, never a diagnostic).
                    let kind = item.get("event").and_then(Value::as_str).unwrap_or("");
                    if !records::is_review_state_transition_kind(kind) {
                        continue;
                    }
                    // Per-event change gate. Transitions are append-only and
                    // immutable, so an unchanged event never re-emits.
                    let event_id = item.get("id").and_then(Value::as_u64).unwrap_or(0);
                    let key = format!("timeline_event:{event_id}");
                    let hash = blake3_hash_value(item);
                    if state.is_unchanged(&key, &hash) {
                        continue;
                    }
                    state.record_hash(key, hash);
                    // A KNOWN-kind event that fails to parse emits a diagnostic,
                    // never a silent drop (mirrors commit/review diagnostics).
                    let emitted = serde_json::from_value::<model::TimelineEvent>(item.clone())
                        .map_or_else(
                            |_| {
                                records::timeline_transition_unparseable(
                                    &ctx, number, event_id, kind,
                                )
                            },
                            |ev| records::timeline_transition_records(&ctx, number, &ev),
                        );
                    push_emitted(
                        &mut graph,
                        &mut emitted_count,
                        &mut seen_identities,
                        emitted,
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
    // Persist the current seed-graph fingerprint so the next unchanged-seed run
    // takes the `/pulls` 304 fast path again (#333, Codex round-5).
    state.code_graph_fingerprint = Some(code_graph_fingerprint);

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
///
/// `seen_identities` deduplicates `ExternalIdentity` nodes to one-per-`(system,
/// login)` across the whole run (issue #335): the same login can author reviews
/// on many PRs and be a requested reviewer, so its identity node is minted by
/// several emitters, but the run's JSONL must carry exactly one. The
/// deduplication is keyed on the node's stable id (which embeds `(system,
/// login)`), so the output stays byte-stable and idempotent. The
/// `REVIEWED_BY` / `REQUESTED_REVIEW_FROM` edges are NOT deduplicated — each is
/// a distinct (review-or-task, identity) fact.
fn push_emitted(
    graph: &mut Graph,
    count: &mut usize,
    seen_identities: &mut std::collections::BTreeSet<String>,
    emitted: Emitted,
) {
    for rec in emitted.records {
        if let GraphRecord::Node {
            kind: NodeKind::ExternalIdentity,
            id,
            ..
        } = &rec
            && !seen_identities.insert(id.clone())
        {
            // Already emitted this identity in this run; skip the duplicate node.
            continue;
        }
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

/// Returns a copy of `etags` with every key beginning `prefix` removed.
///
/// Used to suppress the `/pulls` per-page conditional `ETags` when the seed graph
/// changed (#333, Codex round-5), forcing a full 200 refetch of that endpoint
/// while leaving every other endpoint's stored `ETags` intact.
fn etags_without_prefix(
    etags: &std::collections::BTreeMap<String, String>,
    prefix: &str,
) -> std::collections::BTreeMap<String, String> {
    etags
        .iter()
        .filter(|(k, _)| !k.starts_with(prefix))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
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
