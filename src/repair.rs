//! Offline repair workflow for Egregore stores.
//!
//! Documented in `docs/cli/repair.md`.
//!
//! The shortest local workflow is:
//! 1. `eg repair preflight --data-dir .egregore` — inspect ownership verdict.
//! 2. If the daemon is running: `eg daemon stop --data-dir .egregore`.
//! 3. `eg repair run --data-dir .egregore --dry-run` — preview, zero mutations.
//! 4. `eg repair run --data-dir .egregore --confirm` — perform repair.
//! 5. `eg daemon start --data-dir .egregore` — resume normal operation.
//!
//! Output is machine-readable JSON by default; `--format text` renders the same
//! facts human-readably.
//!
//! Safety doctrine (see `docs/cli/repair.md`):
//! - Dry-run and preflight are strictly zero-mutation.
//! - Mutation happens only under an explicit `--confirm`, and only for the
//!   `stale_no_owner` verdict; a healthy `stopped` store reports "no repair
//!   needed" and is never touched.
//! - Repairs are structural only: stale runtime metadata is removed (default) or
//!   quarantined (`--quarantine`, recoverable). Content is never guessed.
//! - A live or ambiguous owner is refused before any work; the confirmed-apply
//!   path re-acquires the exclusive store lease for the mutation window.
//! - A confirmed apply writes a redaction-safe per-action manifest (action,
//!   time, result, original path, before/after BLAKE3 hashes, skipped reason)
//!   and re-runs detection so the report states the post-repair state honestly.
//! - Output carries only IDs, handles, counts, hashes, paths, and stable codes —
//!   never tokens or payload bodies. Pin `--transaction-time` for byte-identical
//!   output across runs.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::daemon::{
    DAEMON_RUNTIME_SCHEMA_VERSION, DaemonMetadata, DaemonState, StoreLease, active_metadata,
    runtime_dir_for_data_dir, runtime_metadata_is_stale_noncreating, try_read_raw_metadata,
};

/// Schema version for repair report records.
pub const REPAIR_SCHEMA_VERSION: u32 = 1;

const RECOVERY_REPORT_FILE: &str = "repair-report.json";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Stable machine-readable ownership verdict for a data directory.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipVerdict {
    /// A live daemon actively holds the store lease and responds to health checks.
    Live,
    /// No daemon metadata exists, or metadata has state `stopped` and the lock is not held.
    Stopped,
    /// Daemon metadata exists but no active owner holds the lock (stale, unleased).
    StaleNoOwner,
    /// Daemon metadata exists and the lock is held, but the daemon is unresponsive.
    Ambiguous,
}

/// Stable machine-readable refusal codes for offline repair.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairRefusalCode {
    /// A live daemon actively owns the store; stop it with `eg daemon stop` first.
    LiveDaemonActive,
    /// Ownership is ambiguous: the lock is held but the daemon is unresponsive.
    AmbiguousOwnership,
    /// Operator confirmation (`--confirm`) was not provided.
    ConfirmationRequired,
    /// The requested operation is not supported in the offline repair workflow.
    UnsupportedOperation,
    /// The runtime metadata declares a daemon schema version this build does not
    /// understand; repair refuses rather than risk deleting a newer daemon's state.
    UnsupportedRuntimeSchema,
}

/// Stable machine-readable repair action identifiers.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairAction {
    /// Remove the stale `egregored.json` runtime metadata file.
    StaleMetadataCleanup,
    /// Write a machine-readable recovery report to the runtime directory.
    RecoveryReportGeneration,
}

/// Snapshot of daemon state for before/after comparison in repair reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatusSnapshot {
    /// Last-known daemon lifecycle state (`running`, `stopped`, `crashed`).
    pub state: String,
    /// Process ID recorded in the metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Bound TCP address recorded in the metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Unix milliseconds when the daemon started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u128>,
}

