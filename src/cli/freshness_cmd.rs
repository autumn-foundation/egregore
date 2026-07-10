use super::*;

/// Machine-readable report emitted by `eg freshness`.
///
/// `freshness` carries the stable code (`fresh` / `stale_head` / `stale_dirty` /
/// `unknown`); `current_head` and `stored_snapshot` reuse the on-disk snapshot
/// serialization so the report is self-describing.
#[derive(Debug, Serialize)]
pub(crate) struct FreshnessReport {
    /// Stable freshness code.
    freshness: String,
    /// Convenience boolean: `true` only when `freshness == "fresh"`.
    fresh: bool,
    /// Stable `Repository` record ID the freshness was computed for.
    repository_id: String,
    /// Where the store was read from: `"graph"` or `"data_dir"`.
    store_kind: String,
    /// Current working-tree HEAD state.
    current_head: SnapshotHead,
    /// Current working-tree dirty flag.
    current_dirty: bool,
    /// The snapshot the store was built from; absent for pre-stamping stores.
    #[serde(skip_serializing_if = "Option::is_none")]
    stored_snapshot: Option<crate::ir::SourceSnapshotPayload>,
    /// Human-oriented one-line explanation of the verdict.
    message: String,
}

/// Renders a [`SnapshotHead`] for human-readable output.
pub(crate) fn head_display(head: &SnapshotHead) -> String {
    match head {
        SnapshotHead::Commit { sha } => format!("commit {sha}"),
        SnapshotHead::NoGit => "no_git".to_owned(),
        SnapshotHead::UnbornHead => "unborn_head".to_owned(),
    }
}

/// Builds the human-oriented explanation for a freshness verdict.
pub(crate) fn freshness_message(verdict: Freshness) -> String {
    match verdict {
        Freshness::Fresh => {
            "store matches the current working tree (HEAD unchanged, tree clean)".to_owned()
        }
        Freshness::StaleHead => {
            "current HEAD differs from the stored snapshot; queried file/span handles may be \
             invalid — re-scan before citing them"
                .to_owned()
        }
        Freshness::StaleDirty => {
            "working tree has uncommitted changes relative to the stored snapshot; queried \
             file/span handles may be invalid — re-scan before citing them"
                .to_owned()
        }
        Freshness::Unknown => {
            "store predates snapshot stamping or no Git context exists; freshness cannot be \
             determined"
                .to_owned()
        }
    }
}

