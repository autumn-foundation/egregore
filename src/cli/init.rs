//! One-command repository bootstrap: clone to queryable embedded store (issue #229).
//!
//! `eg init <repo> --data-dir <dir>` orchestrates the existing capture verbs
//! in-process — current-tree scan, embedded ingest with semantic embeddings
//! (generated for the tree batch, on by default), Git history replay, and
//! structural history ingest — into a single embedded store, then
//! prints a machine-readable bootstrap report. It never shells out to a second
//! `eg` binary: it calls the same lane functions the individual commands use
//! ([`scan`], [`scan_history`], and the embedded ingest pipeline from
//! [`ingest`]), so the store it builds cannot drift from the documented
//! multi-step sequence.
//!
//! Exit codes (documented in `docs/cli/init.md`):
//! - `0` — the store was built or rebuilt and is queryable. The report's
//!   `status` distinguishes `rebuilt` (every attempted stage completed) from
//!   `partial` (an optional stage was skipped or failed but the store is still
//!   usable); the report names every stage either way.
//! - `3` ([`INIT_ALREADY_CURRENT_EXIT_CODE`]) — already current: the recorded
//!   source snapshot matches the working tree and the requested semantic state
//!   is present, so nothing was rebuilt.
//! - `1` — a required stage failed. The report is printed first and names the
//!   completed and failed stages; the store is left in the documented
//!   non-corrupt state (whatever the completed stages wrote, exactly as the
//!   manual sequence would leave it).
//! - `2` ([`INIT_REFUSED_EXIT_CODE`]) — refused before writing anything (the
//!   capacity preflight or the embedding-index identity conflict). The store is
//!   untouched.
//!
//! History replay reads Git objects read-only — the `scan-history` lane's
//! contract, preserved here — so the working tree and Git state are never
//! mutated. Intermediate JSONL lives in a system temporary directory, never
//! in the repository being bootstrapped.

use super::*;

/// Exit code distinguishing "already current" (no-op) from "rebuilt" (issue #229).
///
/// A second `eg init` on an unchanged repository exits with this code after
/// printing the bootstrap report; `0` means the store was (re)built.
pub(crate) const INIT_ALREADY_CURRENT_EXIT_CODE: i32 = 3;

/// Exit code for refusing to write anything: the capacity preflight or the
/// embedding-index identity conflict fired before any record was persisted, so
/// the store is untouched.
pub(crate) const INIT_REFUSED_EXIT_CODE: i32 = 2;

/// Resolved inputs for [`init_cmd`].
pub(crate) struct InitArgs {
    /// Repository path to bootstrap.
    pub repo_path: PathBuf,
    /// Explicit `--data-dir` (resolved against `egregore.toml` by the caller).
    pub data_dir: Option<PathBuf>,
    /// Output format for the bootstrap report.
    pub format: OutputFormat,
    /// Skip semantic embedding generation, loudly.
    #[cfg(feature = "embeddings")]
    pub no_embed: bool,
    /// Explicit `--embed-model`.
    #[cfg(feature = "embeddings")]
    pub embed_model: Option<String>,
    /// Explicit `--repo-id-override`.
    pub repo_id_override: Option<String>,
    /// Explicit `--raw-literals`.
    pub raw_literals: bool,
}

/// Lifecycle of one bootstrap stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StageStatus {
    Completed,
    Skipped,
    Failed,
}

impl StageStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

/// Machine-readable outcome of one `eg init` stage.
#[derive(Debug, Clone, Serialize)]
struct StageReport {
    name: &'static str,
    status: StageStatus,
    /// Records the stage produced (scan stages) or attempted (ingest stages).
    records: Option<usize>,
    /// Records successfully written (ingest stages only).
    succeeded: Option<usize>,
    /// The skip reason, or the failure message.
    detail: Option<String>,
}

impl StageReport {
    const fn completed(name: &'static str, records: usize) -> Self {
        Self {
            name,
            status: StageStatus::Completed,
            records: Some(records),
            succeeded: None,
            detail: None,
        }
    }

    const fn completed_ingest(name: &'static str, attempted: usize, succeeded: usize) -> Self {
        Self {
            name,
            status: StageStatus::Completed,
            records: Some(attempted),
            succeeded: Some(succeeded),
            detail: None,
        }
    }

    const fn skipped(name: &'static str, reason: String) -> Self {
        Self {
            name,
            status: StageStatus::Skipped,
            records: None,
            succeeded: None,
            detail: Some(reason),
        }
    }

    const fn failed(name: &'static str, message: String) -> Self {
        Self {
            name,
            status: StageStatus::Failed,
            records: None,
            succeeded: None,
            detail: Some(message),
        }
    }
}

/// Overall bootstrap outcome carried by the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum InitStatus {
    Rebuilt,
    AlreadyCurrent,
    Partial,
    Failed,
}

impl InitStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Rebuilt => "rebuilt",
            Self::AlreadyCurrent => "already_current",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }
}

/// Queryability of the store the bootstrap left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StoreState {
    /// Every attempted stage completed; the store answers structural and
    /// semantic queries as requested.
    Complete,
    /// Semantics were requested but skipped or failed: the store answers
    /// structural queries (`query symbol`, `query context`) but not semantic
    /// ones. The report's `semantics.skipped_reason` names why.
    StructuralOnly,
    /// The history stage was skipped or failed: the tree is fully indexed but
    /// temporal history is absent or incomplete.
    PartialHistory,
}

impl StoreState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::StructuralOnly => "structural_only",
            Self::PartialHistory => "partial_history",
        }
    }
}

/// Machine-readable bootstrap report printed by `eg init` (issue #229).
///
/// `--format json` prints this struct pretty-printed on stdout;
/// `--format text` renders the same fields as human-readable lines. Both carry
/// the repository identity, per-domain record counts, semantic state, and the
/// indexed source snapshot/commit tip, so a caller can verify what was built
/// without a follow-up command.
#[derive(Debug, Clone, Serialize)]
struct InitReport {
    command: &'static str,
    status: InitStatus,
    repository: InitRepository,
    data_dir: String,
    stages: Vec<StageReport>,
    records: InitRecordCounts,
    semantics: InitSemantics,
    snapshot: InitSnapshot,
    store_state: StoreState,
}

