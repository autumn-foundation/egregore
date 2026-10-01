//! Encrypted local store mode (issue #54).
//!
//! Egregore stores are plaintext by default. This module implements the
//! opt-in encrypted-at-rest mode on top of AletheiaDB's native encryption
//! subsystem — **no hand-rolled crypto**. AletheiaDB provides AES-256-GCM
//! (AES-NI) / ChaCha20-Poly1305 via RustCrypto, with per-component data
//! encryption keys derived from the master key via HKDF-SHA256; Egregore only
//! decides *when* encryption is on and *where the key comes from*.
//!
//! ## Contract
//!
//! * The mode is an explicit operator opt-in at creation:
//!   `eg ingest --encrypted --key-file <path> [--passphrase-env <VAR>]`.
//! * The store's own metadata is the authority afterwards. Every later open
//!   (`ingest`, `inspect`, `query`, `export`, daemon) resolves the mode from
//!   the [`StoreMarker`] (`{data_dir}/egregore-store.json`, written only for
//!   encrypted stores; absent means legacy plaintext) cross-checked against
//!   AletheiaDB's durable `encryption.state` authority. No per-command flags.
//! * Key sources are pinned at creation: a raw 32-byte key file ([`KeyProviderConfig::File`])
//!   or a passphrase-wrapped AEKF file ([`KeyProviderConfig::PassphraseFile`],
//!   Argon2id). The marker and the engine authority carry only the *non-secret*
//!   descriptor (paths, env-var names) — never key material.
//! * Fail-closed everywhere: a missing/unreadable key, a wrong key, or a
//!   marker/authority disagreement refuses the open; `--encrypted` on an
//!   existing plaintext store is refused (`storage_mode_mismatch`) — there is
//!   no live migration.
//! * Encryption is orthogonal to redaction: the redaction gate
//!   ([`crate::redaction::validate_record`]) runs on records above the storage
//!   layer and is unchanged by the storage mode.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use aletheiadb::encryption::{Algorithm, EncryptionConfig, KeyProviderConfig};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// File name of the Egregore storage-mode marker inside the data dir.
///
/// Written **only** for encrypted stores. An absent marker means legacy
/// plaintext — the default workflow is unchanged.
pub const STORE_MARKER_FILE: &str = "egregore-store.json";

/// Schema version of the [`StoreMarker`] JSON document.
pub const STORE_MARKER_SCHEMA_VERSION: u32 = 1;

/// Stable machine code: the requested storage mode disagrees with the store's
/// durable state (e.g. `--encrypted` on an existing plaintext store, or the
/// marker and the engine authority disagree).
pub const STORAGE_MODE_MISMATCH_CODE: &str = "storage_mode_mismatch";

/// Stable machine code: an encrypted store's key material cannot be loaded
/// (key file missing/unreadable, passphrase env var unset).
pub const ENCRYPTED_STORE_KEY_UNAVAILABLE_CODE: &str = "encrypted_store_key_unavailable";

/// Stable machine code: the key material was loaded but the encrypted store
/// failed to open under it (wrong key, corrupt key file).
pub const ENCRYPTED_STORE_KEY_ERROR_CODE: &str = "encrypted_store_key_error";

/// Storage mode of an embedded store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageMode {
    /// Default. No marker file; the engine authority must agree.
    #[default]
    Plaintext,
    /// Encrypted at rest. Marker file present; engine authority must agree.
    Encrypted,
}

impl StorageMode {
    /// Machine-readable mode name used in metadata and status responses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plaintext => "plaintext",
            Self::Encrypted => "encrypted",
        }
    }
}

/// Non-secret key-source descriptor persisted in the [`StoreMarker`].
///
/// Mirrors the non-secret fields of [`KeyProviderConfig`] — paths and
/// env-var *names* only. Key material (raw bytes, passphrases) is never
/// persisted here or anywhere else Egregore writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum KeySourceDescriptor {
    /// Raw 32-byte key file.
    File {
        /// Path to the key file, as given at creation.
        path: PathBuf,
    },
    /// Passphrase-wrapped AEKF key file (Argon2id).
    PassphraseFile {
        /// Path to the AEKF file, as given at creation.
        path: PathBuf,
        /// Name of the environment variable holding the passphrase.
        passphrase_env: String,
    },
}