/// Handles `eg freshness [repo_path] (--graph <p> | --data-dir <d>) [--format ...]`.
///
/// Strictly read-only (issue #82 AC4): loads the store through the same
/// read-only path queries use, probes the working tree with `git rev-parse` /
/// `git status`, and never writes anything. Always returns `Ok(())` once a
/// verdict is produced; the verdict (including `unknown`) is the payload, not an
/// error.
pub(crate) fn freshness_cmd(
    repo_path: &Path,
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    repo_id_override: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let store_kind = if graph.is_some() { "graph" } else { "data_dir" };
    let identity = identity::compute_repository_identity(repo_path, repo_id_override);
    // Exclude both known store artifacts plus any in-tree `.egregore` store from
    // the dirty probe (PR #186 E/F/FF1): when checking `--graph`, the companion
    // `.egregore` data-dir created by the documented ingest workflow sits untracked
    // (and vice versa for `--data-dir` + `graph.jsonl`). Mirroring `scan`'s
    // store-artifact exclusions keeps a just-written store from reading as
    // `stale_dirty` before the user gitignores or deletes the intermediate output.
    let exclusions = store_exclusions_including_egregore(repo_path, &[graph, data_dir]);
    let (current_head, current_dirty) =
        identity::working_tree_snapshot_excluding(repo_path, &exclusions);

    // AC4: strictly read-only. A `--graph` JSONL is read directly (a plain file
    // read). A `--data-dir` embedded store is read through a throwaway copy,
    // because the embedded engine re-persists its on-disk index files on open;
    // operating on a copy guarantees the live store's records, indexes, runtime
    // files, and receipts are never created, modified, or deleted.
    let records = match data_dir {
        Some(dir) => load_records_from_data_dir_readonly(dir)?,
        None => load_query_records(graph, None)?,
    };
    // An explicit `--repo-id-override` pins the identity used to locate the stored
    // snapshot, so it must match exactly: a wrong/typo'd override must not borrow an
    // unrelated sole repository's snapshot via the single-repository fallback (which
    // could even report `fresh` under the caller's unmatched ID). Without an
    // override, the auto-detected identity keeps that fallback so legacy single-repo
    // stores still classify (PR #186 follow-up YY1).
    //
    // Report the repository that actually OWNS the matched snapshot, not the
    // recomputed checkout identity: a store scanned with `--repo-id-override` and
    // checked without it classifies the sole repository via the fallback, and the
    // JSON `repository_id` must be that stored Repository's ID so consumers keying
    // the verdict by repository are not misled (PR #186 follow-up ZZ1).
    let (report_repository_id, stored) = if repo_id_override.is_some() {
        // Exact match required; when found, the owner is the requested identity.
        (
            identity.id.clone(),
            freshness::stored_snapshot_exact(&records, &identity.id),
        )
    } else {
        // Mirror the per-row query freshness path (Z1): when the identity probe
        // misses, fall back to the sole STAMPED repository so a combined store with
        // exactly one stamped Repository (e.g. an override-scanned repo alongside a
        // legacy unstamped node) classifies unambiguously instead of reporting
        // `unknown` — and `eg freshness` agrees with `eg query ... --repo-path` on
        // the same store (PR #186 follow-up DDD1).
        match freshness::stored_snapshot_with_owner(&records, &identity.id)
            .or_else(|| freshness::stored_snapshot_sole_stamped(&records))
        {
            Some((owner, snapshot)) => (owner.to_owned(), Some(snapshot)),
            None => (identity.id.clone(), None),
        }
    };
    let mut verdict = freshness::classify(stored, &current_head, current_dirty);
    // A `fresh` verdict still misses a previously scanned source that a
    // sparse-checkout cone change removed: such a file is `skip-worktree` + absent,
    // so `git status` stays blind to it. Downgrade to `stale_dirty` when the store
    // cites such a removed path (FFF1). Only `fresh` is overridden: a `stale_head`
    // store already requires a re-scan.
    if verdict.is_fresh() {
        let removed = identity::index_hidden_absent_source_inputs(repo_path);
        let index = query::RepositoryIndex::build(&records);
        if cited_source_stale_on_disk(&records, &index, &report_repository_id, repo_path, &removed)
        {
            verdict = Freshness::StaleDirty;
        }
    }

    let report = FreshnessReport {
        freshness: verdict.code().to_owned(),
        fresh: verdict.is_fresh(),
        repository_id: report_repository_id,
        store_kind: store_kind.to_owned(),
        current_head,
        current_dirty,
        stored_snapshot: stored.cloned(),
        message: freshness_message(verdict),
    };

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&report).context("failed to serialize freshness report")?
            );
        }
        OutputFormat::Text => {
            println!("freshness: {}", report.freshness);
            println!("repository_id: {}", report.repository_id);
            println!("store: {store_kind}");
            println!(
                "current_head: {} (dirty: {})",
                head_display(&report.current_head),
                report.current_dirty
            );
            match &report.stored_snapshot {
                Some(snapshot) => println!(
                    "stored_head: {} (dirty: {})",
                    head_display(&snapshot.head),
                    snapshot.dirty
                ),
                None => println!("stored_head: (none — store predates snapshot stamping)"),
            }
            println!("message: {}", report.message);
        }
    }
    Ok(())
}

/// Computes the store-freshness code for a query against `repo_path` (issue #82).
///
/// Returns `None` when `repo_path` is absent, so freshness-unaware queries emit
/// byte-identical output to before this feature. When present, returns the stable
/// freshness code (including `"fresh"`) so an agent always sees the signal it asked
/// for and the result is never silently suppressed.
///
/// `repo_id_hint` is an optional known repository ID from the already-resolved
/// `--repo` scope or context owner; when provided and distinct from the
/// auto-detected identity it is tried first so that operator-override IDs win in
/// a multi-repo store, and used as the owner ID when no snapshot is found so that
/// `stamp_freshness` can match selected rows (PR #186 follow-up).
pub(crate) fn query_freshness_code_with_hint(
    records: &[GraphRecord],
    repo_path: Option<&Path>,
    artifacts: &[Option<&Path>],
    repo_id_hint: Option<&str>,
) -> Option<(String, &'static str)> {
    query_freshness_code_inner(records, repo_path, artifacts, repo_id_hint)
}

