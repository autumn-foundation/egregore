//! Protected raw-artifact capture and retrieval (issue #60).
//!
//! This module implements opt-in, local-first, content-addressed storage for raw
//! agent evidence payloads — transcripts, command output, patch bytes, task
//! narratives, and generated reports.  By default Egregore stores only content
//! hashes and handles; this module gives operators a workflow to also retain the
//! raw bytes in a separate directory that the graph, query, and semantic surfaces
//! never read.
//!
//! ## Store layout
//!
//! ```text
//! <store>/blobs/<content_hash>   — raw bytes, content-addressed by BLAKE3 hex
//! <store>/manifest.jsonl         — canonical-sorted ProtectedHandle records
//! <store>/operators.jsonl        — canonical-sorted authorized producer IDs
//! ```
//!
//! ## Relationship to related slices
//!
//! * Issue #41 (redaction) — Redaction runs *before* any payload reaches this
//!   module.  Protected capture is about retention of evidence, not about
//!   bypassing the redaction gate.
//! * Issue #54 (encrypted stores) — This module stores blobs as plain bytes.
//!   Encryption at rest is out of scope here; operators who need encryption
//!   should layer a filesystem-level solution.  Issue #54 owns the Egregore
//!   encrypted-store workflow.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Schema version for [`ProtectedHandle`] records.
pub const PROTECTED_SCHEMA_VERSION: u32 = 1;

/// Stable prefix for protected-artifact handle strings.
pub const PROTECTED_HANDLE_PREFIX: &str = "protected:v1:";

// ── Payload class ──────────────────────────────────────────────────────────────

/// The five protected payload classes recognised by this slice.
///
/// `serde` serialises these as `snake_case` strings so JSONL records are human-
/// readable and stable across binary versions.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectedPayloadClass {
    /// Agent-session transcript text (e.g. Claude Code, Codex, traj file body).
    Transcript,
    /// Command standard output or error (e.g. `cargo test` stdout).
    CommandOutput,
    /// Raw patch bytes (unified diff).
    Patch,
    /// Task or issue narrative (markdown description, AC list, PRD body).
    TaskNarrative,
    /// Generated report (analysis, scan summary, evaluation result).
    Report,
}

impl ProtectedPayloadClass {
    /// Returns the stable `snake_case` string for this class.
    ///
    /// Used as a component in the handle identity hash.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::CommandOutput => "command_output",
            Self::Patch => "patch",
            Self::TaskNarrative => "task_narrative",
            Self::Report => "report",
        }
    }
}

impl std::fmt::Display for ProtectedPayloadClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses a class string, returning [`None`] for unrecognised values.
///
/// Used when validating capture-manifest entries before storage.
#[must_use]
pub fn parse_class(s: &str) -> Option<ProtectedPayloadClass> {
    match s {
        "transcript" => Some(ProtectedPayloadClass::Transcript),
        "command_output" => Some(ProtectedPayloadClass::CommandOutput),
        "patch" => Some(ProtectedPayloadClass::Patch),
        "task_narrative" => Some(ProtectedPayloadClass::TaskNarrative),
        "report" => Some(ProtectedPayloadClass::Report),
        _ => None,
    }
}

// ── Handle ─────────────────────────────────────────────────────────────────────

/// A stable, content-addressed handle for one protected raw payload.
///
/// Fields named `content_hash`, `byte_len`, `source_class`, `source_path`,
/// `producer_id`, and `producer_version` form the queryable metadata.  The
/// raw bytes are stored separately in `<store>/blobs/<content_hash>` and are
/// returned only through an explicit [`ProtectedStore::get`] call.
///
/// `captured_at` is metadata only: it does NOT contribute to the handle
/// identity, so re-capturing an unchanged source yields the same handle and
/// produces zero duplicate manifest entries.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProtectedHandle {
    /// Stable content-addressed identifier: `protected:v1:<blake3 hex>`.
    ///
    /// Identity hash covers `source_class`, `content_hash`, and `source_path`
    /// (excluding `captured_at` to preserve idempotency across re-imports).
    pub handle: String,
    /// Schema version — always [`PROTECTED_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Payload class (transcript, command output, patch, task narrative, report).
    pub source_class: ProtectedPayloadClass,
    /// Repo-relative or absolute source path when known, `None` for synthetic payloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// BLAKE3 hex hash of the full raw payload bytes.
    pub content_hash: String,
    /// Exact byte length of the raw payload.
    pub byte_len: u64,
    /// RFC 3339 wall-clock time when this payload was first captured.
    pub captured_at: String,
    /// Stable identity of the capturing producer (e.g. `"op-1"`, agent ID).
    pub producer_id: String,
    /// Egregore version string at capture time (`CARGO_PKG_VERSION`).
    pub producer_version: String,
}

impl ProtectedHandle {
    /// Computes the BLAKE3-based stable handle string for the given components.
    ///
    /// Excludes `captured_at` so that re-capture of unchanged content yields the
    /// same handle.
    #[must_use]
    pub fn compute_handle(
        class: &ProtectedPayloadClass,
        content_hash: &str,
        source_path: Option<&str>,
    ) -> String {
        let input = format!(
            "{}\n{}\n{}",
            class.as_str(),
            content_hash,
            source_path.unwrap_or("")
        );
        let hash = blake3::hash(input.as_bytes());
        format!("{}{}", PROTECTED_HANDLE_PREFIX, hash.to_hex())
    }
}

// ── Capture manifest entry ─────────────────────────────────────────────────────

/// One entry in the operator-supplied capture manifest JSONL.
///
/// Each line of the manifest file is a JSON object with at least `class` and
/// `source_path`.  Unrecognised fields are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureEntry {
    /// Payload class string (parsed via [`parse_class`]).
    pub class: String,
    /// Path to the source file whose bytes should be captured.
    pub source_path: String,
}

// ── Capture report ─────────────────────────────────────────────────────────────

/// Per-entry outcome within a [`CaptureReport`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureEntryOutcome {
    /// Source path from the manifest entry.
    pub source_path: String,
    /// Stable handle (always present, even in preview/disabled mode).
    pub handle: String,
    /// BLAKE3 hex of the content (always present).
    pub content_hash: String,
    /// Byte length (always present).
    pub byte_len: u64,
    /// `true` when the payload was actually stored (only possible in enabled mode).
    pub stored: bool,
    /// Diagnostic code when this entry could not be stored or read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<EntryDiagnostic>,
}

/// A per-entry diagnostic emitted when capture encounters a non-fatal problem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryDiagnostic {
    /// Stable machine-readable code.  One of:
    /// `unsupported_payload_class`, `stale_source_path`.
    pub code: String,
    /// Human-readable explanation (never echoes payload bytes).
    pub message: String,
}