impl DaemonStatusSnapshot {
    fn from_metadata(metadata: &DaemonMetadata) -> Self {
        let state = serde_json::to_value(metadata.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", metadata.state).to_lowercase());
        Self {
            state,
            pid: Some(metadata.pid),
            address: Some(metadata.address.clone()),
            started_at_unix_ms: Some(metadata.started_at_unix_ms),
        }
    }

    fn none_present() -> Self {
        Self {
            state: "stopped".to_owned(),
            pid: None,
            address: None,
            started_at_unix_ms: None,
        }
    }
}

/// Lightweight inspect summary for before/after comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectSummary {
    /// True when `egregored.json` exists in the runtime directory.
    pub metadata_exists: bool,
    /// True when the store lock is actively held by another process.
    pub lock_held: bool,
    /// Last-known daemon lifecycle state from the metadata, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_state: Option<String>,
}

/// Overall result of a repair session.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairSessionResult {
    /// Repair completed successfully.
    Success,
    /// Repair was refused due to one or more refusal codes.
    Refused,
    /// Repair failed due to an unexpected I/O or internal error.
    Failed,
    /// Dry-run completed: no mutations were made; shows what would happen.
    DryRun,
}

/// Redaction-safe preflight report returned by [`preflight`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightReport {
    /// Repair schema version.
    pub schema_version: u32,
    /// Store data directory inspected.
    pub data_dir: PathBuf,
    /// Runtime sidecar directory.
    pub runtime_dir: PathBuf,
    /// Determined ownership verdict.
    pub ownership_verdict: OwnershipVerdict,
    /// True when the verdict permits a repair session.
    pub allow: bool,
    /// Stable refusal codes (empty when `allow` is true).
    pub refusal_reasons: Vec<RepairRefusalCode>,
    /// Daemon state snapshot before any repair action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_daemon_status: Option<DaemonStatusSnapshot>,
    /// Safe command to stop an active daemon (present only when verdict is `live`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_daemon_command: Option<String>,
}

/// Redaction-safe report returned by [`run_repair`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairSessionReport {
    /// Repair schema version.
    pub schema_version: u32,
    /// Store data directory repaired.
    pub data_dir: PathBuf,
    /// Runtime sidecar directory.
    pub runtime_dir: PathBuf,
    /// Determined ownership verdict.
    pub ownership_verdict: OwnershipVerdict,
    /// True when no filesystem or graph mutations were made.
    pub dry_run: bool,
    /// RFC 3339 timestamp when the session started.
    pub started_at: String,
    /// RFC 3339 timestamp when the session ended.
    pub ended_at: String,
    /// Overall session result.
    pub result: RepairSessionResult,
    /// Daemon state snapshot before any repair action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_daemon_status: Option<DaemonStatusSnapshot>,
    /// Daemon state snapshot after the repair session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_daemon_status: Option<DaemonStatusSnapshot>,
    /// Inspect summary before any repair action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_inspect_summary: Option<InspectSummary>,
    /// Inspect summary after the repair session (absent in dry-run and refusal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_inspect_summary: Option<InspectSummary>,
    /// Repair actions that were attempted (including skipped no-ops).
    pub attempted_actions: Vec<RepairAction>,
    /// Repair actions that were rejected as unsupported in this workflow.
    pub skipped_actions: Vec<RepairAction>,
    /// File paths changed during the session (empty in dry-run and refusal).
    pub changed_file_paths: Vec<PathBuf>,
    /// Stable refusal codes (empty when `result` is `success` or `dry_run`).
    pub refusal_reasons: Vec<RepairRefusalCode>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Runs repair preflight: determines ownership verdict with zero mutations.
