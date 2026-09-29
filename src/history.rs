//! Git history replay for bi-temporal code graph records.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::{Command, Stdio},
};

use crate::{
    PROCESS_STARTED_AT, code_graph_producer,
    crate_attribution::{CrateAttributionIndex, ManifestPackageFact, ManifestParseOutcome},
    error::{CodegraphError, Result},
    fs::SourceFile,
    identity,
    ir::{
        EdgeLabel, Graph, GraphRecord, HistoryReplayTipPayload, NodeKind, Producer, ProducerKind,
        SnapshotHead, SourceSnapshotPayload, TemporalMetadata, stable_id,
    },
    repository_record_from_identity, scan_source_text_records, validate_repository,
};

/// Scans every Git commit reachable from `HEAD` into deterministic temporal
/// graph records.
///
/// The replay reads blobs through Git object commands and does not mutate the
/// caller's working tree.
///
/// # Errors
///
/// Returns an error when the repository path is invalid, Git is unavailable, or
/// a reachable Rust source blob cannot be parsed.
pub fn scan_repository_history(repo_path: impl AsRef<Path>) -> Result<Graph> {
    scan_repository_history_with_override(repo_path, None)
}

/// Scans Git history with an optional identity override.
///
/// See `scan_repository_history` for full documentation.
///
/// # Errors
///
/// Returns an error when the repository path is invalid, Git is unavailable, or
/// a reachable Rust source blob cannot be parsed.
pub fn scan_repository_history_with_override(
    repo_path: impl AsRef<Path>,
    repo_id_override: Option<&str>,
) -> Result<Graph> {
    scan_repository_history_inner(repo_path, repo_id_override, &HistoryWindow::Full)
}

/// Scans Git history with an optional identity override and a commit window
/// (issue #256).
///
/// `HistoryWindow::Full` replays the entire history exactly as
/// [`scan_repository_history`]; the three window forms bound the replay to the
/// selected commits and record the resolved window on a `HistoryReplayWindow`
/// graph node so a windowed store is never mistaken for full history.
///
/// # Errors
///
/// Returns an error when the repository path is invalid, Git is unavailable, a
/// reachable Rust source blob cannot be parsed, a range revision does not
/// resolve, or the window selects no commits.
pub fn scan_repository_history_with_window(
    repo_path: impl AsRef<Path>,
    repo_id_override: Option<&str>,
    window: &HistoryWindow,
) -> Result<Graph> {
    scan_repository_history_inner(repo_path, repo_id_override, window)
}

/// Commit-window selector for history replay (issue #256).
///
/// The window bounds which commits are replayed; the temporal selectors
/// (`since`/`as_of`, issues #66/#118) instead filter *queries* over an
/// already-ingested store. `Full` is the historical default: every commit
/// reachable from `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HistoryWindow {
    /// Replay the entire history reachable from `HEAD` (default; unchanged).
    #[default]
    Full,
    /// Replay the `max_commits` most-recent commits reachable from `HEAD`.
    /// The walk never visits more than `max_commits` commits.
    Count {
        /// Maximum commits to replay; always ≥ 1 (zero is rejected).
        max_commits: usize,
    },
    /// Replay commits whose committer time is at or after the instant.
    Since {
        /// The `--since` instant, normalized to UTC `Z` RFC 3339 form.
        instant: String,
    },
    /// Replay commits in the `<from>..<to>` revision range (`from`
    /// exclusive, `to` inclusive). A missing `to` defaults to `HEAD`; a
    /// missing `from` replays everything reachable from `to`.
    Range {
        /// Range start revision, as given on the command line.
        from: Option<String>,
        /// Range end revision, as given on the command line.
        to: Option<String>,
    },
}

impl HistoryWindow {
    /// Builds the window from raw `scan-history` flag values (issue #256).
    ///
    /// At most one window form may be given: `--max-commits`, `--since`, and
    /// the `--from`/`--to` pair conflict pairwise. `--max-commits` must parse
    /// as a positive integer and `--since` as an RFC 3339 instant (normalized
    /// to UTC `Z` form). Every violation returns
    /// [`CodegraphError::HistoryWindow`] with a machine-readable `code`
    /// (`conflicting_window` or `invalid_window`).
    ///
    /// # Errors
    ///
    /// Returns `HistoryWindow` when the flags conflict or a window value does
    /// not parse.
    pub fn from_flags(
        max_commits: Option<&str>,
        since: Option<&str>,
        from: Option<String>,
        to: Option<String>,
    ) -> Result<Self> {
        let forms = usize::from(max_commits.is_some())
            + usize::from(since.is_some())
            + usize::from(from.is_some() || to.is_some());
        if forms > 1 {
            return Err(CodegraphError::HistoryWindow {
                code: "conflicting_window",
                message: "only one of --max-commits, --since, or --from/--to may be given"
                    .to_owned(),
            });
        }
        if let Some(raw) = max_commits {
            let max_commits = raw
                .parse::<usize>()
                .map_err(|_| CodegraphError::HistoryWindow {
                    code: "invalid_window",
                    message: format!("--max-commits must be a positive integer, got {raw:?}"),
                })?;
            if max_commits == 0 {
                return Err(CodegraphError::HistoryWindow {
                    code: "invalid_window",
                    message: "--max-commits must be at least 1".to_owned(),
                });
            }
            return Ok(Self::Count { max_commits });
        }
        if let Some(raw) = since {
            return Ok(Self::Since {
                instant: normalize_since_instant(raw)?,
            });
        }
        if from.is_some() || to.is_some() {
            return Ok(Self::Range { from, to });
        }
        Ok(Self::Full)
    }
}