/// Repository identity block of the bootstrap report.
#[derive(Debug, Clone, Serialize)]
struct InitRepository {
    id: String,
    identity_source: String,
    path: String,
}

/// Record counts block of the bootstrap report.
#[derive(Debug, Clone, Serialize)]
struct InitRecordCounts {
    total: usize,
    per_domain: BTreeMap<String, usize>,
}

/// Semantic state block of the bootstrap report.
#[derive(Debug, Clone, Serialize)]
struct InitSemantics {
    present: bool,
    model: Option<InitEmbeddingModel>,
    skipped_reason: Option<String>,
}

/// Bounded embedding-model identity (issue #104's allow-listed fields).
#[derive(Debug, Clone, Serialize)]
struct InitEmbeddingModel {
    provider: String,
    name: String,
    version: String,
    dim: u32,
    content_hash: String,
}

/// Source snapshot block of the bootstrap report: the commit tip indexed and
/// whether the tree was dirty when scanned.
///
/// `head` is the bare commit SHA for Git repositories (`"no_git"` /
/// `"unborn_head"` when there is no commit to name, `"unknown"` when the
/// store carries no snapshot at all) — machine-readable without parsing.
#[derive(Debug, Clone, Serialize)]
struct InitSnapshot {
    head: String,
    dirty: bool,
}

/// Renders a [`SnapshotHead`] for the machine-readable bootstrap report: the
/// bare SHA for commits, the stable variant names otherwise.
#[cfg(feature = "embedded-aletheiadb")]
fn snapshot_head_string(head: &SnapshotHead) -> String {
    match head {
        SnapshotHead::Commit { sha } => sha.clone(),
        SnapshotHead::NoGit => "no_git".to_owned(),
        SnapshotHead::UnbornHead => "unborn_head".to_owned(),
    }
}

/// Whether (and how) the bootstrap generates semantic embeddings.
///
/// Embeddings are built by default (issue #229): the only way to a
/// structure-only store is an explicit `--no-embed`, a binary built without
/// the `embeddings` feature, or a generation failure — and every one of those
/// is reported in `semantics.skipped_reason`, never silent.
///
/// Generation covers the current-tree batch only: the history batch is
/// ingested structurally by design, mirroring the manual sequence whose
/// second `eg ingest` passes no `--embed`.
enum EmbedPlan {
    /// Generate embeddings for the tree batch with this resolved model name.
    Attempt { model: String },
    /// The tree batch was embedded; the history batch ingests structurally.
    TreeEmbedded,
    /// Do not generate embeddings; the reason is reported, never silent.
    Skip { reason: String },
}

/// Outcome of ingesting one JSONL graph into the embedded store.
struct BatchIngestOutcome {
    attempted: usize,
    succeeded: usize,
}

/// Read-only view of an existing store, used by the idempotency check, the
/// early identity-conflict refusal, and the final report. Read once per run.
struct ExistingStoreState {
    per_domain: BTreeMap<String, usize>,
    total: usize,
    /// Recorded embedding-model identities (empty when the store has no
    /// semantic index, or the binary lacks the `embeddings` feature).
    semantic_models: Vec<InitEmbeddingModel>,
    /// Recorded source snapshot for this repository, resolved with the same
    /// rules as `eg freshness --data-dir`.
    stored_snapshot: Option<crate::ir::SourceSnapshotPayload>,
    /// Whether the store holds a live `Commit` node for the repository's
    /// current HEAD tip: `None` when there is no commit tip to check for
    /// (non-Git directory, unborn HEAD, git unavailable), `Some(present)`
    /// otherwise. A previous run whose history stages were skipped or failed
    /// leaves the tree indexed but this `false` — the idempotency probe must
    /// not no-op on such a store.
    history_tip_present: Option<bool>,
    snapshot: Option<InitSnapshot>,
}

/// Handles `eg init <repo> --data-dir <dir> [--format json|text]`.
///
/// Runs the current-tree scan, embedded ingest with semantic embeddings, Git
/// history replay, and history ingest end-to-end into one embedded store —
/// the one-command equivalent of the documented multi-step sequence — then
/// prints the bootstrap report. See the module docs for the exit-code
/// contract.
/// Stages 3-4 of the bootstrap: replay Git history read-only (best-effort),
/// then ingest the history graph structurally. A failed history ingest is
/// fatal like any required stage; a skipped or failed replay leaves the
/// tree store queryable and is reported honestly.
#[cfg(feature = "embedded-aletheiadb")]
#[allow(clippy::too_many_arguments)]
fn run_history_stages(
    args: &InitArgs,
    data_dir: &Path,
    identity: &identity::RepositoryIdentity,
    scan_args: &ResolvedScanArgs,
    tree_jsonl: &Path,
    history_jsonl: &Path,
    head_sha: Option<&str>,
    stages: &mut Vec<StageReport>,
    embed_plan: &mut EmbedPlan,
) -> Result<()> {
    // Stage 3: Git history replay (best-effort). The `eg scan-history` lane
    // function reads Git objects read-only, so the working tree and Git state
    // are never mutated; a preflight failure (not a git repo, no history)
    // skips the stage with a recorded reason instead of failing the bootstrap.
    eprintln!("init: replaying Git history…");
    let history_scanned = match git_history_preflight(&args.repo_path) {
        Ok(()) => match scan_history(&args.repo_path, history_jsonl, scan_args, None) {
            Ok(()) => {
                // The history batch stamps its own Repository snapshot; align
                // it to the tree scan's snapshot before ingest so the store's
                // snapshot describes the tree that was actually scanned (see
                // `align_history_snapshot_to_tree`).
                if let Err(error) = align_history_snapshot_to_tree(tree_jsonl, history_jsonl) {
                    stages.push(StageReport::failed(
                        "scan_history",
                        format!("snapshot alignment failed: {error:#}"),
                    ));
                    false
                } else {
                    let history_records = count_jsonl_records(history_jsonl)?;
                    stages.push(StageReport::completed("scan_history", history_records));
                    true
                }
            }
            Err(error) => {
                stages.push(StageReport::failed("scan_history", format!("{error:#}")));
                false
            }
        },
        Err(preflight) => {
            stages.push(StageReport::skipped(
                "scan_history",
                format!("{}: {}", preflight.code, preflight.message),
            ));
            false
        }
    };

    // Stage 4: embedded ingest of the history graph (required when the replay
    // produced one) — the in-process equivalent of the second `eg ingest`.
    // The history batch is structural by design: the manual sequence's second
    // ingest passes no `--embed`, so the embed plan stays `TreeEmbedded` (or
    // the recorded `Skip` when tree generation failed or was declined).
    if history_scanned {
        eprintln!("init: ingesting Git history…");
        match ingest_graph_batch(history_jsonl, data_dir, "history", embed_plan) {
            Ok(outcome) => stages.push(StageReport::completed_ingest(
                "ingest_history",
                outcome.attempted,
                outcome.succeeded,
            )),
            Err(error) => {
                return fail_init(
                    &args.repo_path,
                    data_dir,
                    identity,
                    args.repo_id_override.as_deref(),
                    head_sha,
                    std::mem::take(stages),
                    "ingest_history",
                    &error,
                    embed_plan,
                    args.format,
                );
            }
        }
    } else {
        stages.push(StageReport::skipped(
            "ingest_history",
            "scan_history did not produce a graph".to_owned(),
        ));
    }
    Ok(())
}

