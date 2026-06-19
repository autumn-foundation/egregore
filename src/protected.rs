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

/// Maximum byte size accepted when reading store metadata files (`manifest.jsonl`,
/// `operators.jsonl`).  Prevents memory exhaustion from unexpectedly large or
/// device-backed files (e.g. a symlink to `/dev/zero` that slips past the
/// regular-file check on a platform without `symlink_metadata`).
///
/// Also reused by the CLI capture path to bound `--manifest` reads.
pub(crate) const MAX_STORE_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB

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

    /// Builds the documented `error.detail` object for this variant.
    ///
    /// Constructed per-variant (rather than serializing `self`) so `detail` is
    /// always the flat object promised by `docs/cli/protected-artifacts.md`
    /// (e.g. `error.detail.handle`), not serde's externally-tagged form which
    /// would nest fields under the variant name or emit a bare string for unit
    /// variants.  Secret-bearing fields (`operator`, the raw `handle` argument)
    /// are intentionally omitted so they never reach stderr.
    fn detail(&self) -> serde_json::Value {
        match self {
            Self::RawArtifactModeDisabled
            | Self::Unauthorized { .. }
            | Self::MalformedHandle { .. } => serde_json::json!({}),
            Self::PayloadNotFound { handle } | Self::CorruptManifestRecord { handle } => {
                serde_json::json!({ "handle": handle })
            }
            Self::MissingProtectedPayload {
                handle,
                expected_path,
            } => serde_json::json!({ "handle": handle, "expected_path": expected_path }),
            Self::HashMismatch {
                handle,
                expected,
                actual,
            } => serde_json::json!({ "handle": handle, "expected": expected, "actual": actual }),
        }
    }

    /// Serialises the error as a machine-readable JSON envelope.
    ///
    /// Never includes raw payload bytes, secrets, or bearer tokens.
    ///
    /// # Panics
    ///
    /// Panics only if serialising the envelope fails, which cannot happen for
    /// this well-formed JSON value.
    #[must_use]
    pub fn to_json(&self) -> String {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": self.code(),
                "detail": self.detail(),
            }
        });
        serde_json::to_string(&envelope).expect("envelope serialisation is infallible")
    }
}

/// Error from [`ProtectedStore::get_to_writer`], separating a retrieval or
/// integrity failure from a failure writing the verified bytes to the sink.
///
/// The distinction lets the CLI emit a `get`-style diagnostic envelope for a
/// retrieval failure but an `output_write_error` envelope when the destination
/// (a `--out` file or stdout) cannot be written.
#[derive(Debug)]
pub enum GetStreamError {
    /// Retrieval or integrity verification failed; nothing was written to `out`.
    Get(GetError),
    /// Writing the verified bytes to the destination sink failed.
    Output(io::Error),
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
/// Returns an error when serialized metadata `content` would exceed the read
/// cap that [`ProtectedStore::read_manifest`] / `read_operators` enforce.
///
/// Without this guard, a large-but-successful enabled capture could write a
/// `manifest.jsonl` (or `operators.jsonl`) bigger than [`MAX_STORE_FILE_BYTES`],
/// after which every subsequent `list`, `get`, and `capture` would reject the
/// store as corrupt — a successful write silently rendering the store
/// unreadable.  Failing the write keeps read and write consistent and leaves
/// the prior (readable) file in place, since the caller writes via
/// temp-then-rename only after this check passes.
fn check_within_read_cap(content: &str, what: &str) -> io::Result<()> {
    if content.len() as u64 > MAX_STORE_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{what} would be {} B, exceeding the {} B read cap; \
                 refusing to write a file that later reads would reject",
                content.len(),
                MAX_STORE_FILE_BYTES
            ),
        ));
    }
    Ok(())
}

/// Verifies `path` is a real directory or genuinely absent (no-follow).
///
/// Rejects a symlink, regular file, FIFO, or device at `path` with an
/// `InvalidData` error so a tampered or shared store cannot redirect writes or
/// reads outside the intended boundary via a symlinked directory component.
/// `NotFound` is allowed: the directory is created lazily on first write.
fn require_real_dir_or_absent(path: &Path, what: &str) -> io::Result<()> {
    match path.symlink_metadata() {
        Ok(m) if m.file_type().is_dir() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{what} at {} is not a real directory (symlink/file/device); \
                 the store may have been tampered with",
                path.display()
            ),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

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

    // Remove any pre-existing temp file (including symlinks) before opening.
    // This prevents a pre-placed symlink at `tmp_path` from being followed by
    // the open below.  Errors are ignored: if the path does not exist that is
    // fine; if removal fails for another reason, the `create_new` open below
    // will fail instead.
    let _ = fs::remove_file(&tmp_path);

    // Stage the bytes into the temp file and atomically rename into place.  Any
    // error after the temp file is created (a failed `write_all` on a full or
    // interrupted filesystem, or a failed `rename`) must not leak the staged
    // temp file: for protected blob writes it holds raw payload bytes, and
    // leaving `<store>/blobs/.<hash>.wip` behind would persist payload bytes
    // that no manifest record references even though capture reports failure.
    let staged = stage_and_rename(&tmp_path, path, data);
    if staged.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    staged
}

/// Creates `tmp_path` (`O_CREAT`|`O_EXCL`, mode 0o600 on Unix), writes `data`,
/// and renames it over `path`.  The caller removes `tmp_path` on any error.
fn stage_and_rename(tmp_path: &Path, path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut f = open_private_create_new(tmp_path)?;
    f.write_all(data)?;
    rename_into_place(tmp_path, path)
}

/// Opens `path` for writing with `O_CREAT|O_EXCL` (mode 0o600 on Unix).
///
/// `create_new` makes the open fail rather than follow a symlink that appears at
/// `path` between an earlier `remove_file` and this call.
fn open_private_create_new(path: &Path) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }
}

/// Renames `tmp_path` onto `path`, replacing any existing file without losing it
/// on failure.
///
/// On Unix this is a single `rename(2)`, which atomically replaces the
/// destination entry (including a symlink at that path) without following it.
///
/// On Windows `std::fs::rename` fails when the destination already exists, and a
/// plain remove-then-rename would destroy the previous file if the rename then
/// failed (AV/permission interference) — leaving an initialized store unreadable
/// or missing payloads.  So any existing destination is first moved to a
/// sibling backup; on a successful rename the backup is deleted, and on failure
/// the backup is restored, so the previous file is never lost.  The caller
/// removes `tmp_path` on error.
pub(crate) fn rename_into_place(tmp_path: &Path, path: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(tmp_path, path)
    }
    #[cfg(windows)]
    {
        if fs::symlink_metadata(path).is_err() {
            // No existing destination — a straight rename suffices.
            return fs::rename(tmp_path, path);
        }
        // Move the existing destination aside to a sibling backup first.
        let backup = {
            let mut name = std::ffi::OsString::from(".");
            name.push(
                path.file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("file")),
            );
            name.push(".eg-bak");
            path.parent()
                .map_or_else(|| PathBuf::from(&name), |p| p.join(&name))
        };
        let _ = fs::remove_file(&backup);
        fs::rename(path, &backup)?;
        match fs::rename(tmp_path, path) {
            Ok(()) => {
                let _ = fs::remove_file(&backup);
                Ok(())
            }
            Err(e) => {
                // Restore the previous destination file.
                let _ = fs::rename(&backup, path);
                Err(e)
            }
        }
    }
}

/// Opens `path` for reading and binds validation to the opened descriptor.
///
/// On Unix the open sets `O_NOFOLLOW` (fail if the final component is a symlink)
/// and `O_NONBLOCK` (do not block opening a FIFO before it can be rejected),
/// then `fstat`s the descriptor and requires a regular file.  This closes the
/// TOCTOU window between an earlier path-based stat and this open: another
/// process cannot swap the path for a symlink, FIFO, or device and have bytes
/// read through it.  Regular files ignore `O_NONBLOCK` for reads, so the
/// subsequent streaming reads behave normally.
fn open_source_checked(path: &Path) -> io::Result<fs::File> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = fs::File::open(path)?;

    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a regular file",
        ));
    }
    Ok(file)
}

