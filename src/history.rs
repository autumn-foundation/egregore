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
        EdgeLabel, Graph, GraphRecord, NodeKind, ProducerKind, SnapshotHead, SourceSnapshotPayload,
        TemporalMetadata, stable_id,
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
    scan_repository_history_inner(repo_path, repo_id_override)
}

#[allow(clippy::too_many_lines)]
fn scan_repository_history_inner(
    repo_path: impl AsRef<Path>,
    repo_id_override: Option<&str>,
) -> Result<Graph> {
    std::sync::LazyLock::force(&PROCESS_STARTED_AT);
    let repo_root = repo_path.as_ref();
    validate_repository(repo_root)?;

    let repo_identity = identity::compute_repository_identity(repo_root, repo_id_override);
    let (repository_id, repository) = repository_record_from_identity(&repo_identity);

    // Stamp a source snapshot on the Repository node (BB1 / PR #186 follow-up):
    // without it, `eg freshness --graph history.graph.jsonl` and `--at` queries
    // with `--repo-path` always report `unknown`. The stamped HEAD lets freshness
    // detect `stale_head` after new commits are added.
    //
    // History replay reads only committed Git objects, so the snapshot records the
    // committed HEAD state with `dirty = false` (TT1): uncommitted working-tree
    // edits never enter the replayed graph, and stamping them dirty would leave the
    // store permanently `stale_dirty` even after the edits are reverted with HEAD
    // unchanged. Current working-tree dirtiness is detected live at freshness-check
    // time instead.
    //
    // The transaction time is derived from HEAD's committer date, not wall-clock,
    // so repeated scans of an unchanged repository stay byte-stable across a
    // seconds boundary (TT3 / the history replay determinism contract).
    let head = identity::working_tree_head(repo_root);
    let transaction_time = match &head {
        SnapshotHead::Commit { sha } => commit_metadata(repo_root, sha)?.committed_at,
        _ => PROCESS_STARTED_AT.clone(),
    };
    let snapshot = SourceSnapshotPayload {
        head,
        dirty: false,
        repository_id: repository_id.clone(),
        scanned_at: transaction_time.clone(),
    };
    let mut graph = Graph::new();
    graph.push(
        repository
            .with_valid_time_inferred(&transaction_time)
            .with_source_snapshot(snapshot),
    );

    // Manifest-parse memo keyed by blob OID (issue #117): a `Cargo.toml` is
    // typically unchanged across hundreds of commits, so each distinct manifest
    // blob is read and parsed exactly once for the whole replay.
    let mut manifest_outcome_memo: BTreeMap<String, ManifestParseOutcome> = BTreeMap::new();
    for commit in list_commits(repo_root)? {
        let commit_record = commit_record(&repository_id, &commit);
        let commit_id = commit_record.id().to_owned();
        graph.push(commit_record);
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repository_id.clone(),
            commit_id.clone(),
            Some("1.0".to_owned()),
            format!("Repository contains commit {}", commit.short_sha()),
        ));

        for parent in &commit.parents {
            let parent_id = stable_id(&["node", "commit", &repository_id, parent]);
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

        let mut change_ids_by_path = BTreeMap::new();
        for change in list_changes(repo_root, &commit)? {
            let change_record = change_record(&repository_id, &commit, &change);
            let change_id = change_record.id().to_owned();
            change_ids_by_path.insert(change.path.clone(), change_id.clone());
            graph.push(change_record);
            graph.push(
                GraphRecord::edge(
                    EdgeLabel::Contains,
                    commit_id.clone(),
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

        let mut facts_by_file = BTreeMap::new();
        // One `ls-tree` pass yields both this commit's indexed sources and its
        // Cargo manifests (issue #117), so adding attribution costs no extra
        // Git invocation per commit.
        let commit_tree = list_commit_tree(repo_root, &commit.sha)?;
        let attribution = commit_crate_attribution_index(
            repo_root,
            &commit.sha,
            &commit_tree.manifests,
            &mut manifest_outcome_memo,
        );
        // Records pushed from here on belong to this commit's replayed tree;
        // the same-file resolution labeling pass (issue #134) must only see
        // this commit's slice because the same stable edge ID can recur across
        // commits with different in-repo definition sets.
        let commit_records_start = graph.records().len();
        for path in commit_tree.sources {
            let change_id = change_ids_by_path.get(&path);
            let bytes = git_blob_bytes(repo_root, &commit.sha, &path)?;
            let Ok(source) = std::str::from_utf8(&bytes) else {
                // Issue #438: a non-UTF-8 committed blob is skipped, not aborted.
                // Emit a deterministic `Diagnostic` naming the commit + path
                // (fixed summary, no raw bytes) and keep replaying the tree.
                // `git show` reads objects only, never mutating the checkout.
                let diag_id = stable_id(&[
                    "node",
                    "diagnostic",
                    "non_utf8_source",
                    &repository_id,
                    &commit.sha,
                    &path,
                ]);
                graph.push(
                    GraphRecord::node(
                        diag_id,
                        NodeKind::Diagnostic,
                        Some(path.clone()),
                        None,
                        Some("non_utf8_source".to_owned()),
                        "skipped source file: not valid UTF-8".to_owned(),
                    )
                    .with_temporal(commit.temporal()),
                );
                continue;
            };
            let source_file = SourceFile {
                path: repo_root.join(&path),
                repo_relative_path: path.clone(),
            };
            let (records, facts) = scan_source_text_records(&source_file, source, &repository_id)?;
            if !facts.is_empty() {
                facts_by_file.insert(path.clone(), facts);
            }
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
                                commit_id.clone(),
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

        // Repo-wide cross-file call resolution for this commit's tree
        // (issue #152), stamped with the commit's temporal provenance like
        // every other syntax-backed record replayed at this commit.
        for record in
            crate::languages::cross_file::cross_file_call_records(&repository_id, &facts_by_file)
        {
            graph.push(record.with_temporal(commit.temporal()));
        }
        // Repo-wide cross-file trait resolution (issue #344) for this commit's
        // tree: an out-of-line impl whose trait lives in another file
        // edge-backs here, stamped with the commit's temporal provenance.
        for record in crate::languages::cross_file::cross_file_implements_records(
            &repository_id,
            &facts_by_file,
        ) {
            graph.push(record.with_temporal(commit.temporal()));
        }
        // Same-file resolution labeling (issue #134) over this commit's slice.
        crate::languages::cross_file::label_same_file_call_resolutions(
            &mut graph.records_mut()[commit_records_start..],
            &facts_by_file,
        );
        // Out-of-line `#[cfg(test)] mod x;` test-scope marking (issue #223)
        // over this commit's replayed tree.
        crate::languages::cross_file::apply_out_of_line_test_scope(
            &mut graph.records_mut()[commit_records_start..],
            &facts_by_file,
        );
        // Owning-Cargo-package attribution (issue #117), scoped to THIS
        // COMMIT'S SLICE. Slice-scoping is mandatory, for the same reason the
        // resolution-labeling pass above is scoped: an ADR-0004 symbol ID
        // carries no commit component, so a whole-graph pass would stamp every
        // historical version of a record with the LAST commit's manifest tree —
        // a fabricated fact at a pinned historical point.
        crate::crate_attribution::apply_crate_attribution(
            &mut graph.records_mut()[commit_records_start..],
            &attribution,
        );
    }

    let languages = crate::languages_in_graph(&graph);
    let mut producer = code_graph_producer(&languages);
    producer.producer_kind = ProducerKind::HistoryReplay;
    // Make the producer fully deterministic too (CCC1): `code_graph_producer`
    // sets `producer_started_at` from the wall-clock `PROCESS_STARTED_AT`, which
    // would make two `eg scan-history` runs of the same unchanged repository in
    // separate processes differ. History replay is committed-state-only, so anchor
    // it to the same deterministic HEAD-committer transaction time as the snapshot.
    producer.producer_started_at = transaction_time;
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

fn list_changes(repo_root: &Path, commit: &GitCommit) -> Result<Vec<GitChange>> {
    let output = git_output(
        repo_root,
        &[
            "diff-tree",
            "-m",
            "--no-commit-id",
            "--name-status",
            "-r",
            "--root",
            &commit.sha,
        ],
    )?;
    let mut seen = BTreeSet::new();
    Ok(output
        .lines()
        .filter_map(parse_change_line)
        .filter(|change| seen.insert(change.clone()))
        .collect::<Vec<_>>())
}

fn parse_change_line(line: &str) -> Option<GitChange> {
    let parts = line.split('\t').collect::<Vec<_>>();
    let status = parts.first()?.trim();
    let path = parts.last()?.trim();
    if status.is_empty() || path.is_empty() {
        return None;
    }
    Some(GitChange {
        status: status.to_owned(),
        path: normalize_git_path(path),
    })
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
/// `-z` is load-bearing, not a style choice. `core.quotePath=false` suppresses
/// octal-escaping of NON-ASCII bytes only: `ls-tree` still C-quotes any path
/// containing `"`, `\`, or a control character, wrapping it in literal quotes
/// that no subsequent `git show <sha>:<path>` can resolve. The working-tree walk
/// reads `git ls-files -z`, whose NUL-delimited output is never quoted, so
/// without `-z` here a quote-bearing path is indexed by `eg scan` and silently
/// ABSENT from `eg scan-history`. NUL-delimited output makes the two agree.
///
/// Both filters prune any path with a `target` component, mirroring
/// [`is_indexed_source`], so committed build output never contributes sources
/// or owning packages.
fn list_commit_tree(repo_root: &Path, sha: &str) -> Result<CommitTree> {
    let output = git_output(repo_root, &["ls-tree", "-r", "-z", sha])?;
    let mut tree = CommitTree::default();
    for entry in output.split('\0') {
        // `<mode> SP <type> SP <object> TAB <path>`
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        let (Some(_mode), Some(object_type), Some(oid)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if object_type != "blob" {
            // A `commit` entry is a submodule gitlink: its content belongs to a
            // different repository and cannot be read from this one.
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
        // `-c core.quotePath=false` keeps non-ASCII paths verbatim in
        // `ls-tree`/`diff-tree` output instead of C-quoted octal escapes
        // (`"crates/caf\303\251/src/lib.rs"`). Without it the replay feeds the
        // quoted form back to `git show <sha>:<path>`, which fails — silently
        // dropping an entire directory from the graph. Every other Git call
        // site in the crate already sets it (`src/fs.rs`, `src/identity.rs`).
        .args(["-c", "core.quotePath=false"])
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
