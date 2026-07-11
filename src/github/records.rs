//! Pure transform from GitHub REST models to project-graph records.
//!
//! This module is deterministic and network-free: given the same GitHub models,
//! the same redaction closure, the same transaction time, and the same
//! code-graph file index, it produces byte-identical records (the idempotency
//! invariant from `docs/schema/import-github.md` §9–10).
//!
//! v1 emission scope is the full slice for issue #46: every issue and PR emits
//! one `Task` + one `ExternalLink`; issue comments, PR review summaries, and PR
//! review comments emit `project.Review` records. Cross-domain edges
//! (`EXTERNAL_HANDLE`, `REFERENCES_TASK`, `TOUCHES_FILE`) connect them.

use std::collections::BTreeMap;

use crate::{
    github::model,
    ir::{
        EdgeLabel, GraphRecord, NodeKind, OutputHandle, PROJECT_SCHEMA_VERSION, project_stable_id,
    },
    redaction::REDACTION_POLICY_VERSION,
};

/// Stable importer ID stamped on every emitted record.
pub const IMPORTER_ID: &str = "github";
/// Importer version string; bump when the output contract changes.
pub const IMPORTER_VERSION: &str = "0.1.0";
/// Project domain value carried on every emitted record.
pub const DOMAIN: &str = "project";
/// `source_kind` for issue-derived tasks.
pub const SOURCE_KIND_ISSUE: &str = "github_issue";
/// `source_kind` for PR-derived tasks.
pub const SOURCE_KIND_PR: &str = "github_pr";
/// `source_kind` stamped on every GitHub-derived `Review` node (issue #334).
///
/// Review nodes originate solely from this importer, so the daemon's
/// `REVIEWS_COMMIT` project-edge validator requires this exact value on the FROM
/// node — the review-side analog of the `MERGED_AS` `github_pr` source-kind gate
/// (#333). A `REVIEWS_COMMIT` edge can therefore never originate from any node
/// that is not an importer-stamped Review.
pub const SOURCE_KIND_REVIEW: &str = "github_review";
/// `system` value for GitHub external links.
pub const SYSTEM: &str = "github";
/// Maximum bytes to inline in a handle (matches `CommandRun.stdout_handle`).
const INLINE_CEILING: usize = 16 * 1024;

/// Index from repo-relative file path to the code-graph `File` record IDs that
/// claim it. A path with more than one ID is ambiguous and is not linked.
pub type FileIndex = BTreeMap<String, Vec<String>>;

/// Index from commit SHA to the code-graph `Commit` record IDs that carry it.
///
/// Issue #333. A SHA with zero or more than one match is unresolved and is
/// diagnosed rather than linked (`MERGED_AS` resolve-or-diagnose discipline).
pub type CommitIndex = BTreeMap<String, Vec<String>>;

/// A redaction closure applied to free-text before persistence.
pub type Redact<'a> = dyn Fn(&str) -> String + 'a;

/// Context shared across every record built in one import run.
pub struct Context<'a> {
    /// `<owner>/<repo>` source repository.
    pub source_repo: &'a str,
    /// RFC 3339 store write time for this run.
    pub transaction_time: &'a str,
    /// Redaction closure (default: `crate::redaction::redact_value`).
    pub redact: &'a Redact<'a>,
    /// Code-graph file index for `TOUCHES_FILE` resolution; empty when no
    /// seeded store was provided.
    pub file_index: &'a FileIndex,
    /// Code-graph commit index for `MERGED_AS` resolution (issue #333); empty
    /// when no seeded store was provided.
    pub commit_index: &'a CommitIndex,
}

/// Records and diagnostics emitted for one or more GitHub resources.
#[derive(Default)]
pub struct Emitted {
    /// Project-graph records (nodes + edges).
    pub records: Vec<GraphRecord>,
    /// Count of file-link diagnostics emitted (also present in `records`).
    pub link_diagnostics: usize,
}

impl Emitted {
    fn extend(&mut self, other: Self) {
        self.records.extend(other.records);
        self.link_diagnostics += other.link_diagnostics;
    }
}

// ── Redaction helpers ──────────────────────────────────────────────────────────

/// Redacts a multi-line string line-by-line so structural context survives but
/// any line carrying a secret is replaced with a redaction marker. Used for
/// bodies and (per the `diff_hunk` sub-policy) diff hunks.
fn redact_lines(redact: &Redact<'_>, text: &str) -> String {
    text.split('\n').map(redact).collect::<Vec<_>>().join("\n")
}

/// Builds a redacted `OutputHandle` from already-redacted content.
///
/// Returns a `Box` because every caller stores it in an `Option<Box<_>>` field.
#[allow(clippy::unnecessary_box_returns)]
fn handle_for(content: &str) -> Box<OutputHandle> {
    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    let bytes = content.len() as u64;
    let inline = (content.len() <= INLINE_CEILING).then(|| content.to_owned());
    Box::new(OutputHandle {
        inline,
        hash,
        bytes,
    })
}

// ── Status mapping ─────────────────────────────────────────────────────────────

/// Maps a GitHub issue `state` + `state_reason` to a `Task.status` enum value.
fn issue_status(state: &str, state_reason: Option<&str>) -> &'static str {
    if state == "open" {
        return "open";
    }
    match state_reason {
        Some("not_planned") => "closed_dropped",
        Some("reopened") => "open",
        _ => "closed_completed",
    }
}

/// Maps a GitHub PR `state` + `merged_at` to a `Task.status` enum value.
fn pr_status(state: &str, merged_at: Option<&str>) -> &'static str {
    if merged_at.is_some() {
        "closed_completed"
    } else if state == "open" {
        "open"
    } else {
        "closed_dropped"
    }
}

// ── Common project-field application ────────────────────────────────────────────

/// Stamps the shared project-domain fields on a freshly built node.
fn set_common(record: &mut GraphRecord, id: &str, valid_time: &str, ctx: &Context<'_>) {
    if let GraphRecord::Node {
        schema_version,
        domain,
        entity_id,
        valid_time: vt,
        valid_time_source,
        transaction_time,
        importer_id,
        importer_version,
        redaction_policy_version,
        ..
    } = record
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *domain = Some(DOMAIN.to_owned());
        *entity_id = Some(id.to_owned());
        *vt = Some(valid_time.to_owned());
        *valid_time_source = Some("github_updated_at".to_owned());
        *transaction_time = Some(ctx.transaction_time.to_owned());
        *importer_id = Some(IMPORTER_ID.to_owned());
        *importer_version = Some(IMPORTER_VERSION.to_owned());
        *redaction_policy_version = Some(REDACTION_POLICY_VERSION.to_owned());
    }
}

/// Builds the redacted `Task.body_handle` blob carrying the body plus the
/// GitHub-only metadata that has no dedicated v1 `Task` field.
#[allow(clippy::too_many_arguments, clippy::unnecessary_box_returns)]
fn body_blob(
    ctx: &Context<'_>,
    body: Option<&str>,
    state: &str,
    state_reason: Option<&str>,
    milestone: Option<&str>,
    merged_at: Option<&str>,
    draft: Option<bool>,
    head_sha: Option<&str>,
    base_ref: Option<&str>,
    merge_commit_sha: Option<&str>,
    closed_at: Option<&str>,
) -> Box<OutputHandle> {
    let redacted_body = body.map(|b| redact_lines(ctx.redact, b));
    let redacted_milestone = milestone.map(|m| (ctx.redact)(m));
    let blob = serde_json::json!({
        "body": redacted_body,
        "metadata": {
            "state": state,
            "state_reason": state_reason,
            "milestone_title": redacted_milestone,
            "merged_at": merged_at,
            "draft": draft,
            "head_sha": head_sha,
            "base_ref": base_ref,
            "merge_commit_sha": merge_commit_sha,
            "closed_at": closed_at,
        }
    });
    let content = serde_json::to_string(&blob).unwrap_or_default();
    handle_for(&content)
}

// ── Issue / PR → Task + ExternalLink ───────────────────────────────────────────

/// GitHub PR fields promoted to first-class flat `Task` fields (issue #333).
///
/// Set only on the PR path; issue Tasks pass `None` so serde skips them. These
/// are plaintext query substrate per `docs/schema/import-github.md` §8 and are
/// never routed through redaction. They duplicate (rather than replace) the
/// values still carried in the redacted `body_handle` blob, so existing readers
/// are unaffected.
#[derive(Debug, Clone, Default)]
pub struct PrTaskFields {
    /// Head (source-branch) commit SHA.
    pub head_sha: Option<String>,
    /// Head (source-branch) ref name.
    pub head_ref: Option<String>,
    /// Base (target-branch) ref name.
    pub base_ref: Option<String>,
    /// Merge commit SHA; `Some` only when merged.
    pub merge_commit_sha: Option<String>,
    /// Merge timestamp string; `Some` means merged.
    pub merged_at: Option<String>,
    /// Draft flag.
    pub draft: Option<bool>,
}

/// Emits the `Task`, `ExternalLink`, and `EXTERNAL_HANDLE` edge for one issue.
#[must_use]
pub fn issue_records(ctx: &Context<'_>, issue: &model::Issue) -> Emitted {
    let number = issue.number;
    let native = format!("issue:{number}");
    task_and_link(
        ctx,
        NodeKind::Task,
        SOURCE_KIND_ISSUE,
        number,
        &native,
        &issue.title,
        &issue.html_url,
        &issue.updated_at,
        issue_status(&issue.state, issue.state_reason.as_deref()),
        &issue.labels,
        &issue.assignees,
        issue.user.as_ref().map(|u| u.login.as_str()),
        body_blob(
            ctx,
            issue.body.as_deref(),
            &issue.state,
            issue.state_reason.as_deref(),
            issue.milestone.as_ref().map(|m| m.title.as_str()),
            None,
            None,
            None,
            None,
            None,
            issue.closed_at.as_deref(),
        ),
        None,
    )
}

/// Emits the `Task`, `ExternalLink`, and `EXTERNAL_HANDLE` edge for one PR.
///
/// For an actually-merged PR (`merged_at` present) also emits a `MERGED_AS` edge
/// (or `github_commit_unresolved` diagnostic) when the PR's `merge_commit_sha`
/// resolves against a seeded code graph (issue #333). An unmerged PR's
/// test-merge SHA is never merge evidence: no flat field, edge, or diagnostic.
#[must_use]
pub fn pull_records(ctx: &Context<'_>, pr: &model::PullRequest) -> Emitted {
    let number = pr.number;
    let native = format!("pr:{number}");
    // `merge_commit_sha` is merge evidence only when the PR actually merged
    // (#333, Codex P2). For a mergeable-but-unmerged PR (open, or closed
    // unmerged) GitHub's REST API can populate `merge_commit_sha` with a
    // TEMPORARY TEST-MERGE commit rather than a landed merge commit; treating
    // that as evidence would corrupt the merge surface. Gate on `merged_at`.
    let is_merged = pr.merged_at.is_some();
    // Promote the PR-only fields to first-class flat Task fields (#333). These
    // duplicate the values still carried in `body_blob`, which is left
    // unchanged so existing body-blob readers are unaffected (AC3).
    let pr_fields = PrTaskFields {
        head_sha: pr.head.as_ref().map(|h| h.sha.clone()),
        head_ref: pr.head.as_ref().map(|h| h.ref_name.clone()),
        base_ref: pr.base.as_ref().map(|b| b.ref_name.clone()),
        merge_commit_sha: is_merged.then(|| pr.merge_commit_sha.clone()).flatten(),
        merged_at: pr.merged_at.clone(),
        draft: Some(pr.draft),
    };
    let mut emitted = task_and_link(
        ctx,
        NodeKind::Task,
        SOURCE_KIND_PR,
        number,
        &native,
        &pr.title,
        &pr.html_url,
        &pr.updated_at,
        pr_status(&pr.state, pr.merged_at.as_deref()),
        &pr.labels,
        &pr.assignees,
        pr.user.as_ref().map(|u| u.login.as_str()),
        body_blob(
            ctx,
            pr.body.as_deref(),
            &pr.state,
            None,
            pr.milestone.as_ref().map(|m| m.title.as_str()),
            pr.merged_at.as_deref(),
            Some(pr.draft),
            pr.head.as_ref().map(|h| h.sha.as_str()),
            pr.base.as_ref().map(|b| b.ref_name.as_str()),
            pr.merge_commit_sha.as_deref(),
            pr.closed_at.as_deref(),
        ),
        Some(pr_fields),
    );

    // MERGED_AS resolve-or-diagnose against the seeded code graph (#333). Only
    // an actually-merged PR carries a landed merge commit; an unmerged PR's
    // test-merge SHA is never merge evidence, so it emits neither edge nor
    // diagnostic regardless of the seeded graph (Codex P2).
    if let Some(sha) = pr
        .merge_commit_sha
        .as_deref()
        .filter(|s| !s.is_empty())
        .filter(|_| is_merged)
    {
        let task_id = task_id_for(ctx, "pr", number);
        if let Some(extra) = resolve_merge_commit(ctx, &task_id, number, sha) {
            emitted.records.push(extra.0);
            if extra.1 {
                emitted.link_diagnostics += 1;
            }
        }
    }

    // Requested-reviewer identities + REQUESTED_REVIEW_FROM edges + team
    // diagnostics (issue #335). Derived purely from the /pulls payload; no seed
    // graph needed. Identity nodes dedupe to one-per-login across the run.
    let task_id = task_id_for(ctx, "pr", number);
    emitted
        .records
        .extend(requested_review_records(ctx, &task_id, number, pr));
    emitted
}