/// Result of a [`ProtectedStore::capture`] operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureReport {
    /// `true` when protected-raw-artifact mode was enabled for this run.
    pub enabled: bool,
    /// Per-entry outcomes.
    pub entries: Vec<CaptureEntryOutcome>,
    /// Number of entries successfully stored (always 0 in disabled mode).
    pub stored_count: usize,
    /// Number of entries skipped due to diagnostics.
    pub skipped_count: usize,
}

// ── Get error ──────────────────────────────────────────────────────────────────

/// Errors returned by [`ProtectedStore::get`].
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GetError {
    /// The protected store or manifest does not exist; mode was not enabled.
    RawArtifactModeDisabled,
    /// The requesting operator ID is not in the authorised set.
    Unauthorized {
        /// The operator ID that was rejected.
        ///
        /// Intentionally not serialized to avoid leaking bearer tokens or
        /// other secrets that a caller might accidentally supply as an
        /// operator identifier.
        #[serde(skip)]
        operator: String,
    },
    /// The handle string does not have the `protected:v1:` prefix.
    MalformedHandle {
        /// The invalid handle string.
        ///
        /// Intentionally not serialized: the `--handle` argument position may
        /// accidentally receive a bearer token or other secret, and echoing it
        /// to stderr violates the diagnostic invariant.
        #[serde(skip)]
        handle: String,
    },
    /// The handle is not present in the manifest.
    PayloadNotFound {
        /// The handle that could not be resolved.
        handle: String,
    },
    /// The handle is in the manifest but the blob file is absent.
    MissingProtectedPayload {
        /// The handle whose blob is gone.
        handle: String,
        /// The expected blob path.
        expected_path: String,
    },
    /// The blob bytes do not match the stored BLAKE3 content hash.
    HashMismatch {
        /// The handle whose integrity check failed.
        handle: String,
        /// The hash recorded in the manifest.
        expected: String,
        /// The hash computed from the stored blob.
        actual: String,
    },
    /// The manifest record's `handle` field does not match the handle computed
    /// from its own class, content hash, and source path.  The manifest has
    /// been corrupted or tampered with.
    CorruptManifestRecord {
        /// The requested handle.
        handle: String,
    },
}

impl GetError {
    /// Returns the stable machine-readable error code string.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::RawArtifactModeDisabled => "raw_artifact_mode_disabled",
            Self::Unauthorized { .. } => "unauthorized",
            Self::MalformedHandle { .. } => "malformed_handle",
            Self::PayloadNotFound { .. } => "payload_not_found",
            Self::MissingProtectedPayload { .. } => "missing_protected_payload",
            Self::HashMismatch { .. } => "hash_mismatch",
            Self::CorruptManifestRecord { .. } => "corrupt_manifest_record",
        }
    }

    /// Serialises the error as a machine-readable JSON envelope.
    ///
    /// Never includes raw payload bytes, secrets, or bearer tokens.
    ///
    /// # Panics
    ///
    /// Panics only when `serde_json::to_value(self)` fails, which cannot happen
    /// for this well-formed enum.
    #[must_use]
    pub fn to_json(&self) -> String {
        let detail = serde_json::to_value(self).expect("GetError serialisation is infallible");
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": self.code(),
                "detail": detail,
            }
        });
        serde_json::to_string(&envelope).expect("envelope serialisation is infallible")
    }
}

// ── BLAKE3 hex validation ──────────────────────────────────────────────────────

/// Returns `true` iff `s` is a valid 64-character lowercase BLAKE3 hex string.
///
/// Used to guard against path-traversal attacks where a tampered `content_hash`
/// in `manifest.jsonl` (e.g. `"../../../etc/passwd"`) would otherwise be joined
/// directly onto the blobs directory path.
fn is_valid_blake3_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

// ── Filesystem helpers with private permissions ────────────────────────────────

/// Creates a directory (and all parents) with owner-only permissions.
///
/// On Unix, the directory is created with mode `0o700`.  On other platforms
/// the system default is used; operators must provision filesystem ACLs
/// themselves.
fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Writes `data` to `path` with owner-only permissions.
///
/// On Unix, the file is created with mode `0o600`.  On other platforms the
/// system default is used.
///
/// Uses a write-to-temp-then-rename strategy so that a pre-existing symlink
/// at `path` is atomically replaced (not followed): on Unix `rename(2)` and
/// on Windows `MoveFileExW` both replace the destination entry without
/// following it, preventing a tampered store from redirecting the write to a
/// file outside the protected-store boundary.
fn write_private_file(path: &Path, data: &[u8]) -> io::Result<()> {
    // Build a sibling temp path in the same directory.  Same-directory
    // placement guarantees the rename is on the same filesystem (required for
    // atomicity on most platforms).
    let file_name = path
        .file_name()
        .map_or_else(|| "write".to_owned(), |n| n.to_string_lossy().into_owned());
    let tmp_name = format!(".{file_name}.wip");
    let tmp_path = path.parent().map_or_else(
        || std::path::PathBuf::from(&tmp_name),
        |p| p.join(&tmp_name),
    );

    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)?;
        f.write_all(data)?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&tmp_path, data)?;
    }
    // Atomically replace `path` (including any symlink at that path) with the
    // temp file.  On Unix this is `rename(2)`, which replaces the directory
    // entry without following a symlink at the destination.
    fs::rename(&tmp_path, path)
}

// ── Protected store ────────────────────────────────────────────────────────────

/// Content-addressed store for protected raw artifact payloads.
///
/// All I/O is synchronous and local-first.  The store directory is created
/// lazily on first write (capture with `enabled = true`).
pub struct ProtectedStore {
    root: PathBuf,
}

impl ProtectedStore {
    /// Opens (or prepares to open) a protected store at `root`.
    ///
    /// The directory is created lazily; calling `new` on a non-existent path
    /// is fine as long as no write operations are attempted.
    #[must_use]
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    // ── Internal helpers ───────────────────────────────────────────────────────

    fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.jsonl")
    }

    fn operators_path(&self) -> PathBuf {
        self.root.join("operators.jsonl")
    }

    fn blob_path(&self, content_hash: &str) -> PathBuf {
        self.blobs_dir().join(content_hash)
    }

    /// Returns `true` when the store root and manifest both exist.
    fn is_initialised(&self) -> bool {
        self.manifest_path().exists()
    }

    /// Reads the canonical manifest, returning the de-duplicated set of handles.
    fn read_manifest(&self) -> io::Result<Vec<ProtectedHandle>> {
        let path = self.manifest_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&path)?;
        let mut handles = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let h: ProtectedHandle = serde_json::from_str(line).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("manifest parse error: {e}"),
                )
            })?;
            handles.push(h);
        }
        Ok(handles)
    }

    /// Writes the manifest as canonical-sorted JSONL.
    ///
    /// When two records share the same handle but differ on metadata outside
    /// the handle identity (e.g. `captured_at`, `producer_id`), the first
    /// record in `handles` wins (first-capture-wins contract).  Byte-identical
    /// deduplication alone would leave both records if they differed on any
    /// such field.
    fn write_manifest(&self, handles: &[ProtectedHandle]) -> io::Result<()> {
        let dir = &self.root;
        create_private_dir(dir)?;
        // Collapse by handle: insert-order preserves first-capture-wins because
        // callers build `handles` with existing manifest records first.
        let mut by_handle: std::collections::BTreeMap<&str, &ProtectedHandle> =
            std::collections::BTreeMap::new();
        for h in handles {
            by_handle.entry(h.handle.as_str()).or_insert(h);
        }
        let mut lines: Vec<String> = by_handle
            .values()
            .map(|h| serde_json::to_string(h).expect("ProtectedHandle serialisation is infallible"))
            .collect();
        lines.sort_unstable();
        let content = format!("{}\n", lines.join("\n"));
        write_private_file(&self.manifest_path(), content.as_bytes())
    }

    /// Reads the canonical operators file, returning the set of authorised IDs.
    fn read_operators(&self) -> io::Result<Vec<String>> {
        let path = self.operators_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&path)?;
        // Fail-closed: return an error if ANY line fails to parse as a JSON string.
        // Silently skipping malformed lines would normalize a partially corrupted ACL
        // and could allow a future write to silently revoke the discarded operators.
        // An operators.jsonl with any malformed line must be treated as corrupt.
        let mut ops = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let op: String = serde_json::from_str(line).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("operators.jsonl contains a malformed record: {e}"),
                )
            })?;
            if op.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "operators.jsonl contains an empty operator identity",
                ));
            }
            ops.push(op);
        }
        ops.sort_unstable();
        ops.dedup();
        Ok(ops)
    }

    /// Writes the canonical operators file.
    fn write_operators(&self, ops: &[String]) -> io::Result<()> {
        let dir = &self.root;
        create_private_dir(dir)?;
        let mut lines: Vec<String> = ops
            .iter()
            .map(|o| serde_json::to_string(o).expect("operator serialisation is infallible"))
            .collect();
        lines.sort_unstable();
        lines.dedup();
        let content = format!("{}\n", lines.join("\n"));
        write_private_file(&self.operators_path(), content.as_bytes())
    }

    // ── Public API ─────────────────────────────────────────────────────────────

    /// Captures raw payloads from `entries`.
    ///
    /// When `enabled` is `false` (the default), this is a *preview run*: the
    /// content hash and byte length are computed and returned but **nothing is
    /// written to disk**.  This satisfies AC2: the store is never populated unless
    /// the operator explicitly enables protected-raw-artifact mode.
    ///
    /// When `enabled` is `true`, each payload whose source file is readable is
    /// stored as a blob and registered in the manifest.  Per-entry problems
    /// (`unsupported_payload_class`, `stale_source_path`) produce diagnostics and
    /// do not abort the overall capture.
    ///
    /// # Errors
    ///
    /// Returns an error on manifest or filesystem I/O failure.
    #[allow(clippy::too_many_lines)]
    pub fn capture(
        &self,
        entries: &[CaptureEntry],
        producer_id: &str,
        producer_version: &str,
        captured_at: &str,
        enabled: bool,
    ) -> io::Result<CaptureReport> {
        // Validate at the store API boundary, not just the CLI, so embedded
        // callers cannot write "" into operators.jsonl and authorize get("", "").
        if enabled && producer_id.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "producer_id must not be empty when capture is enabled",
            ));
        }

        // Validate captured_at is RFC3339 before constructing any ProtectedHandle,
        // so manifest.jsonl never contains a non-conforming timestamp.
        if enabled {
            chrono::DateTime::parse_from_rfc3339(captured_at).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("captured_at must be RFC 3339 (got {captured_at:?}): {e}"),
                )
            })?;
        }

        let mut outcomes: Vec<CaptureEntryOutcome> = Vec::new();
        let mut stored_count = 0usize;
        let mut skipped_count = 0usize;

        // Load existing manifest and validate the operators file *before* any
        // blob or manifest writes.  This ensures that a corrupt operators.jsonl
        // causes an early failure rather than leaving orphaned blobs and manifest
        // records that the caller cannot retrieve until ACL state is repaired.
        let mut existing: Vec<ProtectedHandle> = if enabled {
            self.read_manifest()?
        } else {
            Vec::new()
        };
        let mut ops: Vec<String> = if enabled {
            self.read_operators()?
        } else {
            Vec::new()
        };

        for entry in entries {
            // Validate payload class first.
            let Some(class) = parse_class(&entry.class) else {
                skipped_count += 1;
                // Compute a placeholder handle that will be stable for the
                // unknown class string so that callers can track it.
                let fallback_hash = blake3::hash(entry.source_path.as_bytes());
                let fallback_hash_str = fallback_hash.to_hex().to_string();
                let class_str = &entry.class;
                outcomes.push(CaptureEntryOutcome {
                    source_path: entry.source_path.clone(),
                    handle: format!("{PROTECTED_HANDLE_PREFIX}{fallback_hash_str}"),
                    content_hash: fallback_hash_str,
                    byte_len: 0,
                    stored: false,
                    diagnostic: Some(EntryDiagnostic {
                        code: "unsupported_payload_class".to_owned(),
                        message: format!(
                            "payload class {class_str:?} is not supported; \
                             recognised classes: transcript, command_output, \
                             patch, task_narrative, report",
                        ),
                    }),
                });
                continue;
            };

            // Check that the source path is a regular file before reading.
            // `fs::read` follows symlinks and reads FIFOs/character-devices to
            // EOF, which can block indefinitely (FIFO) or exhaust memory
            // (`/dev/zero`).  Stat with `symlink_metadata` (no follow) first
            // and require a regular file, emitting `stale_source_path` for
            // anything else so capture continues to the next entry.
            let source_meta = fs::symlink_metadata(&entry.source_path);
            let source_is_regular = source_meta.as_ref().is_ok_and(|m| m.file_type().is_file());
            if !source_is_regular {
                skipped_count += 1;
                let fallback_hash = blake3::hash(entry.source_path.as_bytes());
                let fallback_hash_str = fallback_hash.to_hex().to_string();
                let src = &entry.source_path;
                let not_found = source_meta.is_err();
                outcomes.push(CaptureEntryOutcome {
                    source_path: entry.source_path.clone(),
                    handle: format!("{PROTECTED_HANDLE_PREFIX}{fallback_hash_str}"),
                    content_hash: fallback_hash_str,
                    byte_len: 0,
                    stored: false,
                    diagnostic: Some(EntryDiagnostic {
                        code: "stale_source_path".to_owned(),
                        message: if not_found {
                            format!(
                                "source path {src:?} is not readable; \
                                 the file may have moved or been deleted",
                            )
                        } else {
                            format!(
                                "source path {src:?} is not a regular file; \
                                 FIFOs, symlinks, and device nodes are not accepted",
                            )
                        },
                    }),
                });
                continue;
            }
            // Read source bytes (known-regular file; no symlink follow risk).
            let Ok(bytes) = fs::read(&entry.source_path) else {
                skipped_count += 1;
                let fallback_hash = blake3::hash(entry.source_path.as_bytes());
                let fallback_hash_str = fallback_hash.to_hex().to_string();
                let src = &entry.source_path;
                outcomes.push(CaptureEntryOutcome {
                    source_path: entry.source_path.clone(),
                    handle: format!("{PROTECTED_HANDLE_PREFIX}{fallback_hash_str}"),
                    content_hash: fallback_hash_str,
                    byte_len: 0,
                    stored: false,
                    diagnostic: Some(EntryDiagnostic {
                        code: "stale_source_path".to_owned(),
                        message: format!(
                            "source path {src:?} is not readable; \
                             the file may have moved or been deleted",
                        ),
                    }),
                });
                continue;
            };

            let byte_len = bytes.len() as u64;
            let content_hash = blake3::hash(&bytes).to_hex().to_string();
            let source_path_opt = Some(entry.source_path.as_str());
            let handle = ProtectedHandle::compute_handle(&class, &content_hash, source_path_opt);

            if !enabled {
                // Preview only — do not write anything.
                outcomes.push(CaptureEntryOutcome {
                    source_path: entry.source_path.clone(),
                    handle,
                    content_hash,
                    byte_len,
                    stored: false,
                    diagnostic: None,
                });
                continue;
            }

            // Enabled: store blob + register in manifest.
            //
            // Before deduplication, remove any existing record that carries the
            // same handle string but whose metadata no longer recomputes to that
            // handle (content_hash or source_path was corrupted / tampered with).
            // Retaining a corrupt record would make `already_exists = true` and
            // skip the write, leaving `eg protected get` broken for that handle.
            existing.retain(|h| {
                if h.handle != handle {
                    return true; // different handle — always keep
                }
                // Same handle: require all metadata fields to be internally
                // consistent.  Fields in the handle identity (class, content_hash,
                // source_path) are verified by recomputing the handle; fields
                // outside that identity (byte_len, schema_version) are checked
                // directly against the ground-truth values from the source read.
                // A tampered byte_len or schema_version causes the record to be
                // dropped so re-capture replaces it with a valid copy.
                ProtectedHandle::compute_handle(
                    &h.source_class,
                    &h.content_hash,
                    h.source_path.as_deref(),
                ) == h.handle
                    && h.byte_len == byte_len
                    && h.schema_version == PROTECTED_SCHEMA_VERSION
            });
            let already_exists = existing.iter().any(|h| h.handle == handle);
            if already_exists {
                // Handle already registered: repair the blob if it is missing,
                // non-regular (symlink / FIFO / device), or its hash no longer
                // matches.  Using `symlink_metadata` (no follow) avoids blocking
                // on a FIFO or exhausting memory via a symlink to `/dev/zero`;
                // non-regular paths are unconditionally treated as needing repair
                // so `get` cannot later block on the same path.
                let blob = self.blob_path(&content_hash);
                let needs_repair = match blob.symlink_metadata() {
                    Err(_) => true,                            // missing
                    Ok(m) if !m.file_type().is_file() => true, // symlink / FIFO / device
                    Ok(_) => fs::read(&blob).map_or(true, |existing| {
                        blake3::hash(&existing).to_hex().to_string() != content_hash
                    }),
                };
                if needs_repair {
                    create_private_dir(&self.blobs_dir())?;
                    write_private_file(&blob, &bytes)?;
                }
            } else {
                // New handle: write blob and register.
                create_private_dir(&self.blobs_dir())?;
                write_private_file(&self.blob_path(&content_hash), &bytes)?;

                let record = ProtectedHandle {
                    handle: handle.clone(),
                    schema_version: PROTECTED_SCHEMA_VERSION,
                    source_class: class,
                    source_path: Some(entry.source_path.clone()),
                    content_hash: content_hash.clone(),
                    byte_len,
                    captured_at: captured_at.to_owned(),
                    producer_id: producer_id.to_owned(),
                    producer_version: producer_version.to_owned(),
                };
                existing.push(record);
            }

            stored_count += 1;
            outcomes.push(CaptureEntryOutcome {
                source_path: entry.source_path.clone(),
                handle,
                content_hash,
                byte_len,
                stored: true,
                diagnostic: None,
            });
        }

        if enabled {
            // Persist de-duplicated manifest.
            self.write_manifest(&existing)?;
            // Record producer as an authorised operator.  The operators list was
            // loaded upfront (before any blob/manifest writes) so a corrupt
            // operators.jsonl is caught before the store is mutated.
            ops.push(producer_id.to_owned());
            self.write_operators(&ops)?;
        }

        Ok(CaptureReport {
            enabled,
            entries: outcomes,
            stored_count,
            skipped_count,
        })
    }

    /// Retrieves the raw bytes for `handle`, verifying the content hash.
    ///
    /// Check order (auth before existence disclosure):
    /// 1. Store / manifest absent → [`GetError::RawArtifactModeDisabled`].
    /// 2. `operator` not in authorised set → [`GetError::Unauthorized`].
    /// 3. `handle` does not start with `protected:v1:` → [`GetError::MalformedHandle`].
    /// 4. `handle` not in manifest → [`GetError::PayloadNotFound`].
    /// 5. Blob file absent → [`GetError::MissingProtectedPayload`].
    /// 6. BLAKE3 mismatch → [`GetError::HashMismatch`].
    /// 7. Else → `Ok(bytes)`.
    ///
    /// # Errors
    ///
    /// Returns a [`GetError`] for every failure mode.  The error JSON never
    /// includes raw payload bytes, secrets, or bearer tokens.
    pub fn get(&self, handle: &str, operator: &str) -> Result<Vec<u8>, GetError> {
        // 1. Mode check.
        if !self.is_initialised() {
            return Err(GetError::RawArtifactModeDisabled);
        }

        // 2. Auth check.
        // A corrupt operators.jsonl (any malformed line) fails closed: deny all
        // authorization rather than partially normalizing the ACL.
        let ops = self.read_operators().map_err(|_| GetError::Unauthorized {
            operator: operator.to_owned(),
        })?;
        if !ops.contains(&operator.to_owned()) {
            return Err(GetError::Unauthorized {
                operator: operator.to_owned(),
            });
        }

        // 3. Handle format check.
        // A valid handle is `protected:v1:` + exactly 64 lowercase hex chars.
        // A prefixed-but-malformed value (e.g. `protected:v1:abc`) must be
        // rejected as MalformedHandle rather than falling through to a
        // PayloadNotFound (exit 2) which misclassifies bad caller input.
        let suffix_valid = handle
            .strip_prefix(PROTECTED_HANDLE_PREFIX)
            .is_some_and(is_valid_blake3_hex);
        if !suffix_valid {
            return Err(GetError::MalformedHandle {
                handle: handle.to_owned(),
            });
        }

        // 4. Manifest lookup.
        // `is_initialised()` above confirmed the manifest exists, so a read
        // error here is store corruption — not "mode disabled".
        let manifest = self
            .read_manifest()
            .map_err(|_| GetError::CorruptManifestRecord {
                handle: handle.to_owned(),
            })?;
        let record = manifest
            .iter()
            .find(|h| h.handle == handle)
            .ok_or_else(|| GetError::PayloadNotFound {
                handle: handle.to_owned(),
            })?;

        // 4a. Handle integrity check: the stored `handle` field must agree with
        // the handle recomputed from the record's own class/content_hash/path.
        // A mismatch means the manifest was corrupted or tampered with.
        let expected_handle = ProtectedHandle::compute_handle(
            &record.source_class,
            &record.content_hash,
            record.source_path.as_deref(),
        );
        if expected_handle != record.handle {
            return Err(GetError::CorruptManifestRecord {
                handle: handle.to_owned(),
            });
        }

        // 4b. Validate content_hash format before constructing the blob path.
        // `PathBuf::join` accepts absolute paths and `..` components, so a
        // tampered manifest entry with e.g. `content_hash = "../../../etc/passwd"`
        // could escape the blobs directory.  A valid BLAKE3 hex is exactly 64
        // lowercase hex characters, so anything else is corrupt.
        if !is_valid_blake3_hex(&record.content_hash) {
            return Err(GetError::CorruptManifestRecord {
                handle: handle.to_owned(),
            });
        }

        // 5. Blob presence and safety check.
        //
        // `symlink_metadata` inspects the path WITHOUT following symlinks, so a
        // tampered store that replaced a blob with a symlink to `/dev/zero` (or a
        // FIFO / device node) is caught here rather than after an unbounded read.
        // Only regular files are accepted.
        let blob_path = self.blob_path(&record.content_hash);
        let expected_path = blob_path.display().to_string();
        let blob_meta =
            blob_path
                .symlink_metadata()
                .map_err(|_| GetError::MissingProtectedPayload {
                    handle: handle.to_owned(),
                    expected_path: expected_path.clone(),
                })?;
        if !blob_meta.file_type().is_file() {
            return Err(GetError::MissingProtectedPayload {
                handle: handle.to_owned(),
                expected_path,
            });
        }
        // Pre-check file size against the manifest byte_len before reading to
        // avoid loading a truncated or unexpectedly large file into memory.
        if blob_meta.len() != record.byte_len {
            return Err(GetError::HashMismatch {
                handle: handle.to_owned(),
                expected: record.content_hash.clone(),
                actual: format!(
                    "(blob size {} B does not match manifest byte_len {} B — not read)",
                    blob_meta.len(),
                    record.byte_len
                ),
            });
        }

        // Read blob.
        let bytes = fs::read(&blob_path).map_err(|_| GetError::MissingProtectedPayload {
            handle: handle.to_owned(),
            expected_path: blob_path.display().to_string(),
        })?;

        // 6. Hash verification.
        let actual_hash = blake3::hash(&bytes).to_hex().to_string();
        if actual_hash != record.content_hash {
            return Err(GetError::HashMismatch {
                handle: handle.to_owned(),
                expected: record.content_hash.clone(),
                actual: actual_hash,
            });
        }

        Ok(bytes)
    }

    /// Lists all protected handles (metadata only — no raw bytes).
    ///
    /// Returns an empty list when the store is not yet initialised.
    ///
    /// # Errors
    ///
    /// Returns an error on manifest I/O failure.
    pub fn list(&self) -> io::Result<Vec<ProtectedHandle>> {
        if !self.is_initialised() {
            return Ok(Vec::new());
        }
        self.read_manifest()
    }
}