/// Normalizes a `--since` flag value to UTC `Z` RFC 3339 form (issue #256).
///
/// Fractional seconds are preserved; the value must carry an explicit offset
/// (a bare date like `2026-01-04` is not RFC 3339 and is rejected).
///
/// # Errors
///
/// Returns `invalid_window` when the value is not a valid RFC 3339 instant.
fn normalize_since_instant(raw: &str) -> Result<String> {
    let parsed =
        chrono::DateTime::parse_from_rfc3339(raw).map_err(|_| CodegraphError::HistoryWindow {
            code: "invalid_window",
            message: format!("--since must be an RFC 3339 instant, got {raw:?}"),
        })?;
    Ok(parsed
        .to_utc()
        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
}

/// A [`HistoryWindow`] resolved against a repository: the selected commits in
/// replay (oldest-first) order plus, for windowed replays, the summary facts
/// recorded on the `HistoryReplayWindow` node.
struct ResolvedWindow {
    /// The commits to replay, oldest first.
    commits: Vec<GitCommit>,
    /// Payload recorded on the `HistoryReplayWindow` node; `None` for the
    /// full-history default, which emits no window node (issue #256 AC1).
    payload: Option<crate::ir::HistoryReplayWindowPayload>,
}

/// Resolves a window to its commit list via read-only `git rev-list` plumbing
/// (issue #256).
///
/// `HistoryWindow::Full` runs the exact pre-#256 `rev-list` invocation, so an
/// unwindowed replay is byte-identical to today. Every other form pushes the
/// bound down into `rev-list` itself, so the walk cost scales with the window
/// rather than total history depth. The commands are read-only and run with
/// `GIT_OPTIONAL_LOCKS=0` like the rest of the replay, preserving issue #110's
/// no-mutation guarantee.
///
/// # Errors
///
/// Returns `unresolvable_rev` when a `--from`/`--to` revision does not resolve
/// to a commit, and `empty_window` when the resolved window selects no
/// commits.
#[allow(clippy::too_many_lines)]
fn resolve_window(repo_root: &Path, window: &HistoryWindow) -> Result<ResolvedWindow> {
    let (kind, max_commits, since_instant, from_rev, to_rev, from_sha, to_sha, rev_args) =
        match window {
            HistoryWindow::Count { max_commits } => (
                "count",
                Some(*max_commits),
                None,
                None,
                None,
                None,
                None,
                vec![format!("-n{max_commits}")],
            ),
            HistoryWindow::Since { instant } => (
                "since",
                None,
                Some(instant.clone()),
                None,
                None,
                None,
                None,
                vec![format!("--since={instant}")],
            ),
            HistoryWindow::Range { from, to } => {
                let to_rev = to.clone().unwrap_or_else(|| "HEAD".to_owned());
                let to_sha = resolve_rev(repo_root, &to_rev, "--to")?;
                let (from_rev, from_sha, range) = match from {
                    Some(from_rev) => {
                        let from_sha = resolve_rev(repo_root, from_rev, "--from")?;
                        let range = format!("{from_sha}..{to_sha}");
                        (Some(from_rev.clone()), Some(from_sha), range)
                    }
                    None => (None, None, to_sha.clone()),
                };
                (
                    "range",
                    None,
                    None,
                    from_rev,
                    Some(to_rev),
                    from_sha,
                    Some(to_sha),
                    vec![range],
                )
            }
            HistoryWindow::Full => {
                // The full-history default runs the exact pre-#256 `rev-list`
                // invocation and records no window node, so an unwindowed
                // replay is byte-identical to today (issue #256 AC1).
                return Ok(ResolvedWindow {
                    commits: list_commits(repo_root)?,
                    payload: None,
                });
            }
        };

    let mut full_args: Vec<&str> = vec!["rev-list", "--reverse", "--topo-order"];
    for bound in &rev_args {
        full_args.push(bound);
    }
    // A range carries its own revision operand (`<from>..<to>` or `<to>`);
    // every other form is bounded from `HEAD`, exactly like the full-history
    // path below.
    if !matches!(window, HistoryWindow::Range { .. }) {
        full_args.push("HEAD");
    }
    let output = git_output(repo_root, &full_args)?;
    let shas: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let mut commits = shas
        .iter()
        .map(|sha| commit_metadata(repo_root, sha))
        .collect::<Result<Vec<GitCommit>>>()?;
    if let HistoryWindow::Since { instant } = window {
        apply_since_backstop(&mut commits, instant);
    }
    if commits.is_empty() {
        return Err(CodegraphError::HistoryWindow {
            code: "empty_window",
            message: format!("the {kind} window selects no commits"),
        });
    }
    let oldest = commits
        .first()
        .map(|commit| commit.sha.clone())
        .unwrap_or_default();
    let newest = commits
        .last()
        .map(|commit| commit.sha.clone())
        .unwrap_or_default();
    let selected_commit_count = commits.len();
    Ok(ResolvedWindow {
        commits,
        payload: Some(crate::ir::HistoryReplayWindowPayload {
            window: kind.to_owned(),
            selected_commit_count,
            max_commits,
            since_instant,
            from_rev,
            to_rev,
            from_sha,
            to_sha,
            oldest_commit_sha: oldest,
            newest_commit_sha: newest,
        }),
    })
}

/// Re-checks a `--since` bound in Rust against each commit's committer date
/// (issue #256).
///
/// Git's `--since` misparses ISO-8601 instants with years >= 2100 (git 2.43
/// silently treats them as the epoch, selecting every commit), so the bound
/// is verified here. Without this a far-future `--since` would replay history
/// instead of failing as an empty window. The bound stays inclusive, matching
/// git's `--since` semantics for well-formed dates.
fn apply_since_backstop(commits: &mut Vec<GitCommit>, instant: &str) {
    if let Ok(since_utc) = chrono::DateTime::parse_from_rfc3339(instant).map(|dt| dt.to_utc()) {
        commits.retain(|commit| {
            chrono::DateTime::parse_from_rfc3339(&commit.committed_at)
                .map(|dt| dt.to_utc() >= since_utc)
                .unwrap_or(true)
        });
    }
}

/// Resolves a `--from`/`--to` revision to a commit SHA (issue #256).
///
/// `rev-parse --verify <rev>^{commit}` is read-only; an unresolvable revision
/// becomes an `unresolvable_rev` diagnostic naming the flag, never Git's raw
/// stderr.
///
/// # Errors
///
/// Returns `unresolvable_rev` when the revision does not resolve to a commit.
fn resolve_rev(repo_root: &Path, rev: &str, flag: &str) -> Result<String> {
    let output = git_output(
        repo_root,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )
    .map_err(|_| CodegraphError::HistoryWindow {
        code: "unresolvable_rev",
        message: format!("{flag} revision {rev:?} does not resolve to a commit"),
    })?;
    let sha = output.trim().to_owned();
    if sha.is_empty() {
        return Err(CodegraphError::HistoryWindow {
            code: "unresolvable_rev",
            message: format!("{flag} revision {rev:?} does not resolve to a commit"),
        });
    }
    Ok(sha)
}

/// The per-run stamps on the `Repository` node (issue #224).
struct RepositoryStamp {
    /// The stamped `Repository` node (source snapshot + inferred valid time).
    node: GraphRecord,
    /// HEAD's committer date (or wall-clock fallback when HEAD is unborn),
    /// anchoring the producer envelope and every inferred valid time.
    transaction_time: String,
    /// Full SHA of HEAD when it resolves to a commit.
    head_sha: Option<String>,
}

/// Stamps the source snapshot on the `Repository` node (issue #224 extraction).
///
/// Without the snapshot, `eg freshness --graph history.graph.jsonl` and
/// `--at` queries with `--repo-path` always report `unknown` (BB1 / PR #186
/// follow-up); the stamped HEAD lets freshness detect `stale_head` after new
/// commits are added.
///
/// History replay reads only committed Git objects, so the snapshot records
/// the committed HEAD state with `dirty = false` (TT1): uncommitted
/// working-tree edits never enter the replayed graph, and stamping them dirty
/// would leave the store permanently `stale_dirty` even after the edits are
/// reverted with HEAD unchanged. Current working-tree dirtiness is detected
/// live at freshness-check time instead.
///
/// The transaction time is derived from HEAD's committer date, not
/// wall-clock, so repeated scans of an unchanged repository stay byte-stable
/// across a seconds boundary (TT3 / the history replay determinism contract).
fn repository_stamp(
    repo_root: &Path,
    repository_id: &str,
    repository: GraphRecord,
) -> Result<RepositoryStamp> {
    let head = identity::working_tree_head(repo_root);
    let head_sha = match &head {
        SnapshotHead::Commit { sha } => Some(sha.clone()),
        _ => None,
    };
    let transaction_time = match &head_sha {
        Some(sha) => commit_metadata(repo_root, sha)?.committed_at,
        None => PROCESS_STARTED_AT.clone(),
    };
    let snapshot = SourceSnapshotPayload {
        head,
        dirty: false,
        repository_id: repository_id.to_owned(),
        scanned_at: transaction_time.clone(),
    };
    let node = repository
        .with_valid_time_inferred(&transaction_time)
        .with_source_snapshot(snapshot);
    Ok(RepositoryStamp {
        node,
        transaction_time,
        head_sha,
    })
}

/// Builds the deterministic `HistoryReplay` producer envelope (issue #224
/// extraction).
///
/// `code_graph_producer` sets `producer_started_at` from the wall-clock
/// `PROCESS_STARTED_AT`, which would make two `scan-history` runs of the same
/// unchanged repository in separate processes differ (CCC1). History replay
/// is committed-state-only, so the envelope is anchored to the same
/// deterministic HEAD-committer transaction time as the snapshot.
fn history_producer(graph: &Graph, transaction_time: &str) -> Producer {
    let languages = crate::languages_in_graph(graph);
    let mut producer = code_graph_producer(&languages);
    producer.producer_kind = ProducerKind::HistoryReplay;
    producer.producer_started_at.clear();
    producer.producer_started_at.push_str(transaction_time);
    producer
}

/// Stable ID of the history-replay tip node for a repository (issue #224).
///
/// The ID folds in only the repository identity, never the tip SHA: a
/// resumed replay replaces the previous tip node in place, so every store
/// holds exactly one tip per repository.
#[must_use]
pub fn history_replay_tip_id(repository_id: &str) -> String {
    stable_id(&["node", "history-replay-tip", repository_id])
}

/// Builds the `HistoryReplayTip` node and its `Repository CONTAINS` edge for
/// a replay covering `covered_commit_count` commits and ending at
/// `tip_commit` (issue #224).
///
/// The node mirrors the `HistoryReplayWindow` metadata-node convention
/// (issue #256): the payload carries the facts, the node's valid time is the
/// replay's deterministic transaction time, and the edge attributes the tip
/// to its repository so it is never an orphan.
fn history_replay_tip_records(
    repository_id: &str,
    tip_commit: &GitCommit,
    covered_commit_count: usize,
    transaction_time: &str,
) -> (GraphRecord, GraphRecord) {
    let tip_id = history_replay_tip_id(repository_id);
    let payload = HistoryReplayTipPayload {
        repository_id: repository_id.to_owned(),
        tip_sha: tip_commit.sha.clone(),
        covered_commit_count,
        tip_committed_at: tip_commit.committed_at.clone(),
    };
    let summary = format!(
        "History replay tip for {repository_id}: commit {} ({}), covering {covered_commit_count} commit{}",
        tip_commit.short_sha(),
        tip_commit.committed_at,
        if covered_commit_count == 1 { "" } else { "s" },
    );
    let tip_node = GraphRecord::node(
        tip_id.clone(),
        NodeKind::HistoryReplayTip,
        None,
        None,
        Some(format!("history replay tip {}", tip_commit.short_sha())),
        summary,
    )
    .with_history_replay_tip(payload)
    .with_valid_time_inferred(transaction_time);
    let tip_edge = GraphRecord::edge(
        EdgeLabel::Contains,
        repository_id.to_owned(),
        tip_id,
        Some("1.0".to_owned()),
        "Repository contains history replay tip".to_owned(),
    )
    .with_valid_time_inferred(transaction_time);
    (tip_node, tip_edge)
}

/// A resume frontier for an incremental history replay (issue #224).
///
/// Produced by [`history_resume_point`] from a prior full replay's records:
/// it names the per-repository identity, the stored tip commit, and carries
/// the prior record set so [`scan_repository_history_resumed`] can merge the
/// new commits' records with it.
#[derive(Debug, Clone)]
pub struct HistoryResumePoint {
    /// Repository identity the tip belongs to; resume points never cross
    /// repository identities, even inside a shared multi-repo store.
    pub repository_id: String,
    /// Full SHA of the newest commit covered by the prior replay.
    pub tip_sha: String,
    /// Number of commits the prior replay covered.
    pub covered_commit_count: usize,
    /// The prior replay's record set. Per-run stamps (the `Repository` node,
    /// the tip node and its edge) are re-derived on resume, so they are
    /// carried along and replaced rather than pre-stripped.
    pub prior_records: Vec<GraphRecord>,
}

/// Outcome of an incremental history replay (issue #224).
#[derive(Debug)]
pub struct HistoryResumeOutcome {
    /// The merged graph: the resumed repository's prior records with fresh
    /// per-run stamps, plus the new commits' records — byte-identical to a
    /// full replay's output for that repository. Other repositories'
    /// records in a shared multi-repo frontier pass through untouched.
    pub graph: Graph,
    /// Commits read from Git and replayed in this run.
    pub processed: usize,
    /// Commits already covered by the prior replay (its
    /// `covered_commit_count`).
    pub skipped: usize,
    /// Full SHA of the new tip commit (`HEAD` after the replay).
    pub new_tip_sha: String,
}

/// Returns the repository identities of all history-replay tips in `records`.
///
/// Used to distinguish a foreign frontier (`repository_identity_mismatch`:
/// the frontier is a valid frontier, but for another repository) from a
/// frontier with no tip at all (`no_resume_point`).
#[must_use]
pub fn history_replay_tip_repository_ids(records: &[GraphRecord]) -> Vec<String> {
    let mut ids = Vec::new();
    for record in records {
        if let GraphRecord::Node {
            kind,
            history_replay_tip,
            ..
        } = record
            && *kind == NodeKind::HistoryReplayTip
            && let Some(tip) = history_replay_tip
        {
            let id = tip.repository_id.clone();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Extracts the resume frontier for `repository_id` from a prior replay's
/// records (issue #224).
///
/// Returns `None` when no `HistoryReplayTip` node is `CONTAINS`-attached to
/// the repository — windowed replays (issue #256) deliberately record no tip,
/// so a windowed store is never a valid resume frontier, and neither is a
/// plain `scan` graph.
#[must_use]
pub fn history_resume_point(
    records: &[GraphRecord],
    repository_id: &str,
) -> Option<HistoryResumePoint> {
    let tip_id = history_replay_tip_id(repository_id);
    let tip = records
        .iter()
        .find(|record| record.id() == tip_id)?
        .history_replay_tip()?;
    // The tip only counts when it is attached to the repository: a tip node
    // without the `Repository CONTAINS` edge is not a frontier.
    let attached = records.iter().any(|record| {
        matches!(
            record,
            GraphRecord::Edge {
                label: EdgeLabel::Contains,
                source,
                target,
                ..
            } if source == repository_id && target == &tip_id
        )
    });
    if !attached {
        return None;
    }
    Some(HistoryResumePoint {
        repository_id: repository_id.to_owned(),
        tip_sha: tip.tip_sha.clone(),
        covered_commit_count: tip.covered_commit_count,
        prior_records: records.to_vec(),
    })
}

/// Returns `true` when `tip_sha` is an ancestor of the current `HEAD`
/// (issue #224).
///
/// Uses `git merge-base --is-ancestor`, which reports via exit status only
/// (0 = ancestor, 1 = not, anything else = a real failure). Read-only:
/// `GIT_OPTIONAL_LOCKS=0` and no index or config writes, so the caller's
/// checkout is never mutated.
fn tip_is_ancestor_of_head(repo_root: &Path, tip_sha: &str) -> Result<bool> {
    let args = ["merge-base", "--is-ancestor", tip_sha, "HEAD"];
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| CodegraphError::GitCommand {
            command: command_display(repo_root, &args),
            message: source.to_string(),
        })?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(CodegraphError::GitCommand {
            command: command_display(repo_root, &args),
            message: format!("merge-base --is-ancestor exited with status {status}"),
        }),
    }
}

/// Lists the commits strictly after `tip_sha` up to `HEAD`, oldest first
/// (issue #224).
///
/// The range resolves through the same `rev-list --reverse --topo-order`
/// invocation as the full replay's [`list_commits`], so the new commits are
/// replayed in exactly the order a full replay would emit them.
fn list_new_commits(repo_root: &Path, tip_sha: &str) -> Result<Vec<GitCommit>> {
    let range = format!("{tip_sha}..HEAD");
    let output = git_output(
        repo_root,
        &["rev-list", "--reverse", "--topo-order", range.as_str()],
    )?;
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|sha| commit_metadata(repo_root, sha.trim()))
        .collect()
}

/// Returns `true` when `record` is one of the per-run stamps that a resumed
/// replay re-derives at the new `HEAD`: the `Repository` node, the
/// `HistoryReplayTip` node, or the `Repository CONTAINS HistoryReplayTip`
/// edge (issue #224).
fn is_resume_stamp(record: &GraphRecord, repository_id: &str, tip_id: &str) -> bool {
    if record.id() == repository_id || record.id() == tip_id {
        return true;
    }
    matches!(
        record,
        GraphRecord::Edge {
            label: EdgeLabel::Contains,
            source,
            target,
            ..
        } if source == repository_id && target == tip_id
    )
}

/// Incrementally extends a prior full history replay with only the commits
/// that landed after its tip (issue #224).
///
/// The prior replay's records are merged with the new commits' records —
/// replayed by the exact same [`replay_commit`] builder a full replay uses —
/// and the per-run stamps (the `Repository` node, the tip node and its edge,
/// the producer envelope) are re-derived at the new `HEAD`. The merged
/// record set is byte-identical to a fresh full replay of the resumed
/// repository; a zero-new-commit resume is a no-op that converges
/// byte-for-byte with the frontier. Other repositories' records in a
/// shared multi-repo frontier pass through the merge untouched (AC4).
///
/// The resume point is keyed per repository identity: presenting another
/// repository's frontier is rejected with `repository_identity_mismatch`.
/// When the stored tip is no longer an ancestor of `HEAD` the history was
/// rewritten (force-push or rebase) and resuming would fork the timeline;
/// the run fails with the machine-readable code `history_rewrite_detected`
/// and the documented recovery is a full replay.
///
/// History replay reads only committed Git objects, so the caller's checkout
/// is never mutated.
///
/// # Errors
///
/// Returns an error when the repository path is invalid, Git is unavailable,
/// a reachable Rust source blob cannot be parsed, the resume point belongs
/// to another repository, or the stored tip is not an ancestor of `HEAD`.
pub fn scan_repository_history_resumed(
    repo_path: impl AsRef<Path>,
    repo_id_override: Option<&str>,
    resume: HistoryResumePoint,
) -> Result<HistoryResumeOutcome> {
    std::sync::LazyLock::force(&PROCESS_STARTED_AT);
    let repo_root = repo_path.as_ref();
    validate_repository(repo_root)?;

    let repo_identity = identity::compute_repository_identity(repo_root, repo_id_override);
    let (repository_id, repository) = repository_record_from_identity(&repo_identity);
    if resume.repository_id != repository_id {
        return Err(CodegraphError::HistoryResume {
            code: "repository_identity_mismatch",
            message: format!(
                "resume point belongs to repository {}; current repository is {repository_id}; resume points never cross repository identities",
                resume.repository_id,
            ),
        });
    }

    // Rewritten history invalidates the frontier: resuming past a
    // force-push or rebase would fork the timeline. The check is read-only.
    if !tip_is_ancestor_of_head(repo_root, &resume.tip_sha)? {
        let short = resume.tip_sha.chars().take(8).collect::<String>();
        return Err(CodegraphError::HistoryResume {
            code: "history_rewrite_detected",
            message: format!(
                "stored history tip {short} is not an ancestor of HEAD; the repository history was rewritten (force-push or rebase). Recovery: run a full scan-history without --resume-from",
            ),
        });
    }

    let new_commits = list_new_commits(repo_root, &resume.tip_sha)?;
    let stamp = repository_stamp(repo_root, &repository_id, repository)?;
    let transaction_time = stamp.transaction_time.clone();
    let tip_id = history_replay_tip_id(&repository_id);

    // Merge: keep every prior record except the per-run stamps, which are
    // re-emitted below at the new HEAD. The resumed repository's records
    // get the re-derived producer envelope at the end; other repositories'
    // records in a shared multi-repo frontier pass through byte-untouched
    // (AC4) — see `stamp_resumed_repository`.
    let mut graph = Graph::new();
    for record in resume.prior_records {
        if !is_resume_stamp(&record, &repository_id, &tip_id) {
            graph.push(record);
        }
    }
    graph.push(stamp.node);

    let mut manifest_outcome_memo: BTreeMap<String, ManifestParseOutcome> = BTreeMap::new();
    for commit in &new_commits {
        replay_commit(
            repo_root,
            &repository_id,
            commit,
            &mut manifest_outcome_memo,
            &mut graph,
        )?;
    }

    // The new tip is HEAD; the ancestry check above guarantees HEAD resolves
    // to a commit, so the fallback only guards the unrepresentable.
    let new_head = match stamp.head_sha {
        Some(sha) => commit_metadata(repo_root, &sha)?,
        None => commit_metadata(repo_root, &resume.tip_sha)?,
    };
    let (tip_node, tip_edge) = history_replay_tip_records(
        &repository_id,
        &new_head,
        resume.covered_commit_count + new_commits.len(),
        &transaction_time,
    );
    graph.push(tip_node);
    graph.push(tip_edge);

    let producer = history_producer(&graph, &transaction_time);
    Ok(HistoryResumeOutcome {
        graph: stamp_resumed_repository(graph, &producer, &repository_id),
        processed: new_commits.len(),
        skipped: resume.covered_commit_count,
        new_tip_sha: new_head.sha,
    })
}

/// Re-stamps the producer envelope after a resumed replay (issue #224).
///
/// The envelope is re-derived at the new `HEAD` — `producer_started_at`
/// and the language inventory move with it — but ONLY the resumed
/// repository's records are re-stamped. Records belonging to other
/// repositories in a shared multi-repo frontier pass through
/// byte-untouched (AC4).
///
/// Ownership is structural: each replay's records form the connected
/// subgraph rooted at its `Repository` node, and every per-commit record
/// carries its commit's SHA as `temporal.git_commit`. A record is foreign
/// when it is reachable from another repository's node or its commit SHA
/// belongs to another repository's commits. Records attributable to
/// neither (only possible for records with no edges and no temporal
/// provenance) are treated as the resumed repository's own, which keeps
/// the single-repo resume byte-identical to a full replay.
fn stamp_resumed_repository(graph: Graph, producer: &Producer, repository_id: &str) -> Graph {
    let foreign = foreign_repository_record_ids(&graph, repository_id);
    let mut stamped = Graph::new();
    for record in graph.into_records() {
        if foreign.contains(record.id()) {
            stamped.push(record);
        } else {
            stamped.push(record.with_producer(producer.clone()));
        }
    }
    stamped
}

/// Returns the IDs of records belonging to a repository OTHER than
/// `repository_id` (issue #224, AC4).
///
/// Two structural signals, either of which marks a record foreign: graph
/// reachability from another repository's `Repository` node (undirected,
/// over edge endpoints — edge records themselves are included via their
/// own IDs), and per-commit temporal provenance (`temporal.git_commit`)
/// naming one of another repository's commits. The second signal covers
/// records with no edges at all, such as `Module`/`Import` syntax nodes
/// and non-UTF-8 `Diagnostic` nodes.
fn foreign_repository_record_ids(graph: &Graph, repository_id: &str) -> BTreeSet<String> {
    let mut other_roots: BTreeSet<&str> = BTreeSet::new();
    // Undirected adjacency over (edge id, source, target) triples, so edge
    // records are visited alongside their endpoints.
    let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut by_id: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    for record in graph.records() {
        by_id.insert(record.id(), record);
        match record {
            GraphRecord::Node {
                id,
                kind: NodeKind::Repository,
                ..
            } if id.as_str() != repository_id => {
                other_roots.insert(id.as_str());
            }
            GraphRecord::Edge {
                id, source, target, ..
            } => {
                adjacency
                    .entry(id.as_str())
                    .or_default()
                    .extend([source.as_str(), target.as_str()]);
                adjacency
                    .entry(source.as_str())
                    .or_default()
                    .push(id.as_str());
                adjacency
                    .entry(target.as_str())
                    .or_default()
                    .push(id.as_str());
            }
            GraphRecord::Node { .. } | GraphRecord::Tombstone { .. } => {}
        }
    }

    // Another repository's commits: the targets of its `Repository
    // CONTAINS` edges, identified by their `temporal.git_commit`.
    let mut other_commit_shas: BTreeSet<&str> = BTreeSet::new();
    for record in graph.records() {
        if let GraphRecord::Edge {
            label: EdgeLabel::Contains,
            source,
            target,
            ..
        } = record
            && other_roots.contains(source.as_str())
            && let Some(commit_sha) = by_id
                .get(target.as_str())
                .and_then(|target_record| record_git_commit(target_record))
        {
            other_commit_shas.insert(commit_sha);
        }
    }

    let mut foreign: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<&str> = other_roots.into_iter().collect();
    while let Some(id) = stack.pop() {
        if !foreign.insert(id.to_owned()) {
            continue;
        }
        if let Some(neighbors) = adjacency.get(id) {
            stack.extend(neighbors.iter().copied());
        }
    }
    for record in graph.records() {
        if let Some(commit_sha) = record_git_commit(record)
            && other_commit_shas.contains(commit_sha)
        {
            foreign.insert(record.id().to_owned());
        }
    }
    foreign
}

/// The commit SHA a record's temporal provenance attributes it to, if any
/// (issue #224).
const fn record_git_commit(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node {
            temporal: Some(temporal),
            ..
        }
        | GraphRecord::Edge {
            temporal: Some(temporal),
            ..
        } => Some(temporal.git_commit.as_str()),
        _ => None,
    }
}