/// A stable marker capturing a PR's `MERGED_AS` resolution outcome against the
/// current commit index, for the PR change-detection hash (issue #333, Codex
/// round-4).
///
/// The merge-link output depends on `ctx.commit_index`, but the PR idempotency
/// key did not, so an unchanged PR payload against a newly-seeded code graph was
/// wrongly suppressed by the `state.is_unchanged("pr:<n>", ...)` gate and the
/// `MERGED_AS` edge never appeared. Folding this marker into `pull_hash` fixes
/// that: the marker changes exactly when the emitted `MERGED_AS` edge/diagnostic
/// outcome changes, so a seed graph that newly resolves (or stops resolving) a
/// merge SHA re-emits the edge, while an unchanged seed keeps re-imports
/// idempotent (AC8).
///
/// Mirrors [`pull_records`]/[`resolve_merge_commit`] exactly: only an
/// actually-merged PR (`merged_at` present) with a non-empty `merge_commit_sha`
/// against a non-empty seed graph produces an edge/diagnostic. Every other case
/// (unmerged, no/empty SHA, or no seeded graph) is the stable `"none"` marker —
/// no edge, no diagnostic. Resolved links carry the target `Commit` record ID so
/// re-resolving the SAME SHA to a DIFFERENT commit also re-emits.
#[must_use]
pub fn merge_resolution_marker(commit_index: &CommitIndex, pr: &model::PullRequest) -> String {
    let Some(sha) = pr
        .merge_commit_sha
        .as_deref()
        .filter(|s| !s.is_empty())
        .filter(|_| pr.merged_at.is_some())
    else {
        return "none".to_owned();
    };
    if commit_index.is_empty() {
        return "none".to_owned();
    }
    match commit_index.get(sha).map(Vec::as_slice) {
        Some([commit_id]) => format!("resolved:{commit_id}"),
        Some(ids) if ids.len() > 1 => format!("ambiguous:{}", ids.len()),
        // None or empty slice → unresolved (no matching Commit in the seed).
        _ => "unresolved".to_owned(),
    }
}

/// The stable record ID of the `MERGED_AS` artifact a PR would emit (issue #333,
/// Codex round-6).
///
/// The `MERGED_AS` edge ID on a unique resolution against `commit_index`, or the
/// `github_commit_unresolved` Diagnostic ID on a zero- or multiple-match — or
/// `None` when the PR emits no merge artifact at all (unmerged, empty/absent
/// `merge_commit_sha`, or no seeded graph).
///
/// Mirrors [`pull_records`]/[`resolve_merge_commit`] exactly so the returned ID
/// is byte-identical to the artifact actually emitted. The importer persists this
/// per-PR (`state.pr_merge_artifacts`) and, when a re-import's outcome changes,
/// retracts the SUPERSEDED prior artifact via [`merge_artifact_tombstone`] before
/// emitting the current one — so a persistent store's current read view never
/// shows both stale and fresh merge evidence for one PR. The ID (not a lossy
/// marker) is persisted so a changed `merge_commit_sha` under a still-unresolved
/// outcome still retracts the prior diagnostic keyed on the OLD sha.
#[must_use]
pub fn merge_artifact_id(
    commit_index: &CommitIndex,
    source_repo: &str,
    pr: &model::PullRequest,
) -> Option<String> {
    let sha = pr
        .merge_commit_sha
        .as_deref()
        .filter(|s| !s.is_empty())
        .filter(|_| pr.merged_at.is_some())?;
    if commit_index.is_empty() {
        return None;
    }
    let number = pr.number;
    match commit_index.get(sha).map(Vec::as_slice) {
        Some([commit_id]) => {
            let task_id = project_stable_id(&[
                "project",
                "Task",
                source_repo,
                &number.to_string(),
                &format!("pr:{number}"),
            ]);
            Some(project_stable_id(&[
                "project",
                "edge",
                EdgeLabel::MergedAs.as_str(),
                &task_id,
                commit_id,
            ]))
        }
        // Zero or multiple matches → the repo-scoped diagnostic.
        _ => Some(commit_diagnostic_id(source_repo, number, sha)),
    }
}

/// Builds a project-domain [`GraphRecord::Tombstone`] retracting a superseded
/// merge-resolution artifact whose PR outcome changed (issue #333, Codex round-6).
///
/// The retracted artifact is a `MERGED_AS` edge or a `github_commit_unresolved`
/// Diagnostic emitted by an earlier import of the same PR.
///
/// The tombstone ID is derived from `(pr, deleted_id)`, so it is deterministic
/// and byte-identical across runs and distinct per retracted target (a
/// resolved-A→resolved-B→resolved-A cycle mints one tombstone per target).
/// `deleted_id` drives the embedded adapter's current-view suppression
/// (`read_all_records`), so a persistent store stops surfacing the stale record.
#[must_use]
pub fn merge_artifact_tombstone(number: u64, deleted_id: &str) -> GraphRecord {
    let native = format!("pr:{number}");
    let id = project_stable_id(&[
        "project",
        "Tombstone",
        IMPORTER_ID,
        &native,
        "merge_resolution_superseded",
        deleted_id,
    ]);
    GraphRecord::Tombstone {
        id,
        schema_version: PROJECT_SCHEMA_VERSION,
        deleted_id: deleted_id.to_owned(),
        summary: format!(
            "[merge_resolution_superseded] PR #{number} merge-resolution outcome changed; \
             retracting superseded artifact {deleted_id}"
        ),
        producer: None,
    }
}

/// Resolves a PR's `merge_commit_sha` to a `MERGED_AS` edge, or a diagnostic.
///
/// Returns `(record, is_diagnostic)`: exactly one matching `Commit` yields the
/// `MERGED_AS` Task→Commit edge; zero or multiple matches yield a
/// `github_commit_unresolved` project `Diagnostic`. An empty commit index (no
/// seeded code graph) resolves to `None` — no edge, no diagnostic.
fn resolve_merge_commit(
    ctx: &Context<'_>,
    task_id: &str,
    number: u64,
    sha: &str,
) -> Option<(GraphRecord, bool)> {
    if ctx.commit_index.is_empty() {
        return None;
    }
    let detail = match ctx.commit_index.get(sha).map(Vec::as_slice) {
        Some([commit_id]) => {
            // MERGED_AS is a project-domain Task→Commit relationship (#333, Codex
            // P2). It must carry a `project:v1:` ID + PROJECT_SCHEMA_VERSION —
            // NOT the codegraph identity `GraphRecord::edge` would stamp — so the
            // daemon project-edge validator sees it and `project:v1:` consumers
            // find the merge link. The Commit target stays a codegraph node.
            let edge = GraphRecord::project_edge(
                EdgeLabel::MergedAs,
                task_id.to_owned(),
                commit_id.clone(),
                None,
                format!("PR #{number} merged as commit {sha}"),
            );
            return Some((edge, false));
        }
        Some(ids) if ids.len() > 1 => {
            format!("{} code-graph Commit records claim this SHA", ids.len())
        }
        // None or empty slice → unresolved (no matching Commit in the seed).
        _ => "no code-graph Commit record matches this SHA in the seeded store".to_owned(),
    };
    Some((commit_diagnostic(ctx, number, sha, task_id, &detail), true))
}

/// The stable `github_commit_unresolved` Diagnostic record ID for a PR/SHA.
///
/// Repo-scoped (#333, Codex round-7): `source_repo` is part of the id, mirroring
/// the Task/Review/ExternalLink ids, so a shared multi-repo store never collides
/// diagnostics for the same PR number + merge SHA across repositories (one repo's
/// import could otherwise overwrite or tombstone another's merge evidence).
///
/// Seed-independent within a repo: the zero-match (unresolved) and multiple-match
/// (ambiguous) cases share ONE id per `(repo, PR, sha)`, so re-emitting either
/// case overwrites the same record and retracting it needs only
/// `(source_repo, number, sha)`. Factored out so the emitter
/// ([`commit_diagnostic`]) and the retraction path ([`merge_artifact_id`]) agree
/// byte-for-byte on the id.
fn commit_diagnostic_id(source_repo: &str, number: u64, sha: &str) -> String {
    let native = format!("pr:{number}");
    project_stable_id(&[
        "project",
        "Diagnostic",
        IMPORTER_ID,
        source_repo,
        &native,
        "github_commit_unresolved",
        sha,
    ])
}

/// Builds a project-domain `Diagnostic` node for an unresolved merge commit
/// (`github_commit_unresolved`), carrying the SHA and Task record ID (#333).
fn commit_diagnostic(
    ctx: &Context<'_>,
    number: u64,
    sha: &str,
    task_id: &str,
    detail: &str,
) -> GraphRecord {
    let code = "github_commit_unresolved";
    let id = commit_diagnostic_id(ctx.source_repo, number, sha);
    let mut rec = GraphRecord::node(
        id.clone(),
        NodeKind::Diagnostic,
        None,
        None,
        None,
        format!("[{code}] {detail}; merge_commit_sha='{sha}' task='{task_id}'"),
    );
    set_common(&mut rec, &id, ctx.transaction_time, ctx);
    rec
}

#[allow(clippy::too_many_arguments)]
fn task_and_link(
    ctx: &Context<'_>,
    kind: NodeKind,
    source_kind: &str,
    number: u64,
    native_id: &str,
    title: &str,
    url: &str,
    updated_at: &str,
    status: &str,
    labels: &[model::Label],
    assignees: &[model::User],
    author: Option<&str>,
    body_handle: Box<OutputHandle>,
    pr_fields: Option<PrTaskFields>,
) -> Emitted {
    let number_s = number.to_string();
    let task_id = project_stable_id(&["project", "Task", ctx.source_repo, &number_s, native_id]);
    let link_id = project_stable_id(&[
        "project",
        "ExternalLink",
        ctx.source_repo,
        &number_s,
        SYSTEM,
        native_id,
    ]);

    // ── Task node ──────────────────────────────────────────────────────────────
    // Redact the title once and use it for BOTH the `name` and `title` fields so
    // a secret in the title never survives in the human-readable `name`.
    let redacted_title = (ctx.redact)(title);
    let mut task = GraphRecord::node(
        task_id.clone(),
        kind,
        None,
        None,
        Some(redacted_title.clone()),
        format!("{source_kind} #{number}"),
    );
    set_common(&mut task, &task_id, updated_at, ctx);
    if let GraphRecord::Node {
        title: t,
        body_handle: bh,
        status: st,
        source_kind: sk,
        source_external_link_id: sel,
        assignees: asg,
        labels: lbl,
        priority,
        author: au,
        head_sha: hs,
        head_ref: hr,
        base_ref: br,
        merge_commit_sha: mcs,
        merged_at: ma,
        draft: dr,
        ..
    } = &mut task
    {
        *t = Some(redacted_title);
        *bh = Some(body_handle);
        *st = Some(status.to_owned());
        *sk = Some(source_kind.to_owned());
        *sel = Some(link_id.clone());
        *asg = Some(assignees.iter().map(|u| (ctx.redact)(&u.login)).collect());
        *lbl = Some(labels.iter().map(|l| (ctx.redact)(&l.name)).collect());
        *priority = Some("unknown".to_owned());
        *au = author.map(str::to_owned);
        // PR-promoted plaintext fields (#333); absent on issue Tasks. Never
        // routed through redaction (see `docs/schema/import-github.md` §8).
        if let Some(pf) = pr_fields {
            *hs = pf.head_sha;
            *hr = pf.head_ref;
            *br = pf.base_ref;
            *mcs = pf.merge_commit_sha;
            *ma = pf.merged_at;
            *dr = pf.draft;
        }
    }

    // ── ExternalLink node ────────────────────────────────────────────────────────
    let mut link = GraphRecord::node(
        link_id.clone(),
        NodeKind::ExternalLink,
        None,
        None,
        None,
        format!("github {native_id}"),
    );
    set_common(&mut link, &link_id, updated_at, ctx);
    if let GraphRecord::Node {
        system,
        url: u,
        system_native_id,
        discovered_at,
        repository_remote,
        ..
    } = &mut link
    {
        *system = Some(SYSTEM.to_owned());
        *u = Some((ctx.redact)(url));
        *system_native_id = Some(native_id.to_owned());
        *discovered_at = Some(ctx.transaction_time.to_owned());
        *repository_remote = Some(format!("https://github.com/{}", ctx.source_repo));
    }

    let edge = GraphRecord::edge(
        EdgeLabel::ExternalHandle,
        task_id,
        link_id,
        None,
        format!("Task #{number} external handle"),
    );

    Emitted {
        records: vec![task, link, edge],
        link_diagnostics: 0,
    }
}