impl KeySourceDescriptor {
    /// Machine-readable key-source type token (for metadata/status).
    #[must_use]
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::PassphraseFile { .. } => "passphrase_file",
        }
    }

    /// Human/machine-readable key-source reference naming the source without
    /// revealing secrets (paths and env-var names are non-secret).
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::File { path } => format!("key file {}", path.display()),
            Self::PassphraseFile {
                path,
                passphrase_env,
            } => format!(
                "passphrase-wrapped key file {} (passphrase from ${})",
                path.display(),
                passphrase_env
            ),
        }
    }

    /// Build the live [`KeyProviderConfig`] for this descriptor.
    fn to_provider_config(&self) -> KeyProviderConfig {
        match self {
            Self::File { path } => KeyProviderConfig::File { path: path.clone() },
            Self::PassphraseFile {
                path,
                passphrase_env,
            } => KeyProviderConfig::PassphraseFile {
                path: path.clone(),
                passphrase_env: passphrase_env.clone(),
            },
        }
    }

    /// Derive the non-secret descriptor from a live key-source config.
    ///
    /// Returns `None` for KMS/Vault sources, which Egregore does not offer
    /// (out of scope for issue #54).
    #[must_use]
    pub fn from_provider_config(config: &KeyProviderConfig) -> Option<Self> {
        match config {
            KeyProviderConfig::File { path } => Some(Self::File { path: path.clone() }),
            KeyProviderConfig::PassphraseFile {
                path,
                passphrase_env,
            } => Some(Self::PassphraseFile {
                path: path.clone(),
                passphrase_env: passphrase_env.clone(),
            }),
            _ => None,
        }
    }
}

/// Storage-mode marker persisted at `{data_dir}/egregore-store.json`.
///
/// Written once, atomically, when a store is created with `--encrypted`.
/// Carries no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreMarker {
    /// [`STORE_MARKER_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Always [`StorageMode::Encrypted`] — the file's existence already
    /// implies it; the field keeps the document self-describing.
    pub storage_mode: StorageMode,
    /// Pinned non-secret key-source descriptor.
    pub key_source: KeySourceDescriptor,
    /// RFC 3339 creation instant.
    pub created_at: String,
    /// Egregore version that created the store.
    pub created_by: String,
}

impl StoreMarker {
    /// Build a fresh marker for a newly encrypted store.
    #[must_use]
    pub fn new(key_source: KeySourceDescriptor) -> Self {
        let created_at = SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
            |_| "1970-01-01T00:00:00Z".to_owned(),
            |d| {
                chrono::DateTime::<chrono::Utc>::from(UNIX_EPOCH + d)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            },
        );
        Self {
            schema_version: STORE_MARKER_SCHEMA_VERSION,
            storage_mode: StorageMode::Encrypted,
            key_source,
            created_at,
            created_by: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Encrypted-store errors. Every variant carries a stable machine code as its
/// `Display` prefix so CLI and daemon surfaces can match on it.
#[derive(Debug, thiserror::Error)]
pub enum EncryptedStoreError {
    /// The requested mode disagrees with the store's durable state.
    #[error("{STORAGE_MODE_MISMATCH_CODE}: {message}")]
    StorageModeMismatch {
        /// Data dir the refusal concerns.
        data_dir: String,
        /// Human-readable diagnosis naming the disagreement and the remedy.
        message: String,
    },
    /// Key material for an encrypted store cannot be loaded.
    #[error("{ENCRYPTED_STORE_KEY_UNAVAILABLE_CODE}: {message}")]
    KeyUnavailable {
        /// Non-secret key-source reference (path / env-var name).
        key_source: String,
        /// Diagnosis naming what is missing and the remedy.
        message: String,
    },
    /// Key material loaded, but the encrypted store failed to open under it.
    #[error("{ENCRYPTED_STORE_KEY_ERROR_CODE}: {message}")]
    KeyError {
        /// Non-secret key-source reference (path / env-var name).
        key_source: String,
        /// Upstream detail plus the remedy.
        message: String,
    },
    /// The marker file itself is unreadable or corrupt.
    #[error("store marker unreadable: {message}")]
    MarkerUnreadable {
        /// Data dir the marker belongs to.
        data_dir: String,
        /// Detail.
        message: String,
    },
    /// Flag combination or key-source construction error (creation time).
    #[error("invalid encryption options: {message}")]
    InvalidOptions {
        /// Detail naming the bad combination and the remedy.
        message: String,
    },
}

impl EncryptedStoreError {
    /// Stable machine code for this error.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::StorageModeMismatch { .. } => STORAGE_MODE_MISMATCH_CODE,
            Self::KeyUnavailable { .. } => ENCRYPTED_STORE_KEY_UNAVAILABLE_CODE,
            Self::KeyError { .. } => ENCRYPTED_STORE_KEY_ERROR_CODE,
            Self::MarkerUnreadable { .. } => "store_marker_unreadable",
            Self::InvalidOptions { .. } => "invalid_encryption_options",
        }
    }