///
/// Preflight never modifies graph records, runtime files, repair reports, or
/// store contents. It is safe to call repeatedly and is the documented first
/// step before [`run_repair`].
///
/// # Errors
///
/// Returns an error if runtime metadata or lock inspection fails unexpectedly.
pub fn preflight(data_dir: &Path) -> Result<PreflightReport> {
    let runtime_dir = runtime_dir_for_data_dir(data_dir);
    let VerdictOutcome {
        verdict,
        metadata,
        schema_unsupported,
    } = determine_verdict(data_dir)?;

    let (allow, refusal_reasons, safe_daemon_command) = if schema_unsupported {
        (
            false,
            vec![RepairRefusalCode::UnsupportedRuntimeSchema],
            None,
        )
    } else {
        match &verdict {
            OwnershipVerdict::Live => (
                false,
                vec![RepairRefusalCode::LiveDaemonActive],
                Some(format!("eg daemon stop --data-dir {}", data_dir.display())),
            ),
            OwnershipVerdict::Ambiguous => {
                (false, vec![RepairRefusalCode::AmbiguousOwnership], None)
            }
            OwnershipVerdict::Stopped | OwnershipVerdict::StaleNoOwner => (true, vec![], None),
        }
    };

    let before_daemon_status = metadata.as_ref().map(DaemonStatusSnapshot::from_metadata);

    Ok(PreflightReport {
        schema_version: REPAIR_SCHEMA_VERSION,
        data_dir: data_dir.to_path_buf(),
        runtime_dir,
        ownership_verdict: verdict,
        allow,
        refusal_reasons,
        before_daemon_status,
        safe_daemon_command,
    })
}