// ── Comments / reviews → Review ─────────────────────────────────────────────────

/// Emits a `Review` (`issue_comment`) plus its `REFERENCES_TASK` edge.
///
/// `/issues/comments` returns comments on both issues and pull-request
/// conversations. PR conversation comments are emitted only as a `Task` under
/// `pr:<n>` (the matching `/issues` item is discarded), so the parent edge must
/// point at the PR `Task`. GitHub distinguishes the two via the comment's
/// `html_url` path segment (`/pull/<n>` vs `/issues/<n>`).
#[must_use]
pub fn issue_comment_records(ctx: &Context<'_>, c: &model::IssueComment) -> Emitted {
    let Some(number) = model::trailing_number(&c.issue_url) else {
        return Emitted::default();
    };
    let parent_kind = if c.html_url.contains("/pull/") {
        "pr"
    } else {
        "issue"
    };
    let native = format!("issue_comment:{number}:{}", c.id);
    let parent = task_id_for(ctx, parent_kind, number);
    let body = c.body.as_deref().map(|b| redact_lines(ctx.redact, b));
    let mut rec = review_node(
        ctx,
        &native,
        number,
        "issue_comment",
        &c.updated_at,
        c.user.as_ref().map(|u| u.login.as_str()),
        body,
        &parent,
    );
    // issue_comment reviews are general PR-conversation comments, never anchored
    // to a specific commit (issue #334 exemption): review_commit_sha stays None
    // and no REVIEWS_COMMIT edge or unanchored diagnostic is ever emitted.
    set_review_extra(&mut rec, None, None, None, None, None, None, None, None);
    let review_id = review_id_for(ctx, &native);
    let mut emitted = edge_and_pack(rec, parent, None);
    // REVIEWED_BY reviewer identity (issue #335): emitted for every review kind,
    // including exempt issue_comment reviews.
    emitted.records.extend(reviewed_by_records(
        ctx,
        &review_id,
        c.user.as_ref().map(|u| u.login.as_str()),
    ));
    emitted
}

/// Emits a `Review` (`pr_review`) plus its `REFERENCES_TASK` edge.
#[must_use]
pub fn pr_review_records(ctx: &Context<'_>, pr_number: u64, r: &model::Review) -> Emitted {
    let native = format!("pr_review:{pr_number}:{}", r.id);
    let parent = task_id_for(ctx, "pr", pr_number);
    let submitted = if r.submitted_at.as_deref().unwrap_or("").is_empty() {
        ctx.transaction_time
    } else {
        r.submitted_at.as_deref().unwrap_or(ctx.transaction_time)
    };
    let body = r.body.as_deref().map(|b| redact_lines(ctx.redact, b));
    let mut rec = review_node(
        ctx,
        &native,
        pr_number,
        "pr_review",
        submitted,
        r.user.as_ref().map(|u| u.login.as_str()),
        body,
        &parent,
    );
    set_review_extra(
        &mut rec,
        Some(&r.state.to_ascii_lowercase()),
        None,
        None,
        None,
        None,
        None,
        None,
        r.commit_id.as_deref(),
    );
    // REVIEWS_COMMIT resolve-or-diagnose against the seeded code graph (#334,
    // the review-side mirror of pull_records' MERGED_AS block). A pr_review is a
    // commit-anchored review kind: a genuinely-absent commit_id is diagnosed
    // (`github_review_unanchored`), never fabricated.
    let review_id = review_id_for(ctx, &native);
    let mut emitted = edge_and_pack(rec, parent, None);
    if let Some((artifact, is_diag)) =
        resolve_review_commit(ctx, &review_id, r.commit_id.as_deref())
    {
        emitted.records.push(artifact);
        if is_diag {
            emitted.link_diagnostics += 1;
        }
    }
    // REVIEWED_BY reviewer identity (issue #335).
    emitted.records.extend(reviewed_by_records(
        ctx,
        &review_id,
        r.user.as_ref().map(|u| u.login.as_str()),
    ));
    emitted
}

/// Emits a `pr_review_comment` `Review` plus its edges.
///
/// Always emits a `REFERENCES_TASK` edge and, when the anchored file resolves
/// unambiguously, a `TOUCHES_FILE` edge. Missing/renamed/ambiguous files
/// produce a diagnostic with source handles instead of a guessed link.
#[must_use]
pub fn review_comment_records(ctx: &Context<'_>, c: &model::ReviewComment) -> Emitted {
    let Some(number) = model::trailing_number(&c.pull_request_url) else {
        return Emitted::default();
    };
    let native = format!("pr_review_comment:{number}:{}", c.id);
    let parent = task_id_for(ctx, "pr", number);
    let body = c.body.as_deref().map(|b| redact_lines(ctx.redact, b));
    let mut rec = review_node(
        ctx,
        &native,
        number,
        "pr_review_comment",
        &c.updated_at,
        c.user.as_ref().map(|u| u.login.as_str()),
        body,
        &parent,
    );
    let in_reply = c
        .in_reply_to_id
        .map(|id| format!("pr_review_comment:{number}:{id}"))
        .map(|n| review_id_for(ctx, &n));
    let diff = c.diff_hunk.as_deref().map(|d| redact_lines(ctx.redact, d));
    set_review_extra(
        &mut rec,
        None,
        in_reply.as_deref(),
        c.path.as_deref(),
        c.line,
        c.start_line,
        diff.as_deref(),
        c.side.as_deref(),
        c.commit_id.as_deref(),
    );

    // File resolution for TOUCHES_FILE (AC7).
    let review_id = review_id_for(ctx, &native);
    let (touches, diag) = resolve_file_link(ctx, &review_id, c);
    let mut emitted = edge_and_pack(rec, parent, touches);
    if let Some(d) = diag {
        emitted.records.push(d);
        emitted.link_diagnostics += 1;
    }
    // REVIEWS_COMMIT resolve-or-diagnose against the seeded code graph (#334).
    // A pr_review_comment is commit-anchored: an absent commit_id is diagnosed
    // (`github_review_unanchored`), never fabricated.
    if let Some((artifact, is_diag)) =
        resolve_review_commit(ctx, &review_id, c.commit_id.as_deref())
    {
        emitted.records.push(artifact);
        if is_diag {
            emitted.link_diagnostics += 1;
        }
    }
    // REVIEWED_BY reviewer identity (issue #335).
    emitted.records.extend(reviewed_by_records(
        ctx,
        &review_id,
        c.user.as_ref().map(|u| u.login.as_str()),
    ));
    emitted
}

/// Resolves a review comment's file to a `TOUCHES_FILE` edge, or a diagnostic.
fn resolve_file_link(
    ctx: &Context<'_>,
    review_id: &str,
    c: &model::ReviewComment,
) -> (Option<GraphRecord>, Option<GraphRecord>) {
    let Some(path) = c.path.as_deref() else {
        return (None, None);
    };
    match ctx.file_index.get(path).map(Vec::as_slice) {
        Some([file_id]) => {
            let edge = GraphRecord::edge(
                EdgeLabel::TouchesFile,
                review_id.to_owned(),
                file_id.clone(),
                None,
                format!("review comment touches {path}"),
            );
            (Some(edge), None)
        }
        Some(ids) if ids.len() > 1 => (
            None,
            Some(link_diagnostic(
                ctx,
                "github_file_ambiguous",
                path,
                &c.html_url,
                review_id,
                &format!("{} code-graph File records claim this path", ids.len()),
            )),
        ),
        // None or empty slice → unresolved (missing or renamed).
        _ => (
            None,
            Some(link_diagnostic(
                ctx,
                "github_file_unresolved",
                path,
                &c.html_url,
                review_id,
                "no code-graph File record matches this path in the seeded store",
            )),
        ),
    }
}

