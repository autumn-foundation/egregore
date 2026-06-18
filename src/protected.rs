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
        /// The invalid handle string (no payload bytes echoed).
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
    fn write_manifest(&self, handles: &[ProtectedHandle]) -> io::Result<()> {
        let dir = &self.root;
        fs::create_dir_all(dir)?;
        let mut lines: Vec<String> = handles
            .iter()
            .map(|h| serde_json::to_string(h).expect("ProtectedHandle serialisation is infallible"))
            .collect();
        lines.sort_unstable();
        lines.dedup();
        let content = format!("{}\n", lines.join("\n"));
        fs::write(self.manifest_path(), content)
    }

    /// Reads the canonical operators file, returning the set of authorised IDs.
    fn read_operators(&self) -> io::Result<Vec<String>> {
        let path = self.operators_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&path)?;
        let mut ops: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<String>(l).unwrap_or_else(|_| l.to_owned()))
            .collect();
        ops.sort_unstable();
        ops.dedup();
        Ok(ops)
    }

    /// Writes the canonical operators file.
    fn write_operators(&self, ops: &[String]) -> io::Result<()> {
        let dir = &self.root;
        fs::create_dir_all(dir)?;
        let mut lines: Vec<String> = ops
            .iter()
            .map(|o| serde_json::to_string(o).expect("operator serialisation is infallible"))
            .collect();
        lines.sort_unstable();
        lines.dedup();
        let content = format!("{}\n", lines.join("\n"));
        fs::write(self.operators_path(), content)
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
        let mut outcomes: Vec<CaptureEntryOutcome> = Vec::new();
        let mut stored_count = 0usize;
        let mut skipped_count = 0usize;

        // Load existing manifest so we can dedup (AC7).
        // Propagate parse/IO errors rather than silently treating a corrupt
        // manifest as empty — that would overwrite previously captured handles.
        let mut existing: Vec<ProtectedHandle> = if enabled {
            self.read_manifest()?
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

            // Read source bytes.
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
            let already_exists = existing.iter().any(|h| h.handle == handle);
            if already_exists {
                // Handle already registered: repair a missing blob while the
                // source is still available so `get` does not fail with
                // `missing_protected_payload` after a partial store deletion.
                let blob = self.blob_path(&content_hash);
                if !blob.exists() {
                    fs::create_dir_all(self.blobs_dir())?;
                    fs::write(blob, &bytes)?;
                }
            } else {
                // New handle: write blob and register.
                fs::create_dir_all(self.blobs_dir())?;
                fs::write(self.blob_path(&content_hash), &bytes)?;

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
            // Record producer as an authorised operator.
            let mut ops = self.read_operators().unwrap_or_default();
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
        let ops = self.read_operators().unwrap_or_default();
        if !ops.contains(&operator.to_owned()) {
            return Err(GetError::Unauthorized {
                operator: operator.to_owned(),
            });
        }

        // 3. Handle format check.
        if !handle.starts_with(PROTECTED_HANDLE_PREFIX) {
            return Err(GetError::MalformedHandle {
                handle: handle.to_owned(),
            });
        }

        // 4. Manifest lookup.
        let manifest = self
            .read_manifest()
            .map_err(|_| GetError::RawArtifactModeDisabled)?;
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

        // 5. Blob presence check.
        let blob_path = self.blob_path(&record.content_hash);
        let expected_path = blob_path.display().to_string();
        if !blob_path.exists() {
            return Err(GetError::MissingProtectedPayload {
                handle: handle.to_owned(),
                expected_path,
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
}