pub(crate) fn query_freshness_code_inner(
    records: &[GraphRecord],
    repo_path: Option<&Path>,
    artifacts: &[Option<&Path>],
    repo_id_hint: Option<&str>,
) -> Option<(String, &'static str)> {
    let repo_path = repo_path?;
    let identity = identity::compute_repository_identity(repo_path, None);
    // Mirror `freshness_cmd`/`scan` and also exclude any in-tree `.egregore*`
    // companion store (PR #186 follow-up II1): the documented workflow leaves an
    // untracked `.egregore` data-dir beside the graph, which must not stamp query
    // rows `stale_dirty` when the graph itself was scanned from a clean tree.
    let exclusions = store_exclusions_including_egregore(repo_path, artifacts);
    let (head, dirty) = identity::working_tree_snapshot_excluding(repo_path, &exclusions);
    // Any explicit hint (a resolved `--repo` scope or context owner) is
    // authoritative: the verdict must be owned by the selected repository, even
    // when the hint equals the auto-detected identity. Look up ONLY its snapshot —
    // never fall back to the auto-detected identity's snapshot or the sole-stamped
    // repo, which may belong to a different repository and would mislabel the
    // verdict's owner. When the selected repo has no snapshot (legacy/pre-stamping
    // rows in a combined store), `matched` stays `None` and the owner below is
    // still the hint, so `stamp_freshness` stamps `unknown` on the selected rows
    // rather than omitting the field (PR #186 follow-up SS1/UU1).
    //
    // Only when NO hint is given does the identity probe run with a sole-stamped
    // fallback: if exactly one Repository node in the store carries a snapshot,
    // that snapshot is unambiguous and should be used. This handles combined
    // stores where --repo-id-override was used on the scanned checkout but no
    // --repo flag was passed to the query command (PR #186 follow-up Z1).
    let matched = match repo_id_hint {
        Some(h) => freshness::stored_snapshot_with_owner(records, h),
        None => freshness::stored_snapshot_with_owner(records, &identity.id)
            .or_else(|| freshness::stored_snapshot_sole_stamped(records)),
    };
    // When no snapshot is found but the caller supplied an explicit hint (from
    // `--repo`), use the hint as the owner ID so `stamp_freshness` can match
    // the selected rows.  Falling back to `identity.id` would emit the unknown
    // verdict under the wrong owner, making freshness invisible on those rows.
    let (owner_id, stored) = match matched {
        Some((owner, snapshot)) => (owner.to_owned(), Some(snapshot)),
        None => (
            repo_id_hint.map(ToOwned::to_owned).unwrap_or(identity.id),
            None,
        ),
    };
    let code = freshness::classify(stored, &head, dirty).code();
    // Downgrade `fresh` to `stale_dirty` when a previously scanned source owned by
    // this repository was removed by a sparse-checkout cone change (skip-worktree +
    // absent, invisible to `git status`) but the store still cites it (FFF1),
    // matching `freshness_cmd`.
    let code = if code == "fresh" {
        let removed = identity::index_hidden_absent_source_inputs(repo_path);
        let index = query::RepositoryIndex::build(records);
        if cited_source_stale_on_disk(records, &index, &owner_id, repo_path, &removed) {
            "stale_dirty"
        } else {
            code
        }
    } else {
        code
    };
    Some((owner_id, code))
}

/// Computes repo-relative dirty-probe exclusions for the store artifact being
/// read (issue #82 / PR #186).
///
/// When the `--graph` file or `--data-dir` directory lives under `repo_path`, it
/// is returned as a repo-relative pathspec so the freshness dirty probe ignores
/// it — an in-tree store the workflow just wrote must not by itself make the tree
/// look `stale_dirty`. Returns empty when there is no artifact or it lives
/// outside the working tree.
pub(crate) fn store_artifact_exclusions(
    repo_path: &Path,
    artifacts: &[Option<&Path>],
) -> Vec<String> {
    let repo_abs = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    artifacts
        .iter()
        .filter_map(|a| *a)
        .filter_map(|artifact| {
            let abs = fs::canonicalize(artifact).unwrap_or_else(|_| artifact.to_path_buf());
            let rel = abs.strip_prefix(&repo_abs).ok()?;
            let s = rel.to_string_lossy().replace('\\', "/");
            (!s.is_empty()).then_some(s)
        })
        .collect()
}

/// Discovers untracked in-tree `.egregore*` embedded-store directories that must
/// be excluded from the dirty probe (PR #186 follow-up FF1/GG1).
///
/// A directory qualifies only when it is fully untracked (`git ls-files` reports
/// no content under it) and contains no `.rs` sources — the hallmark of a store
/// output (`eg ingest ... --data-dir .egregore`) rather than a source directory
/// that merely shares the prefix. The scanner never indexes such a store, so the
/// freshness dirty probe must not count it as source dirtiness.
pub(crate) fn egregore_store_dirs(repo_path: &Path) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    let Ok(entries) = std::fs::read_dir(repo_path) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let name_matches = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(".egregore"));
        // Only directories, not regular files such as `.egregore.rs`.
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !(name_matches && is_dir) {
            continue;
        }
        let path = entry.path();
        // `git ls-files` returns tracked paths under the directory; an empty
        // result means the entire subtree is untracked / gitignored, which is the
        // hallmark of a store output rather than a source directory. Use the entry
        // name directly so `git ls-files` receives a repo-relative path regardless
        // of whether `repo_path` is absolute or relative.
        let name = entry.file_name();
        let has_tracked = std::process::Command::new("git")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .current_dir(repo_path)
            .args(["ls-files", "--", name.to_str().unwrap_or("")])
            .output()
            .is_ok_and(|out| !out.stdout.is_empty());
        // Even when no content is tracked, an untracked directory containing `.rs`
        // files is a source directory, not a store output: its files appear in the
        // graph but are outside `git status`, so excluding it would mask deletions.
        if !has_tracked && !dir_has_sources(&path) {
            dirs.push(path);
        }
    }
    dirs
}

