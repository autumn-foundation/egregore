//! Git history replay for bi-temporal code graph records.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{
    code_graph_producer,
    error::{CodegraphError, Result},
    fs::SourceFile,
    identity,
    ir::{EdgeLabel, Graph, GraphRecord, NodeKind, ProducerKind, TemporalMetadata, stable_id},
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
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let repo_root = repo_path.as_ref();
    validate_repository(repo_root)?;

    let repo_identity = identity::compute_repository_identity(repo_root, repo_id_override);
    let (repository_id, repository) = repository_record_from_identity(&repo_identity);

    let mut graph = Graph::new();
    graph.push(repository);

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

        for path in list_rust_files(repo_root, &commit.sha)? {
            let change_id = change_ids_by_path.get(&path);
            let source = git_blob(repo_root, &commit.sha, &path)?;
            let source_file = SourceFile {
                path: repo_root.join(&path),
                repo_relative_path: path.clone(),
            };
            for record in scan_source_text_records(&source_file, &source, &repository_id)? {
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
    }

    let mut producer = code_graph_producer(&started_at);
    producer.producer_kind = ProducerKind::HistoryReplay;
    Ok(graph.stamp_producer(&producer))
}

#[derive(Debug, Clone)]
struct GitCommit {
    sha: String,
    parents: Vec<String>,
    committed_at: String,
    authored_at: String,
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

    fn short_sha(&self) -> String {
        short_sha(&self.sha)
    }
}

#[derive(Debug, Clone)]
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
        &["show", "-s", "--format=%H%n%P%n%cI%n%aI%n%s", sha],
    )?;
    let mut lines = output.lines();
    let full_sha = required_line(&mut lines, "commit sha")?;
    let parents = required_line(&mut lines, "commit parents")?;
    let committed_at = required_line(&mut lines, "commit time")?;
    let authored_at = required_line(&mut lines, "author time")?;
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
        .filter(|change| seen.insert((change.status.clone(), change.path.clone())))
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

fn list_rust_files(repo_root: &Path, sha: &str) -> Result<Vec<String>> {
    let output = git_output(repo_root, &["ls-tree", "-r", "--name-only", sha])?;
    let mut files = output
        .lines()
        .map(str::trim)
        .filter(|path| {
            Path::new(path)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
        })
        .map(normalize_git_path)
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn git_blob(repo_root: &Path, sha: &str, path: &str) -> Result<String> {
    git_output(repo_root, &["show", &format!("{sha}:{path}")])
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
        command: "git show -s --format=%H%n%P%n%cI%n%aI%n%s".to_owned(),
        message: format!("missing {field_name}"),
    })
}

fn normalize_git_path(path: &str) -> String {
    PathBuf::from(path)
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}