/// Replays ONE commit into `graph`: the `Commit` node, its `Change` records,
/// every indexed source's syntax records, and the repo-wide cross-file passes
/// — all stamped with the commit's temporal provenance (issue #224).
///
/// This is the exact per-commit body the full-history loop always ran,
/// extracted so a resumed replay can run the same builder over only the new
/// commits. The emitted records are a pure function of the commit (its SHA,
/// parents, and parent trees); the manifest-OID memo is order-independent.
/// Slice-scoped passes key off `graph.records().len()` at slice start, so
/// they stay correct when already-represented records precede the new commits
/// in the graph.
///
/// # Errors
///
/// Returns an error when a reachable Rust source blob cannot be parsed or a
/// Git object command fails.
fn replay_commit(
    repo_root: &Path,
    repository_id: &str,
    commit: &GitCommit,
    manifest_outcome_memo: &mut BTreeMap<String, ManifestParseOutcome>,
    graph: &mut Graph,
) -> Result<()> {
    let commit_id = replay_commit_node(repository_id, commit, graph);
    // Attribution covers this commit's `Change` records too (they carry a
    // path), so its slice opens BEFORE them — earlier than the
    // resolution-labeling slice below, which must see only the replayed
    // source records.
    let commit_attribution_start = graph.records().len();
    let (change_ids_by_path, deleted_paths) =
        replay_commit_changes(repo_root, repository_id, commit, &commit_id, graph)?;
    let (facts_by_file, attribution, commit_records_start) = replay_commit_sources(
        repo_root,
        repository_id,
        commit,
        &change_ids_by_path,
        &commit_id,
        manifest_outcome_memo,
        graph,
    )?;
    replay_commit_cross_file_passes(
        repository_id,
        commit,
        &facts_by_file,
        &attribution,
        commit_attribution_start,
        commit_records_start,
        graph,
    );
    replay_commit_deleted_paths(
        repo_root,
        commit,
        &deleted_paths,
        commit_attribution_start,
        commit_records_start,
        manifest_outcome_memo,
        graph,
    )?;
    Ok(())
}