/// Returns `true` iff `path` is a regular file of exactly `expected_len` bytes
/// whose BLAKE3 hash equals `expected_hash`, validating everything on a SINGLE
/// opened descriptor (no-follow).
///
/// Checking the length on the opened descriptor (`fstat`), not a separate path
/// stat, means a blob swapped for a much larger regular file after an earlier
/// stat cannot be hashed to EOF before the size guard fires — the size mismatch
/// is detected on the same fd that would be hashed, so there is no huge-file
/// stall.
fn blob_matches(path: &Path, expected_hash: &str, expected_len: u64) -> bool {
    let Ok(mut f) = open_source_checked(path) else {
        return false;
    };
    match f.metadata() {
        Ok(m) if m.len() == expected_len => {}
        _ => return false, // size mismatch (or stat error) on the opened fd — do not hash
    }
    let mut hasher = blake3::Hasher::new();
    if io::copy(&mut f, &mut hasher).is_err() {
        return false;
    }
    hasher.finalize().to_hex().to_string() == expected_hash
}

/// Reads `path` into a `String`, binding no-follow, regular-file, and size-cap
/// validation to the opened descriptor.
///
/// Opens once via [`open_source_checked`] (rejecting a symlink/FIFO/device),
/// then reads at most `cap` bytes from THAT descriptor, so a path swapped after
/// any earlier stat cannot smuggle in a larger or non-regular file between the
/// check and the read.  Returns `InvalidData` if the content exceeds `cap`.
pub(crate) fn read_capped_regular_file(path: &Path, cap: u64) -> io::Result<String> {
    use std::io::Read as _;
    let f = open_source_checked(path)?;
    let mut s = String::new();
    // `take(cap + 1)` bounds the read; content longer than `cap` is rejected.
    f.take(cap + 1).read_to_string(&mut s)?;
    if s.len() as u64 > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds the maximum allowed size",
        ));
    }
    Ok(s)
}

/// Streams `source` through a BLAKE3 hasher without buffering the whole file,
/// returning its `(hex_hash, byte_len)`.  Used by the capture preview path,
/// which must not write anything to disk.
fn hash_source_streaming(source: &Path) -> io::Result<(String, u64)> {
    let mut src = open_source_checked(source)?;
    // blake3::Hasher implements io::Write, so io::copy streams the file through
    // it in bounded-size chunks and returns the byte count.
    let mut hasher = blake3::Hasher::new();
    let byte_len = io::copy(&mut src, &mut hasher)?;
    Ok((hasher.finalize().to_hex().to_string(), byte_len))
}

/// Distinguishes why [`stream_source_to_temp`] failed.
///
/// Capture must skip an unreadable *source* with a per-entry `stale_source_path`
/// diagnostic, but a *store* I/O error while staging the temp blob (disk full,
/// permissions, a stale directory at the temp path) must fail the whole capture
/// rather than be misreported as a stale source.
enum StageError {
    /// Reading the source file failed — treat as a per-entry stale source.
    /// (The specific error is not surfaced; `stale_read_outcome` builds the
    /// per-entry diagnostic.)
    Source,
    /// Creating or writing the temp blob in the store failed — fail the capture.
    Store(io::Error),
}

/// Streams `source` into a temp file under `blobs_dir`, hashing as it goes.
///
/// Returns `(hex_hash, byte_len, temp_path)`; the caller either renames the temp
/// over the content-addressed blob path (via [`rename_into_place`]) or removes
/// it.  Reading in bounded chunks keeps memory flat regardless of payload size,
/// so a multi-GB transcript or CI log cannot exhaust memory.  The temp file is
/// removed on any streaming error.
fn stream_source_to_temp(
    source: &Path,
    blobs_dir: &Path,
) -> Result<(String, u64, PathBuf), StageError> {
    use std::io::{Read as _, Write as _};

    let tmp_path = blobs_dir.join(".incoming.wip");
    // Clear any stale temp from a previously interrupted capture before the
    // no-follow `create_new` open below.
    let _ = fs::remove_file(&tmp_path);

    let streamed = (|| -> Result<(String, u64), StageError> {
        let mut src = open_source_checked(source).map_err(|_| StageError::Source)?;
        let mut tmp = open_private_create_new(&tmp_path).map_err(StageError::Store)?;
        let mut hasher = blake3::Hasher::new();
        // Heap-allocated so the 64 KiB chunk buffer does not sit on the stack.
        let mut buf = vec![0u8; 64 * 1024];
        let mut byte_len: u64 = 0;
        loop {
            let n = src.read(&mut buf).map_err(|_| StageError::Source)?;
            if n == 0 {
                break;
            }
            tmp.write_all(&buf[..n]).map_err(StageError::Store)?;
            hasher.update(&buf[..n]);
            byte_len += n as u64;
        }
        tmp.flush().map_err(StageError::Store)?;
        Ok((hasher.finalize().to_hex().to_string(), byte_len))
    })();

    match streamed {
        Ok((hash, byte_len)) => Ok((hash, byte_len, tmp_path)),
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

// ── Store-level advisory lock ──────────────────────────────────────────────────

/// Advisory lock for the protected store's write phase.
///
/// Created with `O_CREAT | O_EXCL` semantics so it never follows a symlink.
/// Dropped (and the lock file deleted) when the guard goes out of scope.
///
/// Two concurrent enabled captures against the same store would otherwise
/// both read the same manifest snapshot and the later writer would drop
/// handles added by the earlier one; this lock serializes the read-modify-
/// write cycle.
#[derive(Debug)]
struct StoreLock {
    path: PathBuf,
}

impl StoreLock {
    /// Acquires an exclusive advisory lock by creating a sentinel file.
    ///
    /// Returns `WouldBlock` if the lock is already held by another process.
    fn acquire(store_root: &Path) -> io::Result<Self> {
        let path = store_root.join(".store.lock");
        // `create_new` maps to O_CREAT|O_EXCL on Unix — no-follow, atomic.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "protected store at {} is locked by another process; \
                             retry after the other capture completes, or remove \
                             {} if the locking process has exited",
                            store_root.display(),
                            path.display()
                        ),
                    )
                } else {
                    e
                }
            })?;
        Ok(Self { path })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// ── Blob write transaction ──────────────────────────────────────────────────────

/// Tracks blobs newly written during a single `capture` so they can be rolled
/// back if the capture fails before it commits.
///
/// A capture writes blobs, then `manifest.jsonl`, then `operators.jsonl`.  If a
/// commit step fails (e.g. the manifest would exceed the read cap, or an I/O
/// error), the new blob bytes would otherwise be left in `<store>/blobs` with no
/// manifest record referencing them.  This guard removes those orphans on drop
/// unless [`Self::commit`] was called once the manifest is durably written.
///
/// Only *new-handle* blobs are tracked.  Repair writes overwrite a blob path
/// that an existing (retained) manifest record already references, so removing
/// them on rollback would dangle the prior on-disk manifest.
#[derive(Debug, Default)]
struct BlobTxn {
    blobs: Vec<PathBuf>,
    committed: bool,
}

impl BlobTxn {
    fn track(&mut self, blob: PathBuf) {
        self.blobs.push(blob);
    }