    /// Convert into the adapter-layer error type used by store opens.
    #[must_use]
    pub fn to_adapter_error(&self) -> crate::adapters::AdapterError {
        use crate::adapters::AdapterError;
        match self {
            Self::StorageModeMismatch { data_dir, message } => AdapterError::StorageModeMismatch {
                data_dir: data_dir.clone(),
                message: message.clone(),
            },
            Self::KeyUnavailable {
                key_source,
                message,
            } => AdapterError::EncryptedStoreKeyUnavailable {
                key_source: key_source.clone(),
                message: message.clone(),
            },
            Self::KeyError {
                key_source,
                message,
            } => AdapterError::EncryptedStoreKeyError {
                key_source: key_source.clone(),
                message: message.clone(),
            },
            Self::MarkerUnreadable { data_dir, message } => AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: format!("store marker unreadable for {data_dir}: {message}"),
            },
            Self::InvalidOptions { message } => AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: message.clone(),
            },
        }
    }
}

/// Read the Egregore storage-mode marker.
///
/// * `Ok(None)` — no marker: legacy plaintext store (default workflow).
/// * `Ok(Some(_))` — encrypted store; the descriptor pins the key source.
/// * `Err(_)` — marker present but unreadable/corrupt: fail closed, never
///   silently treated as plaintext.
///
/// # Errors
///
/// Returns [`EncryptedStoreError::MarkerUnreadable`] when the marker file is
/// present but cannot be read or parsed, or carries an unsupported schema.
pub fn read_marker(data_dir: &Path) -> Result<Option<StoreMarker>, EncryptedStoreError> {
    let path = data_dir.join(STORE_MARKER_FILE);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(EncryptedStoreError::MarkerUnreadable {
                data_dir: data_dir.display().to_string(),
                message: error.to_string(),
            });
        }
    };
    let marker: StoreMarker =
        serde_json::from_str(&contents).map_err(|error| EncryptedStoreError::MarkerUnreadable {
            data_dir: data_dir.display().to_string(),
            message: format!(
                "{} is corrupt ({}); manual intervention required",
                path.display(),
                error
            ),
        })?;
    if marker.schema_version != STORE_MARKER_SCHEMA_VERSION {
        return Err(EncryptedStoreError::MarkerUnreadable {
            data_dir: data_dir.display().to_string(),
            message: format!(
                "unsupported marker schema_version {}",
                marker.schema_version
            ),
        });
    }
    if marker.storage_mode != StorageMode::Encrypted {
        return Err(EncryptedStoreError::MarkerUnreadable {
            data_dir: data_dir.display().to_string(),
            message: "marker storage_mode is not encrypted".to_owned(),
        });
    }
    Ok(Some(marker))
}