/// Pushes the `Commit` node, its `Repository CONTAINS` edge, and one
/// `ParentOf` edge per parent (issue #224).
///
/// Returns the commit node's stable ID so later phases can wire edges to it.
fn replay_commit_node(repository_id: &str, commit: &GitCommit, graph: &mut Graph) -> String {
    let commit_record = commit_record(repository_id, commit);
    let commit_id = commit_record.id().to_owned();
    graph.push(commit_record);
    graph.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repository_id.to_owned(),
        commit_id.clone(),
        Some("1.0".to_owned()),
        format!("Repository contains commit {}", commit.short_sha()),
    ));

    for parent in &commit.parents {
        let parent_id = stable_id(&["node", "commit", repository_id, parent]);
        graph.push(
            GraphRecord::edge(
                EdgeLabel::ParentOf,
                parent_id,
                commit_id.clone(),
                Some("1.0".to_owned()),
                format!(
                    "Commit {} is parent of {}",
                    short_sha(parent),
                    commit.short_sha()
                ),
            )
            .with_temporal(commit.temporal()),
        );
    }
    commit_id
}

/// Pushes one `Change` node plus its `Commit CONTAINS` edge per `diff-tree`
/// entry (issue #224).
///
/// Returns the change-node IDs keyed by path (for `ChangedIn` wiring) and
/// the set of deleted paths. A deleted path's `Change` record describes the
/// parent tree, not this one (issue #117), so deletions are re-attributed
/// against the parent tree by [`replay_commit_deleted_paths`].
///
/// # Errors
///
/// Returns an error when the `diff-tree` Git command fails.
fn replay_commit_changes(
    repo_root: &Path,
    repository_id: &str,
    commit: &GitCommit,
    commit_id: &str,
    graph: &mut Graph,
) -> Result<(BTreeMap<String, String>, BTreeSet<String>)> {
    let mut change_ids_by_path = BTreeMap::new();
    let mut deleted_paths: BTreeSet<String> = BTreeSet::new();
    for change in list_changes(repo_root, commit)? {
        if change.status.starts_with('D') {
            deleted_paths.insert(change.path.clone());
        }
        let change_record = change_record(repository_id, commit, &change);
        let change_id = change_record.id().to_owned();
        change_ids_by_path.insert(change.path.clone(), change_id.clone());
        graph.push(change_record);
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Contains,
                commit_id.to_owned(),
                change_id.clone(),
                Some("1.0".to_owned()),
                format!(
                    "Commit {} contains change {}",
                    commit.short_sha(),
                    change.path
                ),
            )
            .with_temporal(commit.temporal()),
        );
    }
    Ok((change_ids_by_path, deleted_paths))
}