/// Builds a project-domain `Diagnostic` node carrying source handles (AC7).
fn link_diagnostic(
    ctx: &Context<'_>,
    code: &str,
    path: &str,
    comment_url: &str,
    review_id: &str,
    detail: &str,
) -> GraphRecord {
    let id = project_stable_id(&["project", "Diagnostic", IMPORTER_ID, review_id, code, path]);
    let mut rec = GraphRecord::node(
        id.clone(),
        NodeKind::Diagnostic,
        Some(path.to_owned()),
        None,
        None,
        format!("[{code}] {detail}; path='{path}' comment='{comment_url}' review='{review_id}'"),
    );
    set_common(&mut rec, &id, ctx.transaction_time, ctx);
    rec
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn review_node(
    ctx: &Context<'_>,
    native_id: &str,
    number: u64,
    review_kind: &str,
    valid_time: &str,
    author: Option<&str>,
    body: Option<String>,
    parent_task_id: &str,
) -> GraphRecord {
    let id = review_id_for(ctx, native_id);
    let mut rec = GraphRecord::node(
        id.clone(),
        NodeKind::Review,
        None,
        None,
        None,
        format!("{review_kind} on #{number}"),
    );
    set_common(&mut rec, &id, valid_time, ctx);
    if let GraphRecord::Node {
        review_kind: rk,
        author: au,
        body_handle: bh,
        system_native_id,
        parent_task_id: pt,
        source_kind: sk,
        ..
    } = &mut rec
    {
        *rk = Some(review_kind.to_owned());
        *au = author.map(str::to_owned);
        *bh = body.as_deref().map(handle_for);
        *system_native_id = Some(native_id.to_owned());
        *pt = Some(parent_task_id.to_owned());
        // Stamp the review source kind so the daemon `REVIEWS_COMMIT` validator
        // can enforce a Review origin (issue #334, mirroring the `MERGED_AS`
        // `github_pr` source-kind gate).
        *sk = Some(SOURCE_KIND_REVIEW.to_owned());
    }
    rec
}

#[allow(clippy::too_many_arguments)]
fn set_review_extra(
    rec: &mut GraphRecord,
    review_state: Option<&str>,
    in_reply_to_id: Option<&str>,
    path: Option<&str>,
    line: Option<u32>,
    start_line: Option<u32>,
    diff_hunk: Option<&str>,
    side: Option<&str>,
    review_commit_sha: Option<&str>,
) {
    if let GraphRecord::Node {
        review_state: rs,
        in_reply_to_id: irt,
        repo_relative_path,
        span,
        diff_hunk_handle,
        review_side,
        review_commit_sha: rcs,
        ..
    } = rec
    {
        *rs = review_state.map(str::to_owned);
        *irt = in_reply_to_id.map(str::to_owned);
        *repo_relative_path = path.map(str::to_owned);
        if let Some(l) = line {
            // GitHub provides 1-based line numbers, not byte offsets; record the
            // line range and leave byte offsets at 0 (not available over REST).
            *span = Some(crate::ir::SourceSpan {
                start_line: start_line.unwrap_or(l) as usize,
                end_line: l as usize,
                start_byte: 0,
                end_byte: 0,
            });
        }
        *diff_hunk_handle = diff_hunk.map(handle_for);
        *review_side = side.map(str::to_owned);
        // Anchor SHA (issue #334): the commit_id the reviewer looked at. Set
        // whenever the payload carries it, with no `merged_at`-style gate — a
        // review commit is a real observed commit, not a throwaway test-merge.
        // Plaintext query substrate (§8 carve-out); never routed through
        // redaction. `issue_comment` reviews pass `None` (commit-anchor exempt).
        *rcs = review_commit_sha.map(str::to_owned);
    }
}

/// Wraps a review record with its `REFERENCES_TASK` edge (and optional
/// `TOUCHES_FILE` edge) into an [`Emitted`].
fn edge_and_pack(
    review: GraphRecord,
    parent_task_id: String,
    touches_file: Option<GraphRecord>,
) -> Emitted {
    let review_id = review.id().to_owned();
    let ref_edge = GraphRecord::edge(
        EdgeLabel::ReferencesTask,
        review_id,
        parent_task_id,
        None,
        "review references task".to_owned(),
    );
    let mut records = vec![review, ref_edge];
    records.extend(touches_file);
    Emitted {
        records,
        link_diagnostics: 0,
    }
}

fn task_id_for(ctx: &Context<'_>, kind: &str, number: u64) -> String {
    let number_s = number.to_string();
    let native = format!("{kind}:{number}");
    project_stable_id(&["project", "Task", ctx.source_repo, &number_s, &native])
}

fn review_id_for(ctx: &Context<'_>, native_id: &str) -> String {
    review_record_id(ctx.source_repo, native_id)
}

/// The stable `project.Review` record ID for `(source_repo, native_id)`.
///
/// Factored out of [`review_id_for`] so the importer's state-side artifact-ID
/// path ([`review_artifact_id`]) reconstructs the exact same review handle the
/// emitters use, without threading a full [`Context`] (issue #334).
fn review_record_id(source_repo: &str, native_id: &str) -> String {
    // native_id is "<review_kind>:<n>:<id>"; the number is the second segment.
    let number = native_id.split(':').nth(1).unwrap_or("0");
    project_stable_id(&["project", "Review", source_repo, number, native_id])
}

/// Whether a review kind is commit-anchored (issue #334).
///
/// `pr_review` and `pr_review_comment` are anchored to the exact commit the
/// reviewer looked at, so they populate `review_commit_sha` and resolve a
/// `REVIEWS_COMMIT` edge (or a diagnostic). `issue_comment` reviews are general
/// PR-conversation comments with no commit anchor and are exempt.
#[must_use]
fn review_kind_is_commit_anchored(review_kind: &str) -> bool {
    matches!(review_kind, "pr_review" | "pr_review_comment")
}

/// The stable `REVIEWS_COMMIT` project-edge ID for `(review, commit)` (#334).
///
/// Must equal what [`GraphRecord::project_edge`] mints so the emitter and the
/// state-side artifact-ID path agree byte-for-byte.
fn reviews_commit_edge_id(review_id: &str, commit_record_id: &str) -> String {
    project_stable_id(&[
        "project",
        "edge",
        EdgeLabel::ReviewsCommit.as_str(),
        review_id,
        commit_record_id,
    ])
}

/// The stable review-anchor `Diagnostic` record ID (issue #334).
///
/// Keyed on the already-repo-scoped `review_id` (which embeds `source_repo`),
/// so diagnostics never collide across repositories in a shared multi-repo store
/// (contract #7, the review-side analog of [`commit_diagnostic_id`]). `sha` is
/// empty for the `github_review_unanchored` case (no `commit_id` to key on).
fn review_diagnostic_id(review_id: &str, code: &str, sha: &str) -> String {
    project_stable_id(&["project", "Diagnostic", IMPORTER_ID, review_id, code, sha])
}

/// Resolves a commit-anchored review's anchor to an edge or a diagnostic.
///
/// Issue #334, the review-side mirror of [`resolve_merge_commit`].
/// Returns `(record, is_diagnostic)`, or `None` when no seeded code graph is
/// present (no edge, no diagnostic — the `review_commit_sha` field is still
/// populated by the caller from the raw payload). Three seeded outcomes:
///
/// 1. `commit_id` present + exactly one matching `Commit` → the `REVIEWS_COMMIT`
///    Review→Commit project edge (`project:v1:` identity).
/// 2. `commit_id` present + zero/multiple matches → a `github_commit_unresolved`
///    project `Diagnostic`.
/// 3. `commit_id` genuinely absent → a `github_review_unanchored` project
///    `Diagnostic` (diagnose the gap; never fabricate a SHA).
///
/// `issue_comment` reviews never reach this resolver (they are exempt).
fn resolve_review_commit(
    ctx: &Context<'_>,
    review_id: &str,
    commit_id: Option<&str>,
) -> Option<(GraphRecord, bool)> {
    if ctx.commit_index.is_empty() {
        return None;
    }
    let Some(sha) = commit_id.filter(|s| !s.is_empty()) else {
        return Some((
            review_diagnostic(
                ctx,
                review_id,
                "github_review_unanchored",
                "",
                "review carries no commit_id; cannot anchor it to a commit",
            ),
            true,
        ));
    };
    let detail = match ctx.commit_index.get(sha).map(Vec::as_slice) {
        Some([commit_record_id]) => {
            // REVIEWS_COMMIT is a project-domain Review→Commit relationship
            // (#334). It must carry a `project:v1:` ID + PROJECT_SCHEMA_VERSION
            // so the daemon project-edge validator sees it and `project:v1:`
            // consumers find the anchor. The Commit target stays a codegraph node.
            let edge = GraphRecord::project_edge(
                EdgeLabel::ReviewsCommit,
                review_id.to_owned(),
                commit_record_id.clone(),
                None,
                format!("review {review_id} anchored to commit {sha}"),
            );
            return Some((edge, false));
        }
        Some(ids) if ids.len() > 1 => {
            format!("{} code-graph Commit records claim this SHA", ids.len())
        }
        // None or empty slice → unresolved (no matching Commit in the seed).
        _ => "no code-graph Commit record matches this SHA in the seeded store".to_owned(),
    };
    Some((
        review_diagnostic(ctx, review_id, "github_commit_unresolved", sha, &detail),
        true,
    ))
}

/// Builds a project-domain review-anchor `Diagnostic` node (issue #334).
fn review_diagnostic(
    ctx: &Context<'_>,
    review_id: &str,
    code: &str,
    sha: &str,
    detail: &str,
) -> GraphRecord {
    let id = review_diagnostic_id(review_id, code, sha);
    let summary = if sha.is_empty() {
        format!("[{code}] {detail}; review='{review_id}'")
    } else {
        format!("[{code}] {detail}; review_commit_sha='{sha}' review='{review_id}'")
    };
    let mut rec = GraphRecord::node(id.clone(), NodeKind::Diagnostic, None, None, None, summary);
    set_common(&mut rec, &id, ctx.transaction_time, ctx);
    rec
}

/// A stable marker of a review's `REVIEWS_COMMIT` resolution outcome.
///
/// For the per-review change hash (issue #334, the review-side mirror of
/// [`merge_resolution_marker`]).
/// Folded into the review change hash so a seed graph that newly resolves (or
/// stops resolving) a review's `commit_id` re-emits the anchor edge/diagnostic
/// even when the review payload is byte-identical, while an unchanged seed keeps
/// re-imports idempotent. `issue_comment` reviews are commit-anchor-exempt and
/// no seed graph both map to the stable `"none"` marker (no artifact). A resolved
/// link carries the target `Commit` record ID so re-resolving the SAME sha to a
/// DIFFERENT commit also re-emits.
#[must_use]
pub fn review_commit_marker(
    commit_index: &CommitIndex,
    review_kind: &str,
    commit_id: Option<&str>,
) -> String {
    if !review_kind_is_commit_anchored(review_kind) || commit_index.is_empty() {
        return "none".to_owned();
    }
    commit_id.filter(|s| !s.is_empty()).map_or_else(
        || "unanchored".to_owned(),
        |sha| match commit_index.get(sha).map(Vec::as_slice) {
            Some([commit_record_id]) => format!("resolved:{commit_record_id}"),
            Some(ids) if ids.len() > 1 => format!("ambiguous:{}", ids.len()),
            // None or empty slice → unresolved (no matching Commit in the seed).
            _ => "unresolved".to_owned(),
        },
    )
}

/// The stable record ID of the review-anchor artifact a review would emit.
///
/// Issue #334, the review-side mirror of [`merge_artifact_id`].
/// The `REVIEWS_COMMIT` edge ID on a unique resolution, the
/// `github_commit_unresolved` Diagnostic ID on a zero/multiple match, the
/// `github_review_unanchored` Diagnostic ID on an absent `commit_id` — or `None`
/// when the review emits no anchor artifact at all (exempt `issue_comment`, or
/// no seeded graph). The importer persists this per-review and, on an outcome
/// change, retracts the SUPERSEDED prior artifact via [`review_artifact_tombstone`]
/// before emitting the current one, so a persistent store's current read view
/// never shows both stale and fresh review-anchor evidence for one review.
#[must_use]
pub fn review_artifact_id(
    commit_index: &CommitIndex,
    source_repo: &str,
    native_id: &str,
    review_kind: &str,
    commit_id: Option<&str>,
) -> Option<String> {
    if !review_kind_is_commit_anchored(review_kind) || commit_index.is_empty() {
        return None;
    }
    let review_id = review_record_id(source_repo, native_id);
    let Some(sha) = commit_id.filter(|s| !s.is_empty()) else {
        return Some(review_diagnostic_id(
            &review_id,
            "github_review_unanchored",
            "",
        ));
    };
    match commit_index.get(sha).map(Vec::as_slice) {
        Some([commit_record_id]) => Some(reviews_commit_edge_id(&review_id, commit_record_id)),
        // Zero or multiple matches → the repo-scoped unresolved diagnostic.
        _ => Some(review_diagnostic_id(
            &review_id,
            "github_commit_unresolved",
            sha,
        )),
    }
}

/// Builds a `Tombstone` retracting a superseded review-anchor artifact.
///
/// Issue #334, the review-side mirror of [`merge_artifact_tombstone`]; retracts a
/// prior artifact whose outcome changed on re-import.
/// The tombstone ID is derived from `(native_id, deleted_id)`, so it is
/// deterministic and distinct per retracted target. `deleted_id` drives the
/// embedded adapter's current-view suppression so a persistent store stops
/// surfacing the stale record.
#[must_use]
pub fn review_artifact_tombstone(native_id: &str, deleted_id: &str) -> GraphRecord {
    let id = project_stable_id(&[
        "project",
        "Tombstone",
        IMPORTER_ID,
        native_id,
        "review_anchor_superseded",
        deleted_id,
    ]);
    GraphRecord::Tombstone {
        id,
        schema_version: PROJECT_SCHEMA_VERSION,
        deleted_id: deleted_id.to_owned(),
        summary: format!(
            "[review_anchor_superseded] review {native_id} commit-anchor outcome changed; \
             retracting superseded artifact {deleted_id}"
        ),
        producer: None,
    }
}

// ── Reviewer identity (issue #335) ───────────────────────────────────────────────

/// The stable `project.ExternalIdentity` record ID for a `(system, login)` pair.
///
/// Deliberately NOT repo-scoped (issue #335): a participant identity is global
/// across repositories, so the SAME login observed in two repositories maps to
/// exactly ONE identity node. Keyed on `["project", "ExternalIdentity", system,
/// login]` alone. Factored out so the emitter and any state-side path agree
/// byte-for-byte on the id.
#[must_use]
pub fn external_identity_id(system: &str, login: &str) -> String {
    project_stable_id(&["project", "ExternalIdentity", system, login])
}

/// Builds the `project.ExternalIdentity` node for a GitHub `login` (issue #335).
///
/// Carries ONLY the login (in the `author` field, the §8 plaintext-login
/// carve-out) and the system (`identity_system`) — never email, display name,
/// avatar, or profile URL. Valid time is the run's transaction time: an
/// identity is a timeless first-seen fact with no natural GitHub timestamp.
fn external_identity_node(ctx: &Context<'_>, login: &str) -> GraphRecord {
    let id = external_identity_id(SYSTEM, login);
    let mut rec = GraphRecord::node(
        id.clone(),
        NodeKind::ExternalIdentity,
        None,
        None,
        None,
        format!("{SYSTEM} identity {login}"),
    );
    set_common(&mut rec, &id, ctx.transaction_time, ctx);
    if let GraphRecord::Node {
        author,
        identity_system,
        ..
    } = &mut rec
    {
        *author = Some(login.to_owned());
        *identity_system = Some(SYSTEM.to_owned());
    }
    rec
}

/// Records the reviewer identity for a `Review`: the `ExternalIdentity` node
/// plus the `REVIEWED_BY` project edge (issue #335).
///
/// Returns an empty vec when the review payload carried no author login (rare):
/// an identity is never fabricated from a missing login. The identity node is
/// deduplicated to one-per-login across the run by the importer's seen-set, and
/// carries a `project:v1:` id so the daemon project-edge validator honours the
/// edge. Emitted for every review kind (`issue_comment`, `pr_review`,
/// `pr_review_comment`).
fn reviewed_by_records(
    ctx: &Context<'_>,
    review_id: &str,
    author: Option<&str>,
) -> Vec<GraphRecord> {
    let Some(login) = author.filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let identity = external_identity_node(ctx, login);
    let identity_id = identity.id().to_owned();
    let edge = GraphRecord::project_edge(
        EdgeLabel::ReviewedBy,
        review_id.to_owned(),
        identity_id,
        None,
        format!("review {review_id} authored by {SYSTEM}:{login}"),
    );
    vec![identity, edge]
}

/// The stable `github_team_review_request_unexpanded` Diagnostic id (issue #335).
///
/// Repo-scoped (mirrors [`commit_diagnostic_id`]) so a shared multi-repo store
/// never collides team diagnostics for the same PR number + slug across
/// repositories.
fn team_review_diagnostic_id(source_repo: &str, number: u64, slug: &str) -> String {
    let native = format!("pr:{number}");
    project_stable_id(&[
        "project",
        "Diagnostic",
        IMPORTER_ID,
        source_repo,
        &native,
        "github_team_review_request_unexpanded",
        slug,
    ])
}

/// Builds a project `Diagnostic` recording an unexpanded team review request
/// (issue #335). A requested TEAM is recorded, never silently dropped and never
/// expanded to member logins; the diagnostic carries the team slug and the PR
/// `Task` record id.
fn team_review_diagnostic(
    ctx: &Context<'_>,
    task_id: &str,
    number: u64,
    slug: &str,
) -> GraphRecord {
    let code = "github_team_review_request_unexpanded";
    let id = team_review_diagnostic_id(ctx.source_repo, number, slug);
    let mut rec = GraphRecord::node(
        id.clone(),
        NodeKind::Diagnostic,
        None,
        None,
        None,
        format!(
            "[{code}] PR #{number} requested review from team '{slug}'; teams are recorded but \
             never expanded to member logins; task='{task_id}'"
        ),
    );
    set_common(&mut rec, &id, ctx.transaction_time, ctx);
    rec
}

/// Records requested-reviewer identities for a PR `Task` (issue #335).
///
/// One `ExternalIdentity` node + `REQUESTED_REVIEW_FROM` edge (Task → identity)
/// per requested-reviewer login, plus one
/// `github_team_review_request_unexpanded` `Diagnostic` per requested TEAM.
/// Identity nodes are deduplicated across the run by the importer's seen-set.
fn requested_review_records(
    ctx: &Context<'_>,
    task_id: &str,
    number: u64,
    pr: &model::PullRequest,
) -> Vec<GraphRecord> {
    let mut out = Vec::new();
    for user in &pr.requested_reviewers {
        if user.login.is_empty() {
            continue;
        }
        let identity = external_identity_node(ctx, &user.login);
        let identity_id = identity.id().to_owned();
        out.push(identity);
        out.push(GraphRecord::project_edge(
            EdgeLabel::RequestedReviewFrom,
            task_id.to_owned(),
            identity_id,
            None,
            format!("PR #{number} requested review from {SYSTEM}:{}", user.login),
        ));
    }
    for team in &pr.requested_teams {
        if team.slug.is_empty() {
            continue;
        }
        out.push(team_review_diagnostic(ctx, task_id, number, &team.slug));
    }
    out
}

/// The stable `project.Task` record id for a PR `number` (issue #335).
///
/// Factored out of the emitter's [`task_id_for`] so the requested-reviewer
/// supersession path in [`crate::github::import`] reconstructs byte-for-byte the
/// same `Task` handle a `REQUESTED_REVIEW_FROM` edge points from.
#[must_use]
pub fn pr_task_id(source_repo: &str, number: u64) -> String {
    let number_s = number.to_string();
    let native = format!("pr:{number}");
    project_stable_id(&["project", "Task", source_repo, &number_s, &native])
}

/// The stable `REQUESTED_REVIEW_FROM` edge id for a `(task, identity)` pair.
///
/// Mirrors [`GraphRecord::project_edge`]'s id recipe exactly, so the id computed
/// here for the prior/current request-set diff equals the id the emitter mints
/// (issue #335, Codex P1). A unit test locks the two together.
fn request_review_edge_id(task_id: &str, identity_id: &str) -> String {
    project_stable_id(&[
        "project",
        "edge",
        EdgeLabel::RequestedReviewFrom.as_str(),
        task_id,
        identity_id,
    ])
}

/// The current set of `REQUESTED_REVIEW_FROM` edge record ids a PR would emit
/// (issue #335, Codex P1) — one per non-empty requested-reviewer login.
///
/// Returned sorted and deduplicated so it is a stable set for the prior/current
/// comparison in [`crate::github::import`]. The importer persists this as the
/// PR's prior request set; any prior edge id NOT in the current set is a reviewer
/// removed since the last run and is retracted via
/// [`request_review_edge_tombstone`].
#[must_use]
pub fn requested_review_edge_ids(source_repo: &str, pr: &model::PullRequest) -> Vec<String> {
    let task_id = pr_task_id(source_repo, pr.number);
    let mut ids = std::collections::BTreeSet::new();
    for user in &pr.requested_reviewers {
        if user.login.is_empty() {
            continue;
        }
        let identity_id = external_identity_id(SYSTEM, &user.login);
        ids.insert(request_review_edge_id(&task_id, &identity_id));
    }
    ids.into_iter().collect()
}

/// Builds a `Tombstone` retracting a superseded `REQUESTED_REVIEW_FROM` edge.
///
/// Issue #335, Codex P1; the requested-reviewer analog of
/// [`merge_artifact_tombstone`]. Retracts the edge for a reviewer removed from a
/// PR's requested set since the last run.
///
/// The tombstone ID is derived from `(pr, deleted_id)`, so it is deterministic,
/// byte-identical across runs, and distinct per retracted edge (a
/// requested → removed → re-requested cycle mints one tombstone per removal).
/// `deleted_id` (the edge id) drives the embedded adapter's current-view
/// suppression so a persistent store stops surfacing the removed reviewer as
/// "requested"; re-requesting the reviewer re-emits the same edge id, which the
/// adapter's `write_edge` revive-after-tombstone then supersedes. Only the edge
/// is retracted — never the global `ExternalIdentity` node.
#[must_use]
pub fn request_review_edge_tombstone(number: u64, deleted_id: &str) -> GraphRecord {
    let native = format!("pr:{number}");
    let id = project_stable_id(&[
        "project",
        "Tombstone",
        IMPORTER_ID,
        &native,
        "requested_review_superseded",
        deleted_id,
    ]);
    GraphRecord::Tombstone {
        id,
        schema_version: PROJECT_SCHEMA_VERSION,
        deleted_id: deleted_id.to_owned(),
        summary: format!(
            "[requested_review_superseded] PR #{number} requested-reviewer set changed; \
             retracting superseded REQUESTED_REVIEW_FROM edge {deleted_id}"
        ),
        producer: None,
    }
}

/// Convenience that folds an iterator of [`Emitted`] into one.
pub fn merge(parts: impl IntoIterator<Item = Emitted>) -> Emitted {
    let mut acc = Emitted::default();
    for p in parts {
        acc.extend(p);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    static EMPTY_COMMIT_INDEX: std::sync::OnceLock<CommitIndex> = std::sync::OnceLock::new();

    fn ctx<'a>(repo: &'a str, idx: &'a FileIndex, redact: &'a Redact<'a>) -> Context<'a> {
        Context {
            source_repo: repo,
            transaction_time: "2026-01-01T00:00:00Z",
            redact,
            file_index: idx,
            commit_index: EMPTY_COMMIT_INDEX.get_or_init(CommitIndex::new),
        }
    }

    fn identity(s: &str) -> String {
        s.to_owned()
    }

    fn sample_issue(n: u64) -> model::Issue {
        model::Issue {
            number: n,
            title: format!("Issue {n}"),
            body: Some("body text".to_owned()),
            state: "open".to_owned(),
            state_reason: None,
            labels: vec![model::Label {
                name: "bug".to_owned(),
                color: "f00".to_owned(),
                description: None,
            }],
            assignees: vec![model::User {
                login: "alice".to_owned(),
            }],
            user: Some(model::User {
                login: "bob".to_owned(),
            }),
            milestone: None,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            closed_at: None,
            html_url: format!("https://github.com/o/r/issues/{n}"),
            pull_request: None,
        }
    }

    #[test]
    fn issue_emits_task_link_and_edge() {
        let idx = FileIndex::new();
        let c = ctx("o/r", &idx, &identity);
        let e = issue_records(&c, &sample_issue(1));
        assert_eq!(e.records.len(), 3);
        let kinds: Vec<_> = e
            .records
            .iter()
            .map(|r| match r {
                GraphRecord::Node { kind, .. } => kind.as_str(),
                GraphRecord::Edge { label, .. } => label.as_str(),
                GraphRecord::Tombstone { .. } => "Tombstone",
            })
            .collect();
        assert!(kinds.contains(&"Task"));
        assert!(kinds.contains(&"ExternalLink"));
        assert!(kinds.contains(&"EXTERNAL_HANDLE"));
    }

    #[test]
    fn ids_are_stable_across_runs() {
        let idx = FileIndex::new();
        let c = ctx("o/r", &idx, &identity);
        let a = issue_records(&c, &sample_issue(1));
        let b = issue_records(&c, &sample_issue(1));
        let ids_a: Vec<_> = a.records.iter().map(|r| r.id().to_owned()).collect();
        let ids_b: Vec<_> = b.records.iter().map(|r| r.id().to_owned()).collect();
        assert_eq!(ids_a, ids_b);
    }

    #[test]
    fn issue_and_pr_links_differ_for_same_number() {
        let idx = FileIndex::new();
        let c = ctx("o/r", &idx, &identity);
        let issue = issue_records(&c, &sample_issue(5));
        let pr = pull_records(
            &c,
            &model::PullRequest {
                number: 5,
                title: "PR 5".to_owned(),
                body: None,
                state: "open".to_owned(),
                merged_at: None,
                draft: false,
                labels: vec![],
                assignees: vec![],
                user: None,
                milestone: None,
                created_at: String::new(),
                updated_at: "2026-01-02T00:00:00Z".to_owned(),
                closed_at: None,
                head: None,
                base: None,
                merge_commit_sha: None,
                requested_reviewers: vec![],
                requested_teams: vec![],
                html_url: "https://github.com/o/r/pull/5".to_owned(),
            },
        );
        let issue_task = issue.records[0].id();
        let pr_task = pr.records[0].id();
        assert_ne!(issue_task, pr_task);
    }

    fn merged_pr(sha: Option<&str>, merged: bool) -> model::PullRequest {
        model::PullRequest {
            number: 30,
            title: "PR 30".to_owned(),
            body: None,
            state: "closed".to_owned(),
            merged_at: merged.then(|| "2026-01-02T00:00:00Z".to_owned()),
            draft: false,
            labels: vec![],
            assignees: vec![],
            user: None,
            milestone: None,
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            closed_at: None,
            head: None,
            base: None,
            merge_commit_sha: sha.map(str::to_owned),
            requested_reviewers: vec![],
            requested_teams: vec![],
            html_url: "https://github.com/o/r/pull/30".to_owned(),
        }
    }

    #[test]
    fn merge_resolution_marker_reflects_seed_graph_outcome() {
        let sha = "merge30";
        // No seeded graph → stable "none" (mirrors: no edge, no diagnostic).
        let empty = CommitIndex::new();
        assert_eq!(
            merge_resolution_marker(&empty, &merged_pr(Some(sha), true)),
            "none"
        );
        // Seeded graph that resolves the SHA → carries the target Commit id.
        let mut resolves = CommitIndex::new();
        resolves.insert(sha.to_owned(), vec!["codegraph:v5:commit-0".to_owned()]);
        assert_eq!(
            merge_resolution_marker(&resolves, &merged_pr(Some(sha), true)),
            "resolved:codegraph:v5:commit-0"
        );
        // Seeded graph without the SHA → unresolved.
        let mut other = CommitIndex::new();
        other.insert(
            "elsewhere".to_owned(),
            vec!["codegraph:v5:commit-9".to_owned()],
        );
        assert_eq!(
            merge_resolution_marker(&other, &merged_pr(Some(sha), true)),
            "unresolved"
        );
        // Ambiguous SHA → ambiguous:<count>.
        let mut ambiguous = CommitIndex::new();
        ambiguous.insert(
            sha.to_owned(),
            vec!["codegraph:v5:a".to_owned(), "codegraph:v5:b".to_owned()],
        );
        assert_eq!(
            merge_resolution_marker(&ambiguous, &merged_pr(Some(sha), true)),
            "ambiguous:2"
        );
        // Unmerged PR carrying a test-merge SHA → "none" even with a matching seed.
        assert_eq!(
            merge_resolution_marker(&resolves, &merged_pr(Some(sha), false)),
            "none"
        );
        // Merged PR with no merge_commit_sha → "none".
        assert_eq!(
            merge_resolution_marker(&resolves, &merged_pr(None, true)),
            "none"
        );
    }

    #[test]
    fn unresolved_commit_diagnostic_id_is_repo_scoped() {
        // Two repositories sharing one store, each with a PR of the SAME number
        // and SAME unresolved merge_commit_sha, must mint DISTINCT diagnostic
        // record ids so neither import overwrites or tombstones the other's
        // merge-resolution evidence (#333, Codex round-7). Before the fix the id
        // omitted `source_repo`, so both collided.
        let idx = FileIndex::new();
        let c_a = ctx("acme/repo-a", &idx, &identity);
        let c_b = ctx("acme/repo-b", &idx, &identity);
        let sha = "deadbeefcafe";
        let number = 42;
        let task_id = "project:v1:task-shared";
        let detail = "no code-graph Commit record matches this SHA in the seeded store";
        let diag_a = commit_diagnostic(&c_a, number, sha, task_id, detail);
        let diag_b = commit_diagnostic(&c_b, number, sha, task_id, detail);
        assert_ne!(
            diag_a.id(),
            diag_b.id(),
            "same PR number + merge SHA in different repos must not collide"
        );
        // The retraction path ([`merge_artifact_id`]) must agree byte-for-byte
        // with the emitted id, and likewise stay repo-scoped.
        assert_eq!(
            diag_a.id(),
            commit_diagnostic_id("acme/repo-a", number, sha),
            "emitter and retraction path must agree on the repo-scoped id"
        );
        assert_ne!(
            commit_diagnostic_id("acme/repo-a", number, sha),
            commit_diagnostic_id("acme/repo-b", number, sha),
            "diagnostic id must include the source repo"
        );
    }

    #[test]
    fn status_mapping() {
        assert_eq!(issue_status("open", None), "open");
        assert_eq!(
            issue_status("closed", Some("completed")),
            "closed_completed"
        );
        assert_eq!(
            issue_status("closed", Some("not_planned")),
            "closed_dropped"
        );
        assert_eq!(
            pr_status("closed", Some("2026-01-01T00:00:00Z")),
            "closed_completed"
        );
        assert_eq!(pr_status("closed", None), "closed_dropped");
        assert_eq!(pr_status("open", None), "open");
    }

    #[test]
    fn token_in_body_is_redacted() {
        let idx = FileIndex::new();
        let redact = crate::redaction::redact_value;
        let c = ctx("o/r", &idx, &redact);
        let mut issue = sample_issue(1);
        let tok = format!("ghp_{}", "A".repeat(40));
        issue.body = Some(format!("see {tok} here"));
        let e = issue_records(&c, &issue);
        let task = &e.records[0];
        let GraphRecord::Node { body_handle, .. } = task else {
            panic!("expected node");
        };
        let inline = body_handle.as_ref().unwrap().inline.as_ref().unwrap();
        assert!(!inline.contains(&tok), "raw token must not survive");
        assert!(inline.contains("<REDACTED:api_token:"), "marker expected");
    }

    #[test]
    fn review_comment_links_unambiguous_file() {
        let mut idx = FileIndex::new();
        idx.insert(
            "src/lib.rs".to_owned(),
            vec!["codegraph:v4:file1".to_owned()],
        );
        let c = ctx("o/r", &idx, &identity);
        let comment = model::ReviewComment {
            id: 99,
            body: Some("nit".to_owned()),
            user: None,
            path: Some("src/lib.rs".to_owned()),
            line: Some(10),
            start_line: None,
            side: Some("RIGHT".to_owned()),
            diff_hunk: Some("@@ -1 +1 @@".to_owned()),
            in_reply_to_id: None,
            pull_request_url: "https://api.github.com/repos/o/r/pulls/3".to_owned(),
            commit_id: None,
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            html_url: "https://github.com/o/r/pull/3#discussion_r99".to_owned(),
        };
        let e = review_comment_records(&c, &comment);
        let labels: Vec<_> = e
            .records
            .iter()
            .filter_map(|r| match r {
                GraphRecord::Edge { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"REFERENCES_TASK"));
        assert!(labels.contains(&"TOUCHES_FILE"));
        assert_eq!(e.link_diagnostics, 0);
    }

    #[test]
    fn review_comment_missing_file_emits_diagnostic_not_guess() {
        let idx = FileIndex::new(); // empty store
        let c = ctx("o/r", &idx, &identity);
        let comment = model::ReviewComment {
            id: 99,
            body: Some("nit".to_owned()),
            user: None,
            path: Some("src/gone.rs".to_owned()),
            line: Some(10),
            start_line: None,
            side: None,
            diff_hunk: None,
            in_reply_to_id: None,
            pull_request_url: "https://api.github.com/repos/o/r/pulls/3".to_owned(),
            commit_id: None,
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            html_url: "https://github.com/o/r/pull/3#discussion_r99".to_owned(),
        };
        let e = review_comment_records(&c, &comment);
        assert_eq!(e.link_diagnostics, 1);
        let has_touches = e.records.iter().any(|r| {
            matches!(
                r, GraphRecord::Edge { label, .. } if label.as_str() == "TOUCHES_FILE"
            )
        });
        assert!(!has_touches, "must not guess a file link");
    }

    #[test]
    fn threaded_review_comments_carry_in_reply_to() {
        let idx = FileIndex::new();
        let c = ctx("o/r", &idx, &identity);
        let root = model::ReviewComment {
            id: 1,
            body: Some("a".to_owned()),
            user: None,
            path: None,
            line: None,
            start_line: None,
            side: None,
            diff_hunk: None,
            in_reply_to_id: None,
            pull_request_url: "https://api.github.com/repos/o/r/pulls/3".to_owned(),
            commit_id: None,
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            html_url: String::new(),
        };
        let reply = model::ReviewComment {
            id: 2,
            in_reply_to_id: Some(1),
            ..root.clone()
        };
        let root_rec = &review_comment_records(&c, &root).records[0];
        let reply_rec = &review_comment_records(&c, &reply).records[0];
        let GraphRecord::Node {
            in_reply_to_id: root_irt,
            ..
        } = root_rec
        else {
            panic!()
        };
        let GraphRecord::Node {
            in_reply_to_id: reply_irt,
            ..
        } = reply_rec
        else {
            panic!()
        };
        assert!(root_irt.is_none(), "thread root has no in_reply_to_id");
        assert!(reply_irt.is_some(), "reply carries in_reply_to_id");
    }

    #[test]
    fn review_comment_preserves_diff_side() {
        let idx = FileIndex::new();
        let c = ctx("o/r", &idx, &identity);
        let comment = model::ReviewComment {
            id: 5,
            body: Some("on the old side".to_owned()),
            user: None,
            path: Some("src/lib.rs".to_owned()),
            line: Some(3),
            start_line: None,
            side: Some("LEFT".to_owned()),
            diff_hunk: None,
            in_reply_to_id: None,
            pull_request_url: "https://api.github.com/repos/o/r/pulls/3".to_owned(),
            commit_id: None,
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            html_url: String::new(),
        };
        let rec = &review_comment_records(&c, &comment).records[0];
        let GraphRecord::Node { review_side, .. } = rec else {
            panic!("expected node");
        };
        assert_eq!(review_side.as_deref(), Some("LEFT"));
    }

    #[test]
    fn pr_conversation_comment_links_to_pr_task() {
        // A comment on a PR conversation arrives via /issues/comments but its
        // html_url contains /pull/. Its REFERENCES_TASK edge must target the PR
        // Task (pr:<n>), not a non-existent issue Task (issue:<n>).
        let idx = FileIndex::new();
        let c = ctx("o/r", &idx, &identity);
        let pr_comment = model::IssueComment {
            id: 77,
            body: Some("looks good".to_owned()),
            user: Some(model::User {
                login: "rev".to_owned(),
            }),
            issue_url: "https://api.github.com/repos/o/r/issues/9".to_owned(),
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            html_url: "https://github.com/o/r/pull/9#issuecomment-77".to_owned(),
        };
        let e = issue_comment_records(&c, &pr_comment);
        // The REFERENCES_TASK edge target must equal the PR Task id for #9.
        let pr_task_id = task_id_for(&c, "pr", 9);
        let issue_task_id = task_id_for(&c, "issue", 9);
        let edge_target = e
            .records
            .iter()
            .find_map(|r| match r {
                GraphRecord::Edge { label, target, .. } if label.as_str() == "REFERENCES_TASK" => {
                    Some(target.clone())
                }
                _ => None,
            })
            .expect("REFERENCES_TASK edge present");
        assert_eq!(edge_target, pr_task_id, "edge targets the PR Task");
        assert_ne!(
            edge_target, issue_task_id,
            "edge does not target an issue Task"
        );
    }

    #[test]
    fn issue_conversation_comment_links_to_issue_task() {
        let idx = FileIndex::new();
        let c = ctx("o/r", &idx, &identity);
        let issue_comment = model::IssueComment {
            id: 88,
            body: Some("a thought".to_owned()),
            user: None,
            issue_url: "https://api.github.com/repos/o/r/issues/4".to_owned(),
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            html_url: "https://github.com/o/r/issues/4#issuecomment-88".to_owned(),
        };
        let e = issue_comment_records(&c, &issue_comment);
        let issue_task_id = task_id_for(&c, "issue", 4);
        let edge_target = e
            .records
            .iter()
            .find_map(|r| match r {
                GraphRecord::Edge { label, target, .. } if label.as_str() == "REFERENCES_TASK" => {
                    Some(target.clone())
                }
                _ => None,
            })
            .expect("REFERENCES_TASK edge present");
        assert_eq!(edge_target, issue_task_id, "edge targets the issue Task");
    }

    // ── Issue #334: review commit anchoring ─────────────────────────────────────

    /// Builds a Context with a populated commit index for anchor-resolution tests.
    fn ctx_with_commits<'a>(
        repo: &'a str,
        files: &'a FileIndex,
        commits: &'a CommitIndex,
        redact: &'a Redact<'a>,
    ) -> Context<'a> {
        Context {
            source_repo: repo,
            transaction_time: "2026-01-01T00:00:00Z",
            redact,
            file_index: files,
            commit_index: commits,
        }
    }

    fn sample_review(id: u64, commit_id: Option<&str>) -> model::Review {
        model::Review {
            id,
            body: Some("looks good".to_owned()),
            state: "APPROVED".to_owned(),
            user: Some(model::User {
                login: "rev".to_owned(),
            }),
            submitted_at: Some("2026-01-02T00:00:00Z".to_owned()),
            commit_id: commit_id.map(str::to_owned),
            html_url: "https://github.com/o/r/pull/3#pullrequestreview-1".to_owned(),
        }
    }

    fn sample_review_comment(id: u64, commit_id: Option<&str>) -> model::ReviewComment {
        model::ReviewComment {
            id,
            body: Some("nit".to_owned()),
            user: None,
            path: None,
            line: None,
            start_line: None,
            side: None,
            diff_hunk: None,
            in_reply_to_id: None,
            pull_request_url: "https://api.github.com/repos/o/r/pulls/3".to_owned(),
            commit_id: commit_id.map(str::to_owned),
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            html_url: "https://github.com/o/r/pull/3#discussion_r99".to_owned(),
        }
    }

    fn find_review_node(e: &Emitted) -> &GraphRecord {
        e.records
            .iter()
            .find(|r| matches!(r, GraphRecord::Node { kind, .. } if *kind == NodeKind::Review))
            .expect("a Review node is present")
    }

    fn reviews_commit_edges(e: &Emitted) -> Vec<&GraphRecord> {
        e.records
            .iter()
            .filter(|r| matches!(r, GraphRecord::Edge { label, .. } if *label == EdgeLabel::ReviewsCommit))
            .collect()
    }

    fn diagnostics_with_code<'a>(e: &'a Emitted, code: &str) -> Vec<&'a GraphRecord> {
        let needle = format!("[{code}]");
        e.records
            .iter()
            .filter(|r| matches!(r, GraphRecord::Node { kind: NodeKind::Diagnostic, summary, .. } if summary.contains(&needle)))
            .collect()
    }

    #[test]
    fn pr_review_resolves_reviews_commit_edge() {
        // commit_id present + exactly one Commit match → review_commit_sha field
        // AND a REVIEWS_COMMIT project edge (Review→Commit, project:v1: id).
        let files = FileIndex::new();
        let mut commits = CommitIndex::new();
        commits.insert("sha-a".to_owned(), vec!["codegraph:v5:commit-a".to_owned()]);
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        let e = pr_review_records(&c, 3, &sample_review(100, Some("sha-a")));

        let review = find_review_node(&e);
        let GraphRecord::Node {
            review_commit_sha,
            source_kind,
            ..
        } = review
        else {
            panic!("expected node");
        };
        assert_eq!(review_commit_sha.as_deref(), Some("sha-a"));
        assert_eq!(source_kind.as_deref(), Some(SOURCE_KIND_REVIEW));

        let edges = reviews_commit_edges(&e);
        assert_eq!(edges.len(), 1, "exactly one REVIEWS_COMMIT edge");
        let GraphRecord::Edge {
            id, source, target, ..
        } = edges[0]
        else {
            panic!("expected edge");
        };
        assert!(
            id.starts_with("project:v1:"),
            "edge must carry project:v1: identity, got {id}"
        );
        assert_eq!(source, &review.id().to_owned(), "edge FROM the Review");
        assert_eq!(target, "codegraph:v5:commit-a", "edge TO the Commit");
        assert!(
            diagnostics_with_code(&e, "github_commit_unresolved").is_empty()
                && diagnostics_with_code(&e, "github_review_unanchored").is_empty(),
            "a resolved anchor emits no diagnostic"
        );
    }

    #[test]
    fn pr_review_comment_resolves_reviews_commit_edge() {
        let files = FileIndex::new();
        let mut commits = CommitIndex::new();
        commits.insert("sha-b".to_owned(), vec!["codegraph:v5:commit-b".to_owned()]);
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        let e = review_comment_records(&c, &sample_review_comment(200, Some("sha-b")));

        let GraphRecord::Node {
            review_commit_sha, ..
        } = find_review_node(&e)
        else {
            panic!("expected node");
        };
        assert_eq!(review_commit_sha.as_deref(), Some("sha-b"));
        let edges = reviews_commit_edges(&e);
        assert_eq!(edges.len(), 1);
        let GraphRecord::Edge { target, .. } = edges[0] else {
            panic!();
        };
        assert_eq!(target, "codegraph:v5:commit-b");
    }

    #[test]
    fn pr_review_unresolved_commit_emits_diagnostic_not_guess() {
        // commit_id present but zero match → github_commit_unresolved diagnostic,
        // no edge, field still carries the raw SHA.
        let files = FileIndex::new();
        let mut commits = CommitIndex::new();
        commits.insert("elsewhere".to_owned(), vec!["codegraph:v5:x".to_owned()]);
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        let e = pr_review_records(&c, 3, &sample_review(101, Some("sha-missing")));

        let GraphRecord::Node {
            review_commit_sha, ..
        } = find_review_node(&e)
        else {
            panic!();
        };
        assert_eq!(
            review_commit_sha.as_deref(),
            Some("sha-missing"),
            "field carries the SHA even when unresolved"
        );
        assert!(reviews_commit_edges(&e).is_empty(), "no edge on unresolved");
        assert_eq!(
            diagnostics_with_code(&e, "github_commit_unresolved").len(),
            1,
            "one unresolved diagnostic"
        );
        assert_eq!(e.link_diagnostics, 1);
    }

    #[test]
    fn pr_review_ambiguous_commit_emits_diagnostic() {
        // commit_id present + multiple matches → github_commit_unresolved, no edge.
        let files = FileIndex::new();
        let mut commits = CommitIndex::new();
        commits.insert(
            "sha-dup".to_owned(),
            vec!["codegraph:v5:a".to_owned(), "codegraph:v5:b".to_owned()],
        );
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        let e = pr_review_records(&c, 3, &sample_review(102, Some("sha-dup")));
        assert!(reviews_commit_edges(&e).is_empty());
        let diags = diagnostics_with_code(&e, "github_commit_unresolved");
        assert_eq!(diags.len(), 1);
        let GraphRecord::Node { summary, .. } = diags[0] else {
            panic!();
        };
        assert!(
            summary.contains("2 code-graph Commit records claim this SHA"),
            "diagnostic states the ambiguity count: {summary}"
        );
    }

    #[test]
    fn pr_review_absent_commit_id_emits_unanchored_diagnostic() {
        // commit_id genuinely absent on a commit-anchored review → the DISTINCT
        // github_review_unanchored diagnostic (diagnose the gap; never fabricate).
        let files = FileIndex::new();
        let mut commits = CommitIndex::new();
        commits.insert("sha-a".to_owned(), vec!["codegraph:v5:commit-a".to_owned()]);
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        let e = pr_review_records(&c, 3, &sample_review(103, None));

        let GraphRecord::Node {
            review_commit_sha, ..
        } = find_review_node(&e)
        else {
            panic!();
        };
        assert_eq!(review_commit_sha, &None, "no SHA is fabricated");
        assert!(reviews_commit_edges(&e).is_empty());
        assert!(
            diagnostics_with_code(&e, "github_commit_unresolved").is_empty(),
            "absent commit_id is NOT github_commit_unresolved"
        );
        assert_eq!(
            diagnostics_with_code(&e, "github_review_unanchored").len(),
            1,
            "absent commit_id → github_review_unanchored"
        );
    }

    #[test]
    fn issue_comment_review_is_commit_anchor_exempt() {
        // issue_comment reviews are general PR-conversation comments: field None,
        // never a REVIEWS_COMMIT edge, never an unanchored diagnostic — even with
        // a seeded graph.
        let files = FileIndex::new();
        let mut commits = CommitIndex::new();
        commits.insert("sha-a".to_owned(), vec!["codegraph:v5:commit-a".to_owned()]);
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        let comment = model::IssueComment {
            id: 77,
            body: Some("looks good".to_owned()),
            user: None,
            issue_url: "https://api.github.com/repos/o/r/issues/3".to_owned(),
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            html_url: "https://github.com/o/r/pull/3#issuecomment-77".to_owned(),
        };
        let e = issue_comment_records(&c, &comment);
        let GraphRecord::Node {
            review_commit_sha, ..
        } = find_review_node(&e)
        else {
            panic!();
        };
        assert_eq!(review_commit_sha, &None);
        assert!(reviews_commit_edges(&e).is_empty());
        assert!(diagnostics_with_code(&e, "github_review_unanchored").is_empty());
        assert_eq!(
            review_commit_marker(&commits, "issue_comment", None),
            "none",
            "issue_comment is always the stable none marker"
        );
    }

    #[test]
    fn no_seed_graph_emits_field_but_no_anchor_artifact() {
        // No seeded commit index → the field is still populated from the payload,
        // but no edge and no diagnostic (mirrors MERGED_AS's empty-seed None).
        let files = FileIndex::new();
        let commits = CommitIndex::new();
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        let e = pr_review_records(&c, 3, &sample_review(104, Some("sha-a")));
        let GraphRecord::Node {
            review_commit_sha, ..
        } = find_review_node(&e)
        else {
            panic!();
        };
        assert_eq!(review_commit_sha.as_deref(), Some("sha-a"));
        assert!(reviews_commit_edges(&e).is_empty());
        assert!(diagnostics_with_code(&e, "github_commit_unresolved").is_empty());
        assert!(diagnostics_with_code(&e, "github_review_unanchored").is_empty());
    }

    #[test]
    fn review_id_unchanged_with_or_without_commit_id() {
        // commit_id must NOT participate in the Review stable-ID composition.
        let files = FileIndex::new();
        let commits = CommitIndex::new();
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        let with = pr_review_records(&c, 3, &sample_review(105, Some("sha-a")));
        let without = pr_review_records(&c, 3, &sample_review(105, None));
        assert_eq!(
            find_review_node(&with).id(),
            find_review_node(&without).id(),
            "review record id is independent of commit_id"
        );
    }

    #[test]
    fn review_commit_marker_reflects_seed_graph_outcome() {
        let sha = "sha-a";
        let empty = CommitIndex::new();
        assert_eq!(review_commit_marker(&empty, "pr_review", Some(sha)), "none");
        let mut resolves = CommitIndex::new();
        resolves.insert(sha.to_owned(), vec!["codegraph:v5:commit-a".to_owned()]);
        assert_eq!(
            review_commit_marker(&resolves, "pr_review", Some(sha)),
            "resolved:codegraph:v5:commit-a"
        );
        assert_eq!(
            review_commit_marker(&resolves, "pr_review", None),
            "unanchored"
        );
        let mut other = CommitIndex::new();
        other.insert("elsewhere".to_owned(), vec!["codegraph:v5:z".to_owned()]);
        assert_eq!(
            review_commit_marker(&other, "pr_review_comment", Some(sha)),
            "unresolved"
        );
        let mut ambiguous = CommitIndex::new();
        ambiguous.insert(
            sha.to_owned(),
            vec!["codegraph:v5:a".to_owned(), "codegraph:v5:b".to_owned()],
        );
        assert_eq!(
            review_commit_marker(&ambiguous, "pr_review", Some(sha)),
            "ambiguous:2"
        );
    }

    #[test]
    fn review_artifact_id_matches_emitted_ids_and_is_repo_scoped() {
        // The state-side artifact id must agree byte-for-byte with the emitted
        // record's id (edge or diagnostic), and be repo-scoped (contract #7).
        let files = FileIndex::new();
        let mut commits = CommitIndex::new();
        commits.insert("sha-a".to_owned(), vec!["codegraph:v5:commit-a".to_owned()]);

        // Resolved edge case.
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        let e = pr_review_records(&c, 3, &sample_review(200, Some("sha-a")));
        let edge_id = reviews_commit_edges(&e)[0].id().to_owned();
        assert_eq!(
            review_artifact_id(
                &commits,
                "o/r",
                "pr_review:3:200",
                "pr_review",
                Some("sha-a")
            ),
            Some(edge_id),
            "artifact id matches the emitted REVIEWS_COMMIT edge id"
        );

        // Unresolved diagnostic case: emitted id equals artifact id.
        let mut nomatch = CommitIndex::new();
        nomatch.insert("z".to_owned(), vec!["codegraph:v5:z".to_owned()]);
        let c2 = ctx_with_commits("o/r", &files, &nomatch, &identity);
        let e2 = pr_review_records(&c2, 3, &sample_review(201, Some("gone")));
        let diag_id = diagnostics_with_code(&e2, "github_commit_unresolved")[0]
            .id()
            .to_owned();
        assert_eq!(
            review_artifact_id(
                &nomatch,
                "o/r",
                "pr_review:3:201",
                "pr_review",
                Some("gone")
            ),
            Some(diag_id)
        );

        // Repo scoping: same review key + sha in different repos → distinct ids.
        assert_ne!(
            review_artifact_id(
                &nomatch,
                "acme/a",
                "pr_review:3:201",
                "pr_review",
                Some("gone")
            ),
            review_artifact_id(
                &nomatch,
                "acme/b",
                "pr_review:3:201",
                "pr_review",
                Some("gone")
            ),
            "diagnostic ids must be repo-scoped"
        );

        // Exempt / no-seed cases → no artifact.
        assert_eq!(
            review_artifact_id(&commits, "o/r", "issue_comment:3:1", "issue_comment", None),
            None
        );
        assert_eq!(
            review_artifact_id(
                &CommitIndex::new(),
                "o/r",
                "pr_review:3:200",
                "pr_review",
                Some("sha-a")
            ),
            None
        );
    }

    #[test]
    fn legacy_review_node_without_commit_field_parses_to_none() {
        // A Review node with no review_commit_sha (the #333-era shape, since the
        // field is skip_serializing_if=none) must round-trip with the field
        // defaulting to None — proving legacy records deserialize cleanly.
        let files = FileIndex::new();
        let commits = CommitIndex::new();
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        // No seed graph → review_commit_sha stays None, so it is omitted on the
        // wire; this is byte-identical to a pre-#334 serialized Review node.
        let e = pr_review_records(&c, 3, &sample_review(400, None));
        let node = find_review_node(&e);
        let json = serde_json::to_string(node).expect("serialize");
        assert!(
            !json.contains("review_commit_sha"),
            "absent field is omitted on the wire: {json}"
        );
        let parsed: GraphRecord = serde_json::from_str(&json).expect("legacy Review parses");
        let GraphRecord::Node {
            review_commit_sha, ..
        } = parsed
        else {
            panic!("expected node");
        };
        assert_eq!(review_commit_sha, None);
    }

    #[test]
    fn review_commit_sha_survives_redaction_on_export() {
        // Issue #334 §8 carve-out: review_commit_sha is plaintext query
        // substrate and must NOT be enumerated by the redaction engine's
        // sensitive-field index, so a redaction-on export leaves it in plaintext
        // even while the body is redacted.
        let files = FileIndex::new();
        let commits = CommitIndex::new();
        let redact = crate::redaction::redact_value;
        let c = ctx_with_commits("o/r", &files, &commits, &redact);
        let mut review = sample_review(500, Some("deadbeefcafe"));
        review.body = Some("token ghp_0123456789abcdefghijklmnopqrstuvwxyzA".to_owned());
        let e = pr_review_records(&c, 3, &review);
        let node = find_review_node(&e);

        // The SHA is present in plaintext on the record.
        let GraphRecord::Node {
            review_commit_sha,
            body_handle,
            ..
        } = node
        else {
            panic!("expected node");
        };
        assert_eq!(review_commit_sha.as_deref(), Some("deadbeefcafe"));

        // The redaction engine never enumerates review_commit_sha as sensitive,
        // so no export pass can rewrite it.
        let sensitive = crate::redaction::sensitive_fields(node);
        assert!(
            !sensitive.iter().any(|(_, v)| *v == "deadbeefcafe"),
            "review_commit_sha must not be a sensitive field: {sensitive:?}"
        );
        // Sanity: the body handle WAS routed through redaction (secret gone).
        if let Some(h) = body_handle
            && let Some(inline) = h.inline.as_deref()
        {
            assert!(
                !inline.contains("ghp_0123456789"),
                "body secret should be redacted: {inline}"
            );
        }
    }

    #[test]
    fn review_anchor_ids_are_byte_stable_across_runs() {
        let files = FileIndex::new();
        let mut commits = CommitIndex::new();
        commits.insert("sha-a".to_owned(), vec!["codegraph:v5:commit-a".to_owned()]);
        let c = ctx_with_commits("o/r", &files, &commits, &identity);
        let a = pr_review_records(&c, 3, &sample_review(300, Some("sha-a")));
        let b = pr_review_records(&c, 3, &sample_review(300, Some("sha-a")));
        let ids_a: Vec<_> = a.records.iter().map(|r| r.id().to_owned()).collect();
        let ids_b: Vec<_> = b.records.iter().map(|r| r.id().to_owned()).collect();
        assert_eq!(ids_a, ids_b);
    }

    // ── Issue #335: reviewer identity ────────────────────────────────────────────

    fn identity_nodes(e: &Emitted) -> Vec<&GraphRecord> {
        e.records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Node {
                        kind: NodeKind::ExternalIdentity,
                        ..
                    }
                )
            })
            .collect()
    }

    fn edges_with_label(e: &Emitted, label: EdgeLabel) -> Vec<&GraphRecord> {
        e.records
            .iter()
            .filter(|r| matches!(r, GraphRecord::Edge { label: l, .. } if *l == label))
            .collect()
    }

    fn sample_pull(number: u64, reviewers: &[&str], teams: &[&str]) -> model::PullRequest {
        model::PullRequest {
            number,
            title: "A PR".to_owned(),
            body: None,
            state: "open".to_owned(),
            merged_at: None,
            draft: false,
            labels: vec![],
            assignees: vec![],
            user: Some(model::User {
                login: "author".to_owned(),
            }),
            milestone: None,
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            closed_at: None,
            head: None,
            base: None,
            merge_commit_sha: None,
            requested_reviewers: reviewers
                .iter()
                .map(|l| model::User {
                    login: (*l).to_owned(),
                })
                .collect(),
            requested_teams: teams
                .iter()
                .map(|s| model::Team {
                    slug: (*s).to_owned(),
                })
                .collect(),
            html_url: format!("https://github.com/o/r/pull/{number}"),
        }
    }

    #[test]
    fn external_identity_id_is_repo_independent_and_keyed_on_system_login() {
        // The identity id is keyed ONLY on (system, login) — NOT repo-scoped —
        // so the same login in two repos maps to one identity node.
        let a = external_identity_id("github", "octocat");
        let b = external_identity_id("github", "octocat");
        assert_eq!(a, b, "same (system, login) → same id");
        assert!(a.starts_with("project:v1:"));
        assert_ne!(a, external_identity_id("github", "other"));
    }

    #[test]
    fn identity_node_carries_only_login_and_system() {
        let files = FileIndex::new();
        let c = ctx("o/r", &files, &identity);
        let node = external_identity_node(&c, "octocat");
        let GraphRecord::Node {
            kind,
            domain,
            author,
            identity_system,
            title,
            body_handle,
            url,
            assignees,
            review_commit_sha,
            ..
        } = &node
        else {
            panic!("expected node");
        };
        assert_eq!(*kind, NodeKind::ExternalIdentity);
        assert_eq!(domain.as_deref(), Some("project"));
        assert_eq!(author.as_deref(), Some("octocat"), "login lives in author");
        assert_eq!(identity_system.as_deref(), Some("github"));
        // No email / display name / avatar / profile URL / other attributes.
        assert!(title.is_none());
        assert!(body_handle.is_none());
        assert!(url.is_none());
        assert!(assignees.is_none());
        assert!(review_commit_sha.is_none());
    }

    #[test]
    fn every_review_kind_emits_reviewed_by_to_its_author() {
        let files = FileIndex::new();
        let c = ctx("o/r", &files, &identity);
        // pr_review (author "rev")
        let pr_rev = pr_review_records(&c, 3, &sample_review(100, None));
        // pr_review_comment (author "cmt")
        let mut comment = sample_review_comment(200, None);
        comment.user = Some(model::User {
            login: "cmt".to_owned(),
        });
        let prc = review_comment_records(&c, &comment);
        // issue_comment (author "icm")
        let ic = model::IssueComment {
            id: 300,
            body: Some("hi".to_owned()),
            user: Some(model::User {
                login: "icm".to_owned(),
            }),
            issue_url: "https://api.github.com/repos/o/r/issues/3".to_owned(),
            created_at: String::new(),
            updated_at: "2026-01-02T00:00:00Z".to_owned(),
            html_url: "https://github.com/o/r/pull/3#issuecomment-300".to_owned(),
        };
        let icr = issue_comment_records(&c, &ic);

        for (e, login) in [(&pr_rev, "rev"), (&prc, "cmt"), (&icr, "icm")] {
            let edges = edges_with_label(e, EdgeLabel::ReviewedBy);
            assert_eq!(edges.len(), 1, "exactly one REVIEWED_BY per review");
            let ids = identity_nodes(e);
            assert_eq!(ids.len(), 1, "exactly one identity node");
            let GraphRecord::Edge {
                id, source, target, ..
            } = edges[0]
            else {
                panic!("edge");
            };
            assert!(
                id.starts_with("project:v1:"),
                "REVIEWED_BY carries project id"
            );
            // FROM the Review, TO the author identity.
            assert_eq!(*source, find_review_node(e).id());
            assert_eq!(*target, external_identity_id("github", login));
        }
    }

    #[test]
    fn review_without_author_mints_no_identity() {
        // A payload with no user login never fabricates an identity.
        let files = FileIndex::new();
        let c = ctx("o/r", &files, &identity);
        let mut r = sample_review(100, None);
        r.user = None;
        let e = pr_review_records(&c, 3, &r);
        assert!(identity_nodes(&e).is_empty());
        assert!(edges_with_label(&e, EdgeLabel::ReviewedBy).is_empty());
    }

    #[test]
    fn pr_emits_requested_review_from_per_reviewer_and_team_diagnostic() {
        let files = FileIndex::new();
        let c = ctx("o/r", &files, &identity);
        let pr = sample_pull(7, &["alice", "bob"], &["backend"]);
        let e = pull_records(&c, &pr);

        let edges = edges_with_label(&e, EdgeLabel::RequestedReviewFrom);
        assert_eq!(
            edges.len(),
            2,
            "one REQUESTED_REVIEW_FROM per reviewer login"
        );
        let task_id = task_id_for(&c, "pr", 7);
        for edge in &edges {
            let GraphRecord::Edge {
                id, source, target, ..
            } = edge
            else {
                panic!("edge");
            };
            assert!(id.starts_with("project:v1:"));
            assert_eq!(*source, task_id, "FROM the PR Task");
            assert!(
                target.starts_with("project:v1:"),
                "TO an identity: {target}"
            );
        }
        // Both reviewer identities present.
        let ids: std::collections::BTreeSet<_> = identity_nodes(&e)
            .iter()
            .map(|r| r.id().to_owned())
            .collect();
        assert!(ids.contains(&external_identity_id("github", "alice")));
        assert!(ids.contains(&external_identity_id("github", "bob")));
        // Team → diagnostic, never an edge, never expanded to members.
        let team_diags = diagnostics_with_code(&e, "github_team_review_request_unexpanded");
        assert_eq!(team_diags.len(), 1);
        let GraphRecord::Node { summary, .. } = team_diags[0] else {
            panic!("node");
        };
        assert!(
            summary.contains("backend"),
            "diagnostic names the team slug"
        );
        assert!(
            !ids.contains(&external_identity_id("github", "backend")),
            "a team is never expanded into a member/identity node"
        );
    }

    #[test]
    fn requested_reviewer_edge_and_identity_ids_are_byte_stable() {
        let files = FileIndex::new();
        let c = ctx("o/r", &files, &identity);
        let a = pull_records(&c, &sample_pull(7, &["alice"], &["backend"]));
        let b = pull_records(&c, &sample_pull(7, &["alice"], &["backend"]));
        let ids_a: Vec<_> = a.records.iter().map(|r| r.id().to_owned()).collect();
        let ids_b: Vec<_> = b.records.iter().map(|r| r.id().to_owned()).collect();
        assert_eq!(ids_a, ids_b, "byte-stable across runs");
    }

    #[test]
    fn pr_task_id_matches_emitter_task_id() {
        // The supersession helper reconstructs the SAME PR Task handle the
        // emitter mints; a drift would tombstone edges pointing from a phantom
        // task and never suppress the live ones.
        let files = FileIndex::new();
        let c = ctx("o/r", &files, &identity);
        assert_eq!(pr_task_id("o/r", 7), task_id_for(&c, "pr", 7));
    }

    #[test]
    fn requested_review_edge_ids_match_emitted_edge_ids() {
        // The prior/current diff set must equal, byte-for-byte, the ids the
        // emitter mints for REQUESTED_REVIEW_FROM edges (#335, Codex P1).
        let files = FileIndex::new();
        let c = ctx("o/r", &files, &identity);
        let pr = sample_pull(7, &["bob", "alice"], &["backend"]);
        let emitted: std::collections::BTreeSet<String> =
            edges_with_label(&pull_records(&c, &pr), EdgeLabel::RequestedReviewFrom)
                .iter()
                .map(|r| r.id().to_owned())
                .collect();
        let computed: std::collections::BTreeSet<String> =
            requested_review_edge_ids("o/r", &pr).into_iter().collect();
        assert_eq!(
            computed, emitted,
            "computed request-edge id set must equal the emitted edge ids"
        );
        // Sorted + deduplicated, one per non-empty login.
        assert_eq!(requested_review_edge_ids("o/r", &pr).len(), 2);
    }

    #[test]
    fn requested_review_edge_ids_skip_empty_logins() {
        let pr = sample_pull(7, &["alice", ""], &[]);
        assert_eq!(
            requested_review_edge_ids("o/r", &pr).len(),
            1,
            "an empty login mints no edge id"
        );
    }

    #[test]
    fn request_review_edge_tombstone_is_deterministic_repo_scoped_and_carries_deleted_id() {
        let deleted = "project:v1:some-request-edge";
        let a = request_review_edge_tombstone(7, deleted);
        let b = request_review_edge_tombstone(7, deleted);
        assert_eq!(a.id(), b.id(), "tombstone id is deterministic");
        let GraphRecord::Tombstone {
            id,
            deleted_id,
            summary,
            ..
        } = &a
        else {
            panic!("expected a tombstone");
        };
        assert!(id.starts_with("project:v1:"));
        assert_eq!(deleted_id, deleted, "deleted_id is the retracted edge id");
        assert!(summary.contains("requested_review_superseded"));
        // Distinct per PR (the native handle is repo-agnostic but PR-scoped) and
        // per retracted target.
        assert_ne!(a.id(), request_review_edge_tombstone(8, deleted).id());
        assert_ne!(
            a.id(),
            request_review_edge_tombstone(7, "project:v1:other").id()
        );
    }

    #[test]
    fn segregation_of_duties_is_deterministically_computable() {
        // AC8: for a merged PR, {approving identity ids} minus {author identity id}
        // is computable, and author-approved-own-PR is distinguishable from a
        // non-author approval with zero misclassifications.
        let files = FileIndex::new();
        let c = ctx("o/r", &files, &identity);

        // PR #1: author "carol"; approved by a DIFFERENT reviewer "dave".
        let mut r_dave = sample_review(10, None);
        r_dave.user = Some(model::User {
            login: "dave".to_owned(),
        });
        let e1 = pr_review_records(&c, 1, &r_dave);
        let author1 = external_identity_id("github", "carol");
        let approvers1: std::collections::BTreeSet<String> = identity_nodes(&e1)
            .iter()
            .map(|r| r.id().to_owned())
            .collect();
        // Segregation of duties: {approvers} minus {author} is the reviewer set;
        // a non-author approval leaves the author OUT of the approver set.
        assert!(
            !approvers1.contains(&author1),
            "non-author approval must NOT be flagged as self-approval"
        );

        // PR #2: author "erin" approved their OWN PR.
        let mut r_erin = sample_review(20, None);
        r_erin.user = Some(model::User {
            login: "erin".to_owned(),
        });
        let e2 = pr_review_records(&c, 2, &r_erin);
        let author2 = external_identity_id("github", "erin");
        let approvers2: std::collections::BTreeSet<String> = identity_nodes(&e2)
            .iter()
            .map(|r| r.id().to_owned())
            .collect();
        assert!(
            approvers2.contains(&author2),
            "author-approved-own-PR must be detectable (author identity in approver set)"
        );
    }

    #[test]
    fn team_diagnostic_id_is_repo_scoped() {
        let a = team_review_diagnostic_id("o/r", 7, "backend");
        let b = team_review_diagnostic_id("o/other", 7, "backend");
        assert_ne!(a, b, "team diagnostics never collide across repos");
    }
}