// ── Unit tests (RED then GREEN) ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn fixed_ts() -> &'static str {
        "2026-06-18T00:00:00Z"
    }

    // ── Unit: handle identity ──────────────────────────────────────────────────

    #[test]
    fn handle_id_is_stable_and_excludes_capture_time() {
        let hash = "abc123";
        let h1 = ProtectedHandle::compute_handle(
            &ProtectedPayloadClass::Transcript,
            hash,
            Some("src/foo.txt"),
        );
        let h2 = ProtectedHandle::compute_handle(
            &ProtectedPayloadClass::Transcript,
            hash,
            Some("src/foo.txt"),
        );
        assert_eq!(h1, h2, "handles must be deterministic");
        assert!(
            h1.starts_with(PROTECTED_HANDLE_PREFIX),
            "handle must start with protected:v1:"
        );
    }

    #[test]
    fn handle_id_differs_by_class_or_content_or_path() {
        let hash = "abc123";
        let base = ProtectedHandle::compute_handle(
            &ProtectedPayloadClass::Transcript,
            hash,
            Some("src/foo.txt"),
        );
        let diff_class = ProtectedHandle::compute_handle(
            &ProtectedPayloadClass::CommandOutput,
            hash,
            Some("src/foo.txt"),
        );
        let diff_hash = ProtectedHandle::compute_handle(
            &ProtectedPayloadClass::Transcript,
            "def456",
            Some("src/foo.txt"),
        );
        let diff_path = ProtectedHandle::compute_handle(
            &ProtectedPayloadClass::Transcript,
            hash,
            Some("src/bar.txt"),
        );
        let no_path =
            ProtectedHandle::compute_handle(&ProtectedPayloadClass::Transcript, hash, None);
        assert_ne!(base, diff_class);
        assert_ne!(base, diff_hash);
        assert_ne!(base, diff_path);
        assert_ne!(base, no_path);
    }

    #[test]
    fn manifest_roundtrip_is_canonical_sorted() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let entries = vec![
            CaptureEntry {
                class: "report".to_owned(),
                source_path: dir.path().join("b.txt").to_string_lossy().into_owned(),
            },
            CaptureEntry {
                class: "transcript".to_owned(),
                source_path: dir.path().join("a.txt").to_string_lossy().into_owned(),
            },
        ];
        for e in &entries {
            fs::write(&e.source_path, &e.source_path).unwrap();
        }
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let manifest_content = fs::read_to_string(dir.path().join("manifest.jsonl")).unwrap();
        let lines: Vec<&str> = manifest_content.lines().filter(|l| !l.is_empty()).collect();
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        assert_eq!(
            lines, sorted,
            "manifest lines must be lexicographically sorted"
        );
    }

    #[test]
    fn unknown_payload_class_string_is_rejected() {
        let class = parse_class("llm_inference_log");
        assert!(class.is_none(), "unknown class must return None");
        let known = parse_class("transcript");
        assert!(known.is_some());
    }

    // ── Unit: capture disabled ─────────────────────────────────────────────────

    #[test]
    fn capture_disabled_writes_nothing_returns_hashes() {
        let store_dir = tempdir().unwrap();
        let src_dir = tempdir().unwrap();
        let src = src_dir.path().join("payload.txt");
        fs::write(&src, b"hello world").unwrap();

        let store = ProtectedStore::new(store_dir.path());
        let entries = vec![CaptureEntry {
            class: "transcript".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];
        let report = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), false)
            .unwrap();

        assert!(!report.enabled);
        assert_eq!(report.stored_count, 0);
        assert!(
            !store_dir.path().join("blobs").exists(),
            "blobs dir must not be created in disabled mode"
        );
        assert!(
            !store_dir.path().join("manifest.jsonl").exists(),
            "manifest must not be created in disabled mode"
        );
        assert!(!report.entries[0].content_hash.is_empty());
        assert!(
            report.entries[0]
                .handle
                .starts_with(PROTECTED_HANDLE_PREFIX)
        );
    }

    // ── Unit: get errors ───────────────────────────────────────────────────────

    #[test]
    fn get_disabled_returns_raw_artifact_mode_disabled() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let err = store.get("protected:v1:abc123", "op-1").unwrap_err();
        assert_eq!(err.code(), "raw_artifact_mode_disabled");
        assert!(!err.to_json().contains("abc123") || err.to_json().contains("code"));
    }

    #[test]
    fn get_unauthorized_operator_rejected() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        // Initialise store by writing a dummy manifest and operators file.
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(dir.path().join("manifest.jsonl"), "\n").unwrap();
        store.write_operators(&["op-allowed".to_owned()]).unwrap();

        let err = store.get("protected:v1:abc", "op-other").unwrap_err();
        assert_eq!(err.code(), "unauthorized");
        // Must not echo operator token in a way that leaks secrets.
        let json = err.to_json();
        assert!(json.contains("unauthorized"));
    }

    #[test]
    fn get_malformed_handle_rejected() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        fs::write(dir.path().join("manifest.jsonl"), "\n").unwrap();
        store.write_operators(&["op-1".to_owned()]).unwrap();

        let err = store.get("notaprotectedhandle:abc", "op-1").unwrap_err();
        assert_eq!(err.code(), "malformed_handle");
    }

    #[test]
    fn unauthorized_operator_not_echoed_in_error_json() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(dir.path().join("manifest.jsonl"), "\n").unwrap();
        store.write_operators(&["op-allowed".to_owned()]).unwrap();

        let err = store
            .get("protected:v1:abc", "bearer-secret-token")
            .unwrap_err();
        assert_eq!(err.code(), "unauthorized");
        let json = err.to_json();
        assert!(json.contains("unauthorized"), "code must be present");
        assert!(
            !json.contains("bearer-secret-token"),
            "operator must not be echoed in error JSON"
        );
    }

    #[test]
    fn corrupt_manifest_handle_rejected() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"hello world").unwrap();
        let entries = vec![CaptureEntry {
            class: "transcript".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        // Tamper with the handle field in manifest.jsonl.
        let manifest_path = dir.path().join("manifest.jsonl");
        let content = fs::read_to_string(&manifest_path).unwrap();
        let mut rec: serde_json::Value =
            serde_json::from_str(content.trim()).expect("manifest must parse");
        let tampered =
            "protected:v1:0000000000000000000000000000000000000000000000000000000000000000";
        rec["handle"] = serde_json::json!(tampered);
        fs::write(&manifest_path, serde_json::to_string(&rec).unwrap() + "\n").unwrap();

        let err = store.get(tampered, "op-1").unwrap_err();
        assert_eq!(
            err.code(),
            "corrupt_manifest_record",
            "tampered handle must surface store corruption"
        );
    }

    #[test]
    fn capture_repairs_missing_blob() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"repair test content").unwrap();
        let entries = vec![CaptureEntry {
            class: "transcript".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        // Initial capture.
        let report = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let content_hash = report.entries[0].content_hash.clone();
        let handle = report.entries[0].handle.clone();

        // Delete the blob.
        fs::remove_file(dir.path().join("blobs").join(&content_hash)).unwrap();
        assert!(
            !dir.path().join("blobs").join(&content_hash).exists(),
            "blob deleted"
        );

        // Re-capture with source still present — should repair.
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        assert!(
            dir.path().join("blobs").join(&content_hash).exists(),
            "blob must be repaired by re-capture"
        );

        // get() must now succeed.
        let bytes = store.get(&handle, "op-1").expect("get after repair");
        assert_eq!(bytes, b"repair test content");
    }

    #[test]
    fn capture_corrupt_manifest_returns_error() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        fs::create_dir_all(dir.path()).unwrap();
        // Write a corrupt (non-JSON) manifest.
        fs::write(dir.path().join("manifest.jsonl"), "this is not json\n").unwrap();

        let src = dir.path().join("payload.txt");
        fs::write(&src, b"hello").unwrap();
        let entries = vec![CaptureEntry {
            class: "transcript".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        let result = store.capture(&entries, "op-1", "0.1.0", fixed_ts(), true);
        assert!(
            result.is_err(),
            "capture must fail when manifest is corrupt"
        );
    }

    #[test]
    fn malformed_handle_not_echoed_in_error_json() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        fs::write(dir.path().join("manifest.jsonl"), "\n").unwrap();
        store.write_operators(&["op-1".to_owned()]).unwrap();

        // Simulate accidentally passing a bearer token in the handle position.
        let err = store
            .get("Bearer sk-secret-handle-value", "op-1")
            .unwrap_err();
        assert_eq!(err.code(), "malformed_handle");
        let json = err.to_json();
        assert!(
            !json.contains("sk-secret-handle-value"),
            "handle value must not be echoed in error JSON: {json}"
        );
    }

    #[test]
    fn capture_rejects_empty_producer_id_at_store_api() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"hello").unwrap();
        let entries = vec![CaptureEntry {
            class: "transcript".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        let result = store.capture(&entries, "", "0.1.0", fixed_ts(), true);
        assert!(result.is_err(), "empty producer_id must be rejected");
        let err = result.unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "must return InvalidInput for empty producer_id"
        );

        // Whitespace-only producer should also be rejected.
        let result2 = store.capture(&entries, "   ", "0.1.0", fixed_ts(), true);
        assert!(
            result2.is_err(),
            "whitespace-only producer_id must be rejected"
        );
    }

    #[test]
    fn capture_repairs_corrupted_blob() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"original content").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        // Initial capture.
        let report = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let content_hash = report.entries[0].content_hash.clone();
        let handle = report.entries[0].handle.clone();

        // Corrupt the blob (file exists but content is wrong).
        fs::write(dir.path().join("blobs").join(&content_hash), b"corrupted").unwrap();

        // Re-capture with source still present — must repair corrupted blob.
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        // get() must now return the original content.
        let bytes = store
            .get(&handle, "op-1")
            .expect("get after corruption repair");
        assert_eq!(bytes, b"original content");
    }

    #[test]
    fn get_manifest_read_error_returns_corrupt_manifest_record() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        // Initialise with a valid manifest so is_initialised() returns true.
        fs::write(dir.path().join("manifest.jsonl"), "\n").unwrap();
        store.write_operators(&["op-1".to_owned()]).unwrap();

        // Overwrite manifest with invalid JSON so read_manifest() fails.
        fs::write(dir.path().join("manifest.jsonl"), "not valid json\n").unwrap();

        let err = store
            .get(
                "protected:v1:0000000000000000000000000000000000000000000000000000000000000000",
                "op-1",
            )
            .unwrap_err();
        assert_eq!(
            err.code(),
            "corrupt_manifest_record",
            "manifest read failure must return corrupt_manifest_record, not raw_artifact_mode_disabled"
        );
    }

    #[test]
    fn get_rejects_content_hash_path_traversal() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"path traversal test").unwrap();
        let entries = vec![CaptureEntry {
            class: "patch".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        // Tamper: set content_hash to a path-traversal string and compute a
        // matching handle so the integrity check passes.
        let manifest_path = dir.path().join("manifest.jsonl");
        let content = fs::read_to_string(&manifest_path).unwrap();
        let mut rec: serde_json::Value = serde_json::from_str(content.trim()).expect("parse");
        let traversal_hash =
            "../../../etc/passwd/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        rec["content_hash"] = serde_json::json!(traversal_hash);
        // Re-compute handle so integrity check passes.
        let class: crate::protected::ProtectedPayloadClass =
            serde_json::from_value(rec["source_class"].clone()).unwrap();
        let new_handle =
            ProtectedHandle::compute_handle(&class, traversal_hash, rec["source_path"].as_str());
        rec["handle"] = serde_json::json!(&new_handle);
        fs::write(&manifest_path, serde_json::to_string(&rec).unwrap() + "\n").unwrap();

        let err = store.get(&new_handle, "op-1").unwrap_err();
        assert_eq!(
            err.code(),
            "corrupt_manifest_record",
            "path-traversal content_hash must be rejected before building blob path"
        );
    }

    #[test]
    fn malformed_operator_record_not_authorized() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        // Write operators.jsonl with one valid JSON string and one bare/unquoted line.
        fs::write(
            dir.path().join("operators.jsonl"),
            "\"op-valid\"\nbare-not-json-string\n",
        )
        .unwrap();
        fs::write(dir.path().join("manifest.jsonl"), "\n").unwrap();

        // Any malformed line in operators.jsonl causes ALL authorization to fail.
        // This is stricter than silently skipping malformed lines: a partially
        // corrupt ACL file must deny everyone, not just unknown callers.
        let err_valid = store.get("protected:v1:abc", "op-valid").unwrap_err();
        assert_eq!(
            err_valid.code(),
            "unauthorized",
            "corrupt operators.jsonl must deny even operators that appear on valid lines"
        );

        // The bare line must also NOT grant access.
        let err_bare = store
            .get("protected:v1:abc", "bare-not-json-string")
            .unwrap_err();
        assert_eq!(
            err_bare.code(),
            "unauthorized",
            "bare/unquoted operator record must not grant access"
        );
    }

    #[test]
    fn capture_replaces_corrupt_manifest_record_on_recapture() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"real content").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        // Initial capture produces a valid record.
        let report = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let real_hash = report.entries[0].content_hash.clone();
        let handle = report.entries[0].handle.clone();

        // Tamper with the manifest: corrupt content_hash while keeping the handle
        // field unchanged (so handle != compute_handle(class, fake_hash, path)).
        let manifest_path = dir.path().join("manifest.jsonl");
        let content = fs::read_to_string(&manifest_path).unwrap();
        let mut rec: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        let fake_hash = "a".repeat(64);
        rec["content_hash"] = serde_json::json!(&fake_hash);
        fs::write(&manifest_path, serde_json::to_string(&rec).unwrap() + "\n").unwrap();

        // get() must fail now — integrity check catches the tampered record.
        let err = store.get(&handle, "op-1").unwrap_err();
        assert_eq!(err.code(), "corrupt_manifest_record");

        // Re-capture while the source is still present — must replace the corrupt
        // record with a valid one.
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        // get() must now succeed and return the original bytes.
        let bytes = store
            .get(&handle, "op-1")
            .expect("get after corrupt-record repair");
        assert_eq!(bytes, b"real content");

        // Manifest must contain only one record for this handle (no duplicates).
        let handles = store.list().unwrap();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].content_hash, real_hash);
    }

    #[test]
    fn capture_fails_before_writes_when_operators_file_is_corrupt() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        // Write a corrupt operators.jsonl (bare non-JSON line).
        fs::write(dir.path().join("operators.jsonl"), "not-a-json-string\n").unwrap();
        // Write a valid (empty) manifest so the store appears initialised.
        fs::write(dir.path().join("manifest.jsonl"), "\n").unwrap();

        let src = dir.path().join("payload.txt");
        fs::write(&src, b"hello").unwrap();
        let entries = vec![CaptureEntry {
            class: "transcript".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        // Capture must fail early — before writing any blobs.
        let result = store.capture(&entries, "op-1", "0.1.0", fixed_ts(), true);
        assert!(
            result.is_err(),
            "corrupt operators.jsonl must abort capture"
        );

        // No blobs must have been written.
        assert!(
            !dir.path().join("blobs").exists(),
            "blobs directory must not be created when operators.jsonl is corrupt"
        );
    }

    #[test]
    fn empty_operator_identity_is_rejected_by_read_operators() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        // Write operators.jsonl containing a valid JSON empty string.
        fs::write(dir.path().join("operators.jsonl"), "\"\"\n").unwrap();
        fs::write(dir.path().join("manifest.jsonl"), "\n").unwrap();

        // An empty operator identity must be rejected, not authorized.
        let err = store.get("protected:v1:abc", "").unwrap_err();
        assert_eq!(
            err.code(),
            "unauthorized",
            "empty operator identity must be denied even if present in operators.jsonl"
        );
    }

    #[test]
    fn capture_rejects_non_rfc3339_captured_at() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"hello").unwrap();
        let entries = vec![CaptureEntry {
            class: "transcript".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        let result = store.capture(&entries, "op-1", "0.1.0", "not-a-date", true);
        assert!(result.is_err(), "non-RFC3339 captured_at must be rejected");
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput,
            "must return InvalidInput for invalid captured_at"
        );

        // Disabled mode must not validate captured_at (preview never persists).
        let result2 = store.capture(&entries, "op-1", "0.1.0", "not-a-date", false);
        assert!(
            result2.is_ok(),
            "non-RFC3339 captured_at must be accepted in disabled (preview) mode"
        );
    }

    #[test]
    fn capture_replaces_tampered_byte_len_or_schema_version() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"test content").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        // Initial capture.
        let report = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let real_byte_len = report.entries[0].byte_len;

        // Tamper: corrupt byte_len while keeping handle valid (integrity check passes).
        let manifest_path = dir.path().join("manifest.jsonl");
        let content = fs::read_to_string(&manifest_path).unwrap();
        let mut rec: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        rec["byte_len"] = serde_json::json!(9999u64);
        fs::write(&manifest_path, serde_json::to_string(&rec).unwrap() + "\n").unwrap();

        // list() reports the tampered byte_len.
        let before = store.list().unwrap();
        assert_eq!(before[0].byte_len, 9999, "tampered record is in manifest");

        // Re-capture with source available — must replace the tampered record.
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        // list() must now report the correct byte_len.
        let after = store.list().unwrap();
        assert_eq!(
            after.len(),
            1,
            "manifest must have exactly one record after repair"
        );
        assert_eq!(
            after[0].byte_len, real_byte_len,
            "byte_len must be correct after repair"
        );
    }

    #[test]
    fn get_handle_with_short_suffix_is_malformed_not_not_found() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        fs::write(dir.path().join("manifest.jsonl"), "\n").unwrap();
        store.write_operators(&["op-1".to_owned()]).unwrap();

        // `protected:v1:abc` has the right prefix but a 3-char suffix (not 64 hex).
        let err = store.get("protected:v1:abc", "op-1").unwrap_err();
        assert_eq!(
            err.code(),
            "malformed_handle",
            "prefixed-but-short handle must be MalformedHandle, not PayloadNotFound"
        );
    }

    #[cfg(unix)]
    #[test]
    fn blob_symlink_is_rejected_before_read() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"symlink test").unwrap();
        let entries = vec![CaptureEntry {
            class: "transcript".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        let report = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let content_hash = report.entries[0].content_hash.clone();
        let handle = report.entries[0].handle.clone();

        // Replace the real blob with a symlink to /dev/null.
        let blob_path = dir.path().join("blobs").join(&content_hash);
        fs::remove_file(&blob_path).unwrap();
        std::os::unix::fs::symlink("/dev/null", &blob_path).unwrap();

        // get() must reject the symlink before reading, not follow it.
        let err = store.get(&handle, "op-1").unwrap_err();
        assert_eq!(
            err.code(),
            "missing_protected_payload",
            "symlink blob must be rejected as MissingProtectedPayload"
        );
    }

    // ── Unit: write_private_file symlink safety ────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn write_private_file_replaces_symlink_not_target() {
        let dir = tempdir().unwrap();
        // Create a "sensitive" file outside the store that a symlink might
        // redirect to.
        let target = dir.path().join("sensitive.txt");
        fs::write(&target, b"original sensitive data").unwrap();

        // Place a symlink at the intended write path pointing at the sensitive
        // file.
        let write_path = dir.path().join("store_file.txt");
        std::os::unix::fs::symlink(&target, &write_path).unwrap();

        // write_private_file must replace the symlink, not follow it, so the
        // sensitive file is untouched and `store_file.txt` becomes a regular
        // file containing the new data.
        write_private_file(&write_path, b"new data").unwrap();

        // The symlink itself was replaced — write_path is now a regular file.
        let meta = write_path.symlink_metadata().unwrap();
        assert!(
            meta.file_type().is_file(),
            "write path must be a regular file after write, not a symlink"
        );
        assert_eq!(fs::read(&write_path).unwrap(), b"new data");

        // The sensitive file must not have been overwritten.
        assert_eq!(
            fs::read(&target).unwrap(),
            b"original sensitive data",
            "symlink target must not be overwritten"
        );
    }

    // ── Unit: write_manifest handle dedup (first-capture-wins) ────────────────

    #[test]
    fn write_manifest_deduplicates_by_handle_first_capture_wins() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"deduplicate test").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        // First capture — establishes producer "op-first".
        store
            .capture(&entries, "op-first", "0.1.0", "2026-01-01T00:00:00Z", true)
            .unwrap();

        // Second capture with a different producer and timestamp.
        store
            .capture(&entries, "op-second", "0.1.0", "2026-06-18T00:00:00Z", true)
            .unwrap();

        let handles = store.list().unwrap();
        assert_eq!(
            handles.len(),
            1,
            "manifest must not contain duplicate handles"
        );
        assert_eq!(
            handles[0].producer_id, "op-first",
            "first-capture-wins: producer_id of earliest capture must be retained"
        );
        assert_eq!(
            handles[0].captured_at, "2026-01-01T00:00:00Z",
            "first-capture-wins: captured_at of earliest capture must be retained"
        );
    }

    // ── Unit: source file type check ──────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn capture_rejects_fifo_source_as_stale_source_path() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());

        // Create a FIFO (named pipe) as the source path.
        let fifo_path = dir.path().join("source.fifo");
        nix_mkfifo(&fifo_path);

        let entries = vec![CaptureEntry {
            class: "transcript".to_owned(),
            source_path: fifo_path.to_string_lossy().into_owned(),
        }];

        // Disabled mode: must emit stale_source_path diagnostic (not block).
        let report = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), false)
            .unwrap();
        assert_eq!(
            report.entries[0].diagnostic.as_ref().unwrap().code,
            "stale_source_path"
        );
        assert!(!report.entries[0].stored);

        // Enabled mode: same — must not block on the FIFO.
        let report2 = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        assert_eq!(
            report2.entries[0].diagnostic.as_ref().unwrap().code,
            "stale_source_path",
            "FIFO source must produce stale_source_path diagnostic"
        );
        assert!(!report2.entries[0].stored);
        // No blobs must have been written.
        assert!(
            !dir.path().join("blobs").exists(),
            "no blobs for FIFO source"
        );
    }

    // ── Unit: blob repair replaces symlink ────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn capture_repair_replaces_symlink_blob() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"repair symlink blob").unwrap();
        let entries = vec![CaptureEntry {
            class: "transcript".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        // Initial capture.
        let report = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let content_hash = report.entries[0].content_hash.clone();
        let handle = report.entries[0].handle.clone();

        // Replace the blob with a symlink to /dev/null.
        let blob_path = dir.path().join("blobs").join(&content_hash);
        fs::remove_file(&blob_path).unwrap();
        std::os::unix::fs::symlink("/dev/null", &blob_path).unwrap();

        // Re-capture while source is still present — must repair (replace
        // symlink with real blob via write-to-temp-and-rename).
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        // Blob must now be a regular file containing the original bytes.
        let meta = blob_path.symlink_metadata().unwrap();
        assert!(
            meta.file_type().is_file(),
            "blob must be a regular file after repair"
        );
        let bytes = store
            .get(&handle, "op-1")
            .expect("get after symlink repair");
        assert_eq!(bytes, b"repair symlink blob");
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Creates a FIFO (named pipe) at `path` using the `mkfifo` shell command.
    #[cfg(unix)]
    fn nix_mkfifo(path: &std::path::Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo command must be available on Unix");
        assert!(status.success(), "mkfifo failed");
    }
}