/// Replays the commit's tree: one `ls-tree` pass yields both the indexed
/// sources and the Cargo manifests (issue #117), and each source's syntax
/// records are pushed with the commit's temporal provenance (issue #224).
///
/// Returns the per-file facts for the cross-file passes, this commit's
/// crate-attribution index, and the slice start of the pushed source
/// records. The resolution-labeling passes must see only this commit's
/// slice: the same stable edge ID can recur across commits with different
/// in-repo definition sets (issue #134).
///
/// # Errors
///
/// Returns an error when the `ls-tree` Git command fails, a reachable blob
/// cannot be read, or a reachable Rust source blob cannot be parsed.
fn replay_commit_sources(
    repo_root: &Path,
    repository_id: &str,
    commit: &GitCommit,
    change_ids_by_path: &BTreeMap<String, String>,
    commit_id: &str,
    manifest_outcome_memo: &mut BTreeMap<String, ManifestParseOutcome>,
    graph: &mut Graph,
) -> Result<(
    BTreeMap<String, crate::languages::cross_file::FileFacts>,
    CrateAttributionIndex,
    usize,
)> {
    let mut facts_by_file = BTreeMap::new();
    let commit_tree = list_commit_tree(repo_root, &commit.sha)?;
    let attribution = commit_crate_attribution_index(
        repo_root,
        &commit.sha,
        &commit_tree.manifests,
        &mut *manifest_outcome_memo,
    );
    // Records pushed from here on belong to this commit's replayed tree.
    let commit_records_start = graph.records().len();
    for path in &commit_tree.sources {
        let bytes = git_blob_bytes(repo_root, &commit.sha, path)?;
        let Ok(source) = std::str::from_utf8(&bytes) else {
            replay_non_utf8_diagnostic(repository_id, commit, path, graph);
            continue;
        };
        let source_file = SourceFile {
            path: repo_root.join(path),
            repo_relative_path: path.clone(),
        };
        let (records, facts) = scan_source_text_records(&source_file, source, repository_id)?;
        if !facts.is_empty() {
            facts_by_file.insert(path.clone(), facts);
        }
        let change_id = change_ids_by_path.get(path);
        for record in records {
            let record = record.with_temporal(commit.temporal());
            if is_temporal_change_target(&record) {
                let source_id = record.id().to_owned();
                graph.push(record);
                if let Some(change_id) = change_id {
                    graph.push(
                        GraphRecord::edge(
                            EdgeLabel::ChangedIn,
                            source_id.clone(),
                            commit_id.to_owned(),
                            Some("1.0".to_owned()),
                            format!("{path} changed in commit {}", commit.short_sha()),
                        )
                        .with_temporal(commit.temporal()),
                    );
                    graph.push(
                        GraphRecord::edge(
                            EdgeLabel::ChangedIn,
                            source_id,
                            change_id.clone(),
                            Some("1.0".to_owned()),
                            format!("{path} changed in change {}", commit.short_sha()),
                        )
                        .with_temporal(commit.temporal()),
                    );
                }
            } else {
                graph.push(record);
            }
        }
    }
    Ok((facts_by_file, attribution, commit_records_start))
}

/// Pushes the deterministic `Diagnostic` for a non-UTF-8 committed blob:
/// the blob is skipped, not aborted, and the diagnostic names the commit +
/// path with a fixed summary — never raw bytes (issue #438). `git show`
/// reads objects only, never mutating the checkout.
fn replay_non_utf8_diagnostic(
    repository_id: &str,
    commit: &GitCommit,
    path: &str,
    graph: &mut Graph,
) {
    let diag_id = stable_id(&[
        "node",
        "diagnostic",
        "non_utf8_source",
        repository_id,
        &commit.sha,
        path,
    ]);
    graph.push(
        GraphRecord::node(
            diag_id,
            NodeKind::Diagnostic,
            Some(path.to_owned()),
            None,
            Some("non_utf8_source".to_owned()),
            "skipped source file: not valid UTF-8".to_owned(),
        )
        .with_temporal(commit.temporal()),
    );
}

/// Runs the repo-wide cross-file passes over this commit's replayed tree and
/// the slice-scoped labeling passes, then applies owning-Cargo-package
/// attribution (issue #224).
///
/// The CALLS (issue #152), IMPLEMENTS (issue #344), and IMPORTS-target
/// (issue #444) passes are stamped with the commit's temporal provenance
/// like every other syntax-backed record replayed at this commit. The
/// resolution-labeling (issue #134), test-scope (issue #223), and test-role
/// (issue #238) passes are scoped to `commit_records_start..` — they must
/// see only this commit's slice. Attribution (issue #117) is scoped wider,
/// to `commit_attribution_start..`, so it also covers this commit's
/// `Change` records. Slice-scoping is mandatory: an ADR-0004 symbol ID
/// carries no commit component, so a whole-graph pass would stamp every
/// historical version of a record with the LAST commit's manifest tree — a
/// fabricated fact at a pinned historical point.
fn replay_commit_cross_file_passes(
    repository_id: &str,
    commit: &GitCommit,
    facts_by_file: &BTreeMap<String, crate::languages::cross_file::FileFacts>,
    attribution: &CrateAttributionIndex,
    commit_attribution_start: usize,
    commit_records_start: usize,
    graph: &mut Graph,
) {
    // Repo-wide cross-file call resolution for this commit's tree
    // (issue #152), stamped with the commit's temporal provenance like
    // every other syntax-backed record replayed at this commit.
    for record in
        crate::languages::cross_file::cross_file_call_records(repository_id, facts_by_file)
    {
        graph.push(record.with_temporal(commit.temporal()));
    }
    // Repo-wide cross-file trait resolution (issue #344) for this commit's
    // tree: an out-of-line impl whose trait lives in another file
    // edge-backs here, stamped with the commit's temporal provenance.
    for record in
        crate::languages::cross_file::cross_file_implements_records(repository_id, facts_by_file)
    {
        graph.push(record.with_temporal(commit.temporal()));
    }
    // Repo-wide macro-invocation resolution (issue #148) for this commit's
    // tree: a `macro_invocation` naming the commit's unique repo-defined
    // macro edge-backs here; external/ambiguous names keep their
    // Diagnostics. Stamped with the commit's temporal provenance like the
    // CALLS/IMPLEMENTS passes above.
    for record in
        crate::languages::cross_file::cross_file_macro_records(repository_id, facts_by_file)
    {
        graph.push(record.with_temporal(commit.temporal()));
    }
    // Inbound IMPORTS edges to imported Module/File targets (issue #444)
    // for this commit's tree: each resolvable Rust `use` mints
    // `File —IMPORTS→ Module|File`, stamped with the commit's temporal
    // provenance like the CALLS/IMPLEMENTS passes above. `attribution` is
    // this commit's per-commit index, built above. Fail-closed:
    // unresolvable imports mint no edge.
    let import_target_edges: Vec<GraphRecord> =
        crate::languages::cross_file::cross_file_import_target_edges(
            repository_id,
            &graph.records()[commit_records_start..],
            facts_by_file,
            attribution,
        );
    for record in import_target_edges {
        graph.push(record.with_temporal(commit.temporal()));
    }
    // Same-file resolution labeling (issue #134) over this commit's slice.
    crate::languages::cross_file::label_same_file_call_resolutions(
        &mut graph.records_mut()[commit_records_start..],
        facts_by_file,
    );
    // Out-of-line `#[cfg(test)] mod x;` test-scope marking (issue #223)
    // over this commit's replayed tree.
    crate::languages::cross_file::apply_out_of_line_test_scope(
        &mut graph.records_mut()[commit_records_start..],
        facts_by_file,
    );
    // Out-of-line `#[cfg(test)] mod x;` File-role stamping (issue #238),
    // slice-scoped to this commit like the pass above.
    crate::languages::cross_file::apply_out_of_line_test_roles(
        &mut graph.records_mut()[commit_records_start..],
        facts_by_file,
    );
    // Owning-Cargo-package attribution (issue #117), scoped to THIS
    // COMMIT'S SLICE (see the doc comment above for why).
    crate::crate_attribution::apply_crate_attribution(
        &mut graph.records_mut()[commit_attribution_start..],
        attribution,
    );
}

