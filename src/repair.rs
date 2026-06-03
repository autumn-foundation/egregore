//! Offline repair workflow for Egregore stores.
//!
//! Documented in `docs/cli/repair.md`.
//!
//! The shortest local workflow is:
//! 1. `eg repair preflight --data-dir .egregore` — inspect ownership verdict.
//! 2. If the daemon is running: `eg daemon stop --data-dir .egregore`.
//! 3. `eg repair run --data-dir .egregore --confirm` — perform repair.
//! 4. `eg repair preflight --data-dir .egregore` — confirm the store is clean.
//! 5. `eg daemon start --data-dir .egregore` — resume normal operation.
//!
//! All output is machine-readable JSON.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::daemon::{
    DaemonMetadata, DaemonState, active_metadata, runtime_dir_for_data_dir,
    runtime_metadata_is_stale, try_read_raw_metadata,
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
    let (verdict, metadata) = determine_verdict(data_dir)?;

    let (allow, refusal_reasons, safe_daemon_command) = match &verdict {
        OwnershipVerdict::Live => (
            false,
            vec![RepairRefusalCode::LiveDaemonActive],
            Some(format!("eg daemon stop --data-dir {}", data_dir.display())),
        ),
        OwnershipVerdict::Ambiguous => (false, vec![RepairRefusalCode::AmbiguousOwnership], None),
        OwnershipVerdict::Stopped | OwnershipVerdict::StaleNoOwner => (true, vec![], None),
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

    let (verdict, raw_metadata) = determine_verdict(data_dir)?;

    let before_daemon_status = raw_metadata
        .as_ref()
        .map(DaemonStatusSnapshot::from_metadata);
    let before_inspect_summary = Some(build_inspect_summary(data_dir, raw_metadata.as_ref()));

    // Determine refusal conditions
    let refusal_reasons: Vec<RepairRefusalCode> = match &verdict {
        OwnershipVerdict::Live => vec![RepairRefusalCode::LiveDaemonActive],
        OwnershipVerdict::Ambiguous => vec![RepairRefusalCode::AmbiguousOwnership],
        OwnershipVerdict::Stopped | OwnershipVerdict::StaleNoOwner => {
            if !dry_run && !confirm {
                vec![RepairRefusalCode::ConfirmationRequired]
            } else {
                vec![]
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

    // Proceed: execute (or simulate) repair actions
    let RepairActions {
        attempted_actions,
        changed_file_paths,
    } = execute_repair_actions(&runtime_dir, dry_run)?;

    let ended_at = now_rfc3339();
    let after_inspect_summary = (!dry_run).then(|| InspectSummary {
        metadata_exists: runtime_dir.join("egregored.json").exists(),
        lock_held: !is_store_unleased(data_dir),
        daemon_state: None,
    });
    let after_daemon_status = (!dry_run).then(DaemonStatusSnapshot::none_present);
    let result = if dry_run {
        RepairSessionResult::DryRun
    } else {
        RepairSessionResult::Success
    };

    let report = RepairSessionReport {
        schema_version: REPAIR_SCHEMA_VERSION,
        data_dir: data_dir.to_path_buf(),
        runtime_dir: runtime_dir.clone(),
        ownership_verdict: verdict,
        dry_run,
        started_at,
        ended_at,
        result,
        before_daemon_status,
        after_daemon_status,
        before_inspect_summary,
        after_inspect_summary,
        attempted_actions,
        skipped_actions: vec![],
        changed_file_paths,
        refusal_reasons: vec![],
    };

    if !dry_run {
        // Write recovery report to runtime dir so operator can reference it.
        let _ = fs::create_dir_all(&runtime_dir);
        let report_path = runtime_dir.join(RECOVERY_REPORT_FILE);
        if let Ok(json) = serde_json::to_vec_pretty(&report) {
            let _ = fs::write(&report_path, json);
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

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

/// Determines the stable ownership verdict for a data directory.
///
/// This function never creates directories or modifies runtime files.
fn determine_verdict(data_dir: &Path) -> Result<(OwnershipVerdict, Option<DaemonMetadata>)> {
    let runtime_dir = runtime_dir_for_data_dir(data_dir);

    // If the runtime dir does not exist there can be no daemon or metadata.
    if !runtime_dir.is_dir() {
        return Ok((OwnershipVerdict::Stopped, None));
    }

    // Check for a live, responsive daemon.
    if let Some(metadata) = active_metadata(data_dir)? {
        return Ok((OwnershipVerdict::Live, Some(metadata)));
    }

    // Read raw metadata (if any) without staleness validation.
    let raw_metadata = try_read_raw_metadata(data_dir)?;

    match raw_metadata {
        None => Ok((OwnershipVerdict::Stopped, None)),
        Some(metadata) => {
            // Metadata exists but daemon is not alive; check the lock.
            // runtime_metadata_is_stale returns true when metadata exists AND the
            // lock is NOT held (i.e., no process owns the lease).
            if runtime_metadata_is_stale(data_dir)? {
                // Lock is not held: stale state with no active owner.
                Ok((OwnershipVerdict::StaleNoOwner, Some(metadata)))
            } else {
                // Lock is still held but the daemon is unresponsive: ambiguous.
                Ok((OwnershipVerdict::Ambiguous, Some(metadata)))
            }
        }
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
fn is_store_unleased(data_dir: &Path) -> bool {
    // We don't want to create the runtime dir here, so check it first.
    let runtime_dir = runtime_dir_for_data_dir(data_dir);
    if !runtime_dir.is_dir() {
        return true;
    }
    runtime_metadata_is_stale(data_dir).unwrap_or(true)
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
    let is_stale = runtime_metadata_is_stale(data_dir).ok()?;
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