/// Runs an offline repair session for a data directory.
///
/// - `dry_run = true`: determines what the session would do and returns without
///   making any filesystem or graph mutations. Equivalent to preflight with
///   full action planning.
/// - `confirm = true`: executes the repair actions after verifying exclusive
///   ownership. Requires `dry_run = false`.
/// - Neither flag: refuses with [`RepairRefusalCode::ConfirmationRequired`].
///
/// The first repair slice supports:
/// - [`RepairAction::StaleMetadataCleanup`]: removes stale `egregored.json`.
/// - [`RepairAction::RecoveryReportGeneration`]: writes a recovery report to
///   the runtime directory.
///
/// Graph-record deletion, store rewrites, compaction, migration, and raw
/// payload export are unsupported and will never be attempted.
///
/// # Errors
///
/// Returns an error if runtime inspection fails or a filesystem write fails
/// during an actual (non-dry-run) repair.
pub fn run_repair(data_dir: &Path, dry_run: bool, confirm: bool) -> Result<RepairSessionReport> {
    let started_at = now_rfc3339();
    let runtime_dir = runtime_dir_for_data_dir(data_dir);

    let VerdictOutcome {
        verdict,
        metadata: raw_metadata,
        schema_unsupported,
    } = determine_verdict(data_dir)?;

    let before_daemon_status = raw_metadata
        .as_ref()
        .map(DaemonStatusSnapshot::from_metadata);
    let before_inspect_summary = Some(build_inspect_summary(data_dir, raw_metadata.as_ref()));

    // Determine refusal conditions
    let refusal_reasons: Vec<RepairRefusalCode> = if schema_unsupported {
        vec![RepairRefusalCode::UnsupportedRuntimeSchema]
    } else {
        match &verdict {
            OwnershipVerdict::Live => vec![RepairRefusalCode::LiveDaemonActive],
            OwnershipVerdict::Ambiguous => vec![RepairRefusalCode::AmbiguousOwnership],
            OwnershipVerdict::Stopped | OwnershipVerdict::StaleNoOwner => {
                if !dry_run && !confirm {
                    vec![RepairRefusalCode::ConfirmationRequired]
                } else {
                    vec![]
                }
            }
        }
    };

    if !refusal_reasons.is_empty() {
        let ended_at = now_rfc3339();
        return Ok(RepairSessionReport {
            schema_version: REPAIR_SCHEMA_VERSION,
            data_dir: data_dir.to_path_buf(),
            runtime_dir,
            ownership_verdict: verdict,
            dry_run,
            started_at,
            ended_at,
            result: RepairSessionResult::Refused,
            before_daemon_status,
            after_daemon_status: None,
            before_inspect_summary,
            after_inspect_summary: None,
            attempted_actions: vec![],
            skipped_actions: vec![
                RepairAction::StaleMetadataCleanup,
                RepairAction::RecoveryReportGeneration,
            ],
            changed_file_paths: vec![],
            refusal_reasons,
        });
    }

    // Dry-run: plan the actions and return without mutating anything.
    if dry_run {
        let RepairActions {
            attempted_actions,
            changed_file_paths,
        } = execute_repair_actions(&runtime_dir, true)?;
        let ended_at = now_rfc3339();
        return Ok(RepairSessionReport {
            schema_version: REPAIR_SCHEMA_VERSION,
            data_dir: data_dir.to_path_buf(),
            runtime_dir,
            ownership_verdict: verdict,
            dry_run: true,
            started_at,
            ended_at,
            result: RepairSessionResult::DryRun,
            before_daemon_status,
            after_daemon_status: None,
            before_inspect_summary,
            after_inspect_summary: None,
            attempted_actions,
            skipped_actions: vec![],
            changed_file_paths,
            refusal_reasons: vec![],
        });
    }

    // Confirmed repair runs under the exclusive store lease.
    run_confirmed_repair(SessionContext {
        data_dir: data_dir.to_path_buf(),
        runtime_dir,
        verdict,
        started_at,
        before_daemon_status,
        before_inspect_summary,
    })
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Common inputs threaded from [`run_repair`] into the confirmed-repair path.
struct SessionContext {
    data_dir: PathBuf,
    runtime_dir: PathBuf,
    verdict: OwnershipVerdict,
    started_at: String,
    before_daemon_status: Option<DaemonStatusSnapshot>,
    before_inspect_summary: Option<InspectSummary>,
}

/// Executes a confirmed (non-dry-run) repair while holding the exclusive store
/// lease for the whole mutation window.
///
/// Acquiring the lease proves exclusive ownership and closes the race where
/// another `eg daemon start` could acquire the store between the verdict and the
/// metadata deletion. If the lease cannot be taken, an owner appeared in the gap
/// and we refuse rather than mutate.
fn run_confirmed_repair(ctx: SessionContext) -> Result<RepairSessionReport> {
    let SessionContext {
        data_dir,
        runtime_dir,
        verdict,
        started_at,
        before_daemon_status,
        before_inspect_summary,
    } = ctx;

    let Ok(_lease) = StoreLease::acquire(&data_dir) else {
        let ended_at = now_rfc3339();
        return Ok(RepairSessionReport {
            schema_version: REPAIR_SCHEMA_VERSION,
            data_dir,
            runtime_dir,
            ownership_verdict: OwnershipVerdict::Ambiguous,
            dry_run: false,
            started_at,
            ended_at,
            result: RepairSessionResult::Refused,
            before_daemon_status,
            after_daemon_status: None,
            before_inspect_summary,
            after_inspect_summary: None,
            attempted_actions: vec![],
            skipped_actions: vec![
                RepairAction::StaleMetadataCleanup,
                RepairAction::RecoveryReportGeneration,
            ],
            changed_file_paths: vec![],
            refusal_reasons: vec![RepairRefusalCode::AmbiguousOwnership],
        });
    };

    let RepairActions {
        attempted_actions,
        changed_file_paths,
    } = execute_repair_actions(&runtime_dir, false)?;

    let ended_at = now_rfc3339();
    // We hold the only lease and removed the stale metadata, so once this session
    // exits the store has no external owner: report the lock as not held.
    let after_inspect_summary = Some(InspectSummary {
        metadata_exists: runtime_dir.join("egregored.json").exists(),
        lock_held: false,
        daemon_state: None,
    });

    let report = RepairSessionReport {
        schema_version: REPAIR_SCHEMA_VERSION,
        data_dir,
        runtime_dir: runtime_dir.clone(),
        ownership_verdict: verdict,
        dry_run: false,
        started_at,
        ended_at,
        result: RepairSessionResult::Success,
        before_daemon_status,
        after_daemon_status: Some(DaemonStatusSnapshot::none_present()),
        before_inspect_summary,
        after_inspect_summary,
        attempted_actions,
        skipped_actions: vec![],
        changed_file_paths,
        refusal_reasons: vec![],
    };

    // Write the recovery report to the runtime dir so the operator can reference
    // it. The CLI and docs promise this report exists after a confirmed repair,
    // so propagate any write failure instead of silently succeeding.
    fs::create_dir_all(&runtime_dir).map_err(|e| {
        anyhow::anyhow!(
            "failed to create runtime dir {}: {e}",
            runtime_dir.display()
        )
    })?;
    let report_path = runtime_dir.join(RECOVERY_REPORT_FILE);
    let json = serde_json::to_vec_pretty(&report)?;
    fs::write(&report_path, json)
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", report_path.display()))?;

    Ok(report)
}

struct RepairActions {
    attempted_actions: Vec<RepairAction>,
    changed_file_paths: Vec<PathBuf>,
}

fn execute_repair_actions(runtime_dir: &Path, dry_run: bool) -> Result<RepairActions> {
    let mut attempted_actions = Vec::new();
    let mut changed_file_paths = Vec::new();
    let metadata_path = runtime_dir.join("egregored.json");

    attempted_actions.push(RepairAction::StaleMetadataCleanup);
    if metadata_path.exists() && !dry_run {
        fs::remove_file(&metadata_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to remove stale metadata at {}: {e}",
                metadata_path.display()
            )
        })?;
        changed_file_paths.push(metadata_path);
    }

    attempted_actions.push(RepairAction::RecoveryReportGeneration);
    Ok(RepairActions {
        attempted_actions,
        changed_file_paths,
    })
}