/// Re-resolves this commit's DELETION `Change` records against the parent
/// tree where the file last existed (issue #117).
///
/// A DELETION's `Change` describes a path this commit no longer has, so the
/// per-commit attribution above resolved it against a tree the file is
/// absent from. If the commit also removed the enclosing `Cargo.toml` — a
/// whole-package removal — that walk reaches an OUTER manifest and claims
/// the file belonged to a package it never belonged to. Re-resolving
/// against the FIRST PARENT's tree makes a deletion cite the package that
/// lost it.
///
/// Costs one extra `ls-tree` only for commits that delete something; the
/// blob-OID parse memo is shared, so manifests already parsed at an
/// earlier commit are not re-parsed. A root commit deletes nothing.
///
/// A MERGE needs the reporting parent, not the mainline one: `diff-tree
/// -m` diffs against every parent and `--no-commit-id` discards which
/// produced each entry, so a file deleted on a side branch — one the
/// first parent never had — would be resolved against a tree with no
/// nested manifest and inherit an outer package. Each parent is asked
/// separately (only for merges; a single-parent commit reports all of
/// its deletions by definition and pays no extra call).
///
/// # Errors
///
/// Returns an error when a per-parent `diff-tree` or `ls-tree` Git command
/// fails.
fn replay_commit_deleted_paths(
    repo_root: &Path,
    commit: &GitCommit,
    deleted_paths: &BTreeSet<String>,
    commit_attribution_start: usize,
    commit_records_start: usize,
    manifest_outcome_memo: &mut BTreeMap<String, ManifestParseOutcome>,
    graph: &mut Graph,
) -> Result<()> {
    if deleted_paths.is_empty() {
        return Ok(());
    }
    let mut remaining = deleted_paths.clone();
    for parent_sha in &commit.parents {
        if remaining.is_empty() {
            break;
        }
        // Which of the remaining deletions THIS parent reports. A
        // single-parent commit reports all of them by definition, so it
        // pays no extra Git call; only a merge needs asking, because
        // `diff-tree -m` discards which parent produced each entry and
        // a file deleted on one branch may not exist on the other at
        // all. First parent wins an overlap, deterministically.
        let mine: BTreeSet<String> = if commit.parents.len() == 1 {
            remaining.clone()
        } else {
            let reported = deletions_against_parent(repo_root, parent_sha, &commit.sha)?;
            remaining.intersection(&reported).cloned().collect()
        };
        if mine.is_empty() {
            continue;
        }
        let parent_tree = list_commit_tree(repo_root, parent_sha)?;
        let parent_attribution = commit_crate_attribution_index(
            repo_root,
            parent_sha,
            &parent_tree.manifests,
            &mut *manifest_outcome_memo,
        );
        crate::crate_attribution::apply_crate_attribution_where(
            &mut graph.records_mut()[commit_attribution_start..commit_records_start],
            &parent_attribution,
            |record| {
                matches!(
                    record,
                    GraphRecord::Node {
                        kind: NodeKind::Change,
                        repo_relative_path: Some(path),
                        ..
                    } if mine.contains(path)
                )
            },
        );
        remaining.retain(|path| !mine.contains(path));
    }
    // A deletion no parent reports (only reachable if Git's `-m` output
    // and the per-parent diffs disagree) keeps the post-commit answer
    // rather than being resolved against an arbitrary tree.
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn scan_repository_history_inner(
    repo_path: impl AsRef<Path>,
    repo_id_override: Option<&str>,
    window: &HistoryWindow,
) -> Result<Graph> {
    std::sync::LazyLock::force(&PROCESS_STARTED_AT);
    let repo_root = repo_path.as_ref();
    validate_repository(repo_root)?;

    let repo_identity = identity::compute_repository_identity(repo_root, repo_id_override);
    let (repository_id, repository) = repository_record_from_identity(&repo_identity);

    // The Repository node's per-run stamps (source snapshot at HEAD, inferred
    // valid time) are re-derived identically by a resumed replay, so the merged
    // record set converges byte-for-byte with a fresh full replay (issue #224).
    let stamp = repository_stamp(repo_root, &repository_id, repository)?;
    let transaction_time = stamp.transaction_time.clone();
    let mut graph = Graph::new();
    graph.push(stamp.node);
    // Issue #256: resolve the commit window before walking any history. The
    // full-history default resolves through the exact pre-#256 `rev-list`
    // invocation and records no window node; every windowed form records one
    // `HistoryReplayWindow` node, `CONTAINS`-attached to the Repository, so a
    // bounded store is never mistaken for full history.
    let resolved = resolve_window(repo_root, window)?;
    if let Some(payload) = resolved.payload {
        // The window node's stable ID folds in the window semantics (form and
        // normalized bounds), not just the selected endpoints: different window
        // forms can select the same oldest/newest commits (e.g. `--max-commits
        // 3` versus `--since <t>`), and each must remain a distinct node.
        let max_commits = payload
            .max_commits
            .map(|n| n.to_string())
            .unwrap_or_default();
        let window_id = stable_id(&[
            "node",
            "history-replay-window",
            &repository_id,
            &payload.window,
            &max_commits,
            payload.since_instant.as_deref().unwrap_or(""),
            payload.from_rev.as_deref().unwrap_or(""),
            payload.to_rev.as_deref().unwrap_or(""),
            payload.from_sha.as_deref().unwrap_or(""),
            payload.to_sha.as_deref().unwrap_or(""),
            &payload.oldest_commit_sha,
            &payload.newest_commit_sha,
        ]);
        let summary = format!(
            "History replay window '{}' selected {} commit{}",
            payload.window,
            payload.selected_commit_count,
            if payload.selected_commit_count == 1 {
                ""
            } else {
                "s"
            },
        );
        graph.push(
            GraphRecord::node(
                window_id.clone(),
                NodeKind::HistoryReplayWindow,
                None,
                None,
                None,
                summary,
            )
            .with_history_replay_window(payload)
            .with_valid_time_inferred(&transaction_time),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repository_id.clone(),
            window_id,
            Some("1.0".to_owned()),
            "Repository contains history replay window summary".to_owned(),
        ));
    }

    // Manifest-parse memo keyed by blob OID (issue #117): a `Cargo.toml` is
    // typically unchanged across hundreds of commits, so each distinct manifest
    // blob is read and parsed exactly once for the whole replay.
    let mut manifest_outcome_memo: BTreeMap<String, ManifestParseOutcome> = BTreeMap::new();
    for commit in &resolved.commits {
        replay_commit(
            repo_root,
            &repository_id,
            commit,
            &mut manifest_outcome_memo,
            &mut graph,
        )?;
    }

    // Issue #224: a full, unwindowed history replay ends at a named frontier
    // so the next replay can start from the new commits instead of replaying
    // everything. The tip node records the newest covered commit; windowed
    // replays deliberately record NO tip — a bounded store is not a valid
    // resume frontier (resuming from one would silently drop the
    // bounded-out history).
    if matches!(window, HistoryWindow::Full)
        && let Some(tip_commit) = resolved.commits.last()
    {
        let (tip_node, tip_edge) = history_replay_tip_records(
            &repository_id,
            tip_commit,
            resolved.commits.len(),
            &transaction_time,
        );
        graph.push(tip_node);
        graph.push(tip_edge);
    }

    let producer = history_producer(&graph, &transaction_time);
    Ok(graph.stamp_producer(&producer))
}

#[derive(Debug, Clone)]
struct GitCommit {
    sha: String,
    parents: Vec<String>,
    committed_at: String,
    authored_at: String,
    author_name: String,
    author_email: String,
    subject: String,
}

impl GitCommit {
    fn temporal(&self) -> TemporalMetadata {
        TemporalMetadata {
            git_commit: self.sha.clone(),
            git_parent_commits: self.parents.clone(),
            valid_time: self.committed_at.clone(),
            author_time: Some(self.authored_at.clone()),
            observed_at: self.committed_at.clone(),
            valid_time_source: Some("git_commit_committer_date".to_owned()),
        }
    }