/// Stages 1-2 of the bootstrap: scan the current tree, then ingest it
/// (embedding by default). A failed required stage prints the partial report
/// and returns the error (exit 1); the caller does not continue.
#[cfg(feature = "embedded-aletheiadb")]
#[allow(clippy::too_many_arguments)]
fn run_tree_stages(
    args: &InitArgs,
    data_dir: &Path,
    identity: &identity::RepositoryIdentity,
    scan_args: &ResolvedScanArgs,
    tree_jsonl: &Path,
    head_sha: Option<&str>,
    stages: &mut Vec<StageReport>,
    embed_plan: &mut EmbedPlan,
) -> Result<()> {
    // Stage 1: current-tree scan (required). Calls the `eg scan` lane
    // function directly — same flags, same JSONL contract.
    eprintln!("init: scanning repository tree…");
    if let Err(error) = scan(&args.repo_path, tree_jsonl, scan_args) {
        stages.push(StageReport::failed("scan_tree", format!("{error:#}")));
        let report = final_report(
            &args.repo_path,
            data_dir,
            identity,
            args.repo_id_override.as_deref(),
            head_sha,
            InitStatus::Failed,
            std::mem::take(stages),
            embed_plan,
        )?;
        print_report(&report, args.format)?;
        return Err(error).context("eg init: scan_tree stage failed");
    }
    let tree_records = count_jsonl_records(tree_jsonl)?;
    stages.push(StageReport::completed("scan_tree", tree_records));

    // Stage 2: embedded ingest of the tree (required), with embeddings by
    // default — the in-process equivalent of
    // `eg ingest <tree> --adapter embedded --data-dir <dir> --embed`.
    eprintln!("init: ingesting repository tree…");
    match ingest_graph_batch(tree_jsonl, data_dir, "tree", embed_plan) {
        Ok(outcome) => {
            stages.push(StageReport::completed_ingest(
                "ingest_tree",
                outcome.attempted,
                outcome.succeeded,
            ));
            // The tree batch is the only one ever embedded: the history batch
            // below ingests structurally by design, mirroring the manual
            // sequence whose second `eg ingest` passes no `--embed`. A failed
            // tree-batch generation already recorded its own skip reason.
            if matches!(embed_plan, EmbedPlan::Attempt { .. }) {
                *embed_plan = EmbedPlan::TreeEmbedded;
            }
        }
        Err(error) => {
            return fail_init(
                &args.repo_path,
                data_dir,
                identity,
                args.repo_id_override.as_deref(),
                head_sha,
                std::mem::take(stages),
                "ingest_tree",
                &error,
                embed_plan,
                args.format,
            );
        }
    }
    Ok(())
}