/// Write the storage-mode marker atomically (temp file + rename).
///
/// Called once, after the engine's durable encryption authority has been
/// flipped — the marker never leads the authority.
///
/// # Errors
///
/// Returns [`EncryptedStoreError::MarkerUnreadable`] when the marker cannot
/// be written.
pub fn write_marker(data_dir: &Path, marker: &StoreMarker) -> Result<(), EncryptedStoreError> {
    let path = data_dir.join(STORE_MARKER_FILE);
    let contents = serde_json::to_string_pretty(marker).map_err(|error| {
        EncryptedStoreError::MarkerUnreadable {
            data_dir: data_dir.display().to_string(),
            message: error.to_string(),
        }
    })?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file =
            fs::File::create(&tmp).map_err(|error| EncryptedStoreError::MarkerUnreadable {
                data_dir: data_dir.display().to_string(),
                message: error.to_string(),
            })?;
        file.write_all(contents.as_bytes()).map_err(|error| {
            EncryptedStoreError::MarkerUnreadable {
                data_dir: data_dir.display().to_string(),
                message: error.to_string(),
            }
        })?;
        file.sync_all()
            .map_err(|error| EncryptedStoreError::MarkerUnreadable {
                data_dir: data_dir.display().to_string(),
                message: error.to_string(),
            })?;
    }
    fs::rename(&tmp, &path).map_err(|error| EncryptedStoreError::MarkerUnreadable {
        data_dir: data_dir.display().to_string(),
        message: error.to_string(),
    })?;
    Ok(())
}

/// Read `AletheiaDB`'s durable encryption authority (`{data_dir}/encryption.state`).
///
/// Returns `Ok(None)` when the authority file is absent (fresh or plaintext
/// store). Parses only the `enabled=` line — the Egregore marker (not this
/// file) is the key-source authority, so the full source descriptor is not
/// needed here. A present-but-unparseable authority fails closed.
fn read_engine_authority_enabled(data_dir: &Path) -> Result<Option<bool>, EncryptedStoreError> {
    let path = data_dir.join("encryption.state");
    let body = match fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(EncryptedStoreError::StorageModeMismatch {
                data_dir: data_dir.display().to_string(),
                message: format!(
                    "encryption.state present but unreadable ({error}); manual intervention required"
                ),
            });
        }
    };
    for line in body.lines() {
        if let Some(value) = line.strip_prefix("enabled=") {
            return match value {
                "true" => Ok(Some(true)),
                "false" => Ok(Some(false)),
                other => Err(EncryptedStoreError::StorageModeMismatch {
                    data_dir: data_dir.display().to_string(),
                    message: format!(
                        "encryption.state carries an unparseable enabled={other:?}; manual intervention required"
                    ),
                }),
            };
        }
    }
    Err(EncryptedStoreError::StorageModeMismatch {
        data_dir: data_dir.display().to_string(),
        message: "encryption.state has no enabled= line; manual intervention required".to_owned(),
    })
}