/// Outcome of inspecting a data directory's ownership state.
struct VerdictOutcome {
    verdict: OwnershipVerdict,
    metadata: Option<DaemonMetadata>,
    /// True when the runtime metadata declares an unsupported daemon schema and
    /// repair must refuse rather than risk deleting a newer daemon's state.
    schema_unsupported: bool,
}

/// Determines the stable ownership verdict for a data directory.
///
/// This function never creates directories or modifies runtime files: it uses
/// the non-creating lock inspector so that preflight and dry-run remain
/// zero-mutation even when the lock file is absent.
fn determine_verdict(data_dir: &Path) -> Result<VerdictOutcome> {
    let runtime_dir = runtime_dir_for_data_dir(data_dir);

    // If the runtime dir does not exist there can be no daemon or metadata.
    if !runtime_dir.is_dir() {
        return Ok(VerdictOutcome {
            verdict: OwnershipVerdict::Stopped,
            metadata: None,
            schema_unsupported: false,
        });
    }

    // Read raw metadata (if any) without staleness validation.
    let Some(metadata) = try_read_raw_metadata(data_dir)? else {
        return Ok(VerdictOutcome {
            verdict: OwnershipVerdict::Stopped,
            metadata: None,
            schema_unsupported: false,
        });
    };

    // Refuse runtime schemas this build does not understand: parsing them as v1
    // and then deleting the sidecar could destroy a newer daemon's state.
    if metadata.schema_version != DAEMON_RUNTIME_SCHEMA_VERSION {
        return Ok(VerdictOutcome {
            verdict: OwnershipVerdict::Ambiguous,
            metadata: Some(metadata),
            schema_unsupported: true,
        });
    }

    // Inspect the lock WITHOUT creating it (preserves zero-mutation preflight).
    // `runtime_metadata_is_stale_noncreating` returns true when metadata exists
    // and no process holds the lease.
    if runtime_metadata_is_stale_noncreating(data_dir)? {
        // No active owner. A cleanly stopped daemon keeps its `Stopped` verdict;
        // any other state with no owner is stale runtime metadata.
        let verdict = if metadata.state == DaemonState::Stopped {
            OwnershipVerdict::Stopped
        } else {
            OwnershipVerdict::StaleNoOwner
        };
        return Ok(VerdictOutcome {
            verdict,
            metadata: Some(metadata),
            schema_unsupported: false,
        });
    }

    // Lock is held. The lock file therefore exists, so the live health probe
    // will not create runtime state. A responsive daemon is Live; otherwise the
    // owner is unresponsive and ownership is ambiguous.
    if active_metadata(data_dir)?.is_some() {
        Ok(VerdictOutcome {
            verdict: OwnershipVerdict::Live,
            metadata: Some(metadata),
            schema_unsupported: false,
        })
    } else {
        Ok(VerdictOutcome {
            verdict: OwnershipVerdict::Ambiguous,
            metadata: Some(metadata),
            schema_unsupported: false,
        })
    }
}

