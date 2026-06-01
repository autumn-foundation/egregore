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
/// `system` value for GitHub external links.
pub const SYSTEM: &str = "github";
/// Maximum bytes to inline in a handle (matches `CommandRun.stdout_handle`).
const INLINE_CEILING: usize = 16 * 1024;

/// Index from repo-relative file path to the code-graph `File` record IDs that
/// claim it. A path with more than one ID is ambiguous and is not linked.
pub type FileIndex = BTreeMap<String, Vec<String>>;

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
    )
}

/// Emits the `Task`, `ExternalLink`, and `EXTERNAL_HANDLE` edge for one PR.
#[must_use]
pub fn pull_records(ctx: &Context<'_>, pr: &model::PullRequest) -> Emitted {
    let number = pr.number;
    let native = format!("pr:{number}");
    task_and_link(
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
    )
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
    set_review_extra(&mut rec, None, None, None, None, None, None, None);
    edge_and_pack(rec, parent, None)
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
    );
    edge_and_pack(rec, parent, None)
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
    );

    // File resolution for TOUCHES_FILE (AC7).
    let review_id = review_id_for(ctx, &native);
    let (touches, diag) = resolve_file_link(ctx, &review_id, c);
    let mut emitted = edge_and_pack(rec, parent, touches);
    if let Some(d) = diag {
        emitted.records.push(d);
        emitted.link_diagnostics += 1;
    }
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
        ..
    } = &mut rec
    {
        *rk = Some(review_kind.to_owned());
        *au = author.map(str::to_owned);
        *bh = body.as_deref().map(handle_for);
        *system_native_id = Some(native_id.to_owned());
        *pt = Some(parent_task_id.to_owned());
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
) {
    if let GraphRecord::Node {
        review_state: rs,
        in_reply_to_id: irt,
        repo_relative_path,
        span,
        diff_hunk_handle,
        review_side,
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
    // native_id is "<review_kind>:<n>:<id>"; the number is the second segment.
    let number = native_id.split(':').nth(1).unwrap_or("0");
    project_stable_id(&["project", "Review", ctx.source_repo, number, native_id])
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

    fn ctx<'a>(repo: &'a str, idx: &'a FileIndex, redact: &'a Redact<'a>) -> Context<'a> {
        Context {
            source_repo: repo,
            transaction_time: "2026-01-01T00:00:00Z",
            redact,
            file_index: idx,
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
                html_url: "https://github.com/o/r/pull/5".to_owned(),
            },
        );
        let issue_task = issue.records[0].id();
        let pr_task = pr.records[0].id();
        assert_ne!(issue_task, pr_task);
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
}