/// Converges to a no-op when the store already reflects this working tree,
/// or refuses before the expensive scan when the store's vector index was
/// built by a different embedding model (issue #104). Both paths leave
/// through process exit; otherwise returns normally.
#[cfg(feature = "embedded-aletheiadb")]
fn probe_idempotency_and_refuse(
    args: &InitArgs,
    data_dir: &Path,
    identity: &identity::RepositoryIdentity,
    head_sha: Option<&str>,
    embed_plan: &EmbedPlan,
    existing: Option<&ExistingStoreState>,
) -> Result<()> {
    // Idempotency (issue #229): when the store already reflects this working
    // tree at the requested semantic state, converge to a no-op instead of
    // rebuilding.
    if let Some(report) = check_already_current(
        &args.repo_path,
        data_dir,
        identity,
        head_sha,
        embed_plan,
        existing,
    ) {
        print_report(&report, args.format)?;
        process::exit(INIT_ALREADY_CURRENT_EXIT_CODE);
    }

    // Refuse early (before the expensive scan) when the store's vector index
    // was built by a different model than the one init would embed with:
    // blending two vector spaces is never allowed (issue #104), and the
    // remedy is a fresh `--data-dir`.
    if let EmbedPlan::Attempt { model } = embed_plan
        && let Some(state) = existing
        && !state.semantic_models.is_empty()
        && !state.semantic_models.iter().any(|m| m.name == *model)
    {
        let built_by = state
            .semantic_models
            .iter()
            .map(|m| m.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        refuse_init(
            data_dir,
            "embedding_index_identity_conflict",
            &format!(
                "this store's semantic vector index was built by a different embedding model \
                 ({built_by}); embedding with {model} would leave the index holding vectors \
                 from both models, which no ranking can compare"
            ),
        );
    }
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn init_cmd(args: &InitArgs) -> Result<()> {
    if !args.repo_path.is_dir() {
        anyhow::bail!(
            "repository path is not a directory: {}",
            args.repo_path.display()
        );
    }
    warn_on_unconsumed_scope_pins();

    let data_dir = resolve_data_dir(args.data_dir.as_deref());
    let scan_args = resolve_scan_args(args.repo_id_override.clone(), args.raw_literals);
    let identity =
        identity::compute_repository_identity(&args.repo_path, args.repo_id_override.as_deref());

    #[cfg(feature = "embeddings")]
    let mut embed_plan = if args.no_embed {
        EmbedPlan::Skip {
            reason: "operator passed --no-embed".to_owned(),
        }
    } else {
        let (model, _) = resolve_embed_model(args.embed_model.clone());
        EmbedPlan::Attempt { model }
    };
    #[cfg(not(feature = "embeddings"))]
    let mut embed_plan = EmbedPlan::Skip {
        reason: "binary built without the embeddings feature".to_owned(),
    };

    // Read the existing store once (if any): the idempotency probe and the
    // early identity-conflict refusal below both need it. The current commit
    // tip is resolved read-only first so the single store pass can also note
    // whether history was ever completed for this tip.
    let head_sha = read_only_head_sha(&args.repo_path);
    let existing = read_existing_store(
        &data_dir,
        &identity,
        args.repo_id_override.as_deref(),
        head_sha.as_deref(),
    )?;

    probe_idempotency_and_refuse(
        args,
        &data_dir,
        &identity,
        head_sha.as_deref(),
        &embed_plan,
        existing.as_ref(),
    )?;

    let mut stages: Vec<StageReport> = Vec::new();
    let workdir = tempfile::tempdir().context("failed to create init working directory")?;
    let tree_jsonl = workdir.path().join("init-tree.graph.jsonl");
    let history_jsonl = workdir.path().join("init-history.graph.jsonl");

    run_tree_stages(
        args,
        &data_dir,
        &identity,
        &scan_args,
        &tree_jsonl,
        head_sha.as_deref(),
        &mut stages,
        &mut embed_plan,
    )?;

    run_history_stages(
        args,
        &data_dir,
        &identity,
        &scan_args,
        &tree_jsonl,
        &history_jsonl,
        head_sha.as_deref(),
        &mut stages,
        &mut embed_plan,
    )?;

    // An optional stage that was skipped or failed still leaves a queryable
    // store, but the bootstrap is honestly `partial`, not `rebuilt` — the
    // report names every stage either way. The same holds when semantic
    // embedding was requested but unavailable: embedding generation failed,
    // or this binary was built without the embeddings feature. An explicit
    // `--no-embed` is an operator choice, not a degradation, so it still
    // reports `rebuilt`.
    #[cfg(feature = "embeddings")]
    let embed_unavailable = !args.no_embed && matches!(embed_plan, EmbedPlan::Skip { .. });
    #[cfg(not(feature = "embeddings"))]
    let embed_unavailable = true;
    let status = if stages.iter().any(|s| s.status != StageStatus::Completed) || embed_unavailable {
        InitStatus::Partial
    } else {
        InitStatus::Rebuilt
    };
    let report = final_report(
        &args.repo_path,
        &data_dir,
        &identity,
        args.repo_id_override.as_deref(),
        head_sha.as_deref(),
        status,
        stages,
        &embed_plan,
    )?;
    print_report(&report, args.format)?;
    Ok(())
}

/// Machine-readable preflight outcome for the Git history stage.
struct HistoryPreflight {
    code: &'static str,
    message: String,
}

/// Mirrors the three preflight checks at the top of [`scan_history`]
/// (git available, is a git repo, history readable) with identical diagnostic
/// codes — but returns them instead of exiting, so `eg init` can record the
/// skip and keep bootstrapping the tree store (issue #229's partial-failure
/// contract). `scan_history` is only called after this passes, so its own
/// `process::exit(2)` preflights are unreachable from `init`.
///
/// The history-window validation inside `scan_history` is not mirrored: `init`
/// passes no window flags, and the default window always resolves.
#[cfg(feature = "embedded-aletheiadb")]
fn git_history_preflight(repo_path: &Path) -> std::result::Result<(), HistoryPreflight> {
    let git_available = process::Command::new("git")
        .arg("--version")
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !git_available {
        return Err(HistoryPreflight {
            code: "git_unavailable",
            message: "git command not found in PATH".to_owned(),
        });
    }

    let is_git_repo = process::Command::new("git")
        .args(["-c", "core.excludesFile="])
        .arg("-C")
        .arg(repo_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["rev-parse", "--git-dir"])
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !is_git_repo {
        return Err(HistoryPreflight {
            code: "not_a_git_repository",
            message: format!("path is not a git repository: {}", repo_path.display()),
        });
    }

    let git_history_readable = process::Command::new("git")
        .args(["-c", "core.excludesFile="])
        .arg("-C")
        .arg(repo_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["log", "-1"])
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !git_history_readable {
        return Err(HistoryPreflight {
            code: "git_history_unreadable",
            message: "git history is not readable (e.g. repository has no commits)".to_owned(),
        });
    }

    Ok(())
}

/// The idempotency probe (issue #229): returns `Some` report when the store is
/// already current — the recorded source snapshot classifies `fresh` against
/// the live working tree, the requested semantic state is present, and (for
/// repositories with a commit tip) the store holds a live `Commit` node for
/// the current HEAD — so the caller can converge to a no-op. Returns `None`
/// on a cold start or when any check misses, in which case the caller
/// rebuilds.
///
/// `head_sha` is the repository's current commit tip (see
/// [`read_only_head_sha`]); `None` for non-Git directories, unborn HEADs, or
/// git unavailable, where history replay has nothing to index and the tip
/// check is skipped.
#[cfg(feature = "embedded-aletheiadb")]
fn check_already_current(
    repo_path: &Path,
    data_dir: &Path,
    identity: &identity::RepositoryIdentity,
    head_sha: Option<&str>,
    embed_plan: &EmbedPlan,
    existing: Option<&ExistingStoreState>,
) -> Option<InitReport> {
    let state = existing?;

    // Mirror `eg freshness --data-dir`: the same store-artifact exclusions
    // keep a just-written in-tree store from reading as `stale_dirty`.
    let exclusions = store_exclusions_including_egregore(repo_path, &[Some(data_dir)]);
    let (current_head, current_dirty) =
        identity::working_tree_snapshot_excluding(repo_path, &exclusions);
    if !freshness::classify(state.stored_snapshot.as_ref(), &current_head, current_dirty).is_fresh()
    {
        return None;
    }

    // The requested semantic state must be present: with embeddings requested,
    // the store must carry a vector-index identity for the resolved model (a
    // same-name check here; the full write-time identity gate still runs on
    // rebuild). With `--no-embed` (or no embeddings feature) a structural
    // store is the requested end state.
    let semantics_ok = match embed_plan {
        // Unreachable before any ingest runs; kept exhaustive by construction.
        EmbedPlan::Skip { .. } | EmbedPlan::TreeEmbedded => true,
        EmbedPlan::Attempt { model } => state.semantic_models.iter().any(|m| m.name == *model),
    };
    if !semantics_ok {
        return None;
    }

    // History completeness: a fresh snapshot with matching semantics is not
    // enough. A previous run whose history stages were skipped or failed
    // leaves the tree fully indexed but temporal history absent — converging
    // to a no-op there would report `already_current` over a store that can
    // never grow its history. For repositories with a commit tip, require a
    // live `Commit` node for the current HEAD; otherwise rebuild so history
    // gets another chance. (The lookup is read-only: `git rev-parse HEAD`
    // never touches the working tree or Git state.)
    if head_sha.is_some() && !state.history_tip_present.unwrap_or(false) {
        eprintln!("init: store has no history for the current HEAD tip; rebuilding…");
        return None;
    }

    let semantics = semantics_block(state, embed_plan);
    // The tip guard above guarantees a live Commit node for the current
    // HEAD whenever the repository has a commit tip. Without a tip
    // (non-Git directory, unborn HEAD) history replay had nothing to
    // index, so the store is honestly partial_history, not complete.
    let store_state = if !semantics.present && semantics.skipped_reason.is_some() {
        StoreState::StructuralOnly
    } else if head_sha.is_some() {
        StoreState::Complete
    } else {
        StoreState::PartialHistory
    };

    Some(InitReport {
        command: "init",
        status: InitStatus::AlreadyCurrent,
        repository: repository_block(identity, repo_path),
        data_dir: data_dir.display().to_string(),
        stages: Vec::new(),
        records: InitRecordCounts {
            total: state.total,
            per_domain: state.per_domain.clone(),
        },
        semantics,
        snapshot: state.snapshot.clone().unwrap_or_else(|| InitSnapshot {
            head: snapshot_head_string(&current_head),
            dirty: current_dirty,
        }),
        store_state,
    })
}

/// Gives the history batch the current-tree snapshot before ingest.
///
/// The history replay stamps its own Repository snapshot (`dirty=false`,
/// committed HEAD state) so a standalone history graph reads as
/// freshness-clean. But `eg init` merges the history batch into the same
/// store as the tree batch, and the embedded sink upserts the Repository
/// node — the history snapshot would supersede the tree's, misreporting a
/// dirty working tree as clean in the bootstrap report and in later
/// `eg freshness` checks. Worse, reverting the uncommitted edits afterwards
/// would then classify `fresh` (stored clean, tree clean, same HEAD) and
/// converge to a false `already_current` over records built from the
/// reverted edits. Rewriting the history batch's Repository record to carry
/// the tree scan's snapshot keeps the store's snapshot describing the tree
/// that was actually scanned.
#[cfg(feature = "embedded-aletheiadb")]
fn align_history_snapshot_to_tree(tree_jsonl: &Path, history_jsonl: &Path) -> Result<()> {
    let tree_records = load_records_from_jsonl(tree_jsonl)?;
    let tree_snapshot = tree_records
        .iter()
        .find_map(|record| match record {
            GraphRecord::Node {
                kind: NodeKind::Repository,
                source_snapshot: Some(snapshot),
                ..
            } => Some(snapshot.as_ref().clone()),
            _ => None,
        })
        .context("tree graph should stamp a source snapshot on its Repository record")?;
    let mut history_records = load_records_from_jsonl(history_jsonl)?;
    let mut aligned = 0;
    for record in &mut history_records {
        if let GraphRecord::Node {
            kind: NodeKind::Repository,
            source_snapshot,
            ..
        } = record
        {
            *source_snapshot = Some(Box::new(tree_snapshot.clone()));
            aligned += 1;
        }
    }
    if aligned == 0 {
        anyhow::bail!("history graph contains no Repository record to align");
    }
    let mut jsonl = String::new();
    for record in &history_records {
        jsonl.push_str(
            &serde_json::to_string(record).context("failed to serialize aligned history record")?,
        );
        jsonl.push('\n');
    }
    fs::write(history_jsonl, jsonl).context("failed to write aligned history graph")?;
    Ok(())
}

/// Resolves the repository's current commit tip without touching the working
/// tree or Git state (`git rev-parse HEAD` is read-only plumbing).
/// Returns `None` for non-Git directories, unborn HEADs, or when git is
/// unavailable — all cases where history replay has nothing to index.
#[cfg(feature = "embedded-aletheiadb")]
fn read_only_head_sha(repo_path: &Path) -> Option<String> {
    let output = process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["rev-parse", "HEAD"])
        .stdin(process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim().to_owned();
    if sha.is_empty() { None } else { Some(sha) }
}

/// Read-only view of an existing store, or `None` when the data dir holds no
/// store yet (cold start: missing or empty directory).
///
/// The recorded source snapshot is resolved with the same rules as
/// `eg freshness --data-dir`: an explicit `--repo-id-override` requires an
/// exact identity match; otherwise the single-repository fallback applies.
///
/// `head_sha` is the repository's current commit tip (see
/// [`read_only_head_sha`]); while the records stream past once, the loader
/// also notes whether a live `Commit` node names that tip, so the
/// idempotency probe can tell a history-complete store from one whose
/// history stages were skipped or failed on a previous run.
#[cfg(feature = "embedded-aletheiadb")]
fn read_existing_store(
    data_dir: &Path,
    identity: &identity::RepositoryIdentity,
    repo_id_override: Option<&str>,
    head_sha: Option<&str>,
) -> Result<Option<ExistingStoreState>> {
    let has_store = fs::read_dir(data_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    if !has_store {
        return Ok(None);
    }
    let records = load_records_from_data_dir_readonly(data_dir)?;
    let mut per_domain: BTreeMap<String, usize> = BTreeMap::new();
    let mut history_tip_present = false;
    for record in &records {
        // The report (and docs/cli/init.md) names the code-graph domain
        // "code"; the schema-internal domain string is "codegraph".
        let domain: String = match record_version(record).domain.as_str() {
            "codegraph" => "code".to_owned(),
            other => other.to_owned(),
        };
        *per_domain.entry(domain).or_insert(0) += 1;
        if let (
            Some(sha),
            GraphRecord::Node {
                kind: NodeKind::Commit,
                name: Some(name),
                ..
            },
        ) = (head_sha, record)
            && sha == name.as_str()
        {
            history_tip_present = true;
        }
    }
    #[cfg(feature = "embeddings")]
    let semantic_models = crate::embeddings::indexed_identities(&records)
        .into_iter()
        .map(|model| InitEmbeddingModel {
            provider: model.provider.clone(),
            name: model.name.clone(),
            version: model.version.clone(),
            dim: model.dim,
            content_hash: model.content_hash,
        })
        .collect::<Vec<_>>();
    #[cfg(not(feature = "embeddings"))]
    let semantic_models = Vec::new();

    let stored_snapshot = if repo_id_override.is_some() {
        freshness::stored_snapshot_exact(&records, &identity.id).cloned()
    } else {
        freshness::stored_snapshot_with_owner(&records, &identity.id)
            .or_else(|| freshness::stored_snapshot_sole_stamped(&records))
            .map(|(_, snapshot)| snapshot.clone())
    };
    let snapshot = stored_snapshot.as_ref().map(|snapshot| InitSnapshot {
        head: snapshot_head_string(&snapshot.head),
        dirty: snapshot.dirty,
    });

    Ok(Some(ExistingStoreState {
        total: records.len(),
        per_domain,
        semantic_models,
        stored_snapshot,
        history_tip_present: head_sha.map(|_| history_tip_present),
        snapshot,
    }))
}

/// Ingests one JSONL graph into the embedded store, mirroring
/// `eg ingest <graph> --adapter embedded --data-dir <dir> [--embed]` exactly:
/// capacity preflight, embedding generation, the issue #104 identity-conflict
/// refusal, the identity record, quarantine policy, persist.
///
/// A generation failure does not fail the batch: per issue #229 the bootstrap
/// continues structurally and records the reason in the embed plan (and the
/// final report) rather than silently producing a structure-only store.
/// Whether embedding with `model` would blend two vector spaces: the store
/// already carries a vector-index identity that is not exactly this model.
/// Compares the full identity (provider, name, version, dimensions, weight
/// hash) — a reused model name with drifted weights still conflicts.
#[cfg(feature = "embedded-aletheiadb")]
fn embedding_identity_conflicts(
    indexed: &[crate::ir::EmbeddingModel],
    model: &crate::ir::EmbeddingModel,
) -> bool {
    !(indexed.is_empty() || indexed.len() == 1 && indexed[0] == *model)
}

#[cfg(feature = "embedded-aletheiadb")]
fn ingest_graph_batch(
    graph: &Path,
    data_dir: &Path,
    batch: &'static str,
    embed_plan: &mut EmbedPlan,
) -> Result<BatchIngestOutcome> {
    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    #[cfg_attr(not(feature = "embeddings"), allow(unused_mut))]
    let mut records = records_from_jsonl(&jsonl).context("failed to parse graph JSONL")?;

    // Capacity preflight (issue #439): refuse fast BEFORE opening the store,
    // exactly like `eg ingest`. Nothing has been written at this point.
    if let Err(refusal) = check_ingest_capacity(&records, false) {
        refuse_init(
            data_dir,
            INGEST_CAPACITY_EXCEEDED_CODE,
            &format!(
                "ingest would overflow AletheiaDB's process-global string-interner cap of {} entries",
                refusal.limit,
            ),
        );
    }

    #[cfg(feature = "embeddings")]
    let mut sink = match embed_plan {
        EmbedPlan::Attempt { model } => match generate_embeddings(&records, model) {
            Ok((vectors, dimensions, embedding_model)) => {
                let sink =
                    EmbeddedAletheiaSink::open_with_embeddings(data_dir, vectors, dimensions)
                        .map_err(|error| embedded_write_open_error(data_dir, error))?;
                // Refuse before writing anything when the index was built by
                // a different model (issue #104): the early name check cannot
                // see version, dimension, or weight-hash drift under a reused
                // model name, so the full identity gate runs here against the
                // just-loaded model. A conflict is a pre-write refusal
                // (exit 2), like the capacity preflight above — never a
                // stage failure. (Mirrors the condition in
                // `refuse_conflicting_index_identity`, whose ingest-style
                // envelope init does not print: init reports refusals through
                // its own envelope.)
                let indexed = sink
                    .read_all_records()
                    .map_err(|error| anyhow::anyhow!("failed to read from embedded store: {error}"))
                    .map(|records| crate::embeddings::indexed_identities(&records))?;
                if embedding_identity_conflicts(&indexed, &embedding_model) {
                    let built_by = indexed
                        .iter()
                        .map(|existing| {
                            format!(
                                "{} (version {}, {} dims)",
                                existing.name, existing.version, existing.dim
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    refuse_init(
                        data_dir,
                        "embedding_index_identity_conflict",
                        &format!(
                            "this store's semantic vector index was built by a different embedding model \
                             ({built_by}); embedding with {} would leave the index holding vectors from \
                             both models, which no ranking can compare",
                            embedding_model.name,
                        ),
                    );
                }
                records.push(crate::embeddings::embedding_index_identity_record(
                    &embedding_model,
                ));
                sink
            }
            Err(error) => {
                let reason = format!("embedding generation failed for {batch} batch: {error:#}");
                eprintln!("init: {reason}; continuing with a structural ingest");
                *embed_plan = EmbedPlan::Skip { reason };
                EmbeddedAletheiaSink::open(data_dir)
                    .map_err(|error| embedded_write_open_error(data_dir, error))?
            }
        },
        EmbedPlan::Skip { .. } | EmbedPlan::TreeEmbedded => EmbeddedAletheiaSink::open(data_dir)
            .map_err(|error| embedded_write_open_error(data_dir, error))?,
    };
    #[cfg(not(feature = "embeddings"))]
    let mut sink = {
        let _ = (batch, embed_plan);
        EmbeddedAletheiaSink::open(data_dir)
            .map_err(|error| embedded_write_open_error(data_dir, error))?
    };

    let report =
        ingest_records_with_policy(&records, &mut sink, DanglingCitationPolicy::Quarantine);
    // A capacity overflow surfaced as a per-record write failure is fatal
    // (never a generic failure): a partial store whose interner is at the cap
    // cannot be persisted — same rule as `eg ingest`.
    if let Some(failure) = report
        .failures
        .iter()
        .find(|failure| crate::adapters::is_string_interner_capacity_error(&failure.message))
    {
        refuse_init(
            data_dir,
            INGEST_CAPACITY_EXCEEDED_CODE,
            &format!(
                "AletheiaDB reached its string-interner cap during write: {}",
                failure.message
            ),
        );
    }
    if !report.is_success() {
        for failure in &report.failures {
            eprintln!("{}: {}", failure.record_id, failure.message);
        }
        anyhow::bail!("ingest failed for {} records", report.failed);
    }
    sink.persist_indexes()
        .with_context(|| format!("failed to persist embedded store {}", data_dir.display()))?;

    Ok(BatchIngestOutcome {
        attempted: report.attempted,
        succeeded: report.succeeded,
    })
}

/// Prints the machine-readable refusal envelope on stdout and exits with
/// [`INIT_REFUSED_EXIT_CODE`]. Nothing has been written at this point, so the
/// store is untouched.
#[cfg(feature = "embedded-aletheiadb")]
fn refuse_init(data_dir: &Path, code: &str, message: &str) -> ! {
    let envelope = serde_json::json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message,
            "data_dir": data_dir.display().to_string(),
            "remedy": "use a fresh --data-dir to re-bootstrap",
        },
    });
    println!("{envelope}");
    eprintln!("eg init refused: {message}");
    process::exit(INIT_REFUSED_EXIT_CODE);
}

/// Records a failed required stage, prints the bootstrap report with
/// `status: "failed"`, and returns the error (the caller propagates it, so
/// the process exits 1).
#[cfg(feature = "embedded-aletheiadb")]
#[allow(clippy::too_many_arguments)]
fn fail_init(
    repo_path: &Path,
    data_dir: &Path,
    identity: &identity::RepositoryIdentity,
    repo_id_override: Option<&str>,
    head_sha: Option<&str>,
    mut stages: Vec<StageReport>,
    stage_name: &'static str,
    error: &anyhow::Error,
    embed_plan: &EmbedPlan,
    format: OutputFormat,
) -> Result<()> {
    stages.push(StageReport::failed(stage_name, format!("{error:#}")));
    let report = final_report(
        repo_path,
        data_dir,
        identity,
        repo_id_override,
        head_sha,
        InitStatus::Failed,
        stages,
        embed_plan,
    )?;
    print_report(&report, format)?;
    Err(anyhow::anyhow!(
        "eg init: {stage_name} stage failed: {error:#}"
    ))
}

/// Builds the final bootstrap report by reading back the store that was
/// built (read-only), so the reported counts describe the actual store.
#[cfg(feature = "embedded-aletheiadb")]
#[allow(clippy::too_many_arguments)]
fn final_report(
    repo_path: &Path,
    data_dir: &Path,
    identity: &identity::RepositoryIdentity,
    repo_id_override: Option<&str>,
    head_sha: Option<&str>,
    status: InitStatus,
    stages: Vec<StageReport>,
    embed_plan: &EmbedPlan,
) -> Result<InitReport> {
    let state = read_existing_store(data_dir, identity, repo_id_override, head_sha)?.unwrap_or(
        ExistingStoreState {
            total: 0,
            per_domain: BTreeMap::new(),
            semantic_models: Vec::new(),
            stored_snapshot: None,
            history_tip_present: None,
            snapshot: None,
        },
    );
    let history_incomplete = stages.iter().any(|s| {
        (s.name == "scan_history" || s.name == "ingest_history")
            && s.status != StageStatus::Completed
    });
    let semantics = semantics_block(&state, embed_plan);
    // A failed history ingest leaves the tree store complete but temporal
    // history absent or incomplete — even when the history scan itself
    // completed.
    let store_state = if !semantics.present && semantics.skipped_reason.is_some() {
        StoreState::StructuralOnly
    } else if history_incomplete {
        StoreState::PartialHistory
    } else {
        StoreState::Complete
    };
    Ok(InitReport {
        command: "init",
        status,
        repository: repository_block(identity, repo_path),
        data_dir: data_dir.display().to_string(),
        stages,
        records: InitRecordCounts {
            total: state.total,
            per_domain: state.per_domain,
        },
        semantics,
        snapshot: state.snapshot.unwrap_or_else(|| InitSnapshot {
            head: "unknown".to_owned(),
            dirty: false,
        }),
        store_state,
    })
}

/// Builds the report's repository identity block.
#[cfg(feature = "embedded-aletheiadb")]
fn repository_block(identity: &identity::RepositoryIdentity, repo_path: &Path) -> InitRepository {
    let identity_source = serde_json::to_value(identity.payload.identity_source)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    InitRepository {
        id: identity.id.clone(),
        identity_source,
        path: repo_path.display().to_string(),
    }
}

/// Builds the report's semantics block from the store's recorded
/// vector-index identities and the embed plan.
#[cfg(feature = "embedded-aletheiadb")]
fn semantics_block(state: &ExistingStoreState, embed_plan: &EmbedPlan) -> InitSemantics {
    let present = !state.semantic_models.is_empty();
    let skipped_reason = match embed_plan {
        EmbedPlan::Attempt { .. } => {
            if present {
                None
            } else {
                Some("embeddings requested but no vector-index identity was recorded".to_owned())
            }
        }
        // The tree batch was embedded this run; the history batch is
        // structural by design, so there is no skip to report.
        EmbedPlan::TreeEmbedded => None,
        EmbedPlan::Skip { reason } => Some(reason.clone()),
    };
    InitSemantics {
        present,
        model: state.semantic_models.first().cloned(),
        skipped_reason,
    }
}

/// Counts non-empty lines in a JSONL file (one record per line).
#[cfg(feature = "embedded-aletheiadb")]
fn count_jsonl_records(path: &Path) -> Result<usize> {
    let jsonl = fs::read_to_string(path)
        .with_context(|| format!("failed to read graph JSONL from {}", path.display()))?;
    Ok(jsonl.lines().filter(|line| !line.trim().is_empty()).count())
}

/// Prints the bootstrap report: pretty JSON for `--format json`, stable
/// human-readable lines for `--format text`.
#[cfg(feature = "embedded-aletheiadb")]
fn print_report(report: &InitReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).context("failed to serialize init report")?
            );
        }
        OutputFormat::Text => {
            println!("init: {}", report.status.as_str());
            println!(
                "repository: {} ({})",
                report.repository.id, report.repository.identity_source
            );
            println!("data_dir: {}", report.data_dir);
            for stage in &report.stages {
                use std::fmt::Write as _;
                let mut line = format!("stage {}: {}", stage.name, stage.status.as_str());
                if let Some(records) = stage.records {
                    let _ = write!(line, " (records: {records}");
                    if let Some(succeeded) = stage.succeeded {
                        let _ = write!(line, ", succeeded: {succeeded}");
                    }
                    line.push(')');
                }
                if let Some(detail) = &stage.detail {
                    let _ = write!(line, " — {detail}");
                }
                println!("{line}");
            }
            println!("records: {} total", report.records.total);
            for (domain, count) in &report.records.per_domain {
                println!("  domain {domain}: {count}");
            }
            if report.semantics.present {
                match &report.semantics.model {
                    Some(model) => println!(
                        "semantics: present ({} {}, dim={})",
                        model.provider, model.name, model.dim
                    ),
                    None => println!("semantics: present"),
                }
            } else {
                println!("semantics: absent");
            }
            if let Some(reason) = &report.semantics.skipped_reason {
                println!("semantics_skipped_reason: {reason}");
            }
            println!(
                "snapshot: {} (dirty: {})",
                report.snapshot.head, report.snapshot.dirty
            );
            println!("store_state: {}", report.store_state.as_str());
        }
    }
    Ok(())
}