/// Check that the key material a descriptor points at can be loaded, without
/// reading secrets into Egregore-owned buffers longer than necessary.
///
/// * `File` — the key file must exist and be readable.
/// * `PassphraseFile` — the AEKF file must exist and the passphrase env var
///   must be set and non-empty.
///
/// Failures map to [`EncryptedStoreError::KeyUnavailable`]; the error names
/// the key *source* (path / variable name), never the key material.
fn check_key_material_available(
    descriptor: &KeySourceDescriptor,
) -> Result<(), EncryptedStoreError> {
    let source = descriptor.describe();
    match descriptor {
        KeySourceDescriptor::File { path } => match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(()),
            Ok(_) => Err(EncryptedStoreError::KeyUnavailable {
                key_source: source,
                message: format!(
                    "not a regular file: {}. Remedy: point the store at the key file created with `eg keygen`.",
                    path.display()
                ),
            }),
            Err(error) => Err(EncryptedStoreError::KeyUnavailable {
                key_source: source,
                message: format!(
                    "cannot read {} ({}). Remedy: restore the key file or point the store at a valid one; \
                     without the key the store cannot be opened.",
                    path.display(),
                    error
                ),
            }),
        },
        KeySourceDescriptor::PassphraseFile {
            path,
            passphrase_env,
        } => {
            if fs::metadata(path).is_err() {
                return Err(EncryptedStoreError::KeyUnavailable {
                    key_source: source,
                    message: format!(
                        "cannot read {}. Remedy: restore the passphrase-wrapped key file created with `eg keygen`.",
                        path.display()
                    ),
                });
            }
            match std::env::var(passphrase_env) {
                Ok(passphrase) if !passphrase.is_empty() => {
                    // Zeroize the env-var copy immediately; the engine reads
                    // the env var itself at open. The buffer is deliberately
                    // never read, only wiped, hence the scoped allow.
                    #[allow(clippy::collection_is_never_read)]
                    let mut owned = passphrase;
                    owned.zeroize();
                    Ok(())
                }
                _ => Err(EncryptedStoreError::KeyUnavailable {
                    key_source: source,
                    message: format!(
                        "environment variable ${passphrase_env} is unset or empty. Remedy: export it \
                         with the passphrase before opening the store."
                    ),
                }),
            }
        }
    }
}

/// Resolved encryption posture for opening `data_dir`.
pub struct ResolvedEncryption {
    /// Engine encryption config to install on the open.
    pub config: EncryptionConfig,
    /// Storage mode the marker/authority resolved to.
    pub mode: StorageMode,
    /// Pinned non-secret key-source descriptor (`Some` iff encrypted).
    pub key_source: Option<KeySourceDescriptor>,
}

/// Resolve the [`EncryptionConfig`] for opening `data_dir`, fail-closed.
///
/// This is the single hook every embedded open funnels through
/// (`EmbeddedAletheiaSink::open_inner`):
///
/// * Marker says encrypted → the engine authority must agree (`enabled=true`);
///   the key material must be loadable; returns an enabled config built from
///   the pinned descriptor.
/// * No marker (legacy plaintext) → the engine authority must not claim
///   enabled; returns the disabled config — the default workflow, unchanged.
/// * Any disagreement → [`EncryptedStoreError::StorageModeMismatch`].
/// * Missing/unloadable key material → [`EncryptedStoreError::KeyUnavailable`].
///
/// A wrong key (material loads, decryption fails) surfaces at open time and
/// is classified by the caller into [`EncryptedStoreError::KeyError`] via
/// [`classify_encrypted_open_error`].
///
/// # Errors
///
/// Returns [`EncryptedStoreError::StorageModeMismatch`] when the marker and
/// the engine authority disagree, [`EncryptedStoreError::KeyUnavailable`]
/// when the pinned key material cannot be loaded, and
/// [`EncryptedStoreError::MarkerUnreadable`] when the marker or authority
/// cannot be read.
pub fn resolve_encryption_config(
    data_dir: &Path,
) -> Result<ResolvedEncryption, EncryptedStoreError> {
    let marker = read_marker(data_dir)?;
    let authority_enabled = read_engine_authority_enabled(data_dir)?;

    match (marker, authority_enabled) {
        (Some(marker), Some(true)) => {
            check_key_material_available(&marker.key_source)?;
            let config = EncryptionConfig {
                enabled: true,
                algorithm: Algorithm::Auto,
                key_provider: marker.key_source.to_provider_config(),
                audit: aletheiadb::encryption::AuditConfig::default(),
            };
            Ok(ResolvedEncryption {
                config,
                mode: StorageMode::Encrypted,
                key_source: Some(marker.key_source),
            })
        }
        (Some(_), authority) => Err(EncryptedStoreError::StorageModeMismatch {
            data_dir: data_dir.display().to_string(),
            message: format!(
                "the Egregore store marker claims this store is encrypted but the engine \
                 authority disagrees (enabled={authority:?}); refusing to open rather than \
                 guessing. Remedy: restore encryption.state from backup, or re-examine which \
                 store this directory holds."
            ),
        }),
        (None, Some(true)) => Err(EncryptedStoreError::StorageModeMismatch {
            data_dir: data_dir.display().to_string(),
            message: format!(
                "the engine authority claims {} is encrypted but Egregore has no \
                 storage-mode marker for it (encryption was enabled outside `eg`, or the \
                 marker was deleted); refusing to open rather than guessing the key source. \
                 Remedy: re-create the store with `eg ingest --encrypted`, or restore the marker.",
                data_dir.display()
            ),
        }),
        (None, _) => Ok(ResolvedEncryption {
            config: EncryptionConfig::disabled(),
            mode: StorageMode::Plaintext,
            key_source: None,
        }),
    }
}