/// Builds dirty-probe exclusions for the explicit store `artifacts` plus any
/// in-tree `.egregore*` embedded-store directories discovered under `repo_path`
/// (PR #186 follow-up FF1/GG1).
///
/// All three store-producing/checking entry points (`scan`, `scan-history`,
/// `freshness`) share this so a companion store written by one workflow never
/// makes another's output read `stale_dirty`.
pub(crate) fn store_exclusions_including_egregore(
    repo_path: &Path,
    artifacts: &[Option<&Path>],
) -> Vec<String> {
    let egregore_dirs = egregore_store_dirs(repo_path);
    let mut all: Vec<Option<&Path>> = artifacts.to_vec();
    for dir in &egregore_dirs {
        all.push(Some(dir.as_path()));
    }
    store_artifact_exclusions(repo_path, &all)
}

/// Returns `true` if `dir` or any subdirectory contains a supported source file.
///
/// Used in the `.egregore*` auto-exclusion check: an untracked directory whose
/// subtree contains source files is a source directory, not a store output, and
/// must not be excluded from the snapshot dirty probe.
pub(crate) fn dir_has_sources(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if dir_has_sources(&path) {
                return true;
            }
        } else if crate::languages::is_supported_source(&path) {
            return true;
        }
    }
    false
}

/// Returns `true` when an ancestor directory of `repo_path/rel` (below the repo
/// root) contains a nested `.git` sentinel, so the scanner would no longer reach
/// `rel`.
///
/// Walks the parents of the cited file up to — but not including — `repo_path`, so
/// the repository's own `.git` never counts. A submodule/worktree (`.git` file) or
/// nested clone (`.git` directory) appearing over a previously scanned tree makes
/// `fs::should_descend` skip it, yet `git status` cannot see that conversion
/// (GGG3 / PR #186 follow-up).
pub(crate) fn path_behind_nested_git(repo_path: &Path, rel: &str) -> bool {
    let full = repo_path.join(rel);
    let mut dir = full.parent();
    while let Some(d) = dir {
        if d == repo_path || !d.starts_with(repo_path) {
            break;
        }
        if d.join(".git").exists() {
            return true;
        }
        dir = d.parent();
    }
    false
}

/// Returns `true` when the store cites a `File` (owned by `owner_id`) that the
/// working tree no longer makes available to the scanner — a source the graph
/// indexed but that `git status` cannot flag.
///
/// Two cases, both keyed off the store's actual contents (so a path matters only
/// when cited — distinguishing a change to a *previously scanned* file from one
/// that was never indexed, which the pure working-tree probe cannot tell apart):
/// - `removed`: index-hidden (`skip-worktree`/`assume-unchanged`) yet absent
///   paths from [`identity::index_hidden_absent_source_inputs`] — a sparse-checkout
///   cone change removed a scanned file (FFF1) vs. a baseline omission (AAA1);
/// - a cited file now sitting behind a nested `.git` sentinel (GGG3).
pub(crate) fn cited_source_stale_on_disk(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    owner_id: &str,
    repo_path: &Path,
    removed: &[String],
) -> bool {
    let removed: std::collections::HashSet<&str> = removed.iter().map(String::as_str).collect();
    records.iter().any(|record| {
        matches!(
            record,
            GraphRecord::Node {
                kind: NodeKind::File,
                id,
                repo_relative_path: Some(path),
                ..
            } if index.owner_of(id) == Some(owner_id)
                && (removed.contains(path.as_str()) || path_behind_nested_git(repo_path, path))
        )
    })
}

/// Stamps the freshness `code` on each result whose repository matches the
/// checkout the code was computed for (issue #82). Rows owned by a different
/// repository (multi-repo stores) are left unstamped rather than mislabeled.
///
/// History (`scan-history`) source rows are attributed to their repository the
/// same way `scan` rows are: replay emits `Repository CONTAINS File` and
/// `File DEFINES Symbol` edges per commit, so `RepositoryIndex::owner_of`
/// resolves them and the verdict attaches via the normal ownership match
/// (verified by `query_symbol_repo_path_stamps_freshness_on_history_graph`).
pub(crate) fn stamp_freshness(
    results: &mut [SymbolResult<'_>],
    freshness: Option<&(String, &'static str)>,
) {
    let Some((repo_id, code)) = freshness else {
        return;
    };
    for result in results.iter_mut() {
        if result.repository_id == Some(repo_id.as_str()) {
            result.freshness = Some(code);
        }
    }
}
