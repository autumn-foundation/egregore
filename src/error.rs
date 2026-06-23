use std::{io, path::PathBuf};

/// Result type used by Egregore code graph operations.
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

    /// A cached or persisted graph record uses an unsupported schema version.
    #[error("{message}")]
    UnsupportedSchemaVersion {
        /// Version compatibility failure detail.
        message: String,
    },

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

    /// An importer received a file with no parseable events.
    #[error("no parseable events in {path}: file is empty or entirely malformed")]
    EmptyImport {
        /// Path of the file that produced no events.
        path: PathBuf,
    },

    /// A caller supplied an invalid argument value.
    #[error("invalid argument: {message}")]
    InvalidArgument {
        /// Detail message.
        message: String,
    },

    /// A graph record was rejected because a sensitive field contains unredacted
    /// raw secret material.
    ///
    /// The error names the field path but **never echoes the raw secret value**.
    /// Apply the default redaction policy (`crate::redaction::redact_value`) to
    /// the field, set `redaction_policy_version`, and resubmit the record.
    ///
    /// Documented in `docs/schema/redaction.md` and `docs/cli/redaction.md`.
    #[error(
        "redaction_required: field '{field_path}' contains unredacted sensitive material; \
        apply the v1 redaction policy before submission"
    )]
    RedactionRequired {
        /// Dot-separated path to the field containing sensitive material.
        /// Example: `"text"`, `"validation_summary"`, `"stdout_handle.inline"`.
        field_path: String,
    },

    /// A graph record carries a redaction marker in a sensitive field but the node
    /// is not stamped with a `redaction_policy_version`.
    ///
    /// Set `redaction_policy_version` via
    /// [`GraphRecord::with_redaction_policy_version`](crate::ir::GraphRecord::with_redaction_policy_version)
    /// before submission.
    #[error(
        "redaction_metadata_missing: field '{field_path}' carries a redaction marker but \
        redaction_policy_version is not set on the node; stamp the node with the current \
        policy version before submission"
    )]
    RedactionMetadataMissing {
        /// Dot-separated path to the field carrying the unversioned marker.
        field_path: String,
    },

    /// A bundle verification failed.
    #[error("bundle verification failed: {message}")]
    BundleVerificationFailed {
        /// Detail message.
        message: String,
    },
}
