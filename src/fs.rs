//! Filesystem discovery for local repository scans.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use crate::{
    error::{CodegraphError, Result},
    normalize_path,
};

/// Source file discovered under a repository root.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SourceFile {
    /// Absolute or caller-relative path used for filesystem reads.
    pub path: PathBuf,
    /// Repository-relative portable path used in graph records.
    pub repo_relative_path: String,
}

/// Discovers Rust source files under a repository root.
///
/// # Errors
///
/// Returns an error when directory traversal cannot read an entry or when a
/// discovered source file cannot be relativized against the repository root.
pub fn discover_rust_source_files(repo_root: &Path) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    collect_rust_source_files(repo_root, &mut files)?;
    files.sort_by(|left, right| {
        let left = normalize_path(left);
        let right = normalize_path(right);
        left.cmp(&right)
    });

    files
        .into_iter()
        .map(|path| {
            let repo_relative_path = repo_relative_path(repo_root, &path)?;
            Ok(SourceFile {
                path,
                repo_relative_path,
            })
        })
        .collect()
}

fn collect_rust_source_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(directory).map_err(|source| CodegraphError::ReadDirectory {
        path: directory.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| CodegraphError::ReadDirectoryEntry {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|source| CodegraphError::InspectPath {
                path: path.clone(),
                source,
            })?;

        if metadata.is_dir() {
            if should_descend(&path) {
                collect_rust_source_files(&path, files)?;
            }
        } else if metadata.is_file() && path.extension() == Some(OsStr::new("rs")) {
            files.push(path);
        }
    }

    Ok(())
}

fn should_descend(path: &Path) -> bool {
    !path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, ".git" | "target"))
}

fn repo_relative_path(repo_root: &Path, path: &Path) -> Result<String> {
    let relative =
        path.strip_prefix(repo_root)
            .map_err(|_| CodegraphError::PathOutsideRepository {
                path: path.to_path_buf(),
                root: repo_root.to_path_buf(),
            })?;
    Ok(normalize_path(relative))
}