/// Whether the data dir looks like a fresh (never-opened) store.
#[must_use]
pub fn is_fresh_store(data_dir: &Path) -> bool {
    match fs::read_dir(data_dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// Write the `encryption.state` authority file for a fresh encrypted store.
///
/// Issue #54: aletheiadb 0.2.0's `enable_encryption` does not write the
/// authority file for fresh databases. Writing it before the first open lets
/// the engine enable encryption from the start, instead of relying on the
/// migration path.
///
/// # Errors
///
/// Returns [`EncryptedStoreError::KeyUnavailable`] when the data dir cannot
/// be created, the key source cannot be serialized, or the authority file
/// cannot be written.
pub fn write_fresh_encryption_state(
    data_dir: &Path,
    key_source: &aletheiadb::encryption::KeyProviderConfig,
) -> Result<(), EncryptedStoreError> {
    let state_path = data_dir.join("encryption.state");
    if state_path.exists() {
        return Ok(());
    }
    let key_json =
        serde_json::to_string(key_source).map_err(|e| EncryptedStoreError::KeyUnavailable {
            key_source: "key-source".to_owned(),
            message: format!("failed to serialize key source: {e}"),
        })?;
    let content =
        format!("version=3\nenabled=true\nkey_source_json={key_json}\nalgorithm=aes256gcm\n");
    fs::create_dir_all(data_dir).map_err(|e| EncryptedStoreError::KeyUnavailable {
        key_source: "data-dir".to_owned(),
        message: format!("failed to create data dir: {e}"),
    })?;
    fs::write(&state_path, content).map_err(|e| EncryptedStoreError::KeyUnavailable {
        key_source: "encryption.state".to_owned(),
        message: format!("failed to write encryption.state: {e}"),
    })?;
    Ok(())
}

/// Build the live [`KeyProviderConfig`] from the `eg ingest --encrypted`
/// flags.
///
/// * `--key-file <path>` alone → raw 32-byte key file.
/// * `--key-file <path> --passphrase-env <VAR>` → passphrase-wrapped AEKF file.
/// * KMS/Vault sources are not offered (out of scope).
///
/// Fail-closed: refuses to construct a config whose material cannot be
/// loaded (missing key file, unset passphrase var), so a typo'd path can
/// never silently create a store nobody can reopen.
///
/// # Errors
///
/// Returns [`EncryptedStoreError::InvalidOptions`] when `--key-file` is
/// missing, not a regular file, or not a valid raw key file, or when the
/// passphrase env var is unset or empty.
pub fn key_source_from_flags(
    key_file: Option<&Path>,
    passphrase_env: Option<&str>,
) -> Result<KeyProviderConfig, EncryptedStoreError> {
    let key_file = key_file.ok_or_else(|| EncryptedStoreError::InvalidOptions {
        message:
            "--encrypted requires --key-file <path> naming the key file (created with `eg keygen`)"
                .to_owned(),
    })?;
    if !key_file.is_file() {
        return Err(EncryptedStoreError::InvalidOptions {
            message: format!(
                "--key-file {} does not exist or is not a regular file; create it with `eg keygen --out {}` first",
                key_file.display(),
                key_file.display()
            ),
        });
    }
    let descriptor = if let Some(var) = passphrase_env {
        if std::env::var(var).map_or(true, |v| v.is_empty()) {
            return Err(EncryptedStoreError::InvalidOptions {
                message: format!(
                    "--passphrase-env {var} names an env var that is unset or empty; export the passphrase first"
                ),
            });
        }
        KeySourceDescriptor::PassphraseFile {
            path: key_file.to_path_buf(),
            passphrase_env: var.to_owned(),
        }
    } else {
        aletheiadb::encryption::cli::validate_key_file(key_file).map_err(|error| {
            EncryptedStoreError::InvalidOptions {
                message: format!(
                    "--key-file {} is not a valid raw key file ({}); create one with `eg keygen --out {}`",
                    key_file.display(),
                    error,
                    key_file.display()
                ),
            }
        })?;
        KeySourceDescriptor::File {
            path: key_file.to_path_buf(),
        }
    };
    Ok(descriptor.to_provider_config())
}

/// Decide what `eg ingest --encrypted` should do for `data_dir`.
///
/// Returns `Ok(Some(key_source))` when the caller must enable encryption after
/// opening (fresh store), `Ok(None)` when the store is already encrypted (the
/// `--encrypted` flags are redundant; the pinned marker governs), and
/// `Err(StorageModeMismatch)` when the store already exists as plaintext —
/// there is no live migration.
///
/// # Errors
///
/// Returns [`EncryptedStoreError::InvalidOptions`] for bad flag
/// combinations, [`EncryptedStoreError::StorageModeMismatch`] when the store
/// already exists as plaintext, and [`EncryptedStoreError::MarkerUnreadable`]
/// / [`EncryptedStoreError::KeyUnavailable`] when the marker or authority
/// cannot be written.
pub fn prepare_encrypted_ingest(
    data_dir: &Path,
    key_file: Option<&Path>,
    passphrase_env: Option<&str>,
) -> Result<Option<KeyProviderConfig>, EncryptedStoreError> {
    let key_source = key_source_from_flags(key_file, passphrase_env)?;
    if is_fresh_store(data_dir) {
        // Issue #54: write the authority file and marker before the first
        // open so the engine enables encryption from the start. The marker
        // never leads the authority — the state file is written first.
        write_fresh_encryption_state(data_dir, &key_source)?;
        if let Some(descriptor) = KeySourceDescriptor::from_provider_config(&key_source) {
            let marker = StoreMarker::new(descriptor);
            write_marker(data_dir, &marker)?;
        }
        return Ok(Some(key_source));
    }
    match read_marker(data_dir)? {
        Some(_) => Ok(None),
        None => Err(EncryptedStoreError::StorageModeMismatch {
            data_dir: data_dir.display().to_string(),
            message: format!(
                "{} already exists as a plaintext store; --encrypted cannot convert it in place \
                 (no live migration). Remedy: ingest into a fresh --data-dir with --encrypted, \
                 or keep using this store as plaintext.",
                data_dir.display()
            ),
        }),
    }
}

/// Generate a key file for `eg keygen`.
///
/// * Without `passphrase_env`: a raw 32-byte key file (0600, refuses to
///   overwrite).
/// * With `passphrase_env`: an AEKF passphrase-wrapped file (Argon2id);
///   the passphrase is read from the named env var and zeroized after use.
///
/// Returns the non-secret descriptor. Never prints or logs key material.
///
/// # Errors
///
/// Returns [`EncryptedStoreError::InvalidOptions`] when the passphrase env
/// var is unset or empty, the target path already exists, or the key file
/// cannot be written.
pub fn generate_key_file(
    path: &Path,
    passphrase_env: Option<&str>,
) -> Result<KeySourceDescriptor, EncryptedStoreError> {
    use aletheiadb::encryption::cli as encryption_cli;

    if let Some(var) = passphrase_env {
        let passphrase = std::env::var(var).map_err(|_| EncryptedStoreError::InvalidOptions {
            message: format!(
                "--passphrase-env {var} names an env var that is unset; export the passphrase first"
            ),
        })?;
        if passphrase.is_empty() {
            return Err(EncryptedStoreError::InvalidOptions {
                message: format!("--passphrase-env {var} names an env var that is empty"),
            });
        }
        let result =
            encryption_cli::generate_passphrase_key_with_overwrite(path, &passphrase, false);
        // Zeroize our copy of the passphrase the moment it is no longer
        // needed. The buffer is deliberately never read, only wiped, hence
        // the scoped allow.
        #[allow(clippy::collection_is_never_read)]
        let mut owned = passphrase;
        owned.zeroize();
        result.map_err(|error| EncryptedStoreError::InvalidOptions {
            message: format!(
                "failed to write passphrase-wrapped key file {}: {error}",
                path.display()
            ),
        })?;
        Ok(KeySourceDescriptor::PassphraseFile {
            path: path.to_path_buf(),
            passphrase_env: var.to_owned(),
        })
    } else {
        encryption_cli::generate_key_with_overwrite(path, false).map_err(|error| {
            EncryptedStoreError::InvalidOptions {
                message: format!("failed to write key file {}: {error}", path.display()),
            }
        })?;
        Ok(KeySourceDescriptor::File {
            path: path.to_path_buf(),
        })
    }
}

/// Classify an open failure for a store the marker claims is encrypted.
///
/// When the key material loaded but `AletheiaDB::open` still failed, the
/// overwhelmingly likely cause is a wrong key (or a key file whose bytes
/// changed since creation). The upstream detail is preserved verbatim for
/// diagnosis.
#[must_use]
pub fn classify_encrypted_open_error(
    data_dir: &Path,
    descriptor: &KeySourceDescriptor,
    upstream: &str,
) -> EncryptedStoreError {
    EncryptedStoreError::KeyError {
        key_source: descriptor.describe(),
        message: format!(
            "failed to open the encrypted store at {} with {}; the key material did not decrypt \
             the store (wrong key, or the key file changed since creation). Nothing was modified. \
             Remedy: restore the key file used at creation, then retry. Upstream detail: {upstream}",
            data_dir.display(),
            descriptor.describe()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_gate_rejects_unredacted_record() {
        // AC2 (issue #54): the encryption feature must not weaken or bypass
        // the redaction gate. The gate is a pure function over records above
        // the storage layer — this pins its rejection of an unredacted
        // sensitive record, identical for plaintext and encrypted stores.
        let mut record = crate::ir::GraphRecord::node(
            "agent_memory:v1:eg54test".to_owned(),
            crate::ir::NodeKind::Observation,
            None,
            None,
            Some("test observation".to_owned()),
            "test observation".to_owned(),
        );
        if let crate::ir::GraphRecord::Node { text, .. } = &mut record {
            *text = Some("saw a live sk-eg54testsecret123456789".to_owned());
        }
        let error = crate::redaction::validate_record(&record)
            .expect_err("unredacted secret must fail the gate");
        assert!(
            matches!(error, crate::CodegraphError::RedactionRequired { .. }),
            "gate must raise RedactionRequired, got {error:?}"
        );
    }

    #[test]
    fn marker_round_trip() {
        let marker = StoreMarker::new(KeySourceDescriptor::File {
            path: PathBuf::from("/keys/eg.bin"),
        });
        let json = serde_json::to_string(&marker).expect("serialize");
        assert!(json.contains("\"encrypted\""));
        assert!(!json.contains("eg54secret"), "no secrets in fixture");
        let back: StoreMarker = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(marker, back);
    }

    #[test]
    fn fresh_store_detection() {
        let temp = tempfile::tempdir().expect("temp");
        assert!(is_fresh_store(temp.path()));
        fs::write(temp.path().join("x"), "y").expect("write");
        assert!(!is_fresh_store(temp.path()));
        assert!(is_fresh_store(&temp.path().join("missing")));
    }
}
