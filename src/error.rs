use std::{io, path::PathBuf};

/// Result type used by Aletheia Codegraph operations.
pub type Result<T> = std::result::Result<T, CodegraphError>;

/// Errors produced while extracting or serializing a code graph.
#[derive(Debug, thiserror::Error)]
pub enum CodegraphError {
    /// The requested repository path does not exist.
    #[error("repository path does not exist: {path}")]
    RepositoryMissing {
        /// Path supplied by the caller.
        path: PathBuf,
    },

    /// The requested repository path is not a directory.
    #[error("repository path is not a directory: {path}")]
    RepositoryNotDirectory {
        /// Path supplied by the caller.
        path: PathBuf,
    },

    /// A directory could not be read during source discovery.
    #[error("failed to read directory {path}: {source}")]
    ReadDirectory {
        /// Directory being read.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },

    /// A directory entry could not be read during source discovery.
    #[error("failed to read entry in {path}: {source}")]
    ReadDirectoryEntry {
        /// Directory containing the unreadable entry.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },

    /// Filesystem metadata could not be read.
    #[error("failed to inspect path {path}: {source}")]
    InspectPath {
        /// Path being inspected.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },

    /// A source file could not be read.
    #[error("failed to read source file {path}: {source}")]
    ReadFile {
        /// Source file path.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },

    /// A file could not be written.
    #[error("failed to write file {path}: {source}")]
    WriteFile {
        /// File path.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },

    /// A discovered path was not inside the repository root.
    #[error("path {path} is not under repository root {root}")]
    PathOutsideRepository {
        /// Path that could not be relativized.
        path: PathBuf,
        /// Repository root.
        root: PathBuf,
    },

    /// Graph JSON serialization failed.
    #[error("failed to serialize graph record: {0}")]
    Serialize(#[from] serde_json::Error),

    /// Tree-sitter could not load a language grammar.
    #[error("failed to load parser language: {0}")]
    ParserLanguage(String),

    /// Tree-sitter did not produce a syntax tree.
    #[error("parser did not produce a syntax tree for {path}")]
    Parse {
        /// Source file path.
        path: PathBuf,
    },

    /// A Git command failed while replaying history.
    #[error("git command failed ({command}): {message}")]
    GitCommand {
        /// Command that failed.
        command: String,
        /// Failure detail.
        message: String,
    },
}