/// Anti-drift: `eg init` is neither a query lane nor a capture lane, so it
/// carries its own doc-coverage contract (issue #229): `docs/cli/init.md`
/// must exist and must name the `init` command.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_doc_exists_and_names_the_lane() {
        let doc = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/cli/init.md");
        let body = std::fs::read_to_string(&doc)
            .unwrap_or_else(|_| panic!("docs/cli/init.md must exist for the `init` command"));
        assert!(
            body.contains("eg init"),
            "docs/cli/init.md must document the `eg init` command"
        );
    }

    #[test]
    fn init_exit_codes_are_documented() {
        let doc = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/cli/init.md");
        let body = std::fs::read_to_string(&doc).expect("docs/cli/init.md must exist");
        let lowered = body.to_lowercase();
        for needle in ["already current", "exit code", "exit 3"] {
            assert!(
                lowered.contains(needle),
                "docs/cli/init.md must document the already-current exit code"
            );
        }
        assert_eq!(
            INIT_ALREADY_CURRENT_EXIT_CODE, 3,
            "the already-current exit code is a stable contract"
        );
        assert_eq!(
            INIT_REFUSED_EXIT_CODE, 2,
            "the refused-before-writing exit code is a stable contract"
        );
    }

    fn fake_model(
        name: &str,
        version: &str,
        dim: u32,
        content_hash: &str,
    ) -> crate::ir::EmbeddingModel {
        crate::ir::EmbeddingModel {
            provider: "test-provider".to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
            dim,
            content_hash: content_hash.to_owned(),
        }
    }

    /// The write-time identity gate compares the full model identity, not
    /// just the name: a reused model name with drifted version, dimensions,
    /// or weights must still refuse (issue #104), and every such refusal is
    /// a pre-write exit-2 refusal, never a generic failure.
    #[test]
    fn init_identity_gate_compares_full_model_identity() {
        let current = fake_model("minilm", "1", 384, "hash-a");
        // No index yet: nothing to conflict with.
        assert!(!embedding_identity_conflicts(&[], &current));
        // The exact same identity: no conflict.
        assert!(!embedding_identity_conflicts(
            &[fake_model("minilm", "1", 384, "hash-a")],
            &current
        ));
        // Same name, drifted version: conflict.
        assert!(embedding_identity_conflicts(
            &[fake_model("minilm", "2", 384, "hash-a")],
            &current
        ));
        // Same name, drifted dimensions: conflict.
        assert!(embedding_identity_conflicts(
            &[fake_model("minilm", "1", 768, "hash-a")],
            &current
        ));
        // Same name and version, drifted weights: conflict.
        assert!(embedding_identity_conflicts(
            &[fake_model("minilm", "1", 384, "hash-b")],
            &current
        ));
        // A different model name: conflict.
        assert!(embedding_identity_conflicts(
            &[fake_model("other", "1", 384, "hash-a")],
            &current
        ));
        // More than one indexed identity: conflict.
        assert!(embedding_identity_conflicts(
            &[
                fake_model("minilm", "1", 384, "hash-a"),
                fake_model("minilm", "1", 384, "hash-a"),
            ],
            &current
        ));
    }
}