    /// Marks the new blobs as durably referenced; suppresses rollback on drop.
    const fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for BlobTxn {
    fn drop(&mut self) {
        if !self.committed {
            for blob in &self.blobs {
                let _ = fs::remove_file(blob);
            }
        }
    }
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

    /// Returns the blobs directory path after verifying it is a real directory.
    ///
    /// A tampered or shared store could replace `<store>/blobs` with a symlink
    /// to an attacker-controlled directory.  Directory path components are
    /// followed transparently by the OS, so the per-blob `symlink_metadata`
    /// check on the leaf file does **not** catch a symlinked parent — writes
    /// would stage payloads, and reads would resolve, outside the protected
    /// store boundary.  Rejecting anything at the `blobs` path that is not a
    /// genuine directory (symlink, regular file, FIFO, device) closes that gap.
    ///
    /// A genuinely absent `blobs` directory (`NotFound`) is allowed — it is
    /// created lazily by [`create_private_dir`] on first write.
    fn checked_blobs_dir(&self) -> io::Result<PathBuf> {
        let dir = self.blobs_dir();
        require_real_dir_or_absent(&dir, "blobs path")?;
        Ok(dir)
    }

    /// Verifies the store root is a real directory (not a symlink) before it is
    /// created or used for the lock, manifest, operators, and blobs.
    ///
    /// If `<store>` itself is a symlink to another directory, every nested path
    /// (`.store.lock`, `manifest.jsonl`, `operators.jsonl`, `blobs/…`) resolves
    /// through it, so enabled capture would write protected raw bytes outside the
    /// intended store boundary.  An absent root (`NotFound`) is allowed — it is
    /// created lazily as a real directory on first write.
    fn checked_root(&self) -> io::Result<()> {
        require_real_dir_or_absent(&self.root, "store root")
    }

    /// Returns `true` when the manifest path exists in any form.
    ///
    /// Distinguishes a genuinely absent store (`NotFound` → `false`) from one
    /// whose manifest exists but is corrupt or tampered (symlink, FIFO, device,
    /// or a stat error such as permission denied → `true`).  Returning `true`
    /// for the corrupt cases lets callers proceed to [`Self::read_manifest`],
    /// which surfaces the "not a regular file" diagnostic — rather than
    /// silently reporting the store as uninitialised and masking the tampering
    /// (`protected list` → empty success, `get` → `raw_artifact_mode_disabled`).
    ///
    /// Uses `symlink_metadata` (no-follow) so a symlink at the manifest path is
    /// never followed during the existence check.
    fn is_initialised(&self) -> bool {
        match self.manifest_path().symlink_metadata() {
            Ok(_) => true,
            Err(e) => e.kind() != io::ErrorKind::NotFound,
        }
    }

    /// Reads the canonical manifest, returning the de-duplicated set of handles.
    ///
    /// Rejects non-regular files (symlinks, FIFOs, devices) before reading to
    /// prevent memory exhaustion via e.g. a symlink to `/dev/zero`.  Also
    /// bounds the read to [`MAX_STORE_FILE_BYTES`].
    fn read_manifest(&self) -> io::Result<Vec<ProtectedHandle>> {
        let path = self.manifest_path();
        // Read through a single no-follow, regular-file, size-capped descriptor
        // so a manifest swapped/grown after a separate stat cannot make this
        // follow a symlink/FIFO or read past the cap (TOCTOU-safe).
        let content = match read_capped_regular_file(&path, MAX_STORE_FILE_BYTES) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "manifest.jsonl at {} is not a bounded regular file; \
                         the store may have been tampered with ({e})",
                        path.display()
                    ),
                ));
            }
        };
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
        check_within_read_cap(&content, "manifest.jsonl")?;
        write_private_file(&self.manifest_path(), content.as_bytes())
    }

    /// Reads the canonical operators file, returning the set of authorised IDs.
    ///
    /// Applies the same regular-file and size checks as [`Self::read_manifest`].
    fn read_operators(&self) -> io::Result<Vec<String>> {
        let path = self.operators_path();
        // Read through a single no-follow, regular-file, size-capped descriptor
        // (TOCTOU-safe), mirroring `read_manifest`.
        let content = match read_capped_regular_file(&path, MAX_STORE_FILE_BYTES) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "operators.jsonl at {} is not a bounded regular file; \
                         the store may have been tampered with ({e})",
                        path.display()
                    ),
                ));
            }
        };
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

    /// Serializes the operators file's canonical content (sorted, de-duplicated).
    ///
    /// Shared by [`Self::write_operators`] and the capture preflight so the size
    /// checked before committing the manifest matches the bytes actually written.
    fn serialize_operators(ops: &[String]) -> String {
        let mut lines: Vec<String> = ops
            .iter()
            .map(|o| serde_json::to_string(o).expect("operator serialisation is infallible"))
            .collect();
        lines.sort_unstable();
        lines.dedup();
        format!("{}\n", lines.join("\n"))
    }

    /// Writes the canonical operators file.
    fn write_operators(&self, ops: &[String]) -> io::Result<()> {
        let dir = &self.root;
        create_private_dir(dir)?;
        let content = Self::serialize_operators(ops);
        check_within_read_cap(&content, "operators.jsonl")?;
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

        // Acquire a store-level advisory lock before reading the manifest and
        // operators files.  Two concurrent enabled captures against the same
        // store would otherwise each read a stale manifest snapshot, write
        // their blobs, and then overwrite each other's manifest; the later
        // writer would drop handles written by the earlier one, leaving stored
        // blobs unreachable via `protected list` / `get`.
        //
        // The lock is held until `_lock` is dropped at the end of this
        // function.  On process kill, the lock file (.store.lock) is left
        // behind; the operator can remove it manually.
        if enabled {
            // Reject a symlinked store root before creating it or resolving the
            // lock / manifest / operators / blobs through it.
            self.checked_root()?;
            create_private_dir(&self.root)?;
        }
        let _lock: Option<StoreLock> = if enabled {
            Some(StoreLock::acquire(&self.root)?)
        } else {
            None
        };

        // Load existing manifest and validate the operators file *before* any
        // blob or manifest writes.  This ensures that a corrupt operators.jsonl
        // causes an early failure rather than leaving orphaned blobs and manifest
        // records that the caller cannot retrieve until ACL state is repaired.
        let mut existing: Vec<ProtectedHandle> = if enabled {
            self.read_manifest()?
        } else {
            Vec::new()
        };
        // Snapshot the content hashes the ON-DISK manifest references before this
        // capture mutates `existing`.  Used to decide blob rollback tracking: a
        // blob already referenced by a committed record must survive rollback,
        // while a pre-existing orphan/tampered blob we replace must not.
        let original_blob_hashes: std::collections::HashSet<String> =
            existing.iter().map(|h| h.content_hash.clone()).collect();
        let ops: Vec<String> = if enabled {
            // Fail closed: an initialized store (manifest present) whose
            // operators.jsonl is absent must NOT be silently reinitialized with
            // only the current producer — that would authorize them to retrieve
            // every previously captured handle.  A missing ACL on an initialized
            // store is treated as tampering.  (A brand-new store has no manifest
            // yet, so both files are created together on first capture.)
            if self.is_initialised()
                && matches!(
                    self.operators_path().symlink_metadata(),
                    Err(ref e) if e.kind() == io::ErrorKind::NotFound
                )
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "operators.jsonl is missing from an initialized store at {}; \
                         refusing to reinitialize the ACL (the store may have been tampered with)",
                        self.root.display()
                    ),
                ));
            }
            self.read_operators()?
        } else {
            Vec::new()
        };

        // Rolls back blobs newly written by this capture if any later step (a
        // subsequent blob write, the manifest commit, or the ACL preflight)
        // fails before the manifest is durable.  Declared after `_lock` so it
        // drops — and cleans up — while the store lock is still held.
        let mut blob_txn = BlobTxn::default();

        // Per-entry diagnostic for a source that became unreadable between the
        // regular-file stat and the streaming read.
        let stale_read_outcome = |source_path: &str| {
            let fallback = blake3::hash(source_path.as_bytes()).to_hex().to_string();
            CaptureEntryOutcome {
                source_path: source_path.to_owned(),
                handle: format!("{PROTECTED_HANDLE_PREFIX}{fallback}"),
                content_hash: fallback,
                byte_len: 0,
                stored: false,
                diagnostic: Some(EntryDiagnostic {
                    code: "stale_source_path".to_owned(),
                    message: format!(
                        "source path {source_path:?} is not readable; \
                         the file may have moved or been deleted"
                    ),
                }),
            }
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
            // Compute the content hash by STREAMING the source rather than
            // slurping it into one `Vec`, so a multi-GB transcript or CI log
            // cannot exhaust memory before a diagnostic or blob write.
            if !enabled {
                // Preview only — hash without writing anything to disk.
                let Ok((content_hash, byte_len)) =
                    hash_source_streaming(Path::new(&entry.source_path))
                else {
                    skipped_count += 1;
                    outcomes.push(stale_read_outcome(&entry.source_path));
                    continue;
                };
                let handle = ProtectedHandle::compute_handle(
                    &class,
                    &content_hash,
                    Some(entry.source_path.as_str()),
                );
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

            // Enabled: validate the blobs directory (no-follow) and create it,
            // then stream the source into a temp blob there, hashing as it goes.
            // The temp is promoted to the content-addressed path (or discarded if
            // a matching blob already exists) below.  Validating before the
            // existing-handle fast path keeps a symlinked `<store>/blobs`
            // rejected consistently with `get`.
            let blobs = self.checked_blobs_dir()?;
            create_private_dir(&blobs)?;
            let (content_hash, byte_len, tmp) =
                match stream_source_to_temp(Path::new(&entry.source_path), &blobs) {
                    Ok(staged) => staged,
                    // Unreadable source — skip this entry with a diagnostic.
                    Err(StageError::Source) => {
                        skipped_count += 1;
                        outcomes.push(stale_read_outcome(&entry.source_path));
                        continue;
                    }
                    // Store I/O error staging the blob (disk full, permissions,
                    // stale temp path) — fail the capture rather than misreport
                    // it as a stale source and commit a partial capture.
                    Err(StageError::Store(e)) => return Err(e),
                };
            let handle = ProtectedHandle::compute_handle(
                &class,
                &content_hash,
                Some(entry.source_path.as_str()),
            );

            // Enabled: store blob + register in manifest.
            //
            // Before deduplication, drop any existing record that represents the
            // SAME payload as this capture unless it is fully valid.  A record is
            // "the same payload" when either its stored `handle` equals the
            // current handle OR its recomputed identity equals the current handle
            // — the latter catches a record whose `handle` field itself was
            // tampered (its stored handle no longer matches, so a plain
            // `h.handle == handle` test would treat the corrupt duplicate as
            // unrelated and leave `protected list` with both it and the repaired
            // record).  Unrelated payloads are always kept.
            existing.retain(|h| {
                let recomputed = ProtectedHandle::compute_handle(
                    &h.source_class,
                    &h.content_hash,
                    h.source_path.as_deref(),
                );
                if h.handle != handle && recomputed != handle {
                    return true; // genuinely different payload — keep
                }
                // Same payload: keep only when every field is internally
                // consistent AND schema-valid, so the `already_exists` path can
                // safely short-circuit and preserve the original capture
                // metadata.  Identity fields (class, content_hash, source_path)
                // are verified by recomputing the handle and requiring it to
                // equal both the stored handle and the current handle; the rest
                // are checked against ground truth (byte_len, schema_version) or
                // for schema validity (captured_at RFC 3339, producer_id
                // non-empty).  Any failure drops the record so recapture writes a
                // single canonical replacement.
                recomputed == h.handle
                    && h.handle == handle
                    && h.byte_len == byte_len
                    && h.schema_version == PROTECTED_SCHEMA_VERSION
                    && chrono::DateTime::parse_from_rfc3339(&h.captured_at).is_ok()
                    && !h.producer_id.trim().is_empty()
            });
            let blob = blobs.join(&content_hash);
            let already_exists = existing.iter().any(|h| h.handle == handle);

            // Decide whether a VALID content-addressed blob already exists at
            // this path, then either keep it (discard the streamed temp) or
            // promote the temp to repair a missing / corrupt / non-regular /
            // wrong-sized / wrong-hash blob.  This runs for BOTH an
            // already-registered handle AND a new handle whose bytes hash to an
            // existing blob path, so a shared blob whose bytes were corrupted is
            // repaired rather than silently reused (which would leave `get`
            // failing for the freshly committed handle).  `symlink_metadata` (no
            // follow) avoids blocking on a FIFO; the size guard avoids reading a
            // huge/tampered blob — only an equal-sized blob is read to verify.
            // Validate the existing blob on a SINGLE opened descriptor: a
            // regular file of exactly `byte_len` bytes hashing to `content_hash`.
            // Binding the size check to the fd (not a separate stat) prevents a
            // blob swapped for a huge regular file from being hashed to EOF
            // before the size guard fires, and streaming keeps memory flat.
            let blob_valid = blob_matches(&blob, &content_hash, byte_len);
            if blob_valid {
                // On-disk blob already matches — discard the streamed temp.
                let _ = fs::remove_file(&tmp);
            } else {
                // Promote the freshly streamed temp, repairing a missing/corrupt
                // blob.
                if let Err(e) = rename_into_place(&tmp, &blob) {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
                // Track for rollback unless the blob is already referenced by the
                // ORIGINAL on-disk manifest.  Filesystem existence is not enough:
                // a pre-created/tampered orphan blob (a symlink or corrupt file no
                // committed record references) is replaced by our streamed bytes
                // here and must be removed on rollback, while a blob a prior
                // record references must survive so that record does not dangle.
                if !original_blob_hashes.contains(&content_hash) {
                    blob_txn.track(blob.clone());
                }
            }

            if !already_exists {
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

        // Only mutate the store — including extending the ACL — when at least one
        // payload was actually captured or repaired.  An enabled capture with an
        // empty manifest or only unsupported/stale entries leaves `stored_count`
        // at 0; rewriting operators.jsonl with this producer would otherwise
        // authorize them to retrieve every previously captured handle without
        // storing anything.
        if enabled && stored_count > 0 {
            // Preflight the ACL (operators) growth BEFORE committing the
            // manifest.  Adding this producer to a near-cap operators.jsonl could
            // push it over MAX_STORE_FILE_BYTES; without this check the manifest
            // (and blobs) would commit and only the final ACL write would fail,
            // leaving retrievable handles that do not authorize the producer.
            // On failure here `blob_txn` rolls back the new blobs.
            let mut projected_ops = ops;
            projected_ops.push(producer_id.to_owned());
            check_within_read_cap(
                &Self::serialize_operators(&projected_ops),
                "operators.jsonl",
            )?;

            // Persist de-duplicated manifest.  If this fails (including the
            // manifest exceeding the read cap), `blob_txn`'s drop removes the
            // blobs newly written by this capture so the store is never left with
            // orphaned payload bytes that no manifest record references.
            self.write_manifest(&existing)?;

            // Manifest is durable and references the new blobs — they must
            // persist now, so commit the transaction before the ACL write.
            blob_txn.commit();

            // Record producer as an authorised operator.  Size was preflighted
            // above, so this can only fail on a genuine I/O error; the manifest +
            // blobs are already committed and an idempotent re-run retries the
            // ACL write, so they are left in place rather than orphaned.
            self.write_operators(&projected_ops)?;
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
    /// This buffers the whole payload in memory; prefer [`Self::get_to_writer`]
    /// for large payloads.  Provided for in-memory and embedded callers.
    ///
    /// # Errors
    ///
    /// Returns a [`GetError`] for every failure mode.  The error JSON never
    /// includes raw payload bytes, secrets, or bearer tokens.
    pub fn get(&self, handle: &str, operator: &str) -> Result<Vec<u8>, GetError> {
        // Delegate to the streaming path with an in-memory sink.  Writing to a
        // `Vec` is infallible, so the `Output` channel is unreachable here.
        let mut buf = Vec::new();
        match self.get_to_writer(handle, operator, &mut buf) {
            Ok(_) => Ok(buf),
            Err(GetStreamError::Get(e)) => Err(e),
            Err(GetStreamError::Output(e)) => {
                unreachable!("in-memory Vec sink cannot fail: {e}")
            }
        }
    }

    /// Streams the verified payload for `handle` to `out` after authorizing
    /// `operator`, without buffering the whole payload in memory.
    ///
    /// Two passes over the blob: the first verifies the BLAKE3 hash (so no
    /// unverified bytes are ever released), the second copies the bytes to
    /// `out` in bounded chunks.  Returns the number of bytes written.  This
    /// keeps memory flat for the multi-GB payloads that capture now streams.
    ///
    /// Check order (auth before existence disclosure):
    /// 1. Store / manifest absent → [`GetError::RawArtifactModeDisabled`].
    /// 2. `operator` not in authorised set → [`GetError::Unauthorized`].
    /// 3. `handle` does not start with `protected:v1:` → [`GetError::MalformedHandle`].
    /// 4. `handle` not in manifest → [`GetError::PayloadNotFound`].
    /// 5. Blob file absent → [`GetError::MissingProtectedPayload`].
    /// 6. BLAKE3 mismatch → [`GetError::HashMismatch`].
    ///
    /// # Errors
    ///
    /// [`GetStreamError::Get`] for any retrieval or integrity failure (nothing
    /// is written to `out`); [`GetStreamError::Output`] if writing to `out`
    /// fails after verification.
    pub fn get_to_writer<W: io::Write>(
        &self,
        handle: &str,
        operator: &str,
        out: &mut W,
    ) -> Result<u64, GetStreamError> {
        use std::io::{Read as _, Seek as _};

        let (blob_path, content_hash, byte_len) = self
            .resolve_blob(handle, operator)
            .map_err(GetStreamError::Get)?;

        // Open the blob exactly ONCE (no-follow, regular-file descriptor) and use
        // that SAME descriptor for both verification and emission.  Re-opening
        // the path between verify and copy would reintroduce a TOCTOU window in
        // which a local process could swap the blob and have unverified bytes
        // released, breaking the verify-before-release guarantee.
        let missing = || {
            GetStreamError::Get(GetError::MissingProtectedPayload {
                handle: handle.to_owned(),
                expected_path: blob_path.display().to_string(),
            })
        };
        let mut f = open_source_checked(&blob_path).map_err(|_| missing())?;

        // Bind the size guard to the OPENED descriptor (fstat), not the earlier
        // path stat, so a blob swapped for a much larger regular file cannot be
        // hashed to EOF before the byte_len check fires (huge-file stall / DoS).
        if f.metadata().map_err(|_| missing())?.len() != byte_len {
            return Err(GetStreamError::Get(GetError::HashMismatch {
                handle: handle.to_owned(),
                expected: content_hash,
                actual: "(blob size does not match manifest byte_len — not read)".to_owned(),
            }));
        }

        // Pass 1: verify the BLAKE3 hash by streaming the descriptor (bounded
        // memory) BEFORE emitting any bytes.
        let mut hasher = blake3::Hasher::new();
        io::copy(&mut f, &mut hasher).map_err(|_| missing())?;
        let actual_hash = hasher.finalize().to_hex().to_string();
        if actual_hash != content_hash {
            return Err(GetStreamError::Get(GetError::HashMismatch {
                handle: handle.to_owned(),
                expected: content_hash,
                actual: actual_hash,
            }));
        }

        // Pass 2: rewind the same descriptor and stream the verified bytes to the
        // destination in bounded chunks.
        f.seek(std::io::SeekFrom::Start(0)).map_err(|_| missing())?;
        // Heap-allocated so the 64 KiB chunk buffer does not sit on the stack.
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = f.read(&mut buf).map_err(|_| missing())?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(GetStreamError::Output)?;
            total += n as u64;
        }
        Ok(total)
    }

    /// Resolves and validates the content-addressed blob path for `handle` after
    /// authorizing `operator`.  Returns the verified-safe blob path and its
    /// expected content hash; the caller reads and hash-verifies the bytes.
    fn resolve_blob(
        &self,
        handle: &str,
        operator: &str,
    ) -> Result<(PathBuf, String, u64), GetError> {
        // 0. Reject a symlinked store root (no-follow) before probing any path
        // under it.  Otherwise a configured store path replaced by a symlink
        // would let `get` authorize against and return bytes from outside the
        // intended store boundary — capture already rejects the same root.
        self.checked_root()
            .map_err(|_| GetError::RawArtifactModeDisabled)?;

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

        // 4a-bis. Schema-version check.  The contract is explicitly v1; a record
        // whose `schema_version` was tampered to another value must be rejected
        // before releasing bytes rather than accepting whatever layout happened
        // to deserialize into the current struct.  (The capture path likewise
        // drops non-v1 records during recapture.)
        if record.schema_version != PROTECTED_SCHEMA_VERSION {
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
        // Reject a symlinked/non-directory `blobs` parent before resolving the
        // leaf path, so a tampered store cannot redirect reads outside the
        // protected-store boundary via a symlinked `blobs` directory.
        let blob_path = self
            .checked_blobs_dir()
            .map(|d| d.join(&record.content_hash))
            .map_err(|_| GetError::MissingProtectedPayload {
                handle: handle.to_owned(),
                expected_path: self.blob_path(&record.content_hash).display().to_string(),
            })?;
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

        // The caller reads/streams the blob and verifies the size+hash on the
        // opened descriptor against these expected values.
        Ok((blob_path, record.content_hash.clone(), record.byte_len))
    }

    /// Lists all protected handles (metadata only — no raw bytes).
    ///
    /// Returns an empty list when the store is not yet initialised.
    ///
    /// # Errors
    ///
    /// Returns an error on manifest I/O failure.
    pub fn list(&self) -> io::Result<Vec<ProtectedHandle>> {
        // Reject a symlinked store root (no-follow) before probing under it, so
        // a symlinked store path cannot redirect the listing outside the
        // intended boundary.
        self.checked_root()?;
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

    // ── Unit: StoreLock advisory locking ─────────────────────────────────────

    #[test]
    fn store_lock_prevents_concurrent_acquire() {
        let dir = tempdir().unwrap();
        create_private_dir(dir.path()).unwrap();

        // First acquire succeeds.
        let lock1 = StoreLock::acquire(dir.path()).expect("first acquire must succeed");

        // Second acquire must fail with WouldBlock while first lock is held.
        let err = StoreLock::acquire(dir.path()).expect_err("second acquire must fail");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::WouldBlock,
            "second acquire must return WouldBlock, not another error kind"
        );

        // Dropping the first lock must allow a third acquire to succeed.
        drop(lock1);
        let _lock3 =
            StoreLock::acquire(dir.path()).expect("acquire after lock release must succeed");
    }

    #[test]
    fn store_lock_cleanup_on_drop() {
        let dir = tempdir().unwrap();
        create_private_dir(dir.path()).unwrap();

        let lock_path = dir.path().join(".store.lock");
        {
            let _lock = StoreLock::acquire(dir.path()).unwrap();
            assert!(
                lock_path.exists(),
                "lock file must exist while lock is held"
            );
        } // lock dropped here
        assert!(
            !lock_path.exists(),
            "lock file must be removed when lock is dropped"
        );
    }

    // ── Unit: read_manifest rejects non-regular files ─────────────────────────

    #[cfg(unix)]
    #[test]
    fn corrupt_symlink_manifest_is_surfaced_not_masked_as_absent() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());

        // Create a real file elsewhere and place a symlink at the manifest path.
        let real_file = dir.path().join("real.jsonl");
        fs::write(&real_file, "\n").unwrap();
        let manifest_path = dir.path().join("manifest.jsonl");
        std::os::unix::fs::symlink(&real_file, &manifest_path).unwrap();

        // is_initialised() must treat an existing-but-non-regular manifest as
        // PRESENT (true), not absent — otherwise list/get silently mask the
        // tampering as "store uninitialised".
        assert!(
            store.is_initialised(),
            "is_initialised must return true when manifest.jsonl exists as a symlink"
        );

        // read_manifest() must return an error, not silently follow the symlink.
        let err = store.read_manifest().unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidData,
            "read_manifest must return InvalidData for a symlink at manifest path"
        );
        assert!(
            err.to_string().contains("tampered"),
            "error message must flag tampering: {err}"
        );

        // list() must propagate the corruption error rather than returning an
        // empty success response that hides the tampered store.
        let list_err = store.list().unwrap_err();
        assert_eq!(
            list_err.kind(),
            std::io::ErrorKind::InvalidData,
            "list() must surface the corruption error, not Ok(empty)"
        );
        assert!(
            list_err.to_string().contains("tampered"),
            "list() error must flag tampering: {list_err}"
        );
    }

    #[test]
    fn is_initialised_false_only_when_manifest_absent() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());

        // No manifest yet → genuinely uninitialised.
        assert!(
            !store.is_initialised(),
            "absent manifest must report not-initialised"
        );

        // A regular manifest file → initialised.
        fs::write(store.manifest_path(), "").unwrap();
        assert!(
            store.is_initialised(),
            "present regular manifest must report initialised"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_operators_rejects_symlink_at_operators_path() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());

        // Place a symlink at the operators path pointing at a real file.
        let real_file = dir.path().join("real_ops.jsonl");
        fs::write(&real_file, "\"op-1\"\n").unwrap();
        let ops_path = dir.path().join("operators.jsonl");
        std::os::unix::fs::symlink(&real_file, &ops_path).unwrap();

        // read_operators() must reject the symlink.
        let err = store.read_operators().unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidData,
            "read_operators must return InvalidData for a symlink at operators path"
        );
        assert!(
            err.to_string().contains("tampered"),
            "error message must flag tampering: {err}"
        );
    }

    // ── Unit: read-cap guard on writes (finding 3) ───────────────────────────

    #[test]
    fn check_within_read_cap_rejects_oversized_content() {
        // Within the cap → Ok.
        assert!(check_within_read_cap("small", "manifest.jsonl").is_ok());

        // Exactly at the cap → Ok.
        let cap = usize::try_from(MAX_STORE_FILE_BYTES).expect("cap fits in usize");
        let at_cap = "x".repeat(cap);
        assert!(check_within_read_cap(&at_cap, "manifest.jsonl").is_ok());

        // One byte over the cap → InvalidData error, so write_manifest /
        // write_operators never commit a file later reads would reject.
        let over_cap = "x".repeat(cap + 1);
        let err = check_within_read_cap(&over_cap, "manifest.jsonl").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("exceeding the"),
            "error must explain the cap: {err}"
        );
    }

    // ── Unit: repair skips reading a wrong-sized blob (finding 1) ─────────────

    #[test]
    fn capture_repairs_blob_with_mismatched_size_without_hash_read() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"small original").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        let report = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let content_hash = report.entries[0].content_hash.clone();
        let handle = report.entries[0].handle.clone();

        // Replace the blob with a much LARGER wrong-sized regular file.  The
        // size-mismatch arm must mark it for repair without reading the whole
        // (here oversized) blob to hash it.
        let big = vec![b'Z'; 5 * 1024 * 1024];
        fs::write(dir.path().join("blobs").join(&content_hash), &big).unwrap();

        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        let bytes = store.get(&handle, "op-1").expect("get after size repair");
        assert_eq!(bytes, b"small original");
    }

    // ── Unit: symlinked blobs directory is rejected (finding 2) ───────────────

    #[cfg(unix)]
    #[test]
    fn enabled_capture_rejects_symlinked_blobs_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("store");
        fs::create_dir_all(&root).unwrap();
        // Point <store>/blobs at an attacker-controlled directory outside the store.
        let evil = dir.path().join("evil");
        fs::create_dir_all(&evil).unwrap();
        std::os::unix::fs::symlink(&evil, root.join("blobs")).unwrap();

        let store = ProtectedStore::new(&root);
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"secret bytes").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        let err = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .expect_err("capture must refuse a symlinked blobs directory");
        assert!(
            err.to_string().contains("not a real directory"),
            "error must explain the symlinked blobs dir: {err}"
        );
        // No payload may have been staged inside the symlink target.
        assert!(
            fs::read_dir(&evil).unwrap().next().is_none(),
            "no blob may be written through the symlinked blobs directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn get_rejects_symlinked_blobs_dir() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"original content").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];
        let report = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let handle = report.entries[0].handle.clone();

        // Replace the real blobs directory with a symlink to a copy of itself.
        // The blob is still reachable through the symlink, but get() must refuse
        // to resolve through a symlinked parent.
        let real_blobs = dir.path().join("blobs");
        let moved = dir.path().join("blobs_real");
        fs::rename(&real_blobs, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &real_blobs).unwrap();

        let err = store
            .get(&handle, "op-1")
            .expect_err("get must refuse a symlinked blobs directory");
        assert!(
            matches!(err, GetError::MissingProtectedPayload { .. }),
            "symlinked blobs dir must map to MissingProtectedPayload, got {err:?}"
        );
    }

    // ── Unit: invalid non-identity metadata is repaired on recapture (finding 1)

    #[test]
    fn recapture_replaces_record_with_invalid_captured_at_or_empty_producer() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"hello world").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        // Initial valid capture.
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        // Tamper the record's non-identity fields to schema-invalid values while
        // leaving the handle / content_hash / source_path / byte_len intact, so
        // the record still recomputes to the same handle.
        let manifest_path = dir.path().join("manifest.jsonl");
        let line = fs::read_to_string(&manifest_path).unwrap();
        let mut rec: ProtectedHandle = serde_json::from_str(line.trim()).unwrap();
        rec.captured_at = "not-a-date".to_owned();
        rec.producer_id = String::new();
        fs::write(&manifest_path, serde_json::to_string(&rec).unwrap() + "\n").unwrap();

        // Re-capture with a valid producer + timestamp must DROP the invalid
        // record and replace it, rather than preserving it via already_exists.
        let valid_ts = "2026-07-01T12:00:00Z";
        store
            .capture(&entries, "op-2", "0.1.0", valid_ts, true)
            .unwrap();

        let after_line = fs::read_to_string(&manifest_path).unwrap();
        let after: ProtectedHandle = serde_json::from_str(after_line.trim()).unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(&after.captured_at).is_ok(),
            "captured_at must be repaired to RFC 3339, got {:?}",
            after.captured_at
        );
        assert_eq!(after.captured_at, valid_ts);
        assert_eq!(
            after.producer_id, "op-2",
            "empty producer_id must be repaired on recapture"
        );
    }

    // ── Unit: temp file is removed when a protected write fails (finding 3) ────

    #[test]
    fn write_private_file_cleans_up_temp_on_rename_failure() {
        let dir = tempdir().unwrap();
        // Destination already exists as a directory, so rename(file → dir) fails.
        let dest = dir.path().join("dest");
        fs::create_dir(&dest).unwrap();

        let err = write_private_file(&dest, b"payload bytes")
            .expect_err("write must fail when destination is a directory");

        // The staged temp sibling must not leak raw payload bytes.
        let tmp = dir.path().join(".dest.wip");
        assert!(
            !tmp.exists(),
            "temp file must be removed on rename failure ({err})"
        );
    }

    // ── Unit: BlobTxn rolls back uncommitted blobs (findings 2 + 4) ───────────

    #[test]
    fn blob_txn_rolls_back_uncommitted_and_keeps_committed() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.blob");
        let b = dir.path().join("b.blob");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();

        // Dropped without commit → tracked blobs removed.
        {
            let mut txn = BlobTxn::default();
            txn.track(a.clone());
            txn.track(b.clone());
        }
        assert!(!a.exists(), "uncommitted blob a must be rolled back");
        assert!(!b.exists(), "uncommitted blob b must be rolled back");

        // Committed → blob kept.
        let c = dir.path().join("c.blob");
        fs::write(&c, b"c").unwrap();
        {
            let mut txn = BlobTxn::default();
            txn.track(c.clone());
            txn.commit();
        }
        assert!(c.exists(), "committed blob must be kept");
    }

    // ── Unit: failed manifest commit rolls back the new blob (findings 2 + 4) ─

    #[test]
    fn capture_rolls_back_new_blob_when_manifest_write_fails() {
        let dir = tempdir().unwrap();
        let store_root = dir.path().join("store");
        fs::create_dir_all(&store_root).unwrap();
        let store = ProtectedStore::new(&store_root);

        // Place a directory at the manifest temp path so write_manifest's
        // create_new open fails AFTER the blob has been written and tracked.
        fs::create_dir(store_root.join(".manifest.jsonl.wip")).unwrap();

        let src = dir.path().join("payload.txt");
        fs::write(&src, b"rollback me").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        let result = store.capture(&entries, "op-1", "0.1.0", fixed_ts(), true);
        assert!(
            result.is_err(),
            "capture must fail when the manifest write fails"
        );

        // No orphaned blob may remain: the new blob must have been rolled back.
        let blobs = store_root.join("blobs");
        let remaining: Vec<PathBuf> = fs::read_dir(&blobs)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            remaining.is_empty(),
            "new blob must be rolled back on manifest write failure, found: {remaining:?}"
        );
    }

    // ── Unit: rollback preserves a blob shared with an existing handle (A) ────

    #[cfg(unix)]
    #[test]
    fn rollback_preserves_blob_shared_with_existing_handle() {
        let dir = tempdir().unwrap();
        let store_root = dir.path().join("store");
        fs::create_dir_all(&store_root).unwrap();
        let store = ProtectedStore::new(&store_root);

        // Capture P1 → handle1 backed by blob H.
        let p1 = dir.path().join("p1.txt");
        fs::write(&p1, b"shared bytes").unwrap();
        let r1 = store
            .capture(
                &[CaptureEntry {
                    class: "report".to_owned(),
                    source_path: p1.to_string_lossy().into_owned(),
                }],
                "op-1",
                "0.1.0",
                fixed_ts(),
                true,
            )
            .unwrap();
        let handle1 = r1.entries[0].handle.clone();
        let blob = store_root.join("blobs").join(&r1.entries[0].content_hash);
        assert!(blob.exists());

        // Force the next manifest write to fail after the blob step.
        fs::create_dir(store_root.join(".manifest.jsonl.wip")).unwrap();

        // Capture P2 — same bytes, different source path → new handle sharing
        // blob H.  The manifest write fails and the capture rolls back.
        let p2 = dir.path().join("p2.txt");
        fs::write(&p2, b"shared bytes").unwrap();
        let r2 = store.capture(
            &[CaptureEntry {
                class: "report".to_owned(),
                source_path: p2.to_string_lossy().into_owned(),
            }],
            "op-1",
            "0.1.0",
            fixed_ts(),
            true,
        );
        assert!(r2.is_err(), "capture must fail when manifest write fails");

        // The shared blob must survive — handle1 stays retrievable.
        assert!(
            blob.exists(),
            "blob shared with handle1 must not be rolled back"
        );
        let bytes = store
            .get(&handle1, "op-1")
            .expect("handle1 must still resolve");
        assert_eq!(bytes, b"shared bytes");
    }

    // ── Unit: recapture rejects a symlinked blobs dir on the fast path (B) ────

    #[cfg(unix)]
    #[test]
    fn recapture_rejects_symlinked_blobs_dir_on_fast_path() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"original content").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        // Replace the real blobs dir with a symlink to a copy of itself.  The
        // existing blob still matches, so without the fast-path check recapture
        // would report success even though `get` rejects the symlinked store.
        let real_blobs = dir.path().join("blobs");
        let moved = dir.path().join("blobs_real");
        fs::rename(&real_blobs, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &real_blobs).unwrap();

        let err = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .expect_err("recapture must reject a symlinked blobs directory");
        assert!(
            err.to_string().contains("not a real directory"),
            "err must mention the symlinked blobs dir: {err}"
        );
    }

    // ── Unit: capture rejects a symlinked store root (D) ──────────────────────

    #[cfg(unix)]
    #[test]
    fn capture_rejects_symlinked_store_root() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let root = dir.path().join("store");
        std::os::unix::fs::symlink(&target, &root).unwrap();
        let store = ProtectedStore::new(&root);

        let src = dir.path().join("payload.txt");
        fs::write(&src, b"secret bytes").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        let err = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .expect_err("capture must reject a symlinked store root");
        assert!(
            err.to_string().contains("not a real directory"),
            "err must mention the symlinked store root: {err}"
        );
        // Nothing may be written through the symlink into the target directory.
        assert!(
            fs::read_dir(&target).unwrap().next().is_none(),
            "no files may be written through a symlinked store root"
        );
    }

    // ── Unit: get error detail is the documented flat object (307) ────────────

    #[test]
    fn get_error_detail_is_flat_documented_object() {
        // A struct variant must expose its fields flat under `error.detail`,
        // not nested under the variant name.
        let err = GetError::MissingProtectedPayload {
            handle: "protected:v1:aa".to_owned(),
            expected_path: "/store/blobs/aa".to_owned(),
        };
        let v: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();
        assert_eq!(v["error"]["code"], "missing_protected_payload");
        assert_eq!(v["error"]["detail"]["handle"], "protected:v1:aa");
        assert_eq!(v["error"]["detail"]["expected_path"], "/store/blobs/aa");
        assert!(
            v["error"]["detail"]["missing_protected_payload"].is_null(),
            "detail must not nest fields under the variant name"
        );

        // A unit variant's detail must still be an object, not a bare string.
        let v2: serde_json::Value =
            serde_json::from_str(&GetError::RawArtifactModeDisabled.to_json()).unwrap();
        assert!(
            v2["error"]["detail"].is_object(),
            "detail must always be an object: {v2}"
        );
    }

    // ── Unit: get rejects an unsupported schema_version (1239) ────────────────

    #[test]
    fn get_rejects_unsupported_schema_version() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"data").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];
        let r = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let handle = r.entries[0].handle.clone();

        // Tamper schema_version (not part of the handle identity) to an
        // unsupported value; get must reject before releasing bytes.
        let manifest_path = dir.path().join("manifest.jsonl");
        let mut rec: ProtectedHandle =
            serde_json::from_str(fs::read_to_string(&manifest_path).unwrap().trim()).unwrap();
        rec.schema_version = 2;
        fs::write(&manifest_path, serde_json::to_string(&rec).unwrap() + "\n").unwrap();

        let err = store.get(&handle, "op-1").unwrap_err();
        assert_eq!(
            err.code(),
            "corrupt_manifest_record",
            "an unsupported schema_version must be rejected by get"
        );
    }

    // ── Unit: recapture drops a record with a tampered handle field (1036) ────

    #[test]
    fn recapture_drops_record_with_tampered_handle_field() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"payload bytes").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];
        let r = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let good_handle = r.entries[0].handle.clone();

        // Tamper ONLY the stored handle field; identity fields stay intact so the
        // record still recomputes to the canonical handle.
        let manifest_path = dir.path().join("manifest.jsonl");
        let mut rec: ProtectedHandle =
            serde_json::from_str(fs::read_to_string(&manifest_path).unwrap().trim()).unwrap();
        rec.handle = format!("{}{}", PROTECTED_HANDLE_PREFIX, "0".repeat(64));
        fs::write(&manifest_path, serde_json::to_string(&rec).unwrap() + "\n").unwrap();

        // Recapture must restore a single canonical record, not leave both the
        // tampered and the repaired one.
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        let lines: Vec<String> = fs::read_to_string(&manifest_path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "manifest must have exactly one record after recapture, got: {lines:?}"
        );
        let after: ProtectedHandle = serde_json::from_str(lines[0].trim()).unwrap();
        assert_eq!(
            after.handle, good_handle,
            "the surviving record must be the canonical one"
        );
    }

    // ── Unit: large payloads stream byte-for-byte (C) ─────────────────────────

    #[test]
    fn capture_streams_large_payload_roundtrip() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("big.bin");
        // ~200 KiB crosses the 64 KiB streaming buffer several times.
        let data: Vec<u8> = (0..200 * 1024usize)
            .map(|i| u8::try_from(i % 251).expect("< 251"))
            .collect();
        fs::write(&src, &data).unwrap();
        let entries = vec![CaptureEntry {
            class: "command_output".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        let r = store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();
        let handle = r.entries[0].handle.clone();
        assert_eq!(r.entries[0].byte_len, data.len() as u64);

        let got = store.get(&handle, "op-1").expect("get large payload");
        assert_eq!(got, data, "streamed blob must round-trip byte-for-byte");
    }

    // ── Unit: a corrupt shared blob is repaired for a new handle (A) ──────────

    #[test]
    fn new_handle_repairs_corrupt_shared_blob() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());

        // Capture P1 → handle1 backed by blob H (bytes "shared payload").
        let p1 = dir.path().join("p1.txt");
        fs::write(&p1, b"shared payload").unwrap();
        let r1 = store
            .capture(
                &[CaptureEntry {
                    class: "report".to_owned(),
                    source_path: p1.to_string_lossy().into_owned(),
                }],
                "op-1",
                "0.1.0",
                fixed_ts(),
                true,
            )
            .unwrap();
        let blob = dir.path().join("blobs").join(&r1.entries[0].content_hash);

        // Corrupt blob H in place with same-length wrong bytes (exercises the
        // hash-read arm, not just the size guard).
        fs::write(&blob, b"XXXXXXXXXXXXXX").unwrap();

        // Capture P2 — same bytes from a different source path → new handle that
        // hashes to blob H.  The corrupt shared blob must be repaired, not reused.
        let p2 = dir.path().join("p2.txt");
        fs::write(&p2, b"shared payload").unwrap();
        let r2 = store
            .capture(
                &[CaptureEntry {
                    class: "report".to_owned(),
                    source_path: p2.to_string_lossy().into_owned(),
                }],
                "op-1",
                "0.1.0",
                fixed_ts(),
                true,
            )
            .unwrap();
        let handle2 = r2.entries[0].handle.clone();

        let bytes = store
            .get(&handle2, "op-1")
            .expect("get must succeed for the new handle after the shared blob is repaired");
        assert_eq!(bytes, b"shared payload");
    }

    // ── Unit: a blob staging (store I/O) error fails the capture (C) ──────────

    #[test]
    fn capture_fails_on_blob_staging_error_not_stale_source() {
        let dir = tempdir().unwrap();
        let store_root = dir.path().join("store");
        fs::create_dir_all(&store_root).unwrap();
        let store = ProtectedStore::new(&store_root);

        // Place a directory at the temp blob path so the no-follow create_new
        // open fails — a store I/O error, not an unreadable source.
        let blobs = store_root.join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        fs::create_dir(blobs.join(".incoming.wip")).unwrap();

        let src = dir.path().join("payload.txt");
        fs::write(&src, b"readable source").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        let result = store.capture(&entries, "op-1", "0.1.0", fixed_ts(), true);
        assert!(
            result.is_err(),
            "a blob staging error must fail the capture, not be skipped as a stale source"
        );
    }

    // ── Unit: missing ACL on an initialized store fails closed (D) ────────────

    #[test]
    fn capture_fails_closed_when_acl_missing_on_initialized_store() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"content").unwrap();
        let entries = vec![CaptureEntry {
            class: "report".to_owned(),
            source_path: src.to_string_lossy().into_owned(),
        }];

        // First capture initializes the store (manifest + operators).
        store
            .capture(&entries, "op-1", "0.1.0", fixed_ts(), true)
            .unwrap();

        // Delete the ACL while the manifest remains (tampering).
        fs::remove_file(dir.path().join("operators.jsonl")).unwrap();

        // A second enabled capture by a different producer must FAIL closed,
        // not silently reinitialize the ACL and authorize op-2 for op-1's data.
        let err = store
            .capture(&entries, "op-2", "0.1.0", fixed_ts(), true)
            .expect_err("capture must fail closed when the ACL is missing");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string()
                .contains("missing from an initialized store"),
            "err must explain the missing ACL: {err}"
        );
        assert!(
            !dir.path().join("operators.jsonl").exists(),
            "the ACL must not be reinitialized on failure"
        );
    }

    // ── Unit: get_to_writer streams + verifies before emitting (B) ────────────

    /// A sink that always fails, to exercise the `Output` error channel.
    struct FailingWriter;
    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "sink failed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_one(store: &ProtectedStore, dir: &Path, bytes: &[u8]) -> (String, String) {
        let src = dir.join("payload.txt");
        fs::write(&src, bytes).unwrap();
        let r = store
            .capture(
                &[CaptureEntry {
                    class: "report".to_owned(),
                    source_path: src.to_string_lossy().into_owned(),
                }],
                "op-1",
                "0.1.0",
                fixed_ts(),
                true,
            )
            .unwrap();
        (
            r.entries[0].handle.clone(),
            r.entries[0].content_hash.clone(),
        )
    }

    #[test]
    fn get_to_writer_streams_verified_bytes() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let (handle, _) = capture_one(&store, dir.path(), b"streamed payload bytes");

        let mut out: Vec<u8> = Vec::new();
        let n = store
            .get_to_writer(&handle, "op-1", &mut out)
            .expect("streaming get must succeed");
        assert_eq!(out, b"streamed payload bytes");
        assert_eq!(n, out.len() as u64);
    }

    #[test]
    fn get_to_writer_rejects_size_mismatch_without_hashing() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let (handle, content_hash) = capture_one(&store, dir.path(), b"hello");

        // Replace the blob with a DIFFERENT-sized regular file.  The fd size
        // guard must reject it (no hashing of the larger file).
        let blob = dir.path().join("blobs").join(&content_hash);
        fs::write(&blob, vec![b'Z'; 4096]).unwrap();

        let mut out: Vec<u8> = Vec::new();
        let err = store
            .get_to_writer(&handle, "op-1", &mut out)
            .expect_err("size mismatch must fail");
        assert!(
            matches!(err, GetStreamError::Get(GetError::HashMismatch { .. })),
            "expected HashMismatch, got {err:?}"
        );
        assert!(out.is_empty(), "no bytes may be emitted on size mismatch");
    }

    #[test]
    fn get_to_writer_emits_nothing_on_hash_mismatch() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let (handle, content_hash) = capture_one(&store, dir.path(), b"good bytes");

        // Corrupt the blob with same-length wrong bytes so the size pre-check
        // passes and the pass-1 hash verification fails.
        let blob = dir.path().join("blobs").join(&content_hash);
        fs::write(&blob, b"BAD bytes!").unwrap();

        let mut out: Vec<u8> = Vec::new();
        let err = store
            .get_to_writer(&handle, "op-1", &mut out)
            .expect_err("hash mismatch must fail");
        assert!(
            matches!(err, GetStreamError::Get(GetError::HashMismatch { .. })),
            "expected Get(HashMismatch), got {err:?}"
        );
        assert!(
            out.is_empty(),
            "no bytes may be emitted to the sink on verification failure"
        );
    }

    #[test]
    fn get_to_writer_maps_sink_failure_to_output() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let (handle, _) = capture_one(&store, dir.path(), b"some payload bytes");

        let mut sink = FailingWriter;
        let err = store
            .get_to_writer(&handle, "op-1", &mut sink)
            .expect_err("a failing sink must surface an error");
        assert!(
            matches!(err, GetStreamError::Output(_)),
            "sink write failure must map to Output, got {err:?}"
        );
    }

    // ── Unit: no-follow source open binds validation to the fd (#5) ───────────

    #[cfg(unix)]
    #[test]
    fn open_source_checked_rejects_symlink_and_fifo() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real.txt");
        fs::write(&real, b"x").unwrap();
        assert!(
            open_source_checked(&real).is_ok(),
            "a regular file must open"
        );

        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(
            open_source_checked(&link).is_err(),
            "a symlink must be rejected by O_NOFOLLOW"
        );

        let fifo = dir.path().join("pipe.fifo");
        nix_mkfifo(&fifo);
        assert!(
            open_source_checked(&fifo).is_err(),
            "a FIFO must be rejected after the fstat check"
        );
    }

    // ── Unit: read_capped_regular_file binds validation to the fd (#2) ────────

    #[test]
    fn read_capped_regular_file_enforces_cap_and_regular() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("ok.txt");
        fs::write(&f, b"hello").unwrap();
        assert_eq!(read_capped_regular_file(&f, 1024).unwrap(), "hello");

        // Content exceeding the cap is rejected.
        assert!(
            read_capped_regular_file(&f, 3).is_err(),
            "content over the cap must be rejected"
        );

        // A symlink is rejected (no-follow open).
        #[cfg(unix)]
        {
            let link = dir.path().join("link.txt");
            std::os::unix::fs::symlink(&f, &link).unwrap();
            assert!(
                read_capped_regular_file(&link, 1024).is_err(),
                "a symlink must be rejected by the no-follow open"
            );
        }
    }

    // ── Unit: get/list reject a symlinked store root (#2) ─────────────────────

    #[cfg(unix)]
    #[test]
    fn get_and_list_reject_symlinked_store_root() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real_store");
        fs::create_dir_all(&real).unwrap();
        let store = ProtectedStore::new(&real);
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"content").unwrap();
        let r = store
            .capture(
                &[CaptureEntry {
                    class: "report".to_owned(),
                    source_path: src.to_string_lossy().into_owned(),
                }],
                "op-1",
                "0.1.0",
                fixed_ts(),
                true,
            )
            .unwrap();
        let handle = r.entries[0].handle.clone();

        // Replace the store path with a symlink to the real store directory.
        let link = dir.path().join("link_store");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let linked = ProtectedStore::new(&link);

        assert!(
            matches!(
                linked.get(&handle, "op-1"),
                Err(GetError::RawArtifactModeDisabled)
            ),
            "get must fail closed on a symlinked store root"
        );
        assert!(linked.list().is_err(), "list must reject a symlinked root");

        // The real (non-symlinked) store still resolves.
        assert!(store.get(&handle, "op-1").is_ok());
    }

    // ── Unit: a no-op capture does not authorize the producer (#3) ────────────

    #[test]
    fn empty_or_stale_capture_does_not_authorize_producer() {
        let dir = tempdir().unwrap();
        let store = ProtectedStore::new(dir.path());
        let src = dir.path().join("payload.txt");
        fs::write(&src, b"alpha").unwrap();
        let r = store
            .capture(
                &[CaptureEntry {
                    class: "report".to_owned(),
                    source_path: src.to_string_lossy().into_owned(),
                }],
                "op-1",
                "0.1.0",
                fixed_ts(),
                true,
            )
            .unwrap();
        let handle = r.entries[0].handle.clone();

        // op-2 runs an enabled capture with only an unsupported-class entry, so
        // nothing is stored.
        let rep = store
            .capture(
                &[CaptureEntry {
                    class: "not_a_real_class".to_owned(),
                    source_path: src.to_string_lossy().into_owned(),
                }],
                "op-2",
                "0.1.0",
                fixed_ts(),
                true,
            )
            .unwrap();
        assert_eq!(rep.stored_count, 0, "nothing should have been stored");

        assert!(
            matches!(
                store.get(&handle, "op-2"),
                Err(GetError::Unauthorized { .. })
            ),
            "op-2 must not be authorized after capturing nothing"
        );
        assert!(
            store.get(&handle, "op-1").is_ok(),
            "the original producer remains authorized"
        );
    }

    // ── Unit: rollback removes a replaced orphan blob (#1) ────────────────────

    #[test]
    fn rollback_removes_replaced_orphan_blob() {
        let dir = tempdir().unwrap();
        let store_root = dir.path().join("store");
        fs::create_dir_all(&store_root).unwrap();
        let store = ProtectedStore::new(&store_root);

        // Initialize the store with an unrelated handle.
        let a = dir.path().join("a.txt");
        fs::write(&a, b"alpha").unwrap();
        store
            .capture(
                &[CaptureEntry {
                    class: "report".to_owned(),
                    source_path: a.to_string_lossy().into_owned(),
                }],
                "op-1",
                "0.1.0",
                fixed_ts(),
                true,
            )
            .unwrap();

        // Pre-create a CORRUPT orphan blob at the content-addressed path for the
        // "beta payload" bytes that no manifest record references.
        let beta_hash = blake3::hash(b"beta payload").to_hex().to_string();
        let orphan = store_root.join("blobs").join(&beta_hash);
        fs::write(&orphan, b"corrupt orphan bytes").unwrap();

        // Force the manifest write to fail after the blob step.
        fs::create_dir(store_root.join(".manifest.jsonl.wip")).unwrap();

        // Capture the "beta payload" bytes — a new handle hashing to the orphan
        // path.  The corrupt orphan is replaced, then the manifest write fails,
        // so rollback must remove it (no committed record referenced it).
        let b = dir.path().join("b.txt");
        fs::write(&b, b"beta payload").unwrap();
        let res = store.capture(
            &[CaptureEntry {
                class: "report".to_owned(),
                source_path: b.to_string_lossy().into_owned(),
            }],
            "op-1",
            "0.1.0",
            fixed_ts(),
            true,
        );
        assert!(
            res.is_err(),
            "capture must fail when the manifest write fails"
        );
        assert!(
            !orphan.exists(),
            "a replaced orphan blob must be rolled back on failure"
        );
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