    fn short_sha(&self) -> &str {
        short_sha(&self.sha)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GitChange {
    status: String,
    path: String,
}

fn list_commits(repo_root: &Path) -> Result<Vec<GitCommit>> {
    let output = git_output(
        repo_root,
        &["rev-list", "--reverse", "--topo-order", "HEAD"],
    )?;
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|sha| commit_metadata(repo_root, sha.trim()))
        .collect()
}

fn commit_metadata(repo_root: &Path, sha: &str) -> Result<GitCommit> {
    let output = git_output(
        repo_root,
        &["show", "-s", "--format=%H%n%P%n%cI%n%aI%n%an%n%ae%n%s", sha],
    )?;
    let mut lines = output.lines();
    let full_sha = required_line(&mut lines, "commit sha")?;
    let parents = required_line(&mut lines, "commit parents")?;
    let committed_at = required_line(&mut lines, "commit time")?;
    let authored_at = required_line(&mut lines, "author time")?;
    let author_name = required_line(&mut lines, "author name")?;
    let author_email = required_line(&mut lines, "author email")?;
    let subject = lines.collect::<Vec<_>>().join("\n");

    Ok(GitCommit {
        sha: full_sha.to_owned(),
        parents: parents
            .split_whitespace()
            .filter(|parent| !parent.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        committed_at: normalize_timestamp(committed_at),
        authored_at: normalize_timestamp(authored_at),
        author_name: author_name.to_owned(),
        author_email: author_email.to_owned(),
        subject,
    })
}

/// Normalizes ISO 8601 timestamps to use `Z` suffix for UTC.
fn normalize_timestamp(ts: &str) -> String {
    ts.strip_suffix("+00:00")
        .map_or_else(|| ts.to_owned(), |s| format!("{s}Z"))
}

/// Lists the paths one commit changed, NUL-delimited.
///
/// `-z` must match [`list_commit_tree`]: the two listings are separate Git
/// commands whose paths are joined by `CHANGED_IN` edges, so if only one reads
/// unquoted output a path containing `"`, a tab, or a non-ASCII byte is spelled
/// one way on the `File` node and another on the `Change` node — the file is
/// indexed but silently orphaned from every history lane. Output is read as
/// BYTES and decoded per token so an undecodable path is skipped, never fatal.
///
/// `--name-status -z` emits `status NUL path NUL`, except for rename/copy
/// statuses (`R###` / `C###`), which emit `status NUL source NUL destination
/// NUL`. The destination is the changed path.
fn list_changes(repo_root: &Path, commit: &GitCommit) -> Result<Vec<GitChange>> {
    let output = git_output_bytes(
        repo_root,
        &[
            "diff-tree",
            "-m",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-z",
            "--root",
            &commit.sha,
        ],
    )?;
    Ok(parse_name_status_z(&output))
}

/// The paths one PARENT reports as deleted by `commit` (issue #117).
///
/// `diff-tree -m --no-commit-id` diffs a merge against every parent and
/// discards which one produced a given entry, so a deletion can only be
/// attributed to the tree it actually came from by asking each parent
/// separately. Used ONLY to route merge deletions to the right parent tree;
/// which `Change` records exist is still decided by [`list_changes`].
fn deletions_against_parent(
    repo_root: &Path,
    parent_sha: &str,
    commit_sha: &str,
) -> Result<BTreeSet<String>> {
    let output = git_output_bytes(
        repo_root,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-z",
            parent_sha,
            commit_sha,
        ],
    )?;
    Ok(parse_name_status_z(&output)
        .into_iter()
        .filter(|change| change.status.starts_with('D'))
        .map(|change| change.path)
        .collect())
}

/// Parses `--name-status -z` output into de-duplicated changes.
fn parse_name_status_z(output: &[u8]) -> Vec<GitChange> {
    let mut tokens = output
        .split(|byte| *byte == 0)
        .map(|raw| std::str::from_utf8(raw).ok())
        .filter(|token| token.is_none_or(|token| !token.is_empty()));

    let mut seen = BTreeSet::new();
    let mut changes = Vec::new();
    while let Some(status) = tokens.next() {
        // A status token is always ASCII; an undecodable one means the stream
        // is not where we think it is, so stop rather than mis-pair fields.
        let Some(status) = status else { break };
        // Rename and copy carry a source path before the destination.
        let renamed = status.starts_with('R') || status.starts_with('C');
        let first = tokens.next();
        let path = if renamed { tokens.next() } else { first };
        let Some(path) = path else { continue };
        // An undecodable path is skipped; its status token was consumed above,
        // so the stream stays aligned.
        let Some(path) = path else { continue };
        if status.is_empty() || path.is_empty() {
            continue;
        }
        let change = GitChange {
            status: status.to_owned(),
            path: normalize_git_path(path),
        };
        if seen.insert(change.clone()) {
            changes.push(change);
        }
    }
    changes
}

/// One commit tree's indexed source files and Cargo manifests, from a SINGLE
/// `git ls-tree` invocation (issue #117).
#[derive(Debug, Default)]
struct CommitTree {
    /// Indexed source paths, sorted — the same set the pre-#117
    /// `list_indexed_source_files` produced.
    sources: Vec<String>,
    /// `(repo-relative manifest path, blob OID)` for every `Cargo.toml`,
    /// sorted by path. The OID lets the replay parse each distinct manifest
    /// blob once instead of once per commit.
    manifests: Vec<(String, String)>,
}

/// Lists a commit's indexed sources and Cargo manifests in one `ls-tree` pass.
///
/// Uses the full (non-`--name-only`) form so each entry carries its object TYPE
/// and OID: the type filter drops submodule gitlinks — which are `commit`
/// entries, not readable blobs — and the OID keys the manifest parse memo.
///
/// `-z` is load-bearing, not a style choice. Git C-quotes any path containing a
/// non-ASCII byte, `"`, `\`, or a control character — wrapping it in literal
/// quotes that no subsequent `git show <sha>:<path>` can resolve. (Setting
/// `core.quotePath=false` suppresses only the NON-ASCII half, so it is not a
/// sufficient fix.) The working-tree walk reads `git ls-files -z`, whose
/// NUL-delimited output is never quoted, so without `-z` here a quote-bearing
/// path is indexed by `eg scan` and silently ABSENT from `eg scan-history`.
///
/// Output is read as BYTES and decoded per entry: a Git path is a byte string
/// and need not be UTF-8, so one undecodable path is skipped — mirroring the
/// issue-#438 undecodable-blob skip — rather than aborting the whole replay.
///
/// Both filters prune any path with a `target` component, mirroring
/// [`is_indexed_source`], so committed build output never contributes sources
/// or owning packages.
fn list_commit_tree(repo_root: &Path, sha: &str) -> Result<CommitTree> {
    let output = git_output_bytes(repo_root, &["ls-tree", "-r", "-z", sha])?;
    let mut tree = CommitTree::default();
    for raw in output.split(|byte| *byte == 0) {
        let Ok(entry) = std::str::from_utf8(raw) else {
            // A path whose bytes are not UTF-8: skip this entry, keep replaying.
            continue;
        };
        // `<mode> SP <type> SP <object> TAB <path>`
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        let (Some(mode), Some(object_type), Some(oid)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if object_type != "blob" {
            // A `commit` entry is a submodule gitlink: its content belongs to a
            // different repository and cannot be read from this one.
            continue;
        }
        // A symlink IS a blob (mode 120000) whose content is the link target,
        // not the file's. Reading one as manifest text would parse
        // `../real/Cargo.toml` as TOML and report `unparseable_manifest` about a
        // manifest that is perfectly valid. The working-tree walk excludes
        // symlinks through `symlink_metadata(..).is_file()`; match it, so both
        // discovery paths agree that a symlinked manifest is invisible.
        if mode == "120000" {
            continue;
        }
        // Only the metadata prefix is split off; the path is taken verbatim,
        // since a NUL-delimited entry carries no trailing newline and a path
        // may legitimately begin or end with whitespace.
        let path = normalize_git_path(path);
        let as_path = Path::new(path.as_str());
        if is_indexed_source(as_path) {
            tree.sources.push(path);
            continue;
        }
        if is_cargo_manifest_path(as_path) {
            tree.manifests.push((path, oid.to_owned()));
        }
    }
    tree.sources.sort();
    tree.manifests.sort();
    Ok(tree)
}

/// Matches the working-tree scanner's manifest set (`fs::discover_cargo_manifests`):
/// the basename must be exactly `Cargo.toml` (case-sensitive), and the path must
/// not sit under a `target/` build directory.
fn is_cargo_manifest_path(path: &Path) -> bool {
    path.file_name().and_then(std::ffi::OsStr::to_str) == Some("Cargo.toml")
        && !path.components().any(|c| c.as_os_str() == "target")
}

/// Builds the crate-attribution index for one commit's tree (issue #117).
///
/// Reads each manifest blob through the same read-only `git show` plumbing the
/// source replay uses, and reduces it with `manifest_deps::manifest_package_outcome`
/// — the SAME reduction the working-tree harvest calls, so the two paths cannot
/// disagree about what a manifest declares.
///
/// `memo` caches the reduction by blob OID across the whole replay: a manifest
/// is typically unchanged for hundreds of commits, so this parses each distinct
/// manifest blob exactly once.
///
/// A blob that cannot be read or decoded becomes an `Unreadable` fact, never an
/// aborted replay — mirroring the non-UTF-8 source skip (issue #438).
fn commit_crate_attribution_index(
    repo_root: &Path,
    sha: &str,
    manifests: &[(String, String)],
    memo: &mut BTreeMap<String, ManifestParseOutcome>,
) -> CrateAttributionIndex {
    let mut facts = Vec::with_capacity(manifests.len());
    for (path, oid) in manifests {
        let outcome = memo.get(oid).cloned().unwrap_or_else(|| {
            let outcome = git_blob_bytes(repo_root, sha, path)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .map_or(ManifestParseOutcome::Unreadable, |text| {
                    crate::manifest_deps::manifest_package_outcome(&text)
                });
            memo.insert(oid.clone(), outcome.clone());
            outcome
        });
        facts.push(ManifestPackageFact::new(path.clone(), outcome));
    }
    CrateAttributionIndex::from_facts(facts)
}

/// Matches the live scanner's source set (`fs::discover_source_files`) so the history
/// replay indexes exactly what `eg scan` would, keeping it consistent with the
/// freshness dirty probe (which is scoped the same way):
/// - a **case-sensitive** lowercase supported extension (`.rs`, `.py`, `.ts`, `.tsx`,
///   `.go`) — the scanner matches `extension()` exactly, so an uppercase `LIB.RS` is
///   not a source (GGG1);
/// - never under a `target/` build directory, which `fs::should_descend` prunes,
///   so committed build output is not indexed (GGG2).
fn is_indexed_source(path: &Path) -> bool {
    crate::languages::is_supported_source(path)
        && !path.components().any(|c| c.as_os_str() == "target")
}

/// Reads a committed blob's RAW bytes via `git show <sha>:<path>` (issue #438).
///
/// Returns the bytes undecoded so the caller can decode-or-skip a non-UTF-8
/// blob rather than aborting the whole replay (the shared `git_output` decodes
/// via `String::from_utf8` and would error). `git show` reads Git objects only
/// and never mutates the working tree.
fn git_blob_bytes(repo_root: &Path, sha: &str, path: &str) -> Result<Vec<u8>> {
    git_output_bytes(repo_root, &["show", &format!("{sha}:{path}")])
}

fn commit_record(repository_id: &str, commit: &GitCommit) -> GraphRecord {
    let id = stable_id(&["node", "commit", repository_id, &commit.sha]);
    GraphRecord::node(
        id,
        NodeKind::Commit,
        None,
        None,
        Some(commit.sha.clone()),
        format!(
            "Git commit {} at {}: {}",
            commit.short_sha(),
            commit.committed_at,
            commit.subject
        ),
    )
    .with_temporal(commit.temporal())
    .with_author(
        Some(commit.author_name.clone()),
        Some(commit.author_email.clone()),
    )
}

fn change_record(repository_id: &str, commit: &GitCommit, change: &GitChange) -> GraphRecord {
    let id = stable_id(&[
        "node",
        "change",
        repository_id,
        &commit.sha,
        &change.status,
        &change.path,
    ]);
    GraphRecord::node(
        id,
        NodeKind::Change,
        Some(change.path.clone()),
        None,
        Some(format!("{} {}", change.status, change.path)),
        format!(
            "Git change {} to {} in commit {}",
            change.status,
            change.path,
            commit.short_sha()
        ),
    )
    .with_temporal(commit.temporal())
}

/// Gates ONLY the two `CHANGED_IN` edges (to the commit and to the change) —
/// which are minted for `File`/`Symbol` nodes in a changed path. It does NOT gate
/// temporal-history membership: every record (Module, Import, etc.) is stamped
/// with `commit.temporal()` and `graph.push`-ed at every commit via the `else`
/// arm of the caller, so Module/Import body drift stays content-comparable in
/// scan-history (issue #206; end-to-end regression:
/// `scan_history_detects_inline_module_body_drift_end_to_end`).
const fn is_temporal_change_target(record: &GraphRecord) -> bool {
    matches!(
        record,
        GraphRecord::Node {
            kind: NodeKind::File | NodeKind::Symbol,
            ..
        }
    )
}

fn git_output(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|source| CodegraphError::GitCommand {
            command: command_display(repo_root, args),
            message: source.to_string(),
        })?;

    if !output.status.success() {
        return Err(CodegraphError::GitCommand {
            command: command_display(repo_root, args),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    String::from_utf8(output.stdout).map_err(|source| CodegraphError::GitCommand {
        command: command_display(repo_root, args),
        message: source.to_string(),
    })
}

/// Runs a git command and returns its RAW stdout bytes (issue #438).
///
/// The bytes-returning sibling of [`git_output`]: it applies the same failure
/// handling (a non-zero exit is an error) but never decodes stdout as UTF-8, so
/// a non-UTF-8 blob reaches the caller intact instead of aborting. Used only by
/// `git_blob_bytes`; the shared `git_output` is deliberately left unchanged.
fn git_output_bytes(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|source| CodegraphError::GitCommand {
            command: command_display(repo_root, args),
            message: source.to_string(),
        })?;

    if !output.status.success() {
        return Err(CodegraphError::GitCommand {
            command: command_display(repo_root, args),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(output.stdout)
}

fn command_display(repo_root: &Path, args: &[&str]) -> String {
    let mut parts = vec![
        "git".to_owned(),
        "-C".to_owned(),
        repo_root.display().to_string(),
    ];
    parts.extend(args.iter().map(|arg| (*arg).to_owned()));
    parts.join(" ")
}

fn required_line<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    field_name: &str,
) -> Result<&'a str> {
    lines.next().ok_or_else(|| CodegraphError::GitCommand {
        command: "git show -s --format=%H%n%P%n%cI%n%aI%n%an%n%ae%n%s".to_owned(),
        message: format!("missing {field_name}"),
    })
}

fn normalize_git_path(path: &str) -> String {
    path.split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect::<Vec<_>>()
        .join("/")
}

fn short_sha(sha: &str) -> &str {
    if sha.len() >= 12 { &sha[..12] } else { sha }
}

#[cfg(test)]
mod history_window_tests {
    use super::*;
    use crate::error::CodegraphError;

    fn window_code(error: CodegraphError) -> &'static str {
        match error {
            CodegraphError::HistoryWindow { code, .. } => code,
            other => panic!("expected a HistoryWindow error, got: {other:?}"),
        }
    }

    #[test]
    fn no_flags_selects_full_history() {
        assert_eq!(
            HistoryWindow::from_flags(None, None, None, None).expect("no flags are valid"),
            HistoryWindow::Full
        );
    }

    #[test]
    fn max_commits_parses_positive_counts() {
        assert_eq!(
            HistoryWindow::from_flags(Some("3"), None, None, None).expect("valid count"),
            HistoryWindow::Count { max_commits: 3 }
        );
    }

    #[test]
    fn max_commits_rejects_zero() {
        assert_eq!(
            window_code(
                HistoryWindow::from_flags(Some("0"), None, None, None)
                    .expect_err("zero commits is an empty window")
            ),
            "invalid_window"
        );
    }

    #[test]
    fn max_commits_rejects_non_integers() {
        for raw in ["many", "-2", "3.5", ""] {
            assert_eq!(
                window_code(
                    HistoryWindow::from_flags(Some(raw), None, None, None)
                        .expect_err("non-integer count must be rejected")
                ),
                "invalid_window",
                "input {raw:?} should be an invalid_window"
            );
        }
    }

    #[test]
    fn since_accepts_rfc3339_and_normalizes_to_utc() {
        assert_eq!(
            HistoryWindow::from_flags(None, Some("2026-01-04T00:00:00Z"), None, None)
                .expect("valid instant"),
            HistoryWindow::Since {
                instant: "2026-01-04T00:00:00Z".to_owned()
            }
        );
        assert_eq!(
            HistoryWindow::from_flags(None, Some("2026-01-04T02:00:00+02:00"), None, None)
                .expect("offset instant is valid RFC 3339"),
            HistoryWindow::Since {
                instant: "2026-01-04T00:00:00Z".to_owned()
            }
        );
    }

    #[test]
    fn since_rejects_non_rfc3339() {
        for raw in ["tomorrow", "2026-01-04", "2026-13-99T99:99:99Z", ""] {
            assert_eq!(
                window_code(
                    HistoryWindow::from_flags(None, Some(raw), None, None)
                        .expect_err("non-RFC-3339 instant must be rejected")
                ),
                "invalid_window",
                "input {raw:?} should be an invalid_window"
            );
        }
    }

    #[test]
    fn range_accepts_from_and_to_forms() {
        assert_eq!(
            HistoryWindow::from_flags(None, None, Some("v1.0".to_owned()), Some("main".to_owned()))
                .expect("from+to is a valid range"),
            HistoryWindow::Range {
                from: Some("v1.0".to_owned()),
                to: Some("main".to_owned())
            }
        );
        assert_eq!(
            HistoryWindow::from_flags(None, None, Some("v1.0".to_owned()), None)
                .expect("lone from is a valid range"),
            HistoryWindow::Range {
                from: Some("v1.0".to_owned()),
                to: None
            }
        );
        assert_eq!(
            HistoryWindow::from_flags(None, None, None, Some("main".to_owned()))
                .expect("lone to is a valid range"),
            HistoryWindow::Range {
                from: None,
                to: Some("main".to_owned())
            }
        );
    }

    #[test]
    fn conflicting_window_forms_are_rejected() {
        assert_eq!(
            window_code(
                HistoryWindow::from_flags(Some("3"), Some("2026-01-04T00:00:00Z"), None, None)
                    .expect_err("count and since conflict")
            ),
            "conflicting_window"
        );
        assert_eq!(
            window_code(
                HistoryWindow::from_flags(Some("3"), None, Some("v1.0".to_owned()), None)
                    .expect_err("count and from conflict")
            ),
            "conflicting_window"
        );
        assert_eq!(
            window_code(
                HistoryWindow::from_flags(
                    None,
                    Some("2026-01-04T00:00:00Z"),
                    None,
                    Some("main".to_owned())
                )
                .expect_err("since and to conflict")
            ),
            "conflicting_window"
        );
    }
}