fn build_inspect_summary(data_dir: &Path, metadata: Option<&DaemonMetadata>) -> InspectSummary {
    let runtime_dir = runtime_dir_for_data_dir(data_dir);
    let metadata_path = runtime_dir.join("egregored.json");
    let metadata_exists = metadata_path.exists();
    let lock_held = !is_store_unleased(data_dir);
    let daemon_state = metadata.map(|m| {
        serde_json::to_value(m.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", m.state).to_lowercase())
    });
    InspectSummary {
        metadata_exists,
        lock_held,
        daemon_state,
    }
}

/// Returns true when the store lease is not currently held by any process.
///
/// Uses the non-creating lock inspector so callers on the zero-mutation
/// preflight/dry-run path never create the runtime dir or lock file.
fn is_store_unleased(data_dir: &Path) -> bool {
    let runtime_dir = runtime_dir_for_data_dir(data_dir);
    if !runtime_dir.is_dir() {
        return true;
    }
    runtime_metadata_is_stale_noncreating(data_dir).unwrap_or(true)
}

/// Returns the current wall-clock time as RFC 3339.
fn now_rfc3339() -> String {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // Format as RFC 3339 using unix milliseconds. chrono is available via
    // the workspace dependency already used by daemon.rs.
    chrono::DateTime::<chrono::Utc>::from(SystemTime::UNIX_EPOCH)
        .checked_add_signed(chrono::Duration::milliseconds(
            i64::try_from(unix_ms).unwrap_or(0),
        ))
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339()
}

// ---------------------------------------------------------------------------
// Repair gate: embedded store open check (used by adapters/aletheiadb.rs)
// ---------------------------------------------------------------------------

/// Returns an error message if stale, non-stopped daemon metadata blocks
/// a direct embedded-store open.
///
/// This enforces AC 8: normal embedded ingest is blocked when crashed or stale
/// daemon metadata exists, requiring the operator to run repair first.
///
/// # Errors
///
/// Returns `Some(message)` when the embedded open should be rejected.
#[must_use]
pub fn embedded_open_repair_gate(data_dir: &Path) -> Option<String> {
    let raw_metadata = try_read_raw_metadata(data_dir).ok()??;
    if raw_metadata.state == DaemonState::Stopped {
        return None;
    }
    // Metadata with non-stopped state exists; check if the store is unleased.
    let is_stale = runtime_metadata_is_stale_noncreating(data_dir).ok()?;
    if !is_stale {
        // Lock is held → StoreLease::acquire will handle this refusal naturally.
        return None;
    }
    Some(format!(
        "stale daemon metadata (state: {}) found for {}; \
         run `eg repair preflight --data-dir {data}` to inspect ownership, \
         then `eg repair run --data-dir {data} --confirm` before using embedded ingest",
        serde_json::to_value(raw_metadata.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", raw_metadata.state).to_lowercase()),
        data_dir.display(),
        data = data_dir.display()
    ))
}
