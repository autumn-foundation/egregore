//! Local daemon for shared Egregore store access.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fmt::Write as _,
    fs::{self, File, OpenOptions, TryLockError as FileTryLockError},
    io::{self, Read, Seek, SeekFrom, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, RwLock, RwLockReadGuard, TryLockError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use chrono::DateTime;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    adapters::{
        AdapterError, EmbeddedAletheiaSink, ExpectedRecordState, IngestReport, ingest_records,
    },
    identity::{is_local_remote_url, repository_id_matches_payload},
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, EdgeLabel, EvidenceLink, GraphRecord,
        IdentitySource, NodeKind, OutputHandle, PROJECT_SCHEMA_VERSION,
        SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE, SEMANTIC_SCHEMA_VERSION, TemporalMetadata,
        USER_CONTEXT_SCHEMA_VERSION, UserContextFields, UserContextScope,
        VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id, user_context_stable_id,
    },
    query as graph_query,
    schema_version::{
        RecordVersion, UNKNOWN_SCHEMA_VERSION_CODE, UnknownSchemaVersion, validate_record_version,
    },
};

const RUNTIME_DIR_SUFFIX: &str = ".egregore-runtime";
const LOCK_FILE: &str = "egregored.lock";
const METADATA_FILE: &str = "egregored.json";
const IDEMPOTENCY_FILE: &str = "idempotency.json";
/// Schema version for the daemon runtime directory metadata contract.
///
/// Documented in `docs/schema/daemon-runtime.md`.
pub const DAEMON_RUNTIME_SCHEMA_VERSION: u32 = 1;
/// Environment variable that names a daemon data directory for discovery.
pub const EGREGORE_DATA_DIR_ENV: &str = "EGREGORE_DATA_DIR";
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 37_383;
const START_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const CLIENT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_LIMIT: usize = 1024 * 1024 * 32;
const INLINE_PAYLOAD_CEILING: u64 = 16 * 1024;

/// Configuration for launching the daemon.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// `AletheiaDB` data directory.
    pub data_dir: PathBuf,
    /// Loopback host to bind.
    pub host: String,
    /// TCP port. `0` asks the OS to choose.
    pub port: u16,
    /// Bounded write queue capacity.
    pub write_queue_capacity: usize,
}

impl DaemonConfig {
    /// Creates a daemon config for a data directory.
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
            write_queue_capacity: 64,
        }
    }
}

/// Runtime state written to `egregored.json`.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonState {
    /// The daemon held the lease when it last wrote metadata.
    #[default]
    Running,
    /// The daemon stopped gracefully and released the lease.
    Stopped,
    /// A startup or client-side stale-file check found lingering metadata with no held lease.
    Crashed,
}

/// Reserved transport kinds for future daemon transports.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonTransportKind {
    /// Loopback HTTP transport used by v1.
    Http,
    /// Reserved Unix-domain socket transport.
    Unix,
    /// Reserved Windows named-pipe transport.
    Pipe,
}

/// Reserved transport descriptor for future multi-transport metadata.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DaemonTransport {
    /// Transport kind.
    pub kind: DaemonTransportKind,
    /// Address, socket path, or pipe name.
    pub address: String,
}

/// Metadata written by a daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonMetadata {
    /// Runtime metadata schema version.
    #[serde(default = "daemon_runtime_schema_version")]
    pub schema_version: u32,
    /// Daemon process ID.
    pub pid: u32,
    /// Bound TCP address.
    pub address: String,
    /// Local bearer token.
    pub token: String,
    /// Store data directory.
    pub data_dir: PathBuf,
    /// Daemon version.
    pub version: String,
    /// Unix milliseconds when the daemon started.
    pub started_at_unix_ms: u128,
    /// Last-known daemon lifecycle state.
    #[serde(default)]
    pub state: DaemonState,
    /// Reserved wire API version field. Null in v1 runtime metadata.
    #[serde(default)]
    pub api_version: Option<String>,
    /// Reserved future transport descriptors. Null in v1 runtime metadata.
    #[serde(default)]
    pub transports: Option<Vec<DaemonTransport>>,
    /// Reserved future token-rotation expiry. Null in v1 runtime metadata.
    #[serde(default)]
    pub token_expires_at_unix_ms: Option<u128>,
    /// Reserved future multi-daemon index pointer. Null in v1 runtime metadata.
    #[serde(default)]
    pub daemons_index_url: Option<String>,
}

const fn daemon_runtime_schema_version() -> u32 {
    DAEMON_RUNTIME_SCHEMA_VERSION
}

/// Response returned by daemon-backed ingestion.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DaemonIngestResponse {
    /// Number of records attempted.
    pub attempted: usize,
    /// Number of records written and read back.
    pub succeeded: usize,
    /// Number of failed records.
    pub failed: usize,
    /// Per-record failures.
    pub failures: Vec<DaemonIngestFailure>,
    /// Record IDs included in the request.
    pub record_ids: Vec<String>,
    /// True when returned from the idempotency cache.
    pub idempotent: bool,
}

/// One daemon ingest failure.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DaemonIngestFailure {
    /// Stable record ID.
    pub record_id: String,
    /// Failure message.
    pub message: String,
}

impl DaemonIngestResponse {
    fn from_report(report: IngestReport, record_ids: Vec<String>, idempotent: bool) -> Self {
        Self {
            attempted: report.attempted,
            succeeded: report.succeeded,
            failed: report.failed,
            failures: report
                .failures
                .into_iter()
                .map(|failure| DaemonIngestFailure {
                    record_id: failure.record_id,
                    message: failure.message,
                })
                .collect(),
            record_ids,
            idempotent,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum IdempotencyEntry {
    Pending {
        payload_hash: String,
        record_ids: Vec<String>,
        records: Vec<GraphRecord>,
    },
    Committed {
        payload_hash: String,
        response: DaemonIngestResponse,
    },
}

impl IdempotencyEntry {
    fn payload_hash(&self) -> &str {
        match self {
            Self::Pending { payload_hash, .. } | Self::Committed { payload_hash, .. } => {
                payload_hash
            }
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IdempotencyFile {
    entries: BTreeMap<String, IdempotencyEntry>,
}

struct IdempotencyStore {
    path: PathBuf,
    entries: BTreeMap<String, IdempotencyEntry>,
}

impl IdempotencyStore {
    fn load(path: PathBuf) -> Result<Self> {
        reject_runtime_symlink_components(&path, "runtime file")?;
        let mut created = false;
        let entries = match fs::read_to_string(&path) {
            Ok(contents) => {
                serde_json::from_str::<IdempotencyFile>(&contents)
                    .with_context(|| format!("failed to parse {}", path.display()))?
                    .entries
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                created = true;
                BTreeMap::new()
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let store = Self { path, entries };
        if created {
            store.persist_entries(&store.entries)?;
        }
        Ok(store)
    }

    fn set_entry_durably(&mut self, key: String, entry: IdempotencyEntry) -> Result<()> {
        let mut entries = self.entries.clone();
        entries.insert(key, entry);
        self.persist_entries(&entries)?;
        self.entries = entries;
        Ok(())
    }

    fn persist_entries(&self, entries: &BTreeMap<String, IdempotencyEntry>) -> Result<()> {
        let file = IdempotencyFile {
            entries: entries.clone(),
        };
        let json = serde_json::to_vec_pretty(&file)?;
        atomic_write(&self.path, &json)
    }
}

/// Exclusive embedded-store lease for one data directory.
///
/// Holding this value means the current process is the only process that should
/// open the embedded `AletheiaDB` store for mutation.
pub struct StoreLease {
    file: File,
    path: PathBuf,
}

impl StoreLease {
    /// Acquires the embedded-store lease for a data directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime directory or lock file cannot be opened,
    /// or another process already holds the lease.
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        Self::try_acquire(data_dir)?.ok_or_else(|| {
            anyhow!(
                "embedded store is already leased for {}",
                data_dir.display()
            )
        })
    }

    fn try_acquire(data_dir: &Path) -> Result<Option<Self>> {
        let path = lock_path(data_dir)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        reject_runtime_symlink(&path, "runtime file")?;
        let file = options
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        enforce_runtime_file_permissions(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file, path })),
            Err(error) if lock_error_is_contention(&error) => Ok(None),
            Err(error) => Err(io::Error::from(error))
                .with_context(|| format!("failed to lock {}", path.display())),
        }
    }

    fn write_metadata(&mut self, metadata: &DaemonMetadata) -> Result<()> {
        let contents = serde_json::to_vec_pretty(metadata)?;
        self.file
            .set_len(0)
            .with_context(|| format!("failed to truncate {}", self.path.display()))?;
        self.file
            .seek(SeekFrom::Start(0))
            .with_context(|| format!("failed to seek {}", self.path.display()))?;
        self.file
            .write_all(&contents)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.file
            .sync_all()
            .with_context(|| format!("failed to sync {}", self.path.display()))
    }
}

impl Drop for StoreLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn lock_path(data_dir: &Path) -> Result<PathBuf> {
    let runtime_dir = ensure_runtime_dir(data_dir)?;
    Ok(runtime_dir.join(LOCK_FILE))
}

fn remove_metadata_if_store_unleased(data_dir: &Path) -> Result<bool> {
    let Some(_lease) = StoreLease::try_acquire(data_dir)? else {
        return Ok(false);
    };
    let path = metadata_path(data_dir);
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove stale {}", path.display()))?;
    }
    Ok(true)
}

fn mark_metadata_crashed_if_store_unleased(data_dir: &Path) -> Result<bool> {
    let Some(_lease) = StoreLease::try_acquire(data_dir)? else {
        return Ok(false);
    };
    let Ok(metadata) = read_metadata(data_dir) else {
        return Ok(true);
    };
    if metadata.state == DaemonState::Stopped {
        return Ok(true);
    }
    let crashed = DaemonMetadata {
        state: DaemonState::Crashed,
        ..metadata
    };
    write_metadata(data_dir, &crashed)?;
    Ok(true)
}

fn store_is_unleased(data_dir: &Path) -> Result<bool> {
    Ok(StoreLease::try_acquire(data_dir)?.is_some())
}

fn already_running_error(data_dir: &Path, metadata: &DaemonMetadata) -> anyhow::Error {
    anyhow!(
        "daemon already running for {} at {} (pid {}, runtime dir {})",
        data_dir.display(),
        metadata.address,
        metadata.pid,
        runtime_dir(data_dir).display()
    )
}

#[derive(Clone)]
struct ServerState {
    token: String,
    store_identity: String,
    sink: Arc<RwLock<EmbeddedAletheiaSink>>,
    write_tx: mpsc::SyncSender<WriteCommand>,
    jobs: Arc<Mutex<BTreeMap<String, JobStatus>>>,
    agents: Arc<Mutex<BTreeMap<AgentSessionKey, AgentStatus>>>,
    idempotency: Arc<Mutex<IdempotencyStore>>,
    shutdown: Arc<AtomicBool>,
    pressure: Arc<PressureTracker>,
}

struct WriteCommand {
    idempotency_key: String,
    payload_hash: String,
    records: Vec<GraphRecord>,
    response_tx: mpsc::Sender<WriteResult>,
}

/// Retry-after hint (milliseconds) returned with every `queue_full` rejection.
///
/// Frozen as part of the daemon pressure contract documented in
/// `docs/schema/daemon-api.md`. Agents must wait at least this long before
/// retrying an overloaded write with the same idempotency key.
const QUEUE_FULL_RETRY_AFTER_MS: u64 = 500;

/// Maximum number of recent pressure-transition events retained for operator
/// inspection. The buffer is bounded so diagnostics never grow without limit.
const PRESSURE_EVENT_CAPACITY: usize = 32;
/// Maximum byte length of a `request_id` retained in a pressure diagnostic
/// event or structured log. `request_id` is caller-controlled and bounded only
/// by the request body limit, so the diagnostic copy is truncated to keep
/// `recent_events` and logs bounded. The full id is still echoed to the caller
/// in the `queue_full` error envelope.
const PRESSURE_REQUEST_HANDLE_MAX_BYTES: usize = 64;

/// Truncates a caller-supplied request handle to a bounded length on a UTF-8
/// char boundary, appending an ellipsis when truncation occurred.
fn truncate_request_handle(request_id: &str) -> String {
    if request_id.len() <= PRESSURE_REQUEST_HANDLE_MAX_BYTES {
        return request_id.to_owned();
    }
    let mut end = PRESSURE_REQUEST_HANDLE_MAX_BYTES;
    while end > 0 && !request_id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &request_id[..end])
}

/// Machine-readable write-admission pressure classification surfaced by
/// `GET /v1/status`.
///
/// `idle` and `busy` both mean the daemon is accepting writes; `saturated`
/// means the bounded write queue rejected at least one write and is shedding
/// load via `queue_full`. The daemon is alive in every state — `saturated` is
/// backpressure, not failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressureState {
    /// No writes are queued or in flight.
    Idle,
    /// Writes are queued or in flight but the queue is admitting them.
    Busy,
    /// The bounded write queue is shedding load with `queue_full`.
    Saturated,
}

impl PressureState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Saturated => "saturated",
        }
    }
}

/// One structured diagnostic event recorded when the daemon enters or leaves
/// saturation. Carries only bounded metadata — never submitted payload bodies,
/// graph records, command output, or secrets.
#[derive(Debug, Clone, Serialize)]
struct PressureEvent {
    /// Unix milliseconds when the transition occurred.
    at_unix_ms: u128,
    /// `entered_saturation` or `exited_saturation`.
    transition: &'static str,
    /// Route or operation class that triggered the transition.
    operation: &'static str,
    /// Stable error code associated with the transition.
    code: &'static str,
    /// Caller request handle (`request_id`), or a worker sentinel on recovery.
    request_id: String,
}

/// Tracks bounded write-admission pressure for the daemon.
///
/// The bounded `mpsc::sync_channel` write queue does not expose its depth, so
/// the tracker maintains an in-flight counter incremented on every accepted
/// write and decremented when the worker finishes one. Saturation is sticky:
/// it is raised on the first `queue_full` rejection and cleared once the
/// backlog drains to the recovery watermark, so a momentarily full queue is
/// still reported as `saturated` until it actually recovers.
struct PressureTracker {
    capacity: usize,
    inflight: AtomicUsize,
    saturated: AtomicBool,
    total_rejections: AtomicU64,
    saturation_transitions: AtomicU64,
    last_saturated_at_unix_ms: AtomicU64,
    last_recovered_at_unix_ms: AtomicU64,
    events: Mutex<VecDeque<PressureEvent>>,
}

impl PressureTracker {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inflight: AtomicUsize::new(0),
            saturated: AtomicBool::new(false),
            total_rejections: AtomicU64::new(0),
            saturation_transitions: AtomicU64::new(0),
            last_saturated_at_unix_ms: AtomicU64::new(0),
            last_recovered_at_unix_ms: AtomicU64::new(0),
            events: Mutex::new(VecDeque::new()),
        }
    }

    /// Records that a write was admitted to the bounded queue.
    fn on_enqueue(&self) {
        self.inflight.fetch_add(1, Ordering::SeqCst);
    }

    /// Undoes an `on_enqueue` for a write that was never admitted to the queue.
    fn rollback_enqueue(&self) {
        self.inflight.fetch_sub(1, Ordering::SeqCst);
    }

    /// Rolls back the speculative enqueue count and records the rejection.
    fn on_reject_after_rollback(&self, operation: &'static str, request_id: &str) {
        self.rollback_enqueue();
        self.on_reject(operation, request_id);
    }

    /// Records that the worker finished a write, draining one queue slot.
    fn on_complete(&self) {
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        self.maybe_recover();
    }

    /// Records a `queue_full` rejection and raises saturation on the first one.
    fn on_reject(&self, operation: &'static str, request_id: &str) {
        self.total_rejections.fetch_add(1, Ordering::SeqCst);
        self.last_saturated_at_unix_ms
            .store(unix_ms_u64(), Ordering::SeqCst);
        if !self.saturated.swap(true, Ordering::SeqCst) {
            self.saturation_transitions.fetch_add(1, Ordering::SeqCst);
            self.record_transition("entered_saturation", operation, "queue_full", request_id);
        }
        // Guard against the drain-before-rejection race: if the queue already
        // emptied to the recovery watermark while this rejection was being
        // recorded, recover immediately so status does not report `saturated`
        // with nothing in flight until some future write happens to complete.
        self.maybe_recover();
    }

    /// Clears saturation and emits one recovery event once the backlog has
    /// drained to the recovery watermark. Safe to call from any path: the
    /// atomic swap guarantees exactly one recovery per saturation episode.
    fn maybe_recover(&self) {
        if self.inflight.load(Ordering::SeqCst) <= self.recovery_watermark()
            && self.saturated.swap(false, Ordering::SeqCst)
        {
            self.last_recovered_at_unix_ms
                .store(unix_ms_u64(), Ordering::SeqCst);
            self.record_transition("exited_saturation", "write_worker", "queue_full", "drain");
        }
    }

    const fn recovery_watermark(&self) -> usize {
        self.capacity / 2
    }

    fn state(&self) -> PressureState {
        if self.saturated.load(Ordering::SeqCst) {
            PressureState::Saturated
        } else if self.inflight.load(Ordering::SeqCst) > 0 {
            PressureState::Busy
        } else {
            PressureState::Idle
        }
    }

    fn record_transition(
        &self,
        transition: &'static str,
        operation: &'static str,
        code: &'static str,
        request_id: &str,
    ) {
        let event = PressureEvent {
            at_unix_ms: unix_ms(),
            transition,
            operation,
            code,
            request_id: truncate_request_handle(request_id),
        };
        // Structured operator log. Carries only bounded metadata so that no
        // submitted payload body, graph record, or secret can leak via logs.
        eprintln!(
            "{}",
            json!({
                "egregore_event": "daemon_pressure",
                "transition": event.transition,
                "operation": event.operation,
                "code": event.code,
                "request_id": event.request_id,
                "at_unix_ms": event.at_unix_ms,
            })
        );
        if let Ok(mut events) = self.events.lock() {
            if events.len() >= PRESSURE_EVENT_CAPACITY {
                events.pop_front();
            }
            events.push_back(event);
        }
    }

    /// Builds the machine-readable pressure block embedded in `GET /v1/status`.
    fn snapshot_json(&self) -> serde_json::Value {
        let state = self.state();
        let recent_events: Vec<serde_json::Value> = self
            .events
            .lock()
            .map(|events| events.iter().map(pressure_event_json).collect())
            .unwrap_or_default();
        let mut block = json!({
            "state": state.as_str(),
            "alive": true,
            "queue_capacity": self.capacity,
            "queue_depth": self.inflight.load(Ordering::SeqCst),
            "total_rejections": self.total_rejections.load(Ordering::SeqCst),
            "saturation_transitions": self.saturation_transitions.load(Ordering::SeqCst),
            "last_saturated_at_unix_ms": optional_unix_ms(&self.last_saturated_at_unix_ms),
            "last_recovered_at_unix_ms": optional_unix_ms(&self.last_recovered_at_unix_ms),
            "recent_events": recent_events,
        });
        if state == PressureState::Saturated {
            block["retry_after_ms"] = json!(QUEUE_FULL_RETRY_AFTER_MS);
        }
        block
    }

    #[cfg(test)]
    fn events_snapshot(&self) -> Vec<PressureEvent> {
        self.events
            .lock()
            .map(|events| events.iter().cloned().collect())
            .unwrap_or_default()
    }
}

fn pressure_event_json(event: &PressureEvent) -> serde_json::Value {
    json!({
        "at_unix_ms": event.at_unix_ms,
        "transition": event.transition,
        "operation": event.operation,
        "code": event.code,
        "request_id": event.request_id,
    })
}

fn optional_unix_ms(value: &AtomicU64) -> serde_json::Value {
    match value.load(Ordering::SeqCst) {
        0 => serde_json::Value::Null,
        ms => json!(ms),
    }
}

fn unix_ms_u64() -> u64 {
    u64::try_from(unix_ms()).unwrap_or(u64::MAX)
}

type WriteResult<T = DaemonIngestResponse> = std::result::Result<T, ApiError>;

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct AgentSessionKey {
    agent_id: String,
    session_id: String,
}

impl AgentSessionKey {
    fn new(agent_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            session_id: session_id.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentStatus {
    agent_id: String,
    session_id: String,
    agent_kind: String,
    project_scope: String,
    last_seen_unix_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobStatus {
    job_id: String,
    status: String,
    report: Option<DaemonIngestResponse>,
    events: Vec<String>,
    #[serde(skip)]
    payload_hash: String,
}

/// Schema version for the daemon-query payload contract.
/// Documented in `docs/schema/daemon-query.md`.
pub const DAEMON_QUERY_SCHEMA_VERSION: u32 = 1;

/// Stable, versioned error-code taxonomy for the v1 daemon wire contract.
///
/// Adding a new code is additive. Renaming, removing, or changing semantics
/// requires a `/v2/` API prefix change per `docs/schema/daemon-api.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum ErrorCode {
    Unauthorized,
    BadRequest,
    MissingField,
    InvalidDomain,
    IdempotencyConflict,
    NotFound,
    PayloadTooLarge,
    QueueFull,
    QueryTimeout,
    InternalError,
    NotImplemented,
    ShutdownInProgress,
    RedactionRequired,
    UnresolvedEvidenceTarget,
    LocalPathIdentityUnsupported,
    /// Reserved on #5's error-code enum; returned when a commit prefix matches
    /// more than one distinct commit SHA in the store.
    AmbiguousCommitPrefix,
    /// Added by #11 (verification schema): a verification-domain record is
    /// missing a required evidence handle (`source_artifact_hash`,
    /// `source_artifact_path`, or `stdout_handle.hash`).
    MissingEvidenceHandle,
    /// Added by #13 (agent-actions schema): a `PatchArtifact.patch_status`
    /// mutation attempted to rewrite a pinned validity result.
    PatchStatusPinned,
    /// Added by #13 and reused for #11 handles: inline payload exceeded the
    /// 16 KiB ceiling and must be demoted to handle-only storage.
    InlinePayloadExceedsCeiling,
    /// Reserved by #18: daemon startup refused to create or keep runtime files
    /// with unsafe permissions.
    RuntimePermissionsUnsafe,
    /// Reserved by #18: future token rotation asks clients to re-read
    /// `egregored.json` and retry with the fresh token.
    TokenRotated,
    /// Added by #14 (project graph schema): a verified `AcceptanceCriterion` is
    /// missing the verification record that closed it.
    AcceptanceCriterionMissingVerification,
    /// Added by #15 (semantic drift schema): `prior_record_id` and the
    /// `DRIFTS_PRIOR` edge target disagree or do not target code graph.
    DriftPriorTargetMismatch,
    /// Added by #15 (semantic drift schema): an existing drift ID was
    /// resubmitted with a different score.
    DriftRecordImmutable,
    /// Reserved by the record schema-version policy: a record's
    /// `(domain, kind, schema_version)` tuple is unknown to this reader.
    UnknownSchemaVersion,
    /// Added by #19 (user-context schema): a promotion candidate does not meet
    /// the configured evidence threshold.
    InsufficientPromotionEvidence,
    /// Added by #19 (user-context schema): a durable user-context record lacks
    /// a referenced approval decision.
    UnapprovedDurableUserContext,
}

impl ErrorCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::BadRequest => "bad_request",
            Self::MissingField => "missing_field",
            Self::InvalidDomain => "invalid_domain",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::NotFound => "not_found",
            Self::PayloadTooLarge => "payload_too_large",
            Self::QueueFull => "queue_full",
            Self::QueryTimeout => "query_timeout",
            Self::InternalError => "internal_error",
            Self::NotImplemented => "not_implemented",
            Self::ShutdownInProgress => "shutdown_in_progress",
            Self::RedactionRequired => "redaction_required",
            Self::UnresolvedEvidenceTarget => "unresolved_evidence_target",
            Self::LocalPathIdentityUnsupported => "local_path_identity_unsupported",
            Self::AmbiguousCommitPrefix => "ambiguous_commit_prefix",
            Self::MissingEvidenceHandle => "missing_evidence_handle",
            Self::PatchStatusPinned => "patch_status_pinned",
            Self::InlinePayloadExceedsCeiling => "inline_payload_exceeds_ceiling",
            Self::RuntimePermissionsUnsafe => "runtime_permissions_unsafe",
            Self::TokenRotated => "token_rotated",
            Self::AcceptanceCriterionMissingVerification => {
                "acceptance_criterion_missing_verification"
            }
            Self::DriftPriorTargetMismatch => "drift_prior_target_mismatch",
            Self::DriftRecordImmutable => "drift_record_immutable",
            Self::UnknownSchemaVersion => UNKNOWN_SCHEMA_VERSION_CODE,
            Self::InsufficientPromotionEvidence => "insufficient_promotion_evidence",
            Self::UnapprovedDurableUserContext => "unapproved_durable_user_context",
        }
    }

    const fn http_status(self) -> u16 {
        match self {
            Self::Unauthorized | Self::TokenRotated => 401,
            Self::BadRequest
            | Self::MissingField
            | Self::InvalidDomain
            | Self::InlinePayloadExceedsCeiling
            | Self::AmbiguousCommitPrefix => 400,
            Self::IdempotencyConflict => 409,
            Self::NotFound => 404,
            Self::PayloadTooLarge => 413,
            Self::QueueFull => 429,
            Self::QueryTimeout => 408,
            Self::InternalError | Self::RuntimePermissionsUnsafe => 500,
            Self::NotImplemented => 501,
            Self::ShutdownInProgress => 503,
            Self::RedactionRequired
            | Self::UnresolvedEvidenceTarget
            | Self::LocalPathIdentityUnsupported
            | Self::MissingEvidenceHandle
            | Self::PatchStatusPinned
            | Self::AcceptanceCriterionMissingVerification
            | Self::DriftPriorTargetMismatch
            | Self::DriftRecordImmutable
            | Self::UnknownSchemaVersion
            | Self::InsufficientPromotionEvidence
            | Self::UnapprovedDurableUserContext => 422,
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: u16,
    code: ErrorCode,
    message: String,
    field: Option<String>,
    retry_after_ms: Option<u64>,
    partial_result: Option<bool>,
}

impl ApiError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status: code.http_status(),
            code,
            message: message.into(),
            field: None,
            retry_after_ms: None,
            partial_result: None,
        }
    }

    fn missing_field(field_path: impl Into<String>) -> Self {
        let field = field_path.into();
        Self {
            status: 400,
            code: ErrorCode::MissingField,
            message: format!("required field is missing: {field}"),
            field: Some(field),
            retry_after_ms: None,
            partial_result: None,
        }
    }

    fn unauthorized() -> Self {
        Self::new(ErrorCode::Unauthorized, "missing or invalid bearer token")
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }

    fn bad_request_field(message: impl Into<String>, field: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: ErrorCode::BadRequest,
            message: message.into(),
            field: Some(field.into()),
            retry_after_ms: None,
            partial_result: None,
        }
    }

    fn invalid_domain() -> Self {
        Self::new(
            ErrorCode::InvalidDomain,
            r#"domain must be "codegraph", "agent_memory", "verification", "artifact", "project", "semantic", or "user_context""#,
        )
    }

    fn unknown_schema_version(unknown: &UnknownSchemaVersion) -> Self {
        Self::unknown_record_schema_version(&unknown.version)
    }

    fn unknown_record_schema_version(version: &RecordVersion) -> Self {
        Self {
            status: ErrorCode::UnknownSchemaVersion.http_status(),
            code: ErrorCode::UnknownSchemaVersion,
            message: format!("unsupported record schema version: {version}"),
            field: Some("schema_version".to_owned()),
            retry_after_ms: None,
            partial_result: None,
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::IdempotencyConflict, message)
    }

    fn payload_too_large() -> Self {
        Self::new(
            ErrorCode::PayloadTooLarge,
            "request body exceeds maximum size",
        )
    }

    fn overloaded() -> Self {
        Self {
            status: 429,
            code: ErrorCode::QueueFull,
            message: "write queue is full".into(),
            field: None,
            retry_after_ms: Some(QUEUE_FULL_RETRY_AFTER_MS),
            partial_result: None,
        }
    }

    fn shutdown_in_progress() -> Self {
        Self {
            status: 503,
            code: ErrorCode::ShutdownInProgress,
            message: "daemon is shutting down".into(),
            field: None,
            retry_after_ms: Some(2_000),
            partial_result: None,
        }
    }

    fn query_timeout() -> Self {
        Self {
            status: 408,
            code: ErrorCode::QueryTimeout,
            message: "query budget expired".into(),
            field: None,
            retry_after_ms: None,
            partial_result: Some(false),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InternalError, message)
    }

    fn inline_payload_exceeds_ceiling(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InlinePayloadExceedsCeiling, message)
    }

    fn patch_status_pinned(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PatchStatusPinned, message)
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: serde_json::Value,
}

impl HttpResponse {
    /// Raw response: body is emitted as-is. Use for observability endpoints
    /// (health, status) that return flat JSON rather than the standard envelope.
    const fn json(status: u16, body: serde_json::Value) -> Self {
        Self { status, body }
    }

    /// Standard success envelope: `{ ok: true, request_id, result }`.
    /// Pass `None` for endpoints that do not echo a parsed request ID.
    #[allow(clippy::needless_pass_by_value)]
    fn success(request_id: Option<&str>, status: u16, result: serde_json::Value) -> Self {
        Self {
            status,
            body: json!({
                "ok": true,
                "request_id": request_id,
                "result": result,
            }),
        }
    }

    /// Standard error envelope with no `request_id` (connection-level errors).
    fn error(error: ApiError) -> Self {
        let status = error.status;
        Self {
            status,
            body: build_error_envelope(None, error),
        }
    }

    /// Standard error envelope with the echoed `request_id` from the parsed envelope.
    fn error_with_id(request_id: &str, error: ApiError) -> Self {
        let status = error.status;
        Self {
            status,
            body: build_error_envelope(Some(request_id), error),
        }
    }
}

fn build_error_envelope(request_id: Option<&str>, error: ApiError) -> serde_json::Value {
    let ApiError {
        code,
        message,
        field,
        retry_after_ms,
        partial_result,
        ..
    } = error;
    let mut error_obj = json!({
        "code": code.as_str(),
        "message": message,
    });
    if let Some(f) = field {
        error_obj["field"] = serde_json::Value::String(f);
    }
    if let Some(ms) = retry_after_ms {
        error_obj["retry_after_ms"] = serde_json::Value::Number(ms.into());
    }
    if let Some(pr) = partial_result {
        error_obj["partial_result"] = serde_json::Value::Bool(pr);
    }
    json!({
        "ok": false,
        "request_id": request_id,
        "error": error_obj,
    })
}

#[derive(Debug, Deserialize)]
struct RequestEnvelope {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct IngestPayload {
    records: Vec<GraphRecord>,
}

#[derive(Debug, Deserialize)]
struct AgentRegisterRequest {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    agent_kind: Option<String>,
    #[serde(default)]
    project_scope: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct AgentHeartbeatRequest {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

/// Per `docs/schema/daemon-query.md §2`.
#[derive(Debug, Deserialize)]
struct QueryBudget {
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Bi-temporal selector per `docs/schema/daemon-query.md §5`.
/// `transaction_time` and `since` are reserved; any verb that receives them
/// returns `not_implemented`.
#[derive(Debug, Deserialize, Default)]
struct QueryAsOf {
    /// Valid-time axis — honored by `symbol_by_name`, `file_defines`, `drift_top_n`.
    #[serde(default)]
    valid_time: Option<String>,
    /// Transaction-time axis — reserved; always returns `not_implemented`.
    #[serde(default)]
    transaction_time: Option<String>,
    /// Range query — reserved; always returns `not_implemented`.
    #[serde(default)]
    since: Option<String>,
}

/// Tagged-verb request envelope for `POST /v1/query`.
/// Per `docs/schema/daemon-query.md §2` (`schema_version` 1).
#[derive(Debug, Deserialize)]
struct QueryVerbRequest {
    /// Client-chosen correlation ID; echoed in every response. Required.
    #[serde(default)]
    request_id: Option<String>,
    /// Calling agent identity; optional for code-graph reads.
    #[serde(default)]
    #[allow(dead_code)]
    agent_id: Option<String>,
    /// Verb from the documented enum; required.
    #[serde(default)]
    verb: Option<String>,
    /// Verb-specific parameters object.
    #[serde(default)]
    params: Option<serde_json::Value>,
    /// Bi-temporal selector; optional.
    #[serde(default)]
    as_of: Option<QueryAsOf>,
    /// Read budget; optional. Defaults: `max_results`=5000, `timeout_ms`=5000.
    #[serde(default)]
    budget: Option<QueryBudget>,
    /// Domain filter; optional. Defaults to "codegraph".
    #[serde(default)]
    domain: Option<String>,
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.trim().is_empty())
}

struct AgentRegisterFull {
    agent_id: String,
    session_id: String,
    agent_kind: String,
    project_scope: String,
    registered_at: String,
}

/// Starts a daemon in the background and waits until it responds.
///
/// # Errors
///
/// Returns an error if another daemon is running, the child cannot be spawned,
/// or the daemon does not become healthy before the startup timeout.
pub fn start_background(config: &DaemonConfig) -> Result<DaemonMetadata> {
    if let Some(metadata) = active_metadata(&config.data_dir)? {
        return Err(already_running_error(&config.data_dir, &metadata));
    }
    let metadata_path = metadata_path(&config.data_dir);
    if metadata_path.exists() {
        if !mark_metadata_crashed_if_store_unleased(&config.data_dir)? {
            return Err(anyhow!(
                "daemon metadata is unresponsive but store lease is still held for {}",
                config.data_dir.display()
            ));
        }
    } else if StoreLease::try_acquire(&config.data_dir)?.is_none() {
        return Err(anyhow!(
            "embedded store lease is still held for {}",
            config.data_dir.display()
        ));
    }

    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("daemon")
        .arg("run")
        .arg("--data-dir")
        .arg(&config.data_dir)
        .arg("--host")
        .arg(&config.host)
        .arg("--port")
        .arg(config.port.to_string())
        .arg("--write-queue-capacity")
        .arg(config.write_queue_capacity.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }

    command.spawn().context("failed to spawn egregore daemon")?;
    wait_until_running(&config.data_dir)
}

/// Runs the daemon in the current process.
///
/// # Errors
///
/// Returns an error if the store cannot be opened, the data-dir lease cannot be
/// acquired, or the HTTP listener fails.
pub fn run_foreground(config: &DaemonConfig) -> Result<()> {
    fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("failed to create {}", config.data_dir.display()))?;
    if let Some(metadata) = active_metadata(&config.data_dir)? {
        return Err(already_running_error(&config.data_dir, &metadata));
    }
    let Some(mut lease) = StoreLease::try_acquire(&config.data_dir).with_context(|| {
        format!(
            "failed to acquire embedded store lease for {}",
            config.data_dir.display()
        )
    })?
    else {
        return Err(read_metadata(&config.data_dir).map_or_else(
            |_| {
                anyhow!(
                    "embedded store lease is still held for {}",
                    config.data_dir.display()
                )
            },
            |metadata| already_running_error(&config.data_dir, &metadata),
        ));
    };
    let sink = EmbeddedAletheiaSink::open_unleased(&config.data_dir).with_context(|| {
        format!(
            "failed to open embedded store {}",
            config.data_dir.display()
        )
    })?;
    let listener = TcpListener::bind((config.host.as_str(), config.port))
        .with_context(|| format!("failed to bind {}:{}", config.host, config.port))?;
    listener
        .set_nonblocking(true)
        .context("failed to configure daemon listener")?;
    let address = listener
        .local_addr()
        .context("failed to read daemon listener address")?
        .to_string();
    let token = random_token();
    let metadata = DaemonMetadata {
        schema_version: DAEMON_RUNTIME_SCHEMA_VERSION,
        pid: std::process::id(),
        address,
        token: token.clone(),
        data_dir: store_identity_dir(&config.data_dir),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at_unix_ms: unix_ms(),
        state: DaemonState::Running,
        api_version: None,
        transports: None,
        token_expires_at_unix_ms: None,
        daemons_index_url: None,
    };
    write_metadata(&config.data_dir, &metadata)?;
    lease.write_metadata(&metadata)?;

    let sink = Arc::new(RwLock::new(sink));
    let (write_tx, write_rx) = mpsc::sync_channel(config.write_queue_capacity);
    let idempotency_path = runtime_dir(&config.data_dir).join(IDEMPOTENCY_FILE);
    let idempotency = Arc::new(Mutex::new(IdempotencyStore::load(idempotency_path)?));
    let shutdown = Arc::new(AtomicBool::new(false));
    let pressure = Arc::new(PressureTracker::new(config.write_queue_capacity));
    let state = Arc::new(ServerState {
        token,
        store_identity: store_identity_text(&config.data_dir),
        sink: Arc::clone(&sink),
        write_tx,
        jobs: Arc::new(Mutex::new(BTreeMap::new())),
        agents: Arc::new(Mutex::new(BTreeMap::new())),
        idempotency: Arc::clone(&idempotency),
        shutdown: Arc::clone(&shutdown),
        pressure: Arc::clone(&pressure),
    });
    let worker = spawn_write_worker(write_rx, sink, idempotency, pressure);

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle_connection(stream, state));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error).context("daemon listener failed"),
        }
    }

    drop(state);
    let _ = worker.join();
    let stopped_metadata = DaemonMetadata {
        state: DaemonState::Stopped,
        ..metadata
    };
    let _ = write_metadata(&config.data_dir, &stopped_metadata);
    let _ = lease.write_metadata(&stopped_metadata);
    Ok(())
}

/// Returns active daemon metadata for a data directory if the daemon responds.
///
/// # Errors
///
/// Returns an error if runtime metadata or lock inspection fails.
pub fn active_metadata(data_dir: &Path) -> Result<Option<DaemonMetadata>> {
    if runtime_metadata_is_stale(data_dir)? {
        return Ok(None);
    }
    if !metadata_path(data_dir).exists() {
        return Ok(None);
    }
    let metadata = read_metadata(data_dir)?;
    let client = DaemonClient::for_data_dir(metadata.clone(), data_dir);
    if client.health().is_err() {
        return Ok(None);
    }
    Ok(Some(metadata))
}

/// Stops the running daemon for a data directory.
///
/// # Errors
///
/// Returns an error if metadata is missing or the shutdown request fails.
pub fn stop(data_dir: &Path) -> Result<()> {
    if runtime_metadata_is_stale(data_dir)? {
        if remove_metadata_if_store_unleased(data_dir)? {
            return Ok(());
        }
        return Err(anyhow!(
            "daemon metadata is stale but store lease is still held for {}",
            data_dir.display()
        ));
    }
    let metadata = read_metadata(data_dir)
        .with_context(|| format!("no daemon metadata found for {}", data_dir.display()))?;
    let client = DaemonClient::for_data_dir(metadata, data_dir);
    if let Err(error) = client.shutdown() {
        if active_metadata(data_dir)?.is_none() {
            if remove_metadata_if_store_unleased(data_dir)? {
                return Ok(());
            }
            return Err(anyhow!(
                "daemon is unresponsive but store lease is still held for {}; refusing to remove metadata: {error}",
                data_dir.display()
            ));
        }
        return Err(error);
    }
    wait_until_stopped(data_dir)
}

/// Client for the local Egregore daemon.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    metadata: DaemonMetadata,
    expected_store_identity: String,
}

impl DaemonClient {
    /// Creates a client from daemon metadata.
    #[must_use]
    pub fn new(metadata: DaemonMetadata) -> Self {
        let expected_store_identity = store_identity_text(&metadata.data_dir);
        Self {
            metadata,
            expected_store_identity,
        }
    }

    fn for_data_dir(metadata: DaemonMetadata, data_dir: &Path) -> Self {
        Self {
            metadata,
            expected_store_identity: store_identity_text(data_dir),
        }
    }

    /// Loads metadata from a data directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon metadata cannot be read.
    pub fn from_data_dir(data_dir: &Path) -> Result<Self> {
        let metadata = read_metadata(data_dir)?;
        if runtime_metadata_is_stale(data_dir)? {
            return Err(stale_metadata_error(data_dir));
        }
        Ok(Self::for_data_dir(metadata, data_dir))
    }

    /// Sends graph records to the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon rejects the request or cannot be reached.
    pub fn ingest_records(
        &self,
        records: &[GraphRecord],
        agent_id: &str,
        session_id: &str,
        idempotency_key: &str,
    ) -> Result<DaemonIngestResponse> {
        self.health()
            .context("daemon health validation failed before ingest")?;
        let body = json!({
            "request_id": request_id("ingest", idempotency_key),
            "agent_id": agent_id,
            "session_id": session_id,
            "idempotency_key": idempotency_key,
            "domain": "codegraph",
            "created_at": chrono::Utc::now().to_rfc3339(),
            "payload": { "records": records },
        });
        let (status, body) = self.request(
            "POST",
            "/v1/records/ingest",
            Some(body),
            CLIENT_OPERATION_TIMEOUT,
            true,
        )?;
        if status != 200 {
            return Err(anyhow!("daemon ingest failed with HTTP {status}: {body}"));
        }
        let envelope: serde_json::Value =
            serde_json::from_str(&body).context("failed to parse daemon ingest response")?;
        serde_json::from_value(envelope["result"].clone())
            .context("failed to parse daemon ingest result from envelope")
    }

    /// Checks daemon health.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon does not respond successfully.
    pub fn health(&self) -> Result<()> {
        let (status, body) = self.request("GET", "/v1/health", None, CLIENT_TIMEOUT, false)?;
        if status == 200 {
            let body = serde_json::from_str::<serde_json::Value>(&body)
                .context("failed to parse daemon health response")?;
            if body.get("status").and_then(serde_json::Value::as_str) == Some("ok")
                && body.get("version").and_then(serde_json::Value::as_str)
                    == Some(env!("CARGO_PKG_VERSION"))
                && body.get("data_dir").and_then(serde_json::Value::as_str)
                    == Some(self.expected_store_identity.as_str())
            {
                Ok(())
            } else {
                Err(anyhow!("daemon health response did not match egregore"))
            }
        } else {
            Err(anyhow!("daemon health failed with HTTP {status}: {body}"))
        }
    }

    /// Fetches the daemon's machine-readable status, including the
    /// write-admission pressure block.
    ///
    /// The returned JSON is the stable `GET /v1/status` contract documented in
    /// `docs/schema/daemon-api.md`.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon does not respond successfully or the
    /// response cannot be parsed.
    pub fn status(&self) -> Result<serde_json::Value> {
        let (status, body) = self.request("GET", "/v1/status", None, CLIENT_TIMEOUT, true)?;
        if status != 200 {
            return Err(anyhow!("daemon status failed with HTTP {status}: {body}"));
        }
        serde_json::from_str(&body).context("failed to parse daemon status response")
    }

    /// Fetches all records from the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon does not respond successfully or the
    /// response cannot be parsed.
    pub fn get_all_records(&self) -> Result<(Vec<GraphRecord>, Vec<UnknownSchemaVersion>, String)> {
        let (status, body) =
            self.request("GET", "/v1/records", None, CLIENT_OPERATION_TIMEOUT, true)?;
        if status != 200 {
            return Err(anyhow!(
                "daemon get_all_records failed with HTTP {status}: {body}"
            ));
        }
        let envelope: serde_json::Value =
            serde_json::from_str(&body).context("failed to parse daemon records response")?;
        let records = serde_json::from_value(envelope["result"]["records"].clone())
            .context("failed to parse records from envelope")?;
        let unknown_schema_versions =
            if let Some(val) = envelope["result"].get("unknown_schema_versions") {
                serde_json::from_value(val.clone())
                    .context("failed to parse unknown_schema_versions from envelope")?
            } else {
                Vec::new()
            };
        let snapshot_timestamp = envelope["result"]["snapshot_timestamp"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        Ok((records, unknown_schema_versions, snapshot_timestamp))
    }

    /// Sends a verb query to the daemon and returns the `result.records` array.
    ///
    /// `verb` must be one of the documented verbs in `docs/schema/daemon-query.md`.
    /// `params` is the verb-specific parameter object.
    /// `as_of_valid_time` is an optional RFC3339 valid-time selector.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon rejects the request or cannot be reached.
    pub fn query_verb(
        &self,
        verb: &str,
        params: &serde_json::Value,
        as_of_valid_time: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        let as_of = as_of_valid_time.map(|v| json!({ "valid_time": v }));
        let body = json!({
            "request_id": request_id("query", verb),
            "agent_id": "egregore-cli",
            "verb": verb,
            "params": params,
            "as_of": as_of,
        });
        let (status, body_str) = self.request(
            "POST",
            "/v1/query",
            Some(body),
            CLIENT_OPERATION_TIMEOUT,
            true,
        )?;
        if status != 200 {
            let envelope: serde_json::Value = serde_json::from_str(&body_str).unwrap_or_else(
                |_| json!({ "error": { "code": "parse_error", "message": body_str } }),
            );
            let code = envelope["error"]["code"].as_str().unwrap_or("unknown");
            let message = envelope["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(anyhow!("daemon query error ({code}): {message}"));
        }
        let envelope: serde_json::Value =
            serde_json::from_str(&body_str).context("failed to parse daemon query response")?;
        Ok(envelope["result"]["records"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    /// Requests daemon shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon does not accept shutdown.
    pub fn shutdown(&self) -> Result<()> {
        self.health()
            .context("daemon health validation failed before shutdown")?;
        let (status, body) =
            self.request("POST", "/v1/admin/shutdown", None, CLIENT_TIMEOUT, true)?;
        if status == 200 {
            Ok(())
        } else {
            Err(anyhow!("daemon shutdown failed with HTTP {status}: {body}"))
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
        timeout: Duration,
        include_auth: bool,
    ) -> Result<(u16, String)> {
        let body = body
            .map(|value| serde_json::to_string(&value))
            .transpose()?;
        let body_text = body.as_deref().unwrap_or("");
        let authorization = if include_auth {
            format!("Authorization: Bearer {}\r\n", self.metadata.token)
        } else {
            String::new()
        };
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: egregore\r\n{authorization}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_text}",
            body_text.len()
        );
        let address = self
            .metadata
            .address
            .parse::<SocketAddr>()
            .with_context(|| format!("invalid daemon address {}", self.metadata.address))?;
        let mut stream = TcpStream::connect_timeout(&address, timeout)
            .with_context(|| format!("failed to connect to {}", self.metadata.address))?;
        stream
            .set_read_timeout(Some(timeout))
            .context("failed to set daemon read timeout")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("failed to set daemon write timeout")?;
        stream
            .write_all(request.as_bytes())
            .context("failed to write daemon request")?;
        stream
            .shutdown(Shutdown::Write)
            .context("failed to finish daemon request")?;
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .context("failed to read daemon response")?;
        parse_http_response(&response)
    }
}

fn spawn_write_worker(
    write_rx: mpsc::Receiver<WriteCommand>,
    sink: Arc<RwLock<EmbeddedAletheiaSink>>,
    idempotency: Arc<Mutex<IdempotencyStore>>,
    pressure: Arc<PressureTracker>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(command) = write_rx.recv() {
            let result = apply_write(&command, &sink, &idempotency);
            // Drain one queue slot before replying so pressure recovery is
            // observable as soon as the worker finishes the write.
            pressure.on_complete();
            let _ = command.response_tx.send(result);
        }
    })
}

// Attempts recovery of a pending write using the record_ids stored in the idempotency entry,
// BEFORE re-running evidence-link validation.  Checks both original records (content match)
// and synthesized edge IDs from the pending entry (presence check) so recovery is not
// declared complete when only source nodes committed but the synthesized edges did not.
// Returns Some(response) on successful recovery, None if the write is not yet committed.
fn recover_pending_write_pre_validation(
    command: &WriteCommand,
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
    idempotency: &Arc<Mutex<IdempotencyStore>>,
) -> WriteResult<Option<DaemonIngestResponse>> {
    let pending_record_ids: Vec<String> = {
        let store = idempotency
            .lock()
            .map_err(|_| ApiError::internal("idempotency store lock poisoned"))?;
        match store.entries.get(&command.idempotency_key) {
            Some(IdempotencyEntry::Pending { record_ids, .. }) => record_ids.clone(),
            _ => return Ok(None),
        }
    };
    // Build the set of original record IDs for efficient lookup.
    let original_ids: BTreeSet<&str> = command.records.iter().map(GraphRecord::id).collect();
    let all_matched = {
        let sink_guard = sink
            .read()
            .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
        // Content-match all original records (catches payload mismatches).
        let originals_ok = command.records.iter().all(|r| {
            sink_guard
                .expected_record_state(r)
                .is_ok_and(|s| matches!(s, ExpectedRecordState::Matched))
        });
        // Presence-check synthesized edge IDs (those in the pending entry but not in the
        // original batch) to avoid falsely completing recovery when edges are missing.
        let synthesized_ok = pending_record_ids
            .iter()
            .filter(|id| !original_ids.contains(id.as_str()))
            .all(|id| sink_guard.read_back(id).is_ok_and(|r| r.is_some()));
        originals_ok && synthesized_ok
    };
    if !all_matched {
        return Ok(None);
    }
    let response = DaemonIngestResponse {
        attempted: pending_record_ids.len(),
        succeeded: pending_record_ids.len(),
        failed: 0,
        failures: Vec::new(),
        record_ids: pending_record_ids,
        idempotent: true,
    };
    complete_idempotency_entry(
        &command.idempotency_key,
        &command.payload_hash,
        &response,
        idempotency,
    )?;
    Ok(Some(response))
}

#[allow(clippy::too_many_lines)]
fn apply_write(
    command: &WriteCommand,
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
    idempotency: &Arc<Mutex<IdempotencyStore>>,
) -> WriteResult {
    validate_unique_recovery_keys(&command.records)?;

    // Consult the idempotency cache BEFORE running schema/evidence-link validation so that
    // a committed replay returns the cached response immediately without re-executing
    // validation against the current store state (which can differ from the original
    // write, e.g. the target has since grown additional temporal observations).
    let is_pending = {
        let store = idempotency
            .lock()
            .map_err(|_| ApiError::internal("idempotency store lock poisoned"))?;
        if let Some(entry) = store.entries.get(&command.idempotency_key) {
            if entry.payload_hash() != command.payload_hash {
                return Err(ApiError::conflict(
                    "idempotency key reused with different payload",
                ));
            }
            match entry {
                IdempotencyEntry::Committed { response, .. } => {
                    let mut response = response.clone();
                    response.idempotent = true;
                    return Ok(response);
                }
                IdempotencyEntry::Pending { .. } => true,
            }
        } else {
            false
        }
    };

    // For pending retries: attempt recovery BEFORE re-running evidence-link validation.
    // Re-running validation can fail spuriously when the original write's target nodes are
    // now in the store (e.g. an ambiguous temporal target that appears in both the store
    // and the retry batch).
    if is_pending
        && let Some(response) = recover_pending_write_pre_validation(command, sink, idempotency)?
    {
        return Ok(response);
    }

    validate_record_schema_versions(&command.records)?;
    validate_no_local_path_identity_in_shared_store(&command.records, sink)?;
    validate_verification_domain_records(&command.records)?;
    validate_artifact_domain_records(&command.records, sink)?;
    let project_edges = validate_project_domain_records(&command.records, sink)?;
    let user_context_edges = validate_user_context_domain_records(&command.records, sink)?;
    validate_semantic_domain_records(&command.records, sink)?;

    let (synthesized_edges, canonical_nodes) =
        validate_and_synthesize_evidence_edges(&command.records, sink)?;
    let synthesized_edges = project_edges
        .into_iter()
        .chain(user_context_edges)
        .chain(synthesized_edges)
        .collect::<Vec<_>>();

    // Reject any submitted record whose ID matches a synthesized evidence-edge ID.
    // This prevents a partial ingest where the submitted record is written first and
    // the synthesized edge is then rejected as a mismatched record with the same ID.
    let submitted_ids: BTreeSet<&str> = command.records.iter().map(GraphRecord::id).collect();
    for edge in &synthesized_edges {
        if submitted_ids.contains(edge.id()) {
            return Err(ApiError::conflict(format!(
                "synthesized evidence-edge ID '{}' conflicts with a submitted record",
                edge.id()
            )));
        }
    }

    // Replace triple-resolved source nodes with their canonical versions (evidence_links
    // filled with the resolved target_record_id) so both representations agree.
    let canonical_node_map: BTreeMap<&str, &GraphRecord> =
        canonical_nodes.iter().map(|r| (r.id(), r)).collect();
    let all_records: Vec<GraphRecord> = command
        .records
        .iter()
        .map(|r| {
            canonical_node_map
                .get(r.id())
                .copied()
                .cloned()
                .unwrap_or_else(|| r.clone())
        })
        .chain(synthesized_edges)
        .collect();
    let record_ids = all_records
        .iter()
        .map(|record| record.id().to_owned())
        .collect::<Vec<_>>();

    if !is_pending {
        let mut store = idempotency
            .lock()
            .map_err(|_| ApiError::internal("idempotency store lock poisoned"))?;
        store
            .set_entry_durably(
                command.idempotency_key.clone(),
                IdempotencyEntry::Pending {
                    payload_hash: command.payload_hash.clone(),
                    record_ids: record_ids.clone(),
                    // Store original records so restart recovery re-enqueues the
                    // same payload and recomputes the same hash.  Synthesized edges
                    // are re-derived from the original records on recovery.
                    records: command.records.clone(),
                },
            )
            .map_err(|error| ApiError::internal(error.to_string()))?;
    }

    if is_pending
        && let Some(response) = recover_pending_write(
            &command.idempotency_key,
            &command.payload_hash,
            &all_records,
            sink,
            idempotency,
        )?
    {
        return Ok(response);
    }

    let report = {
        let mut sink = sink
            .write()
            .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
        let report = ingest_records(&all_records, &mut *sink);
        if report.succeeded > 0 {
            sink.persist_indexes()
                .map_err(|error| ApiError::internal(error.to_string()))?;
        }
        report
    };
    let response = DaemonIngestResponse::from_report(report, record_ids, false);
    complete_idempotency_entry(
        &command.idempotency_key,
        &command.payload_hash,
        &response,
        idempotency,
    )?;
    Ok(response)
}

fn recover_pending_write(
    idempotency_key: &str,
    payload_hash: &str,
    records: &[GraphRecord],
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
    idempotency: &Arc<Mutex<IdempotencyStore>>,
) -> WriteResult<Option<DaemonIngestResponse>> {
    if has_duplicate_recovery_keys(records) || has_ambiguous_recovery_keys(records) {
        return Err(ApiError::conflict(
            "idempotency key has duplicate record IDs in pending recovery; manual repair is required",
        ));
    }
    let matched = {
        let sink = sink
            .read()
            .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
        let mut matched = 0;
        let mut mismatched = 0;
        for record in records {
            match sink.expected_record_state(record) {
                Ok(ExpectedRecordState::Matched) => matched += 1,
                Ok(ExpectedRecordState::Mismatched) => mismatched += 1,
                Ok(ExpectedRecordState::Missing) => {}
                Err(error) => return Err(ApiError::internal(error.to_string())),
            }
        }
        (matched, mismatched)
    };
    let (matched, mismatched) = matched;
    if mismatched > 0 {
        return Err(ApiError::conflict(
            "idempotency key has conflicting committed records; manual repair is required",
        ));
    }
    if matched == 0 {
        return Ok(None);
    }
    if matched != records.len() {
        return Err(ApiError::conflict(
            "idempotency key has a partial committed write; manual repair is required",
        ));
    }

    let response = DaemonIngestResponse {
        attempted: records.len(),
        succeeded: records.len(),
        failed: 0,
        failures: Vec::new(),
        record_ids: records
            .iter()
            .map(|record| record.id().to_owned())
            .collect::<Vec<_>>(),
        idempotent: false,
    };
    complete_idempotency_entry(idempotency_key, payload_hash, &response, idempotency)?;
    let mut response = response;
    response.idempotent = true;
    Ok(Some(response))
}

#[allow(clippy::too_many_lines)]
fn validate_no_local_path_identity_in_shared_store(
    records: &[GraphRecord],
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
) -> WriteResult<()> {
    // A Repository node is considered "local-path unsafe" if it has no identity payload
    // (machine-local write path; can't verify the source) or if the payload explicitly
    // declares LocalPath identity.
    let incoming_local_path_ids: Vec<&str> = records
        .iter()
        .filter_map(|record| {
            if let GraphRecord::Node {
                id,
                kind: NodeKind::Repository,
                repository_identity,
                ..
            } = record
            {
                let is_unsafe = repository_identity.as_deref().is_none_or(|payload| {
                    incoming_identity_is_local(payload)
                        || !repository_id_matches_payload(id, payload)
                });
                if is_unsafe {
                    return Some(id.as_str());
                }
            }
            None
        })
        .collect();

    let incoming_repo_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|record| {
            if let GraphRecord::Node {
                id,
                kind: NodeKind::Repository,
                ..
            } = record
            {
                return Some(id.as_str());
            }
            None
        })
        .collect();

    let incoming_has_non_codegraph = records
        .iter()
        .any(|record| !record.id().starts_with("codegraph:"));

    let incoming_tombstoned_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|record| {
            if let GraphRecord::Tombstone { deleted_id, .. } = record {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect();

    let (existing_repository_ids, stored_local_path_ids, store_is_multi_domain) = {
        let sink = sink
            .read()
            .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
        (
            sink.stored_repository_ids()
                .map_err(|error| ApiError::internal(error.to_string()))?,
            sink.stored_local_path_repository_ids()
                .map_err(|error| ApiError::internal(error.to_string()))?,
            sink.has_non_codegraph_records()
                .map_err(|error| ApiError::internal(error.to_string()))?,
        )
    };

    // Inverse check: if the store already has local_path repos, block writes that would
    // make the store shared (different repo ID or non-codegraph records).
    // Exception: a batch that tombstones the stored local-path repo is a migration write;
    // allow it so callers can retire a LocalPath identity and adopt a Remote one atomically.
    for stored_local_path_id in &stored_local_path_ids {
        if incoming_tombstoned_ids.contains(stored_local_path_id.as_str()) {
            continue;
        }
        let incoming_adds_different_repo = incoming_repo_ids
            .iter()
            .any(|id| *id != stored_local_path_id.as_str());
        if incoming_adds_different_repo || incoming_has_non_codegraph {
            return Err(ApiError::new(
                ErrorCode::LocalPathIdentityUnsupported,
                "store already contains a Repository with identity_source 'local_path'; \
                 adding a different repository or non-codegraph records would make it shared. \
                 Use a remote-backed clone or --repo-id-override.",
            ));
        }
    }

    if incoming_local_path_ids.is_empty() {
        return Ok(());
    }

    // Reject if the incoming batch itself contains 2+ distinct local_path Repository IDs.
    let distinct_incoming: BTreeSet<&str> = incoming_local_path_ids.iter().copied().collect();
    if distinct_incoming.len() > 1 {
        return Err(ApiError::new(
            ErrorCode::LocalPathIdentityUnsupported,
            "ingest batch contains multiple distinct Repository nodes with \
             identity_source 'local_path'; only one local-path repository may be \
             ingested into a store",
        ));
    }

    for incoming_id in incoming_local_path_ids {
        let has_other_repo = existing_repository_ids
            .iter()
            .any(|existing| existing != incoming_id)
            || incoming_repo_ids.iter().any(|id| *id != incoming_id);
        if has_other_repo || store_is_multi_domain || incoming_has_non_codegraph {
            return Err(ApiError::new(
                ErrorCode::LocalPathIdentityUnsupported,
                "Repository node with identity_source 'local_path' cannot be ingested into a \
                 shared store. Use a remote-backed clone or --repo-id-override to assign a \
                 stable identity before ingesting into a shared daemon store.",
            ));
        }
    }

    Ok(())
}

/// Returns `true` if an incoming Repository identity payload indicates machine-local identity.
///
/// A `Remote` payload is safe only when `remote_url` is present and non-local.
/// A `LocalRootCommit` payload is safe only when `root_commit_sha` is present and non-empty.
fn incoming_identity_is_local(payload: &crate::ir::RepositoryIdentityPayload) -> bool {
    match payload.identity_source {
        IdentitySource::LocalPath => true,
        IdentitySource::Remote => payload
            .remote_url
            .as_deref()
            .is_none_or(is_local_remote_url),
        IdentitySource::LocalRootCommit => {
            payload.root_commit_sha.as_deref().is_none_or(str::is_empty)
        }
        IdentitySource::OperatorOverride => false,
    }
}

/// Verification-domain node kinds permitted under `verification:v1:` IDs.
const VERIFICATION_NODE_KINDS: &[NodeKind] = &[
    NodeKind::CommandRun,
    NodeKind::Verification,
    NodeKind::TestRun,
    NodeKind::CIStatus,
    NodeKind::BenchmarkRun,
    NodeKind::CoverageReport,
    NodeKind::ProofResult,
];

/// Validates verification-domain records against the rules in
/// `docs/schema/verification.md`:
/// - Every record MUST carry an evidence handle (`source_artifact_hash`,
///   `source_artifact_path`, or `stdout_handle.hash`).
/// - `stdout_handle.inline` MUST be `None` when `stdout_handle.bytes` exceeds
///   the 16 KiB inline ceiling.
/// - `kind` must be one of the verification node kinds.
/// - `executed_at`, when present, must be a valid RFC 3339 timestamp.
/// - `schema_version` must equal `VERIFICATION_SCHEMA_VERSION`.
///
/// A record is treated as verification-domain when its ID starts with
/// `verification:v1:` (per `record_id_matches_domain`) or when it carries
/// `domain = "verification"` explicitly.
fn validate_verification_domain_records(records: &[GraphRecord]) -> WriteResult<()> {
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            domain,
            schema_version,
            source_artifact_hash,
            source_artifact_path,
            stdout_handle,
            stderr_handle,
            executed_at,
            ..
        } = record
        else {
            continue;
        };
        let is_verification =
            id.starts_with("verification:v1:") || domain.as_deref() == Some("verification");
        if !is_verification {
            continue;
        }

        if *schema_version != VERIFICATION_SCHEMA_VERSION {
            return Err(ApiError::bad_request(format!(
                "verification node '{id}' has schema_version {schema_version} but only \
                 version {VERIFICATION_SCHEMA_VERSION} is accepted"
            )));
        }

        if !VERIFICATION_NODE_KINDS.contains(kind) {
            return Err(ApiError::bad_request(format!(
                "node kind '{}' is not permitted under the verification domain; \
                 allowed kinds: CommandRun, Verification, TestRun, CIStatus, BenchmarkRun, \
                 CoverageReport, ProofResult",
                kind.as_str()
            )));
        }

        if let Some(ts) = executed_at.as_deref()
            && DateTime::parse_from_rfc3339(ts).is_err()
        {
            return Err(ApiError::bad_request(format!(
                "verification node '{id}' has invalid executed_at timestamp '{ts}'; \
                 must be RFC 3339"
            )));
        }

        let has_artifact_handle = source_artifact_hash
            .as_deref()
            .is_some_and(|s| !s.is_empty())
            || source_artifact_path
                .as_deref()
                .is_some_and(|s| !s.is_empty());
        let stdout_hash = stdout_handle.as_deref().is_some_and(|h| !h.hash.is_empty());
        let stderr_hash = stderr_handle.as_deref().is_some_and(|h| !h.hash.is_empty());

        if !has_artifact_handle && !stdout_hash && !stderr_hash {
            return Err(ApiError::new(
                ErrorCode::MissingEvidenceHandle,
                "verification-domain records must carry an evidence handle \
                 (source_artifact_hash, source_artifact_path, stdout_handle.hash, \
                 or stderr_handle.hash)",
            ));
        }

        if let Some(h) = stdout_handle.as_deref() {
            validate_verification_output_handle("stdout_handle", h)?;
        }
        if let Some(h) = stderr_handle.as_deref() {
            validate_verification_output_handle("stderr_handle", h)?;
        }
    }
    Ok(())
}

/// Artifact-domain node kinds permitted under `artifact:v1:` IDs.
const ARTIFACT_NODE_KINDS: &[NodeKind] = &[NodeKind::PatchArtifact];

/// `PatchArtifact.patch_status` values defined by `docs/schema/agent-actions.md`.
const PATCH_STATUS_VALUES: &[&str] = &[
    "applied_clean",
    "applied_with_conflicts",
    "invalid_syntax",
    "invalid_no_base",
    "rejected_validation",
    "unverified",
    "superseded",
];

/// Validates artifact-domain records against `docs/schema/agent-actions.md`.
#[allow(clippy::too_many_lines)]
fn validate_artifact_domain_records(
    records: &[GraphRecord],
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
) -> WriteResult<()> {
    let sink_guard = sink
        .read()
        .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            domain,
            schema_version,
            patch_status,
            base_commit,
            unknown_base_reason,
            target_files,
            patch_bytes_hash,
            patch_bytes_size,
            patch_handle,
            validation_summary,
            source_artifact_path,
            source_artifact_hash,
            producer_session_id,
            valid_time,
            valid_time_source,
            ingested_at,
            ..
        } = record
        else {
            continue;
        };
        let is_artifact = id.starts_with("artifact:v1:") || domain.as_deref() == Some("artifact");
        if !is_artifact {
            continue;
        }
        if !id.starts_with("artifact:v1:") {
            return Err(ApiError::bad_request(format!(
                "artifact-domain record '{id}' must use artifact:v1: ID prefix"
            )));
        }

        if *schema_version != ARTIFACT_SCHEMA_VERSION {
            return Err(ApiError::bad_request(format!(
                "artifact node '{id}' has schema_version {schema_version} but only version \
                 {ARTIFACT_SCHEMA_VERSION} is accepted"
            )));
        }
        if !ARTIFACT_NODE_KINDS.contains(kind) {
            return Err(ApiError::bad_request(format!(
                "node kind '{}' is not permitted under the artifact domain; allowed kind: \
                 PatchArtifact",
                kind.as_str()
            )));
        }
        if domain.as_deref() != Some("artifact") {
            return Err(ApiError::missing_field(
                "domain (PatchArtifact requires domain artifact)",
            ));
        }

        let status = required_str(patch_status.as_deref(), "patch_status")?;
        if !PATCH_STATUS_VALUES.contains(&status) {
            return Err(ApiError::bad_request(format!(
                "PatchArtifact.patch_status '{status}' is not recognized; expected one of: {}",
                PATCH_STATUS_VALUES.join(", ")
            )));
        }
        let has_base_commit = base_commit
            .as_deref()
            .is_some_and(|commit| !commit.is_empty());
        if status == "invalid_no_base" && has_base_commit {
            return Err(ApiError::bad_request(
                "PatchArtifact.base_commit must be null when patch_status is invalid_no_base",
            ));
        }
        if has_base_commit
            && unknown_base_reason
                .as_deref()
                .is_some_and(|reason| !reason.is_empty())
        {
            return Err(ApiError::bad_request(
                "PatchArtifact.unknown_base_reason must be null when base_commit is set",
            ));
        }
        if !has_base_commit && unknown_base_reason.as_deref() != Some("unknown_base") {
            return Err(ApiError::missing_field(
                "unknown_base_reason (required when base_commit is null)",
            ));
        }
        let target_files = target_files
            .as_ref()
            .ok_or_else(|| ApiError::missing_field("target_files"))?;
        if status == "invalid_syntax" && !target_files.is_empty() {
            return Err(ApiError::bad_request(
                "PatchArtifact.target_files must be empty when patch_status is invalid_syntax",
            ));
        }
        required_str(patch_bytes_hash.as_deref(), "patch_bytes_hash")?;
        let patch_bytes_size =
            patch_bytes_size.ok_or_else(|| ApiError::missing_field("patch_bytes_size"))?;
        let handle = patch_handle
            .as_deref()
            .ok_or_else(|| ApiError::missing_field("patch_handle"))?;
        if handle.path.is_empty() {
            return Err(ApiError::missing_field("patch_handle.path"));
        }
        if let Some(inline) = handle.inline.as_deref() {
            let inline_len = inline.len() as u64;
            if inline_len > patch_bytes_size {
                return Err(ApiError::bad_request(
                    "PatchArtifact.patch_bytes_size must be >= patch_handle.inline length",
                ));
            }
            if inline_len > INLINE_PAYLOAD_CEILING || patch_bytes_size > INLINE_PAYLOAD_CEILING {
                return Err(ApiError::inline_payload_exceeds_ceiling(
                    "PatchArtifact.patch_handle.inline must be None when patch bytes exceed the \
                     16 KiB ceiling; demote to handle-only before writing",
                ));
            }
        }
        required_str(validation_summary.as_deref(), "validation_summary")?;
        required_str(source_artifact_path.as_deref(), "source_artifact_path")?;
        required_str(source_artifact_hash.as_deref(), "source_artifact_hash")?;
        let producer_session_id =
            required_str(producer_session_id.as_deref(), "producer_session_id")?;
        validate_agent_session_ref(
            "PatchArtifact.producer_session_id",
            producer_session_id,
            records,
            &sink_guard,
        )?;
        let valid_time = required_str(valid_time.as_deref(), "valid_time")?;
        if DateTime::parse_from_rfc3339(valid_time).is_err() {
            return Err(ApiError::bad_request(format!(
                "PatchArtifact.valid_time '{valid_time}' is not a valid RFC 3339 timestamp"
            )));
        }
        if valid_time_source.as_deref() != Some("produced_at") {
            return Err(ApiError::bad_request(
                "PatchArtifact.valid_time_source must equal produced_at",
            ));
        }
        let ingested_at = required_str(ingested_at.as_deref(), "ingested_at")?;
        if DateTime::parse_from_rfc3339(ingested_at).is_err() {
            return Err(ApiError::bad_request(format!(
                "PatchArtifact.ingested_at '{ingested_at}' is not a valid RFC 3339 timestamp"
            )));
        }
    }

    for record in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::PatchArtifact,
            patch_status: Some(new_status),
            ..
        } = record
        else {
            continue;
        };
        match sink_guard.read_back(id) {
            Ok(Some(GraphRecord::Node {
                kind: NodeKind::PatchArtifact,
                patch_status: Some(existing_status),
                ..
            })) if existing_status != *new_status => {
                return Err(ApiError::patch_status_pinned(format!(
                    "PatchArtifact.patch_status is append-only for '{id}'; existing status \
                     '{existing_status}' cannot be rewritten to '{new_status}'"
                )));
            }
            Ok(_) => {}
            Err(error) => return Err(ApiError::internal(error.to_string())),
        }
    }
    Ok(())
}

/// Project-domain node kinds permitted under `project:v1:` IDs.
const PROJECT_NODE_KINDS: &[NodeKind] = &[
    NodeKind::Task,
    NodeKind::AcceptanceCriterion,
    NodeKind::ExternalLink,
    NodeKind::Product,
    NodeKind::Project,
    NodeKind::Plan,
    NodeKind::GitHubIssue,
    NodeKind::PR,
    NodeKind::Review,
    NodeKind::LocalTask,
    // Importer diagnostics are valid project records; they carry entity_id == id
    // and valid_time == transaction_time so partial imports remain ingestible.
    NodeKind::Diagnostic,
];

const PROJECT_FULL_NODE_KINDS: &[NodeKind] = &[
    NodeKind::Task,
    NodeKind::AcceptanceCriterion,
    NodeKind::ExternalLink,
];

const PROJECT_EDGE_LABELS: &[EdgeLabel] = &[
    EdgeLabel::ClosesAcceptanceCriterion,
    EdgeLabel::OwnedByTask,
    EdgeLabel::ExternalHandle,
    EdgeLabel::TouchesFile,
    EdgeLabel::MentionsSymbol,
];

#[allow(clippy::too_many_lines)]
fn validate_project_domain_records(
    records: &[GraphRecord],
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
) -> WriteResult<Vec<GraphRecord>> {
    let sink_guard = sink
        .read()
        .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
    let mut synthesized_edges = Vec::new();
    for record in records {
        match record {
            GraphRecord::Node {
                id,
                kind,
                schema_version,
                domain,
                entity_id,
                title,
                body_handle,
                source_kind,
                source_external_link_id,
                assignees,
                labels,
                priority,
                parent_task_id,
                ordinal,
                text,
                status,
                verification_link_id,
                system,
                url,
                system_native_id,
                discovered_at,
                valid_time,
                valid_time_source,
                transaction_time,
                ..
            } => {
                let is_project = id.starts_with("project:v1:")
                    || domain.as_deref() == Some("project")
                    || matches!(
                        kind,
                        NodeKind::AcceptanceCriterion
                            | NodeKind::ExternalLink
                            | NodeKind::Product
                            | NodeKind::Project
                            | NodeKind::Plan
                            | NodeKind::GitHubIssue
                            | NodeKind::PR
                            | NodeKind::Review
                            | NodeKind::LocalTask
                    );
                if !is_project {
                    continue;
                }
                validate_project_node_base(
                    id,
                    *kind,
                    *schema_version,
                    domain.as_deref(),
                    entity_id.as_deref(),
                    valid_time.as_deref(),
                    valid_time_source.as_deref(),
                    transaction_time.as_deref(),
                )?;
                if !PROJECT_FULL_NODE_KINDS.contains(kind) {
                    continue;
                }
                match kind {
                    NodeKind::Task => {
                        validate_project_task(
                            id,
                            title.as_deref(),
                            body_handle.as_deref(),
                            status.as_deref(),
                            source_kind.as_deref(),
                            source_external_link_id.as_deref(),
                            assignees.as_deref(),
                            labels.as_deref(),
                            priority.as_deref(),
                            records,
                            &sink_guard,
                        )?;
                        let link_id = required_str(
                            source_external_link_id.as_deref(),
                            "Task.source_external_link_id",
                        )?;
                        synthesized_edges.push(project_edge(
                            EdgeLabel::ExternalHandle,
                            id,
                            link_id,
                            "Project Task external source handle",
                        ));
                    }
                    NodeKind::AcceptanceCriterion => {
                        validate_project_acceptance_criterion(
                            id,
                            parent_task_id.as_deref(),
                            *ordinal,
                            text.as_deref(),
                            status.as_deref(),
                            verification_link_id.as_deref(),
                            records,
                            &sink_guard,
                        )?;
                        let parent_id = required_str(
                            parent_task_id.as_deref(),
                            "AcceptanceCriterion.parent_task_id",
                        )?;
                        synthesized_edges.push(project_edge(
                            EdgeLabel::OwnedByTask,
                            id,
                            parent_id,
                            "AcceptanceCriterion belongs to Task",
                        ));
                        if let Some(verification_id) = verification_link_id.as_deref() {
                            synthesized_edges.push(project_edge(
                                EdgeLabel::ClosesAcceptanceCriterion,
                                id,
                                verification_id,
                                "AcceptanceCriterion closed by verification evidence",
                            ));
                        }
                    }
                    NodeKind::ExternalLink => validate_project_external_link(
                        id,
                        system.as_deref(),
                        url.as_deref(),
                        system_native_id.as_deref(),
                        discovered_at.as_deref(),
                    )?,
                    _ => {}
                }
            }
            GraphRecord::Edge {
                id,
                schema_version,
                label,
                source,
                target,
                confidence,
                ..
            } if id.starts_with("project:v1:") => {
                validate_project_edge(
                    id,
                    *schema_version,
                    *label,
                    source,
                    target,
                    confidence.as_deref(),
                    records,
                    &sink_guard,
                )?;
            }
            _ => {}
        }
    }
    Ok(synthesized_edges)
}

fn adapter_read_error_to_api(error: AdapterError) -> ApiError {
    match error {
        AdapterError::TimedOut { .. } => ApiError::query_timeout(),
        AdapterError::UnknownSchemaVersion { version, .. } => {
            ApiError::unknown_record_schema_version(&version)
        }
        other => ApiError::internal(other.to_string()),
    }
}

fn validate_record_schema_versions(records: &[GraphRecord]) -> WriteResult<()> {
    for record in records {
        validate_record_version(record)
            .map_err(|unknown| ApiError::unknown_schema_version(&unknown))?;
    }
    Ok(())
}

fn validate_semantic_domain_records(
    records: &[GraphRecord],
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
) -> WriteResult<()> {
    let store_records = {
        let sink_guard = sink
            .read()
            .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
        sink_guard
            .read_all_records()
            .map_err(|error| ApiError::internal(error.to_string()))?
    };

    for record in records {
        match record {
            GraphRecord::Node { id, domain, .. }
                if id.starts_with("semantic:v1:") || domain.as_deref() == Some("semantic") =>
            {
                validate_semantic_drift_node(record, records, &store_records, sink)?;
            }
            GraphRecord::Edge {
                id,
                schema_version,
                label,
                source,
                target,
                ..
            } if id.starts_with("semantic:v1:") => {
                if *schema_version != SEMANTIC_SCHEMA_VERSION {
                    return Err(ApiError::bad_request(format!(
                        "semantic edge '{id}' has schema_version {schema_version} but only version {SEMANTIC_SCHEMA_VERSION} is accepted"
                    )));
                }
                let sink_guard = sink
                    .read()
                    .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
                validate_semantic_edge(id, *label, source, target, records, &sink_guard)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_semantic_drift_node(
    record: &GraphRecord,
    records: &[GraphRecord],
    store_records: &[GraphRecord],
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
) -> WriteResult<()> {
    let GraphRecord::Node {
        id,
        kind,
        schema_version,
        semantic_drift,
        valid_time,
        valid_time_source,
        ingested_at,
        domain,
        ..
    } = record
    else {
        return Ok(());
    };

    if !id.starts_with("semantic:v1:") {
        return Err(ApiError::bad_request(format!(
            "semantic-domain record '{id}' must use semantic:v1: ID prefix"
        )));
    }
    if *schema_version != SEMANTIC_SCHEMA_VERSION {
        return Err(ApiError::bad_request(format!(
            "semantic node '{id}' has schema_version {schema_version} but only version {SEMANTIC_SCHEMA_VERSION} is accepted"
        )));
    }
    if domain.as_deref() != Some("semantic") {
        return Err(ApiError::bad_request(format!(
            "semantic node '{id}' must carry domain 'semantic'"
        )));
    }
    if *kind != NodeKind::SemanticDrift {
        return Err(ApiError::bad_request(format!(
            "node kind '{}' is not permitted for semantic drift records",
            kind.as_str()
        )));
    }

    required_str(valid_time.as_deref(), "valid_time")?;
    required_str(valid_time_source.as_deref(), "valid_time_source")?;
    required_str(ingested_at.as_deref(), "ingested_at")?;

    let drift = semantic_drift
        .as_deref()
        .ok_or_else(|| ApiError::missing_field("semantic_drift"))?;
    validate_semantic_drift_payload(id, drift)?;
    let sink_guard = sink
        .read()
        .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
    validate_semantic_drift_endpoint(
        "SemanticDrift.target_record_id",
        &drift.target_record_id,
        records,
        &sink_guard,
    )?;
    validate_semantic_drift_endpoint(
        "SemanticDrift.prior_record_id",
        &drift.prior_record_id,
        records,
        &sink_guard,
    )?;
    validate_semantic_drift_edges(id, drift, records, store_records)?;
    validate_semantic_drift_immutable(id, drift, &sink_guard)
}

fn validate_semantic_drift_edges(
    id: &str,
    drift: &crate::ir::SemanticDriftMetadata,
    records: &[GraphRecord],
    store_records: &[GraphRecord],
) -> WriteResult<()> {
    if !semantic_edge_present(
        id,
        EdgeLabel::DriftsFrom,
        &drift.target_record_id,
        records,
        store_records,
    ) {
        return Err(ApiError::new(
            ErrorCode::DriftPriorTargetMismatch,
            format!("SemanticDrift '{id}' is missing DRIFTS_FROM edge to target_record_id"),
        ));
    }
    if !semantic_edge_present(
        id,
        EdgeLabel::DriftsPrior,
        &drift.prior_record_id,
        records,
        store_records,
    ) {
        return Err(ApiError::new(
            ErrorCode::DriftPriorTargetMismatch,
            format!("SemanticDrift '{id}' is missing DRIFTS_PRIOR edge to prior_record_id"),
        ));
    }
    Ok(())
}

fn validate_semantic_drift_immutable(
    id: &str,
    drift: &crate::ir::SemanticDriftMetadata,
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    match sink.read_back(id) {
        Ok(Some(GraphRecord::Node {
            semantic_drift: Some(existing),
            ..
        })) if (existing.score - drift.score).abs() > SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE => {
            Err(ApiError::new(
                ErrorCode::DriftRecordImmutable,
                format!(
                    "SemanticDrift '{id}' is immutable; existing score {} cannot be rewritten to {}",
                    existing.score, drift.score
                ),
            ))
        }
        Ok(_) => Ok(()),
        Err(error) => Err(ApiError::internal(error.to_string())),
    }
}

fn validate_semantic_drift_payload(
    record_id: &str,
    drift: &crate::ir::SemanticDriftMetadata,
) -> WriteResult<()> {
    required_str(
        Some(drift.embedding_model.provider.as_str()),
        "embedding_model.provider",
    )?;
    required_str(
        Some(drift.embedding_model.name.as_str()),
        "embedding_model.name",
    )?;
    required_str(
        Some(drift.embedding_model.version.as_str()),
        "embedding_model.version",
    )?;
    if drift.embedding_model.dim == 0 {
        return Err(ApiError::bad_request(format!(
            "SemanticDrift '{record_id}' embedding_model.dim must be greater than zero"
        )));
    }
    required_str(
        Some(drift.embedding_model.content_hash.as_str()),
        "embedding_model.content_hash",
    )?;
    required_str(Some(drift.metric_kind.as_str()), "metric_kind")?;
    if !drift.score.is_finite() {
        return Err(ApiError::bad_request(format!(
            "SemanticDrift '{record_id}' score must be a finite JSON number"
        )));
    }
    if !drift.selection_threshold.is_finite() {
        return Err(ApiError::bad_request(format!(
            "SemanticDrift '{record_id}' selection_threshold must be finite"
        )));
    }
    required_str(Some(drift.selection_basis.as_str()), "selection_basis")?;
    Ok(())
}

fn validate_semantic_drift_endpoint(
    field: &'static str,
    value: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    if !value.starts_with("codegraph:") {
        return Err(ApiError::new(
            ErrorCode::DriftPriorTargetMismatch,
            format!("{field} must reference a codegraph File or Symbol; got '{value}'"),
        ));
    }
    match lookup_node_kind(value, records, sink)? {
        Some(NodeKind::File | NodeKind::Symbol) => Ok(()),
        Some(kind) => Err(ApiError::new(
            ErrorCode::DriftPriorTargetMismatch,
            format!(
                "{field} must reference a codegraph File or Symbol; target '{value}' has kind {}",
                kind.as_str()
            ),
        )),
        None => Err(ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("{field} target '{value}' not found in store or batch"),
        )),
    }
}

fn semantic_edge_present(
    source: &str,
    label: EdgeLabel,
    target: &str,
    batch: &[GraphRecord],
    store_records: &[GraphRecord],
) -> bool {
    batch.iter().chain(store_records.iter()).any(|record| {
        matches!(
            record,
            GraphRecord::Edge {
                label: edge_label,
                source: edge_source,
                target: edge_target,
                ..
            } if *edge_label == label && edge_source == source && edge_target == target
        )
    })
}

fn validate_semantic_edge(
    edge_id: &str,
    label: EdgeLabel,
    source: &str,
    target: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    if !matches!(
        label,
        EdgeLabel::DriftsFrom | EdgeLabel::DriftsPrior | EdgeLabel::MeasuredBy
    ) {
        return Err(ApiError::bad_request(format!(
            "semantic edge '{edge_id}' uses unsupported label '{}'",
            label.as_str()
        )));
    }
    if !source.starts_with("semantic:v1:") {
        return Err(ApiError::bad_request(format!(
            "semantic edge '{edge_id}' requires a semantic:v1: source; got '{source}'"
        )));
    }
    let source_kind = lookup_node_kind(source, records, sink)?;
    if !matches!(source_kind, Some(NodeKind::SemanticDrift)) {
        return Err(ApiError::bad_request(format!(
            "semantic edge '{edge_id}' requires a SemanticDrift source"
        )));
    }
    match label {
        EdgeLabel::DriftsFrom | EdgeLabel::DriftsPrior => {
            validate_semantic_drift_endpoint("semantic edge target", target, records, sink)
        }
        EdgeLabel::MeasuredBy => {
            if !target.starts_with("semantic:v1:") {
                return Err(ApiError::new(
                    ErrorCode::DriftPriorTargetMismatch,
                    format!(
                        "semantic MEASURED_BY edge '{edge_id}' requires a semantic:v1: EmbeddingModel target; got '{target}'"
                    ),
                ));
            }
            match lookup_node_kind(target, records, sink)? {
                Some(NodeKind::EmbeddingModel) => Ok(()),
                Some(kind) => Err(ApiError::new(
                    ErrorCode::DriftPriorTargetMismatch,
                    format!(
                        "semantic MEASURED_BY edge '{edge_id}' target has kind {}; expected EmbeddingModel",
                        kind.as_str()
                    ),
                )),
                None => Err(ApiError::new(
                    ErrorCode::UnresolvedEvidenceTarget,
                    format!("semantic MEASURED_BY target '{target}' not found in store or batch"),
                )),
            }
        }
        _ => unreachable!("semantic edge labels were checked above"),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_project_node_base(
    id: &str,
    kind: NodeKind,
    schema_version: u32,
    domain: Option<&str>,
    entity_id: Option<&str>,
    valid_time: Option<&str>,
    valid_time_source: Option<&str>,
    transaction_time: Option<&str>,
) -> WriteResult<()> {
    if !id.starts_with("project:v1:") {
        return Err(ApiError::bad_request(format!(
            "project-domain record '{id}' must use project:v1: ID prefix"
        )));
    }
    if schema_version != PROJECT_SCHEMA_VERSION {
        return Err(ApiError::bad_request(format!(
            "project node '{id}' has schema_version {schema_version} but only version {PROJECT_SCHEMA_VERSION} is accepted"
        )));
    }
    if domain != Some("project") {
        return Err(ApiError::bad_request(format!(
            "project node '{id}' must carry domain 'project'"
        )));
    }
    if !PROJECT_NODE_KINDS.contains(&kind) {
        return Err(ApiError::bad_request(format!(
            "node kind '{}' is not permitted under the project domain",
            kind.as_str()
        )));
    }
    let entity_id = required_str(entity_id, "entity_id")?;
    if entity_id != id {
        return Err(ApiError::bad_request(format!(
            "project node '{id}' entity_id must equal its stable record id in v1"
        )));
    }
    validate_rfc3339_field("valid_time", required_str(valid_time, "valid_time")?)?;
    required_str(valid_time_source, "valid_time_source")?;
    validate_rfc3339_field(
        "transaction_time",
        required_str(transaction_time, "transaction_time")?,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_project_task(
    id: &str,
    title: Option<&str>,
    body_handle: Option<&OutputHandle>,
    status: Option<&str>,
    source_kind: Option<&str>,
    source_external_link_id: Option<&str>,
    assignees: Option<&[String]>,
    labels: Option<&[String]>,
    priority: Option<&str>,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    required_str(title, "Task.title")?;
    let handle = body_handle.ok_or_else(|| ApiError::missing_field("Task.body_handle"))?;
    validate_project_output_handle("Task.body_handle", handle)?;
    required_str(status, "Task.status")?;
    required_str(source_kind, "Task.source_kind")?;
    let link_id = required_str(source_external_link_id, "Task.source_external_link_id")?;
    validate_project_ref(
        "Task.source_external_link_id",
        link_id,
        "project:v1:",
        "ExternalLink",
        &[NodeKind::ExternalLink],
        records,
        sink,
    )?;
    assignees.ok_or_else(|| ApiError::missing_field("Task.assignees"))?;
    labels.ok_or_else(|| ApiError::missing_field("Task.labels"))?;
    required_str(priority, "Task.priority")?;
    if id != link_id && !id.starts_with("project:v1:") {
        return Err(ApiError::bad_request("Task.id must use project:v1: prefix"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_project_acceptance_criterion(
    _id: &str,
    parent_task_id: Option<&str>,
    ordinal: Option<u32>,
    text: Option<&str>,
    status: Option<&str>,
    verification_link_id: Option<&str>,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let parent_id = required_str(parent_task_id, "AcceptanceCriterion.parent_task_id")?;
    validate_project_ref(
        "AcceptanceCriterion.parent_task_id",
        parent_id,
        "project:v1:",
        "Task",
        &[NodeKind::Task],
        records,
        sink,
    )?;
    ordinal.ok_or_else(|| ApiError::missing_field("AcceptanceCriterion.ordinal"))?;
    required_str(text, "AcceptanceCriterion.text")?;
    let status = required_str(status, "AcceptanceCriterion.status")?;
    if status == "verified" && verification_link_id.is_none() {
        return Err(ApiError::new(
            ErrorCode::AcceptanceCriterionMissingVerification,
            "AcceptanceCriterion.status verified requires verification_link_id",
        ));
    }
    if let Some(verification_id) = verification_link_id {
        validate_project_verification_ref(
            "AcceptanceCriterion.verification_link_id",
            verification_id,
            records,
            sink,
        )?;
    }
    Ok(())
}

fn validate_project_external_link(
    _id: &str,
    system: Option<&str>,
    url: Option<&str>,
    system_native_id: Option<&str>,
    discovered_at: Option<&str>,
) -> WriteResult<()> {
    required_str(system, "ExternalLink.system")?;
    required_str(url, "ExternalLink.url")?;
    required_str(system_native_id, "ExternalLink.system_native_id")?;
    validate_rfc3339_field(
        "ExternalLink.discovered_at",
        required_str(discovered_at, "ExternalLink.discovered_at")?,
    )
}

fn validate_project_ref(
    field: &'static str,
    value: &str,
    expected_prefix: &'static str,
    expected_kind: &'static str,
    allowed_kinds: &[NodeKind],
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    if !value.starts_with(expected_prefix) {
        return Err(ApiError::bad_request(format!(
            "{field} must reference a {expected_prefix} {expected_kind}; got '{value}'"
        )));
    }
    match lookup_node_kind(value, records, sink)? {
        Some(kind) if allowed_kinds.contains(&kind) => Ok(()),
        Some(kind) => Err(ApiError::bad_request(format!(
            "{field} must reference a {expected_prefix} {expected_kind}; target '{value}' has kind {}",
            kind.as_str()
        ))),
        None => Err(ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("{field} target '{value}' not found in store or batch"),
        )),
    }
}

fn validate_project_verification_ref(
    field: &'static str,
    value: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    validate_project_ref(
        field,
        value,
        "verification:v1:",
        "Verification, CommandRun, or TestRun",
        &[
            NodeKind::Verification,
            NodeKind::CommandRun,
            NodeKind::TestRun,
        ],
        records,
        sink,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_project_edge(
    edge_id: &str,
    schema_version: u32,
    label: EdgeLabel,
    source: &str,
    target: &str,
    confidence: Option<&str>,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    if schema_version != PROJECT_SCHEMA_VERSION {
        return Err(ApiError::bad_request(format!(
            "project edge '{edge_id}' has schema_version {schema_version} but only version {PROJECT_SCHEMA_VERSION} is accepted"
        )));
    }
    if !PROJECT_EDGE_LABELS.contains(&label) {
        return Err(ApiError::bad_request(format!(
            "project edge '{edge_id}' uses unsupported label '{}'",
            label.as_str()
        )));
    }
    let source_kind = lookup_node_kind(source, records, sink)?;
    let target_kind = lookup_node_kind(target, records, sink)?;
    match label {
        EdgeLabel::OwnedByTask => {
            validate_project_edge_kinds(
                edge_id,
                label,
                source_kind,
                &[NodeKind::AcceptanceCriterion],
                target_kind,
                &[NodeKind::Task],
            )?;
        }
        EdgeLabel::ExternalHandle => {
            validate_project_edge_kinds(
                edge_id,
                label,
                source_kind,
                &[NodeKind::Task, NodeKind::AcceptanceCriterion],
                target_kind,
                &[NodeKind::ExternalLink],
            )?;
        }
        EdgeLabel::ClosesAcceptanceCriterion => {
            validate_project_edge_kinds(
                edge_id,
                label,
                source_kind,
                &[NodeKind::AcceptanceCriterion],
                target_kind,
                &[
                    NodeKind::Verification,
                    NodeKind::CommandRun,
                    NodeKind::TestRun,
                ],
            )?;
        }
        EdgeLabel::TouchesFile => {
            validate_project_edge_kinds(
                edge_id,
                label,
                source_kind,
                &[NodeKind::Task],
                target_kind,
                &[NodeKind::File],
            )?;
        }
        EdgeLabel::MentionsSymbol => {
            validate_project_edge_kinds(
                edge_id,
                label,
                source_kind,
                &[NodeKind::Task],
                target_kind,
                &[NodeKind::Symbol],
            )?;
            validate_confidence(edge_id, label, confidence)?;
        }
        _ => {}
    }
    Ok(())
}

fn validate_project_edge_kinds(
    edge_id: &str,
    label: EdgeLabel,
    source_kind: Option<NodeKind>,
    allowed_sources: &[NodeKind],
    target_kind: Option<NodeKind>,
    allowed_targets: &[NodeKind],
) -> WriteResult<()> {
    match source_kind {
        Some(kind) if allowed_sources.contains(&kind) => {}
        Some(kind) => {
            return Err(ApiError::bad_request(format!(
                "project edge '{edge_id}' label '{}' has invalid source kind {}",
                label.as_str(),
                kind.as_str()
            )));
        }
        None => {
            return Err(ApiError::new(
                ErrorCode::UnresolvedEvidenceTarget,
                format!("project edge '{edge_id}' source not found"),
            ));
        }
    }
    match target_kind {
        Some(kind) if allowed_targets.contains(&kind) => Ok(()),
        Some(kind) => Err(ApiError::bad_request(format!(
            "project edge '{edge_id}' label '{}' has invalid target kind {}",
            label.as_str(),
            kind.as_str()
        ))),
        None => Err(ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("project edge '{edge_id}' target not found"),
        )),
    }
}

fn validate_project_output_handle(field: &'static str, handle: &OutputHandle) -> WriteResult<()> {
    if handle.hash.is_empty() {
        return Err(ApiError::bad_request(format!(
            "{field}.hash must not be empty"
        )));
    }
    let inline_len = handle.inline.as_deref().map_or(0, |s| s.len() as u64);
    if inline_len > handle.bytes {
        return Err(ApiError::bad_request(format!(
            "{field}.bytes must be >= inline payload length"
        )));
    }
    if inline_len > INLINE_PAYLOAD_CEILING
        || (handle.inline.is_some() && handle.bytes > INLINE_PAYLOAD_CEILING)
    {
        return Err(ApiError::inline_payload_exceeds_ceiling(format!(
            "{field}.inline must be None when bytes exceeds the 16 KiB ceiling; demote to handle-only before writing"
        )));
    }
    Ok(())
}

fn validate_rfc3339_field(field: &'static str, value: &str) -> WriteResult<()> {
    if DateTime::parse_from_rfc3339(value).is_err() {
        return Err(ApiError::bad_request(format!(
            "{field} '{value}' is not a valid RFC 3339 timestamp"
        )));
    }
    Ok(())
}

fn parse_rfc3339_field(
    field: &'static str,
    value: &str,
) -> WriteResult<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc3339(value).map_err(|_| {
        ApiError::bad_request(format!(
            "{field} '{value}' is not a valid RFC 3339 timestamp"
        ))
    })
}

fn validate_confidence(
    edge_id: &str,
    label: EdgeLabel,
    confidence: Option<&str>,
) -> WriteResult<()> {
    let valid = confidence
        .and_then(|s| s.parse::<f64>().ok())
        .is_some_and(|v| (0.0..=1.0).contains(&v));
    if !valid {
        return Err(ApiError::bad_request(format!(
            "project edge '{edge_id}' label '{}' requires a numeric confidence in [0.0, 1.0]",
            label.as_str()
        )));
    }
    Ok(())
}

fn project_edge(label: EdgeLabel, source: &str, target: &str, summary: &str) -> GraphRecord {
    let id = project_edge_id(label, source, target);
    GraphRecord::Edge {
        id,
        schema_version: PROJECT_SCHEMA_VERSION,
        label,
        source: source.to_owned(),
        target: target.to_owned(),
        confidence: None,
        temporal: None,
        summary: summary.to_owned(),
        producer: None,
    }
}

fn project_edge_id(label: EdgeLabel, source: &str, target: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in ["project", "edge", label.as_str(), source, target] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "project:v{PROJECT_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

const USER_CONTEXT_PROMOTION_EVIDENCE_THRESHOLD_N: usize = 3;
const USER_CONTEXT_PROMOTION_EVIDENCE_THRESHOLD_K: usize = 2;

const USER_CONTEXT_NODE_KINDS: &[NodeKind] = &[
    NodeKind::PromoteCandidate,
    NodeKind::PromotionPrompt,
    NodeKind::PromotionDecision,
    NodeKind::Preference,
    NodeKind::WorkflowRule,
    NodeKind::NamingDecision,
    NodeKind::Constraint,
];

const USER_CONTEXT_DURABLE_NODE_KINDS: &[NodeKind] = &[
    NodeKind::Preference,
    NodeKind::WorkflowRule,
    NodeKind::NamingDecision,
    NodeKind::Constraint,
];

const USER_CONTEXT_PROPOSED_BY_TARGET_KINDS: &[NodeKind] = &[
    NodeKind::Observation,
    NodeKind::AgentTurn,
    NodeKind::Decision,
];

const USER_CONTEXT_CONTRADICTS_TARGET_KINDS: &[NodeKind] =
    &[NodeKind::Preference, NodeKind::WorkflowRule];

const USER_CONTEXT_NAMING_ENTITY_KINDS: &[&str] = &[
    "crate", "module", "type", "function", "field", "feature", "other",
];

const USER_CONTEXT_WORKFLOW_TRIGGERS: &[&str] = &[
    "pre_commit",
    "pre_pr",
    "pre_merge",
    "pre_command",
    "post_command",
];

const USER_CONTEXT_LIFECYCLE_PHASES: &[&str] =
    &["pre_commit", "pre_pr", "pre_merge", "runtime", "any"];

const USER_CONTEXT_EDGE_LABELS: &[EdgeLabel] = &[
    EdgeLabel::ProposedBy,
    EdgeLabel::PromptedFor,
    EdgeLabel::DecidedOn,
    EdgeLabel::MaterializedAs,
    EdgeLabel::RevokedBy,
    EdgeLabel::Contradicts,
    EdgeLabel::ScopedToRepo,
];

const USER_CONTEXT_ONLY_EDGE_LABELS: &[EdgeLabel] = &[
    EdgeLabel::ProposedBy,
    EdgeLabel::PromptedFor,
    EdgeLabel::DecidedOn,
    EdgeLabel::MaterializedAs,
    EdgeLabel::RevokedBy,
    EdgeLabel::ScopedToRepo,
];

#[allow(clippy::too_many_lines)]
fn validate_user_context_domain_records(
    records: &[GraphRecord],
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
) -> WriteResult<Vec<GraphRecord>> {
    let sink_guard = sink
        .read()
        .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
    let mut synthesized_edges = Vec::new();
    for record in records {
        match record {
            GraphRecord::Node {
                id,
                kind,
                schema_version,
                domain,
                confidence,
                superseded_by,
                evidence_quality,
                valid_time,
                valid_time_source,
                user_context,
                ..
            } => {
                let is_user_context = id.starts_with("user_context:v1:")
                    || domain.as_deref() == Some("user_context")
                    || USER_CONTEXT_NODE_KINDS.contains(kind);
                if !is_user_context {
                    if !user_context.is_empty() {
                        return Err(ApiError::bad_request(format!(
                            "non-user-context node '{id}' must not carry user-context fields"
                        )));
                    }
                    continue;
                }
                validate_user_context_node_base(
                    id,
                    *kind,
                    *schema_version,
                    domain.as_deref(),
                    valid_time.as_deref(),
                    valid_time_source.as_deref(),
                )?;
                match kind {
                    NodeKind::PromoteCandidate => {
                        synthesized_edges.extend(validate_promote_candidate(
                            id,
                            confidence.as_deref(),
                            superseded_by.as_deref(),
                            evidence_quality.as_deref(),
                            user_context,
                            records,
                            &sink_guard,
                        )?);
                    }
                    NodeKind::PromotionPrompt => {
                        validate_promotion_prompt(id, user_context, records, &sink_guard)?;
                        synthesized_edges.push(user_context_edge(
                            EdgeLabel::PromptedFor,
                            id,
                            required_str(
                                user_context.candidate_id.as_deref(),
                                "PromotionPrompt.candidate_id",
                            )?,
                            None,
                            "PromotionPrompt prompted for PromoteCandidate",
                        ));
                    }
                    NodeKind::PromotionDecision => {
                        validate_promotion_decision(id, user_context, records, &sink_guard)?;
                        let candidate_id = required_str(
                            user_context.candidate_id.as_deref(),
                            "PromotionDecision.candidate_id",
                        )?;
                        synthesized_edges.push(user_context_edge(
                            EdgeLabel::DecidedOn,
                            id,
                            candidate_id,
                            None,
                            "PromotionDecision decided on PromoteCandidate",
                        ));
                        if matches!(
                            user_context.outcome.as_deref(),
                            Some("approved" | "edited_then_approved")
                        ) {
                            let materialized_id = required_str(
                                user_context.materialized_record_id.as_deref(),
                                "PromotionDecision.materialized_record_id",
                            )?;
                            if promotion_decision_uses_revocation_candidate(
                                user_context,
                                records,
                                &sink_guard,
                            )? {
                                synthesized_edges.push(user_context_edge(
                                    EdgeLabel::RevokedBy,
                                    materialized_id,
                                    id,
                                    None,
                                    "PromotionDecision revoked durable user-context record",
                                ));
                            } else {
                                synthesized_edges.push(user_context_edge(
                                    EdgeLabel::MaterializedAs,
                                    id,
                                    materialized_id,
                                    None,
                                    "PromotionDecision materialized durable user-context record",
                                ));
                            }
                        }
                    }
                    NodeKind::Preference
                    | NodeKind::WorkflowRule
                    | NodeKind::NamingDecision
                    | NodeKind::Constraint => {
                        validate_durable_user_context(
                            id,
                            *kind,
                            user_context,
                            records,
                            &sink_guard,
                        )?;
                    }
                    _ => {}
                }
            }
            GraphRecord::Edge {
                id,
                schema_version,
                label,
                source,
                target,
                confidence,
                ..
            } if id.starts_with("user_context:v1:") => {
                validate_user_context_edge(
                    id,
                    *schema_version,
                    *label,
                    source,
                    target,
                    confidence.as_deref(),
                    records,
                    &sink_guard,
                )?;
            }
            _ => {}
        }
    }
    Ok(synthesized_edges)
}

fn validate_user_context_node_base(
    id: &str,
    kind: NodeKind,
    schema_version: u32,
    domain: Option<&str>,
    valid_time: Option<&str>,
    valid_time_source: Option<&str>,
) -> WriteResult<()> {
    if !id.starts_with("user_context:v1:") {
        return Err(ApiError::bad_request(format!(
            "user-context node '{id}' must use a user_context:v1: ID"
        )));
    }
    match domain {
        Some("user_context") => {}
        Some(domain) => {
            return Err(ApiError::bad_request(format!(
                "user-context node '{id}' must carry domain 'user_context', got '{domain}'"
            )));
        }
        None => return Err(ApiError::missing_field("domain")),
    }
    if schema_version != USER_CONTEXT_SCHEMA_VERSION {
        return Err(ApiError::bad_request(format!(
            "user-context node '{id}' has schema_version {schema_version} but only version {USER_CONTEXT_SCHEMA_VERSION} is accepted"
        )));
    }
    if !USER_CONTEXT_NODE_KINDS.contains(&kind) {
        return Err(ApiError::bad_request(format!(
            "node kind '{}' is not permitted under the user_context domain",
            kind.as_str()
        )));
    }
    validate_rfc3339_field("valid_time", required_str(valid_time, "valid_time")?)?;
    required_str(valid_time_source, "valid_time_source")?;
    Ok(())
}

fn validate_promote_candidate(
    id: &str,
    confidence: Option<&str>,
    superseded_by: Option<&str>,
    evidence_quality: Option<&str>,
    user_context: &UserContextFields,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<Vec<GraphRecord>> {
    required_str(
        user_context.proposed_rule_text.as_deref(),
        "PromoteCandidate.proposed_rule_text",
    )?;
    validate_enum(
        "PromoteCandidate.proposed_rule_kind",
        required_str(
            user_context.proposed_rule_kind.as_deref(),
            "PromoteCandidate.proposed_rule_kind",
        )?,
        &[
            "preference",
            "workflow_rule",
            "naming_decision",
            "constraint",
            "revocation",
        ],
    )?;
    validate_numeric_confidence("PromoteCandidate.confidence", confidence)?;
    validate_enum(
        "PromoteCandidate.evidence_quality",
        required_str(evidence_quality, "PromoteCandidate.evidence_quality")?,
        &["verbatim", "summarized", "referenced_only"],
    )?;
    require_scope(user_context.scope.as_ref(), "PromoteCandidate.scope")?;
    let supporting = user_context
        .supporting_evidence
        .as_deref()
        .ok_or_else(|| ApiError::missing_field("PromoteCandidate.supporting_evidence"))?;
    let mut sessions = BTreeSet::new();
    let mut unique_supporting_targets = BTreeSet::new();
    let mut edges = Vec::new();
    for link in supporting {
        validate_supporting_evidence_link(id, link, records, sink, &mut sessions)?;
        let target = link
            .target_record_id
            .as_deref()
            .expect("validated support link should have target");
        if unique_supporting_targets.insert(target) {
            edges.push(user_context_edge(
                EdgeLabel::ProposedBy,
                id,
                target,
                Some(link.confidence.clone()),
                "PromoteCandidate proposed by Observation",
            ));
        }
    }
    if unique_supporting_targets.len() < USER_CONTEXT_PROMOTION_EVIDENCE_THRESHOLD_N {
        return Err(ApiError::new(
            ErrorCode::InsufficientPromotionEvidence,
            format!(
                "PromoteCandidate '{id}' has {} unique supporting observations; at least {} are required",
                unique_supporting_targets.len(),
                USER_CONTEXT_PROMOTION_EVIDENCE_THRESHOLD_N
            ),
        ));
    }
    if sessions.len() < USER_CONTEXT_PROMOTION_EVIDENCE_THRESHOLD_K {
        return Err(ApiError::new(
            ErrorCode::InsufficientPromotionEvidence,
            format!(
                "PromoteCandidate '{id}' has evidence from {} distinct sessions; at least {} are required",
                sessions.len(),
                USER_CONTEXT_PROMOTION_EVIDENCE_THRESHOLD_K
            ),
        ));
    }
    let contradicting = user_context
        .contradicting_evidence
        .as_deref()
        .ok_or_else(|| ApiError::missing_field("PromoteCandidate.contradicting_evidence"))?;
    for link in contradicting {
        let target_id = validate_contradicting_evidence_link(id, link, records, sink)?;
        edges.push(user_context_edge(
            EdgeLabel::Contradicts,
            id,
            &target_id,
            Some(link.confidence.clone()),
            "PromoteCandidate contradicts durable user-context record",
        ));
    }
    if let Some(rejected_id) = superseded_by {
        validate_superseded_rejection(id, rejected_id, records, sink)?;
        eprintln!(
            "{{\"level\":\"WARN\",\"code\":\"promotion_rejection_debounce\",\"candidate_id\":\"{id}\",\"superseded_by\":\"{rejected_id}\"}}"
        );
    }
    Ok(edges)
}

fn require_scope(scope: Option<&UserContextScope>, field: &'static str) -> WriteResult<()> {
    let scope = scope.ok_or_else(|| ApiError::missing_field(field))?;
    if let Some(lifecycle_phase) = scope.lifecycle_phase.as_deref() {
        validate_enum(
            "scope.lifecycle_phase",
            lifecycle_phase,
            USER_CONTEXT_LIFECYCLE_PHASES,
        )?;
    }
    Ok(())
}

fn validate_supporting_evidence_link(
    candidate_id: &str,
    link: &EvidenceLink,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
    sessions: &mut BTreeSet<String>,
) -> WriteResult<()> {
    if link.target_domain != "agent_memory" || link.relation != EdgeLabel::ProposedBy.as_str() {
        return Err(ApiError::bad_request(format!(
            "PromoteCandidate '{candidate_id}' supporting_evidence must use target_domain 'agent_memory' and relation PROPOSED_BY"
        )));
    }
    validate_numeric_confidence(
        "PromoteCandidate.supporting_evidence[].confidence",
        Some(&link.confidence),
    )?;
    let target_id = link.target_record_id.as_deref().ok_or_else(|| {
        ApiError::missing_field("PromoteCandidate.supporting_evidence[].target_record_id")
    })?;
    let record = lookup_record(target_id, records, sink)?.ok_or_else(|| {
        ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("supporting evidence target '{target_id}' not found"),
        )
    })?;
    let GraphRecord::Node {
        kind: NodeKind::Observation | NodeKind::AgentTurn | NodeKind::Decision,
        session_id,
        ..
    } = record
    else {
        return Err(ApiError::bad_request(format!(
            "supporting evidence target '{target_id}' must be an Observation, AgentTurn, or Decision"
        )));
    };
    sessions
        .insert(required_str(session_id.as_deref(), "supporting_evidence.session_id")?.to_owned());
    Ok(())
}

fn validate_contradicting_evidence_link(
    candidate_id: &str,
    link: &EvidenceLink,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<String> {
    if link.target_domain != "user_context" || link.relation != EdgeLabel::Contradicts.as_str() {
        return Err(ApiError::bad_request(format!(
            "PromoteCandidate.contradicting_evidence for '{candidate_id}' must use \
             target_domain 'user_context' and relation CONTRADICTS"
        )));
    }
    validate_numeric_confidence(
        "PromoteCandidate.contradicting_evidence[].confidence",
        Some(&link.confidence),
    )?;
    let target_id = link.target_record_id.as_deref().ok_or_else(|| {
        ApiError::missing_field("PromoteCandidate.contradicting_evidence[].target_record_id")
    })?;
    match lookup_node_kind(target_id, records, sink)? {
        Some(kind) if USER_CONTEXT_CONTRADICTS_TARGET_KINDS.contains(&kind) => {
            Ok(target_id.to_owned())
        }
        Some(kind) => Err(ApiError::bad_request(format!(
            "PromoteCandidate.contradicting_evidence target '{target_id}' must be a Preference \
             or WorkflowRule, got {}",
            kind.as_str()
        ))),
        None => Err(ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("evidence target '{target_id}' not found"),
        )),
    }
}

fn validate_promotion_prompt(
    id: &str,
    user_context: &UserContextFields,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let candidate_id = required_str(
        user_context.candidate_id.as_deref(),
        "PromotionPrompt.candidate_id",
    )?;
    require_kind(candidate_id, NodeKind::PromoteCandidate, records, sink)?;
    validate_enum(
        "PromotionPrompt.prompt_surface",
        required_str(
            user_context.prompt_surface.as_deref(),
            "PromotionPrompt.prompt_surface",
        )?,
        &["cli", "mcp", "web", "other"],
    )?;
    required_str(
        user_context.prompt_text.as_deref(),
        "PromotionPrompt.prompt_text",
    )?;
    validate_rfc3339_field(
        "PromotionPrompt.prompted_at",
        required_str(
            user_context.prompted_at.as_deref(),
            "PromotionPrompt.prompted_at",
        )?,
    )?;
    required_str(
        user_context.prompted_to.as_deref(),
        "PromotionPrompt.prompted_to",
    )?;
    if let Some(expires_at) = user_context.expires_at.as_deref() {
        validate_rfc3339_field("PromotionPrompt.expires_at", expires_at)?;
    }
    if id == candidate_id {
        return Err(ApiError::bad_request(
            "PromotionPrompt.candidate_id must not point to itself",
        ));
    }
    Ok(())
}

fn validate_promotion_decision(
    id: &str,
    user_context: &UserContextFields,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let candidate_id = required_str(
        user_context.candidate_id.as_deref(),
        "PromotionDecision.candidate_id",
    )?;
    require_kind(candidate_id, NodeKind::PromoteCandidate, records, sink)?;
    let prompt_id = required_str(
        user_context.prompt_id.as_deref(),
        "PromotionDecision.prompt_id",
    )?;
    require_prompt_for_candidate(prompt_id, candidate_id, records, sink)?;
    let outcome = required_str(user_context.outcome.as_deref(), "PromotionDecision.outcome")?;
    validate_enum(
        "PromotionDecision.outcome",
        outcome,
        &[
            "approved",
            "rejected",
            "deferred",
            "expired",
            "edited_then_approved",
        ],
    )?;
    if outcome == "edited_then_approved"
        && promotion_decision_uses_revocation_candidate(user_context, records, sink)?
    {
        return Err(ApiError::bad_request(
            "PromotionDecision.outcome edited_then_approved is not valid for revocation \
             candidates; use approved",
        ));
    }
    validate_rfc3339_field(
        "PromotionDecision.decided_at",
        required_str(
            user_context.decided_at.as_deref(),
            "PromotionDecision.decided_at",
        )?,
    )?;
    required_str(
        user_context.decided_by.as_deref(),
        "PromotionDecision.decided_by",
    )?;
    let materialized_record_id = match outcome {
        "approved" => Some(required_str(
            user_context.materialized_record_id.as_deref(),
            "PromotionDecision.materialized_record_id",
        )?),
        "edited_then_approved" => {
            let materialized_record_id = required_str(
                user_context.materialized_record_id.as_deref(),
                "PromotionDecision.materialized_record_id",
            )?;
            required_str(
                user_context.edited_rule_text.as_deref(),
                "PromotionDecision.edited_rule_text",
            )?;
            Some(materialized_record_id)
        }
        _ if user_context.materialized_record_id.is_some() => {
            return Err(ApiError::bad_request(format!(
                "PromotionDecision.materialized_record_id must be null when outcome is {outcome}"
            )));
        }
        _ => None,
    };
    if let Some(materialized_record_id) = materialized_record_id {
        require_kind_in(
            "PromotionDecision.materialized_record_id",
            materialized_record_id,
            USER_CONTEXT_DURABLE_NODE_KINDS,
            records,
            sink,
        )?;
    }
    if id == candidate_id || id == prompt_id {
        return Err(ApiError::bad_request(
            "PromotionDecision references must not point to itself",
        ));
    }
    Ok(())
}

fn require_prompt_for_candidate(
    prompt_id: &str,
    candidate_id: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let prompt = lookup_record(prompt_id, records, sink)?.ok_or_else(|| {
        ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("record '{prompt_id}' not found"),
        )
    })?;
    match prompt {
        GraphRecord::Node {
            kind: NodeKind::PromotionPrompt,
            user_context,
            ..
        } => {
            let prompt_candidate_id = required_str(
                user_context.candidate_id.as_deref(),
                "PromotionPrompt.candidate_id",
            )?;
            if prompt_candidate_id != candidate_id {
                return Err(ApiError::bad_request(format!(
                    "PromotionDecision.prompt_id '{prompt_id}' was issued for candidate \
                     '{prompt_candidate_id}', not decided candidate '{candidate_id}'"
                )));
            }
            Ok(())
        }
        GraphRecord::Node { kind, .. } => Err(ApiError::bad_request(format!(
            "record '{prompt_id}' must be {}, got {}",
            NodeKind::PromotionPrompt.as_str(),
            kind.as_str()
        ))),
        _ => Err(ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("record '{prompt_id}' not found"),
        )),
    }
}

fn validate_durable_user_context(
    id: &str,
    kind: NodeKind,
    user_context: &UserContextFields,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let approval_id = user_context
        .approval_decision_id
        .as_deref()
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::UnapprovedDurableUserContext,
                format!("{kind:?} '{id}' lacks approval_decision_id"),
            )
        })?;
    let decision = lookup_record(approval_id, records, sink)?.ok_or_else(|| {
        ApiError::new(
            ErrorCode::UnapprovedDurableUserContext,
            format!(
                "approval decision '{approval_id}' not found for durable user-context record '{id}'"
            ),
        )
    })?;
    let GraphRecord::Node {
        kind: NodeKind::PromotionDecision,
        user_context: decision_fields,
        ..
    } = decision
    else {
        return Err(ApiError::new(
            ErrorCode::UnapprovedDurableUserContext,
            format!("approval_decision_id '{approval_id}' does not reference a PromotionDecision"),
        ));
    };
    if !matches!(
        decision_fields.outcome.as_deref(),
        Some("approved" | "edited_then_approved")
    ) || decision_fields.materialized_record_id.as_deref() != Some(id)
    {
        return Err(ApiError::new(
            ErrorCode::UnapprovedDurableUserContext,
            format!("PromotionDecision '{approval_id}' does not approve durable record '{id}'"),
        ));
    }
    require_scope(user_context.scope.as_ref(), "durable.scope")?;
    match kind {
        NodeKind::Preference | NodeKind::WorkflowRule => {
            require_durable_rule_kind(kind, user_context)?;
            required_str(user_context.rule_text.as_deref(), "durable.rule_text")?;
            if kind == NodeKind::WorkflowRule {
                require_workflow_rule_fields(user_context)?;
            }
        }
        NodeKind::NamingDecision => {
            validate_enum(
                "NamingDecision.entity_kind",
                required_str(
                    user_context.entity_kind.as_deref(),
                    "NamingDecision.entity_kind",
                )?,
                USER_CONTEXT_NAMING_ENTITY_KINDS,
            )?;
            required_str(
                user_context.canonical_name.as_deref(),
                "NamingDecision.canonical_name",
            )?;
            user_context
                .alternatives_rejected
                .as_ref()
                .ok_or_else(|| ApiError::missing_field("NamingDecision.alternatives_rejected"))?;
        }
        NodeKind::Constraint => {
            required_str(
                user_context.constraint_text.as_deref(),
                "Constraint.constraint_text",
            )?;
            validate_enum(
                "Constraint.enforcement_level",
                required_str(
                    user_context.enforcement_level.as_deref(),
                    "Constraint.enforcement_level",
                )?,
                &["advisory", "blocking"],
            )?;
        }
        _ => {}
    }
    validate_durable_approval_body(
        id,
        kind,
        user_context,
        approval_id,
        &decision_fields,
        records,
        sink,
    )?;
    validate_durable_active_from(user_context, approval_id, &decision_fields)?;
    if let Some(active_to) = user_context.active_to.as_deref() {
        validate_rfc3339_field("durable.active_to", active_to)?;
    }
    Ok(())
}

fn validate_durable_approval_body(
    id: &str,
    kind: NodeKind,
    user_context: &UserContextFields,
    approval_id: &str,
    decision_fields: &UserContextFields,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let outcome = decision_fields.outcome.as_deref();
    let approval_candidate = if matches!(outcome, Some("approved" | "edited_then_approved")) {
        let candidate_id = required_str(
            decision_fields.candidate_id.as_deref(),
            "PromotionDecision.candidate_id",
        )?;
        let candidate = lookup_record(candidate_id, records, sink)?.ok_or_else(|| {
            ApiError::new(
                ErrorCode::UnresolvedEvidenceTarget,
                format!("approval candidate '{candidate_id}' not found"),
            )
        })?;
        let GraphRecord::Node {
            kind: NodeKind::PromoteCandidate,
            user_context: candidate_fields,
            ..
        } = &candidate
        else {
            return Err(ApiError::bad_request(format!(
                "PromotionDecision.candidate_id '{candidate_id}' does not reference a \
                 PromoteCandidate"
            )));
        };
        if candidate_fields.proposed_rule_kind.as_deref() == Some("revocation") {
            let active_to = required_str(user_context.active_to.as_deref(), "durable.active_to")?;
            validate_rfc3339_field("durable.active_to", active_to)?;
            return Ok(());
        }
        require_candidate_rule_kind_matches_durable(kind, candidate_fields)?;
        Some(candidate)
    } else {
        None
    };

    let (expected_field, expected_body) = match outcome {
        Some("approved") => {
            let GraphRecord::Node {
                kind: NodeKind::PromoteCandidate,
                user_context: candidate_fields,
                ..
            } = approval_candidate
                .as_ref()
                .expect("approved outcome should load candidate above")
            else {
                unreachable!("approved outcome candidate kind was checked above");
            };
            (
                "PromoteCandidate.proposed_rule_text",
                required_str(
                    candidate_fields.proposed_rule_text.as_deref(),
                    "PromoteCandidate.proposed_rule_text",
                )?
                .to_owned(),
            )
        }
        Some("edited_then_approved") => (
            "PromotionDecision.edited_rule_text",
            required_str(
                decision_fields.edited_rule_text.as_deref(),
                "PromotionDecision.edited_rule_text",
            )?
            .to_owned(),
        ),
        _ => return Ok(()),
    };
    let (field, durable_body) = match kind {
        NodeKind::Preference | NodeKind::WorkflowRule => (
            "durable.rule_text",
            required_str(user_context.rule_text.as_deref(), "durable.rule_text")?,
        ),
        NodeKind::NamingDecision => (
            "NamingDecision.canonical_name",
            required_str(
                user_context.canonical_name.as_deref(),
                "NamingDecision.canonical_name",
            )?,
        ),
        NodeKind::Constraint => (
            "Constraint.constraint_text",
            required_str(
                user_context.constraint_text.as_deref(),
                "Constraint.constraint_text",
            )?,
        ),
        _ => return Ok(()),
    };

    if durable_body != expected_body.as_str() {
        return Err(ApiError::bad_request(format!(
            "{field} for durable record '{id}' must match {expected_field} from approval \
             decision '{approval_id}'"
        )));
    }

    Ok(())
}

fn validate_durable_active_from(
    user_context: &UserContextFields,
    approval_id: &str,
    decision_fields: &UserContextFields,
) -> WriteResult<()> {
    let active_from = required_str(user_context.active_from.as_deref(), "durable.active_from")?;
    let active_from = parse_rfc3339_field("durable.active_from", active_from)?;
    let decided_at = required_str(
        decision_fields.decided_at.as_deref(),
        "PromotionDecision.decided_at",
    )?;
    let decided_at = parse_rfc3339_field("PromotionDecision.decided_at", decided_at)?;
    if active_from != decided_at {
        return Err(ApiError::bad_request(format!(
            "durable.active_from must equal PromotionDecision.decided_at from approval decision \
             '{approval_id}'"
        )));
    }
    Ok(())
}

fn require_workflow_rule_fields(user_context: &UserContextFields) -> WriteResult<()> {
    let triggers = user_context
        .triggers
        .as_ref()
        .filter(|triggers| !triggers.is_empty())
        .ok_or_else(|| ApiError::missing_field("WorkflowRule.triggers"))?;
    if triggers.iter().any(String::is_empty) {
        return Err(ApiError::bad_request(
            "WorkflowRule.triggers entries must not be empty",
        ));
    }
    for trigger in triggers {
        validate_enum(
            "WorkflowRule.triggers",
            trigger,
            USER_CONTEXT_WORKFLOW_TRIGGERS,
        )?;
    }
    required_str(
        user_context.action_summary.as_deref(),
        "WorkflowRule.action_summary",
    )?;
    Ok(())
}

fn require_durable_rule_kind(kind: NodeKind, user_context: &UserContextFields) -> WriteResult<()> {
    let Some(expected) = proposed_rule_kind_for_durable(kind) else {
        return Ok(());
    };
    let actual = required_str(
        user_context.proposed_rule_kind.as_deref(),
        "durable.proposed_rule_kind",
    )?;
    if actual != expected {
        return Err(ApiError::bad_request(format!(
            "durable.proposed_rule_kind must be '{expected}' for {}, got '{actual}'",
            kind.as_str()
        )));
    }
    Ok(())
}

fn require_candidate_rule_kind_matches_durable(
    durable_kind: NodeKind,
    candidate_fields: &UserContextFields,
) -> WriteResult<()> {
    let Some(expected) = proposed_rule_kind_for_durable(durable_kind) else {
        return Ok(());
    };
    let actual = required_str(
        candidate_fields.proposed_rule_kind.as_deref(),
        "PromoteCandidate.proposed_rule_kind",
    )?;
    if actual != expected {
        return Err(ApiError::bad_request(format!(
            "PromoteCandidate.proposed_rule_kind must be '{expected}' when materializing {}, got '{actual}'",
            durable_kind.as_str()
        )));
    }
    Ok(())
}

const fn proposed_rule_kind_for_durable(kind: NodeKind) -> Option<&'static str> {
    match kind {
        NodeKind::Preference => Some("preference"),
        NodeKind::WorkflowRule => Some("workflow_rule"),
        NodeKind::NamingDecision => Some("naming_decision"),
        NodeKind::Constraint => Some("constraint"),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_user_context_edge(
    edge_id: &str,
    schema_version: u32,
    label: EdgeLabel,
    source: &str,
    target: &str,
    confidence: Option<&str>,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    if schema_version != USER_CONTEXT_SCHEMA_VERSION {
        return Err(ApiError::bad_request(format!(
            "user-context edge '{edge_id}' has schema_version {schema_version} but only version {USER_CONTEXT_SCHEMA_VERSION} is accepted"
        )));
    }
    if !USER_CONTEXT_EDGE_LABELS.contains(&label) {
        return Err(ApiError::bad_request(format!(
            "user-context edge '{edge_id}' uses unsupported label '{}'",
            label.as_str()
        )));
    }
    if matches!(label, EdgeLabel::ProposedBy | EdgeLabel::Contradicts) {
        validate_numeric_confidence("user-context edge confidence", confidence)?;
    }
    let source_kind = lookup_node_kind(source, records, sink)?;
    let target_kind = lookup_node_kind(target, records, sink)?;
    match label {
        EdgeLabel::ProposedBy => require_edge_kinds(
            edge_id,
            label,
            source_kind,
            target_kind,
            &[NodeKind::PromoteCandidate],
            USER_CONTEXT_PROPOSED_BY_TARGET_KINDS,
        ),
        EdgeLabel::PromptedFor => {
            require_edge_kinds(
                edge_id,
                label,
                source_kind,
                target_kind,
                &[NodeKind::PromotionPrompt],
                &[NodeKind::PromoteCandidate],
            )?;
            validate_prompted_for_edge_payload(edge_id, source, target, records, sink)
        }
        EdgeLabel::DecidedOn => {
            require_edge_kinds(
                edge_id,
                label,
                source_kind,
                target_kind,
                &[NodeKind::PromotionDecision],
                &[NodeKind::PromoteCandidate],
            )?;
            validate_decided_on_edge_payload(edge_id, source, target, records, sink)
        }
        EdgeLabel::MaterializedAs => {
            require_edge_kinds(
                edge_id,
                label,
                source_kind,
                target_kind,
                &[NodeKind::PromotionDecision],
                USER_CONTEXT_DURABLE_NODE_KINDS,
            )?;
            validate_materialized_as_edge_payload(edge_id, source, target, records, sink)
        }
        EdgeLabel::RevokedBy => {
            require_edge_kinds(
                edge_id,
                label,
                source_kind,
                target_kind,
                USER_CONTEXT_DURABLE_NODE_KINDS,
                &[NodeKind::PromotionDecision],
            )?;
            validate_revoked_by_edge_payload(edge_id, source, target, records, sink)
        }
        EdgeLabel::Contradicts => require_edge_kinds(
            edge_id,
            label,
            source_kind,
            target_kind,
            &[NodeKind::PromoteCandidate],
            USER_CONTEXT_CONTRADICTS_TARGET_KINDS,
        ),
        EdgeLabel::ScopedToRepo => {
            require_edge_kinds(
                edge_id,
                label,
                source_kind,
                target_kind,
                USER_CONTEXT_DURABLE_NODE_KINDS,
                &[NodeKind::Repository],
            )?;
            validate_scoped_to_repo_edge_payload(edge_id, source, target, records, sink)
        }
        _ => Ok(()),
    }
}

fn validate_prompted_for_edge_payload(
    edge_id: &str,
    source_prompt_id: &str,
    target_candidate_id: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let prompt_fields = lookup_user_context_fields_for_kind(
        source_prompt_id,
        NodeKind::PromotionPrompt,
        records,
        sink,
        "PROMPTED_FOR source",
    )?;
    let candidate_id = required_str(
        prompt_fields.candidate_id.as_deref(),
        "PromotionPrompt.candidate_id",
    )?;
    if candidate_id != target_candidate_id {
        return Err(ApiError::bad_request(format!(
            "PROMPTED_FOR edge '{edge_id}' target '{target_candidate_id}' must equal \
             PromotionPrompt.candidate_id '{candidate_id}'"
        )));
    }
    Ok(())
}

fn validate_decided_on_edge_payload(
    edge_id: &str,
    source_decision_id: &str,
    target_candidate_id: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let decision_fields = lookup_user_context_fields_for_kind(
        source_decision_id,
        NodeKind::PromotionDecision,
        records,
        sink,
        "DECIDED_ON source",
    )?;
    let candidate_id = required_str(
        decision_fields.candidate_id.as_deref(),
        "PromotionDecision.candidate_id",
    )?;
    if candidate_id != target_candidate_id {
        return Err(ApiError::bad_request(format!(
            "DECIDED_ON edge '{edge_id}' target '{target_candidate_id}' must equal \
             PromotionDecision.candidate_id '{candidate_id}'"
        )));
    }
    Ok(())
}

fn validate_materialized_as_edge_payload(
    edge_id: &str,
    source_decision_id: &str,
    target_durable_id: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let decision_fields = lookup_user_context_fields_for_kind(
        source_decision_id,
        NodeKind::PromotionDecision,
        records,
        sink,
        "MATERIALIZED_AS source",
    )?;
    require_approved_decision_outcome(edge_id, EdgeLabel::MaterializedAs, &decision_fields)?;
    require_decision_materialized_target(
        edge_id,
        EdgeLabel::MaterializedAs,
        &decision_fields,
        target_durable_id,
    )?;
    if promotion_decision_uses_revocation_candidate(&decision_fields, records, sink)? {
        return Err(ApiError::bad_request(format!(
            "MATERIALIZED_AS edge '{edge_id}' cannot represent a revocation decision; use \
             REVOKED_BY from the durable record to the decision"
        )));
    }
    Ok(())
}

fn validate_revoked_by_edge_payload(
    edge_id: &str,
    source_durable_id: &str,
    target_decision_id: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let decision_fields = lookup_user_context_fields_for_kind(
        target_decision_id,
        NodeKind::PromotionDecision,
        records,
        sink,
        "REVOKED_BY target",
    )?;
    require_approved_decision_outcome(edge_id, EdgeLabel::RevokedBy, &decision_fields)?;
    require_decision_materialized_target(
        edge_id,
        EdgeLabel::RevokedBy,
        &decision_fields,
        source_durable_id,
    )?;
    if !promotion_decision_uses_revocation_candidate(&decision_fields, records, sink)? {
        return Err(ApiError::bad_request(format!(
            "REVOKED_BY edge '{edge_id}' requires PromotionDecision.candidate_id to reference a \
             revocation PromoteCandidate"
        )));
    }

    let (_, durable_fields) =
        lookup_user_context_node_fields(source_durable_id, records, sink, "REVOKED_BY source")?;
    let active_to = required_str(durable_fields.active_to.as_deref(), "durable.active_to")?;
    validate_rfc3339_field("durable.active_to", active_to)
}

fn validate_scoped_to_repo_edge_payload(
    edge_id: &str,
    source_durable_id: &str,
    target_repo_id: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let (_, durable_fields) =
        lookup_user_context_node_fields(source_durable_id, records, sink, "SCOPED_TO_REPO source")?;
    let scope = durable_fields
        .scope
        .as_ref()
        .ok_or_else(|| ApiError::missing_field("durable.scope"))?;
    let scoped_repo = required_str(scope.repo.as_deref(), "durable.scope.repo")?;
    if scoped_repo != target_repo_id {
        return Err(ApiError::bad_request(format!(
            "SCOPED_TO_REPO edge '{edge_id}' target '{target_repo_id}' must equal \
             durable scope.repo '{scoped_repo}'"
        )));
    }
    Ok(())
}

fn require_approved_decision_outcome(
    edge_id: &str,
    label: EdgeLabel,
    decision_fields: &UserContextFields,
) -> WriteResult<()> {
    if matches!(
        decision_fields.outcome.as_deref(),
        Some("approved" | "edited_then_approved")
    ) {
        return Ok(());
    }
    Err(ApiError::bad_request(format!(
        "{} edge '{edge_id}' requires PromotionDecision.outcome to be approved or \
         edited_then_approved",
        label.as_str()
    )))
}

fn require_decision_materialized_target(
    edge_id: &str,
    label: EdgeLabel,
    decision_fields: &UserContextFields,
    expected_target_id: &str,
) -> WriteResult<()> {
    let materialized_id = required_str(
        decision_fields.materialized_record_id.as_deref(),
        "PromotionDecision.materialized_record_id",
    )?;
    if materialized_id != expected_target_id {
        return Err(ApiError::bad_request(format!(
            "{} edge '{edge_id}' endpoint '{expected_target_id}' must equal \
             PromotionDecision.materialized_record_id '{materialized_id}'",
            label.as_str()
        )));
    }
    Ok(())
}

fn promotion_decision_uses_revocation_candidate(
    decision_fields: &UserContextFields,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<bool> {
    let candidate_fields = promotion_decision_candidate_fields(decision_fields, records, sink)?;
    let proposed_rule_kind = required_str(
        candidate_fields.proposed_rule_kind.as_deref(),
        "PromoteCandidate.proposed_rule_kind",
    )?;
    Ok(proposed_rule_kind == "revocation")
}

fn promotion_decision_candidate_fields(
    decision_fields: &UserContextFields,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<UserContextFields> {
    let candidate_id = required_str(
        decision_fields.candidate_id.as_deref(),
        "PromotionDecision.candidate_id",
    )?;
    lookup_user_context_fields_for_kind(
        candidate_id,
        NodeKind::PromoteCandidate,
        records,
        sink,
        "PromotionDecision.candidate_id",
    )
}

fn lookup_user_context_fields_for_kind(
    id: &str,
    expected_kind: NodeKind,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
    context: &str,
) -> WriteResult<UserContextFields> {
    let (actual_kind, fields) = lookup_user_context_node_fields(id, records, sink, context)?;
    if actual_kind != expected_kind {
        return Err(ApiError::bad_request(format!(
            "{context} '{id}' must reference a {}, got {}",
            expected_kind.as_str(),
            actual_kind.as_str()
        )));
    }
    Ok(fields)
}

fn lookup_user_context_node_fields(
    id: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
    context: &str,
) -> WriteResult<(NodeKind, UserContextFields)> {
    let record = lookup_record(id, records, sink)?.ok_or_else(|| {
        ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("{context} '{id}' does not resolve to a graph record"),
        )
    })?;
    let GraphRecord::Node {
        kind, user_context, ..
    } = record
    else {
        return Err(ApiError::bad_request(format!(
            "{context} '{id}' must reference a node record"
        )));
    };
    Ok((kind, user_context))
}

fn require_edge_kinds(
    edge_id: &str,
    label: EdgeLabel,
    source_kind: Option<NodeKind>,
    target_kind: Option<NodeKind>,
    allowed_sources: &[NodeKind],
    allowed_targets: &[NodeKind],
) -> WriteResult<()> {
    validate_project_edge_kinds(
        edge_id,
        label,
        source_kind,
        allowed_sources,
        target_kind,
        allowed_targets,
    )
}

fn validate_numeric_confidence(field: &'static str, confidence: Option<&str>) -> WriteResult<()> {
    if confidence
        .and_then(|s| s.parse::<f64>().ok())
        .is_some_and(|value| (0.0..=1.0).contains(&value))
    {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "{field} must be a numeric value in [0.0, 1.0]"
        )))
    }
}

fn validate_enum(field: &'static str, value: &str, allowed: &[&str]) -> WriteResult<()> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "{field} has unsupported value '{value}'"
        )))
    }
}

fn lookup_record(
    id: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<Option<GraphRecord>> {
    if let Some(record) = records.iter().find(|record| record.id() == id) {
        return Ok(Some(record.clone()));
    }
    sink.read_back(id)
        .map_err(|error| ApiError::internal(error.to_string()))
}

fn require_kind(
    id: &str,
    expected: NodeKind,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    match lookup_node_kind(id, records, sink)? {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(ApiError::bad_request(format!(
            "record '{id}' must be {}, got {}",
            expected.as_str(),
            actual.as_str()
        ))),
        None => Err(ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("record '{id}' not found"),
        )),
    }
}

fn require_kind_in(
    field: &'static str,
    id: &str,
    expected: &[NodeKind],
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    match lookup_node_kind(id, records, sink)? {
        Some(actual) if expected.contains(&actual) => Ok(()),
        Some(actual) => {
            let expected_kinds = expected
                .iter()
                .map(|kind| kind.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(ApiError::bad_request(format!(
                "{field} '{id}' must reference one of [{expected_kinds}], got {}",
                actual.as_str()
            )))
        }
        None => Err(ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("{field} target '{id}' not found"),
        )),
    }
}

fn validate_superseded_rejection(
    candidate_id: &str,
    rejected_candidate_id: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    require_kind(
        rejected_candidate_id,
        NodeKind::PromoteCandidate,
        records,
        sink,
    )?;
    let has_rejection = records.iter().any(|record| {
        matches!(
            record,
            GraphRecord::Node {
                kind: NodeKind::PromotionDecision,
                user_context,
                ..
            } if user_context.candidate_id.as_deref() == Some(rejected_candidate_id)
                && user_context.outcome.as_deref() == Some("rejected")
        )
    }) || sink
        .read_all_records()
        .map_err(|error| ApiError::internal(error.to_string()))?
        .iter()
        .any(|record| {
            matches!(
                record,
                GraphRecord::Node {
                    kind: NodeKind::PromotionDecision,
                    user_context,
                    ..
                } if user_context.candidate_id.as_deref() == Some(rejected_candidate_id)
                    && user_context.outcome.as_deref() == Some("rejected")
            )
        });
    if has_rejection {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "PromoteCandidate '{candidate_id}' superseded_by '{rejected_candidate_id}' must reference a rejected candidate"
        )))
    }
}

fn user_context_edge(
    label: EdgeLabel,
    source: &str,
    target: &str,
    confidence: Option<String>,
    summary: &str,
) -> GraphRecord {
    GraphRecord::Edge {
        id: user_context_stable_id(&["edge", label.as_str(), source, target]),
        schema_version: USER_CONTEXT_SCHEMA_VERSION,
        label,
        source: source.to_owned(),
        target: target.to_owned(),
        confidence,
        temporal: None,
        summary: summary.to_owned(),
        producer: None,
    }
}

fn validate_verification_output_handle(
    field: &'static str,
    handle: &OutputHandle,
) -> WriteResult<()> {
    if handle.hash.is_empty() {
        return Err(ApiError::bad_request(format!(
            "verification-domain {field}.hash must not be empty"
        )));
    }
    let inline_len = handle.inline.as_deref().map_or(0, |s| s.len() as u64);
    if inline_len > handle.bytes {
        return Err(ApiError::bad_request(format!(
            "verification-domain {field}.bytes must be >= inline payload length"
        )));
    }
    if inline_len > INLINE_PAYLOAD_CEILING
        || (handle.inline.is_some() && handle.bytes > INLINE_PAYLOAD_CEILING)
    {
        return Err(ApiError::inline_payload_exceeds_ceiling(format!(
            "verification-domain {field}.inline must be None when bytes exceeds the 16 KiB \
             ceiling; demote to handle-only before writing"
        )));
    }
    Ok(())
}

fn required_str<'a>(value: Option<&'a str>, field: &'static str) -> WriteResult<&'a str> {
    value
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::missing_field(field))
}

const TOOL_KIND_VALUES: &[&str] = &[
    "bash",
    "file_edit",
    "file_read",
    "search",
    "network_request",
    "code_execution",
    "other",
];
const TOOL_STATUS_VALUES: &[&str] = &["succeeded", "failed", "interrupted", "unknown"];
const FILE_EDIT_KIND_VALUES: &[&str] = &["create", "modify", "delete", "rename"];

fn validate_agent_action_record(
    record: &GraphRecord,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    let GraphRecord::Node {
        id,
        kind,
        domain,
        repo_relative_path,
        edit_kind,
        before_hash,
        after_hash,
        rename_to,
        hunk_count,
        source_artifact_path,
        source_artifact_hash,
        linked_patch_id,
        linked_turn_id,
        tool_name,
        tool_kind,
        arguments_summary,
        arguments_handle,
        result_handle,
        produced_evidence_id,
        started_at,
        finished_at,
        status,
        ..
    } = record
    else {
        return Ok(());
    };

    match kind {
        NodeKind::ToolCall => validate_tool_call_record(
            id,
            domain.as_deref(),
            source_artifact_path.as_deref(),
            source_artifact_hash.as_deref(),
            linked_turn_id.as_deref(),
            tool_name.as_deref(),
            tool_kind.as_deref(),
            arguments_summary.as_deref(),
            arguments_handle.as_deref(),
            result_handle.as_deref(),
            produced_evidence_id.as_deref(),
            started_at.as_deref(),
            finished_at.as_deref(),
            status.as_deref(),
            records,
            sink,
        ),
        NodeKind::FileEdit => validate_file_edit_record(
            id,
            domain.as_deref(),
            source_artifact_path.as_deref(),
            source_artifact_hash.as_deref(),
            repo_relative_path.as_deref(),
            edit_kind.as_deref(),
            before_hash.as_deref(),
            after_hash.as_deref(),
            rename_to.as_deref(),
            *hunk_count,
            linked_patch_id.as_deref(),
            linked_turn_id.as_deref(),
            records,
            sink,
        ),
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_tool_call_record(
    id: &str,
    domain: Option<&str>,
    source_artifact_path: Option<&str>,
    source_artifact_hash: Option<&str>,
    linked_turn_id: Option<&str>,
    tool_name: Option<&str>,
    tool_kind: Option<&str>,
    arguments_summary: Option<&str>,
    arguments_handle: Option<&OutputHandle>,
    result_handle: Option<&OutputHandle>,
    produced_evidence_id: Option<&str>,
    started_at: Option<&str>,
    finished_at: Option<&str>,
    status: Option<&str>,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    validate_agent_action_domain("ToolCall", domain)?;
    validate_agent_action_source_artifact("ToolCall", source_artifact_path, source_artifact_hash)?;
    required_str(tool_name, "tool_name (required for ToolCall nodes)")?;
    let tool_kind = required_str(tool_kind, "tool_kind (required for ToolCall nodes)")?;
    if !TOOL_KIND_VALUES.contains(&tool_kind) {
        return Err(ApiError::bad_request(format!(
            "ToolCall.tool_kind '{tool_kind}' is not recognized; expected one of: {}",
            TOOL_KIND_VALUES.join(", ")
        )));
    }
    required_str(
        arguments_summary,
        "arguments_summary (required for ToolCall nodes)",
    )?;
    let arguments_handle = arguments_handle
        .ok_or_else(|| ApiError::missing_field("arguments_handle (required for ToolCall nodes)"))?;
    validate_agent_action_output_handle("ToolCall.arguments_handle", arguments_handle)?;
    if let Some(result_handle) = result_handle {
        validate_agent_action_output_handle("ToolCall.result_handle", result_handle)?;
    }
    if let Some(produced_evidence_id) = produced_evidence_id {
        let produced_evidence_id =
            required_str(Some(produced_evidence_id), "produced_evidence_id")?;
        validate_verification_evidence_ref(
            "ToolCall.produced_evidence_id",
            produced_evidence_id,
            records,
            sink,
        )?;
    }
    let linked_turn_id = required_str(
        linked_turn_id,
        "linked_turn_id (required for ToolCall nodes)",
    )?;
    validate_agent_turn_ref("ToolCall.linked_turn_id", linked_turn_id, records, sink)?;
    let started_at = required_str(started_at, "started_at (required for ToolCall nodes)")?;
    if DateTime::parse_from_rfc3339(started_at).is_err() {
        return Err(ApiError::bad_request(format!(
            "ToolCall.started_at '{started_at}' is not a valid RFC 3339 timestamp"
        )));
    }
    let status = required_str(status, "status (required for ToolCall nodes)")?;
    if !TOOL_STATUS_VALUES.contains(&status) {
        return Err(ApiError::bad_request(format!(
            "ToolCall.status '{status}' is not recognized; expected one of: {}",
            TOOL_STATUS_VALUES.join(", ")
        )));
    }
    validate_tool_call_finished_at(status, finished_at)?;
    if id.is_empty() {
        return Err(ApiError::missing_field("id"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_file_edit_record(
    id: &str,
    domain: Option<&str>,
    source_artifact_path: Option<&str>,
    source_artifact_hash: Option<&str>,
    repo_relative_path: Option<&str>,
    edit_kind: Option<&str>,
    before_hash: Option<&str>,
    after_hash: Option<&str>,
    rename_to: Option<&str>,
    hunk_count: Option<u32>,
    linked_patch_id: Option<&str>,
    linked_turn_id: Option<&str>,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    validate_agent_action_domain("FileEdit", domain)?;
    validate_agent_action_source_artifact("FileEdit", source_artifact_path, source_artifact_hash)?;
    required_str(
        repo_relative_path,
        "repo_relative_path (required for FileEdit nodes)",
    )?;
    let edit_kind = required_str(edit_kind, "edit_kind (required for FileEdit nodes)")?;
    if !FILE_EDIT_KIND_VALUES.contains(&edit_kind) {
        return Err(ApiError::bad_request(format!(
            "FileEdit.edit_kind '{edit_kind}' is not recognized; expected one of: {}",
            FILE_EDIT_KIND_VALUES.join(", ")
        )));
    }
    if edit_kind != "rename" && rename_to.is_some() {
        return Err(ApiError::bad_request(
            "FileEdit.rename_to is only permitted when edit_kind is rename",
        ));
    }
    match edit_kind {
        "create" => {
            if before_hash.is_some() {
                return Err(ApiError::bad_request(
                    "FileEdit.before_hash must be null or omitted when edit_kind is create",
                ));
            }
            required_str(
                after_hash,
                "after_hash (required for FileEdit create nodes)",
            )?;
        }
        "delete" => {
            required_str(
                before_hash,
                "before_hash (required for FileEdit delete nodes)",
            )?;
            if after_hash.is_some() {
                return Err(ApiError::bad_request(
                    "FileEdit.after_hash must be null or omitted when edit_kind is delete",
                ));
            }
        }
        "modify" => {
            required_str(
                before_hash,
                "before_hash (required for FileEdit modify nodes)",
            )?;
            required_str(
                after_hash,
                "after_hash (required for FileEdit modify nodes)",
            )?;
        }
        "rename" => {
            required_str(
                before_hash,
                "before_hash (required for FileEdit rename nodes)",
            )?;
            required_str(
                after_hash,
                "after_hash (required for FileEdit rename nodes)",
            )?;
            required_str(rename_to, "rename_to (required for FileEdit rename nodes)")?;
        }
        _ => {}
    }
    hunk_count
        .ok_or_else(|| ApiError::missing_field("hunk_count (required for FileEdit nodes)"))?;
    if let Some(linked_patch_id) = linked_patch_id {
        let linked_patch_id = required_str(Some(linked_patch_id), "linked_patch_id")?;
        validate_patch_artifact_ref("FileEdit.linked_patch_id", linked_patch_id, records, sink)?;
    }
    let linked_turn_id = required_str(
        linked_turn_id,
        "linked_turn_id (required for FileEdit nodes)",
    )?;
    validate_agent_turn_ref("FileEdit.linked_turn_id", linked_turn_id, records, sink)?;
    if id.is_empty() {
        return Err(ApiError::missing_field("id"));
    }
    Ok(())
}

fn validate_agent_action_domain(kind: &'static str, domain: Option<&str>) -> WriteResult<()> {
    match domain {
        Some("agent_memory") => Ok(()),
        Some(other) => Err(ApiError::bad_request(format!(
            "{kind}.domain must be 'agent_memory'; got '{other}'"
        ))),
        None => Err(ApiError::missing_field(format!(
            "domain (required for {kind} nodes)"
        ))),
    }
}

fn validate_agent_action_source_artifact(
    _kind: &'static str,
    source_artifact_path: Option<&str>,
    source_artifact_hash: Option<&str>,
) -> WriteResult<()> {
    required_str(
        source_artifact_path,
        "source_artifact_path (required for agent-action nodes)",
    )?;
    required_str(
        source_artifact_hash,
        "source_artifact_hash (required for agent-action nodes)",
    )?;
    Ok(())
}

fn validate_tool_call_finished_at(status: &str, finished_at: Option<&str>) -> WriteResult<()> {
    match status {
        "succeeded" | "failed" => {
            let finished_at = required_str(
                finished_at,
                "finished_at (required for completed ToolCall nodes)",
            )?;
            if DateTime::parse_from_rfc3339(finished_at).is_err() {
                return Err(ApiError::bad_request(format!(
                    "ToolCall.finished_at '{finished_at}' is not a valid RFC 3339 timestamp"
                )));
            }
        }
        "interrupted" => {
            if finished_at.is_some() {
                return Err(ApiError::bad_request(
                    "ToolCall.finished_at must be null or omitted when status is interrupted",
                ));
            }
        }
        _ => {
            if let Some(finished_at) = finished_at {
                let finished_at = required_str(Some(finished_at), "finished_at")?;
                if DateTime::parse_from_rfc3339(finished_at).is_err() {
                    return Err(ApiError::bad_request(format!(
                        "ToolCall.finished_at '{finished_at}' is not a valid RFC 3339 timestamp"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_agent_turn_ref(
    field: &'static str,
    value: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    validate_record_kind_ref(
        field,
        value,
        "agent_memory:v1:",
        "AgentTurn",
        &[NodeKind::AgentTurn],
        records,
        sink,
    )
}

fn validate_agent_session_ref(
    field: &'static str,
    value: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    validate_record_kind_ref(
        field,
        value,
        "agent_memory:v1:",
        "AgentSession",
        &[NodeKind::AgentSession],
        records,
        sink,
    )
}

fn validate_verification_evidence_ref(
    field: &'static str,
    value: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    validate_record_kind_ref(
        field,
        value,
        "verification:v1:",
        "CommandRun or TestRun",
        &[NodeKind::CommandRun, NodeKind::TestRun],
        records,
        sink,
    )
}

fn validate_patch_artifact_ref(
    field: &'static str,
    value: &str,
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    validate_record_kind_ref(
        field,
        value,
        "artifact:v1:",
        "PatchArtifact",
        &[NodeKind::PatchArtifact],
        records,
        sink,
    )
}

fn validate_record_kind_ref(
    field: &'static str,
    value: &str,
    expected_prefix: &'static str,
    expected_kind: &'static str,
    allowed_kinds: &[NodeKind],
    records: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<()> {
    if !value.starts_with(expected_prefix) {
        return Err(ApiError::bad_request(format!(
            "{field} must reference a {expected_prefix} {expected_kind}; got '{value}'"
        )));
    }
    match lookup_node_kind(value, records, sink)? {
        Some(kind) if allowed_kinds.contains(&kind) => Ok(()),
        Some(kind) => Err(ApiError::bad_request(format!(
            "{field} must reference a {expected_prefix} {expected_kind}; target '{value}' has kind {}",
            kind.as_str()
        ))),
        None => Err(ApiError::bad_request(format!(
            "{field} must reference an existing {expected_prefix} {expected_kind}; target '{value}' was not found"
        ))),
    }
}

fn validate_agent_action_output_handle(
    field: &'static str,
    handle: &OutputHandle,
) -> WriteResult<()> {
    if handle.hash.is_empty() {
        return Err(ApiError::bad_request(format!(
            "{field}.hash must not be empty"
        )));
    }
    let inline_len = handle.inline.as_deref().map_or(0, |s| s.len() as u64);
    if inline_len > handle.bytes {
        return Err(ApiError::bad_request(format!(
            "{field}.bytes must be >= inline payload length"
        )));
    }
    if inline_len > INLINE_PAYLOAD_CEILING
        || (handle.inline.is_some() && handle.bytes > INLINE_PAYLOAD_CEILING)
    {
        return Err(ApiError::inline_payload_exceeds_ceiling(format!(
            "{field}.inline must be None when bytes exceeds the 16 KiB ceiling; demote to handle-only before writing"
        )));
    }
    Ok(())
}

fn validate_unique_recovery_keys(records: &[GraphRecord]) -> WriteResult<()> {
    if has_duplicate_recovery_keys(records) || has_ambiguous_recovery_keys(records) {
        return Err(ApiError::conflict(
            "ingest payload has duplicate record IDs that are not idempotently recoverable",
        ));
    }
    Ok(())
}

// Returns true when `id` has the expected record-ID prefix for `domain`.
fn record_id_matches_domain(id: &str, domain: &str) -> bool {
    match domain {
        "codegraph" => id.starts_with("codegraph:"),
        "agent_memory" => id.starts_with("agent_memory:v1:"),
        "verification" => id.starts_with("verification:v1:"),
        "artifact" => id.starts_with("artifact:v1:"),
        "project" => id.starts_with("project:v1:"),
        "semantic" => id.starts_with("semantic:v1:"),
        "user_context" => id.starts_with("user_context:v1:"),
        _ => true,
    }
}

// Validates that a resolved target ID is consistent with the declared target_domain.
// For domains with stable prefixes, enforces the expected ID prefix.
// "project" and any other domain have no universal prefix requirement; the
// relation-specific checks in validate_evidence_endpoint_constraints enforce per-label rules.
fn validate_evidence_target_domain(id: &str, target_domain: &str) -> WriteResult<()> {
    match target_domain {
        "codegraph" if !id.starts_with("codegraph:") => {
            return Err(ApiError::bad_request(format!(
                "evidence link declares target_domain 'codegraph' but target '{id}' does not have the expected 'codegraph:' prefix",
            )));
        }
        "agent_memory" if !id.starts_with("agent_memory:v1:") => {
            return Err(ApiError::bad_request(format!(
                "evidence link declares target_domain 'agent_memory' but target '{id}' does not have the expected 'agent_memory:v1:' prefix",
            )));
        }
        "verification" if !id.starts_with("verification:v1:") => {
            return Err(ApiError::bad_request(format!(
                "evidence link declares target_domain 'verification' but target '{id}' does not have the expected 'verification:v1:' prefix",
            )));
        }
        "artifact" if !id.starts_with("artifact:v1:") => {
            return Err(ApiError::bad_request(format!(
                "evidence link declares target_domain 'artifact' but target '{id}' does not have the expected 'artifact:v1:' prefix",
            )));
        }
        "project" if !id.starts_with("project:v1:") => {
            return Err(ApiError::bad_request(format!(
                "evidence link declares target_domain 'project' but target '{id}' does not have the expected 'project:v1:' prefix",
            )));
        }
        "semantic" if !id.starts_with("semantic:v1:") => {
            return Err(ApiError::bad_request(format!(
                "evidence link declares target_domain 'semantic' but target '{id}' does not have the expected 'semantic:v1:' prefix",
            )));
        }
        // Other declared domains: no universal prefix requirement.
        _ => {}
    }
    Ok(())
}

// Resolves an evidence link's target ID.  Returns (canonical_record_id, routing_commit) or an error.
// Accepts either a direct `target_record_id` or a (path, span, commit) triple.
// `batch` is the current ingest payload; targets that have not yet been written but
// appear in the same batch are accepted so one request can atomically create a target
// node and an observation that cites it.
// For triple resolution, routing_commit is link.target_git_commit.
// For direct ID with as_of_commit, validates the commit exists as a temporal observation.
#[allow(clippy::too_many_lines)]
fn resolve_evidence_target(
    link: &EvidenceLink,
    sink: &EmbeddedAletheiaSink,
    batch: &[GraphRecord],
) -> WriteResult<(String, Option<String>)> {
    if let Some(id) = &link.target_record_id {
        let sink_record = match sink.read_back(id) {
            Ok(found) => found,
            Err(e) => return Err(ApiError::internal(e.to_string())),
        };
        // Only node records are valid evidence targets — reject edges and tombstones.
        if let Some(ref rec) = sink_record
            && !matches!(rec, GraphRecord::Node { .. })
        {
            return Err(ApiError::bad_request(format!(
                "evidence link target '{id}' is not a node record; only node records may be evidence targets"
            )));
        }
        let in_sink = sink_record.is_some();
        // Also reject batch edges or tombstones that share the ID.
        let in_batch_as_node = batch
            .iter()
            .any(|r| r.id() == id.as_str() && matches!(r, GraphRecord::Node { .. }));
        let in_batch_as_non_node = batch
            .iter()
            .any(|r| r.id() == id.as_str() && !matches!(r, GraphRecord::Node { .. }));
        if in_batch_as_non_node && !in_batch_as_node {
            return Err(ApiError::bad_request(format!(
                "evidence link target '{id}' in the current batch is not a node record"
            )));
        }
        let in_batch = in_batch_as_node;
        if !in_sink && !in_batch {
            return Err(ApiError::new(
                ErrorCode::UnresolvedEvidenceTarget,
                format!("evidence link target '{id}' not found in store or batch"),
            ));
        }
        validate_evidence_target_domain(id, &link.target_domain)?;
        let store_records = sink
            .read_all_records()
            .map_err(|e| ApiError::internal(e.to_string()))?;
        if let Some(commit) = &link.as_of_commit {
            let commit_found = store_records.iter().chain(batch.iter()).any(|record| {
                if let GraphRecord::Node {
                    id: record_id,
                    temporal: Some(t),
                    ..
                } = record
                {
                    record_id == id && &t.git_commit == commit
                } else {
                    false
                }
            });
            if !commit_found {
                return Err(ApiError::new(
                    ErrorCode::UnresolvedEvidenceTarget,
                    format!(
                        "evidence link target '{id}' has no temporal observation at commit '{commit}'"
                    ),
                ));
            }
            return Ok((id.clone(), Some(commit.clone())));
        }
        // No as_of_commit — reject if the target has multiple distinct temporal observations
        // (ambiguous: the edge writer cannot determine which to link to).
        // Deduplicate by (id, git_commit) so an idempotent re-submission that includes an
        // already-written temporal node in both the store and the batch is not double-counted.
        let distinct_temporal_commits: BTreeSet<&str> = store_records
            .iter()
            .chain(batch.iter())
            .filter_map(|r| {
                if let GraphRecord::Node {
                    id: nid,
                    temporal: Some(t),
                    ..
                } = r
                    && nid == id
                {
                    Some(t.git_commit.as_str())
                } else {
                    None
                }
            })
            .collect();
        let temporal_count = distinct_temporal_commits.len();
        if temporal_count > 1 {
            return Err(ApiError::bad_request(format!(
                "evidence link to '{id}' is ambiguous: the target has {temporal_count} temporal observations; supply as_of_commit to select a specific observation"
            )));
        }
        return Ok((id.clone(), None));
    }
    // Triple-based resolution: scan store + batch for matching (path, span, commit).
    // target_span is required to avoid ambiguity for files with multiple path-backed nodes.
    let path = link.target_repo_relative_path.as_deref().ok_or_else(|| {
        ApiError::missing_field(
            "evidence_links[].target_record_id or (target_repo_relative_path, target_git_commit, target_span)",
        )
    })?;
    let commit = link
        .target_git_commit
        .as_deref()
        .ok_or_else(|| ApiError::missing_field("evidence_links[].target_git_commit"))?;
    if link.target_span.is_none() {
        return Err(ApiError::missing_field(
            "evidence_links[].target_span (required for triple evidence target lookup to avoid ambiguity)",
        ));
    }
    let store_records = sink
        .read_all_records()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let found_id = store_records.iter().chain(batch.iter()).find_map(|record| {
        if let GraphRecord::Node {
            id,
            repo_relative_path: Some(rp),
            temporal: Some(t),
            span,
            ..
        } = record
            && rp == path
            && t.git_commit == commit
            && link
                .target_span
                .as_ref()
                .is_none_or(|ts| span.as_ref() == Some(ts))
        {
            Some(id.clone())
        } else {
            None
        }
    });
    match found_id {
        Some(id) => {
            validate_evidence_target_domain(&id, &link.target_domain)?;
            // Reject if an explicit as_of_commit was supplied but differs from the triple commit —
            // the two would route the edge to different temporal observations.
            if let Some(aoc) = &link.as_of_commit
                && Some(aoc.as_str()) != link.target_git_commit.as_deref()
            {
                return Err(ApiError::bad_request(format!(
                    "evidence link triple resolved to commit '{}' but as_of_commit '{aoc}' conflicts; omit as_of_commit or set it to the same value as target_git_commit",
                    link.target_git_commit.as_deref().unwrap_or("")
                )));
            }
            // For triple resolution, route by the target commit so multi-observation
            // temporal targets resolve to the correct historical endpoint.
            Ok((id, link.target_git_commit.clone()))
        }
        None => Err(ApiError::new(
            ErrorCode::UnresolvedEvidenceTarget,
            format!("evidence link target not found by triple (path={path}, commit={commit})"),
        )),
    }
}

// Validates evidence link invariants and synthesizes the required graph Edge records.
struct ResolvedLink {
    node_id: String,
    // Index of this link within the parent node's evidence_links array.
    // Used to canonicalize triple-resolved links back into the source node.
    link_index: usize,
    target_id: String,
    // Validated edge label stored directly to avoid re-parsing in Phase 2.
    edge_label: EdgeLabel,
    confidence: Option<String>,
    // Commit used to route the synthesized edge to the correct temporal observation.
    // For direct-ID links this is the validated as_of_commit; for triple links it is
    // the target_git_commit from the triple.
    routing_commit: Option<String>,
    // True when the link was resolved from the (path, span, commit) triple rather
    // than a direct target_record_id — so the stored node needs canonicalization.
    was_triple_resolved: bool,
}

// Looks up a node's kind from the current batch, or falls back to the store.
fn lookup_node_kind(
    id: &str,
    batch: &[GraphRecord],
    sink: &EmbeddedAletheiaSink,
) -> WriteResult<Option<NodeKind>> {
    for r in batch {
        if r.id() == id {
            return Ok(if let GraphRecord::Node { kind, .. } = r {
                Some(*kind)
            } else {
                None
            });
        }
    }
    match sink.read_back(id) {
        Ok(Some(GraphRecord::Node { kind, .. })) => Ok(Some(kind)),
        Ok(_) => Ok(None),
        Err(e) => Err(ApiError::internal(e.to_string())),
    }
}

// Validates source-kind and target-kind constraints per evidence-link relation.
// `source_kind` is None when the source node cannot be resolved (e.g. a directly
// submitted edge whose source is not in the current batch or store); source-side
// constraints are skipped when the kind is unknown.
#[allow(clippy::too_many_lines)]
fn validate_evidence_endpoint_constraints(
    source_kind: Option<NodeKind>,
    label: EdgeLabel,
    target_kind: Option<NodeKind>,
    target_id: &str,
) -> WriteResult<()> {
    // Source-side constraints.
    match label {
        EdgeLabel::Observes | EdgeLabel::ExplainsChange => {
            if let Some(sk) = source_kind
                && sk != NodeKind::Observation
            {
                return Err(ApiError::bad_request(format!(
                    "evidence link relation '{}' requires an Observation source node, not {}",
                    label.as_str(),
                    sk.as_str()
                )));
            }
        }
        EdgeLabel::ValidatedBy => {
            if let Some(sk) = source_kind
                && !matches!(
                    sk,
                    NodeKind::Observation | NodeKind::Decision | NodeKind::AgentRun
                )
            {
                return Err(ApiError::bad_request(format!(
                    "evidence link relation '{}' requires an Observation, Decision, or legacy AgentRun source node, not {}",
                    label.as_str(),
                    sk.as_str()
                )));
            }
        }
        EdgeLabel::ProducedPatch => {
            if let Some(sk) = source_kind
                && !matches!(
                    sk,
                    NodeKind::FileEdit | NodeKind::AgentTurn | NodeKind::AgentRun
                )
            {
                return Err(ApiError::bad_request(format!(
                    "evidence link relation '{}' requires a FileEdit, AgentTurn, or legacy AgentRun source node, not {}",
                    label.as_str(),
                    sk.as_str()
                )));
            }
        }
        EdgeLabel::ProducedEvidence => {
            if let Some(sk) = source_kind
                && sk != NodeKind::ToolCall
            {
                return Err(ApiError::bad_request(format!(
                    "evidence link relation '{}' requires a ToolCall source node, not {}",
                    label.as_str(),
                    sk.as_str()
                )));
            }
        }
        EdgeLabel::TouchedFile => {
            if let Some(sk) = source_kind
                && !matches!(
                    sk,
                    NodeKind::FileEdit
                        | NodeKind::ToolCall
                        | NodeKind::CommandRun
                        | NodeKind::TestRun
                        | NodeKind::CIStatus
                        | NodeKind::PatchArtifact
                )
            {
                return Err(ApiError::bad_request(format!(
                    "evidence link relation '{}' requires a FileEdit, ToolCall, CommandRun, TestRun, CIStatus, or PatchArtifact source node, not {}",
                    label.as_str(),
                    sk.as_str()
                )));
            }
        }
        // FAILED_ON: supported from TestRun, CIStatus (verification), and Failure (agent_memory).
        EdgeLabel::FailedOn => {
            if let Some(sk) = source_kind
                && !matches!(
                    sk,
                    NodeKind::TestRun | NodeKind::CIStatus | NodeKind::Failure
                )
            {
                return Err(ApiError::bad_request(format!(
                    "evidence link relation 'FAILED_ON' requires a TestRun, CIStatus, or \
                     Failure source node; got source kind {}",
                    sk.as_str()
                )));
            }
        }
        // TouchedFile, MentionsSymbol, and all other labels: any agent-memory source kind is permitted.
        _ => {}
    }
    // Target-side constraints.
    let target_kind_str =
        || target_kind.map_or_else(|| "unknown".to_owned(), |k| k.as_str().to_owned());
    match label {
        EdgeLabel::MentionsSymbol if !matches!(target_kind, Some(NodeKind::Symbol)) => {
            return Err(ApiError::bad_request(format!(
                "evidence link relation '{}' requires a Symbol target; target '{}' has kind {}",
                label.as_str(),
                target_id,
                target_kind_str()
            )));
        }
        EdgeLabel::TouchedFile if !matches!(target_kind, Some(NodeKind::File)) => {
            return Err(ApiError::bad_request(format!(
                "evidence link relation '{}' requires a File target; target '{}' has kind {}",
                label.as_str(),
                target_id,
                target_kind_str()
            )));
        }
        EdgeLabel::ReferencesTask if !matches!(target_kind, Some(NodeKind::Task)) => {
            return Err(ApiError::bad_request(format!(
                "evidence link relation '{}' requires a Task target; target '{}' has kind {}",
                label.as_str(),
                target_id,
                target_kind_str()
            )));
        }
        EdgeLabel::HasEvidence
            if !matches!(
                target_kind,
                Some(
                    NodeKind::Verification
                        | NodeKind::CommandEvidence
                        | NodeKind::TestRun
                        | NodeKind::CIStatus
                        | NodeKind::BenchmarkRun
                        | NodeKind::CoverageReport
                        | NodeKind::ProofResult
                )
            ) =>
        {
            return Err(ApiError::bad_request(format!(
                "evidence link relation '{}' requires a verification-evidence or CommandEvidence target; target '{}' has kind {}",
                label.as_str(),
                target_id,
                target_kind_str()
            )));
        }
        EdgeLabel::ValidatedBy
            if !matches!(
                target_kind,
                Some(
                    NodeKind::Verification
                        | NodeKind::TestRun
                        | NodeKind::CIStatus
                        | NodeKind::BenchmarkRun
                        | NodeKind::CoverageReport
                        | NodeKind::ProofResult
                )
            ) =>
        {
            return Err(ApiError::bad_request(format!(
                "evidence link relation '{}' requires a verification-evidence target; target '{}' has kind {}",
                label.as_str(),
                target_id,
                target_kind_str()
            )));
        }
        EdgeLabel::ExplainsChange
            if !matches!(target_kind, Some(NodeKind::Commit | NodeKind::Change)) =>
        {
            return Err(ApiError::bad_request(format!(
                "evidence link relation '{}' requires a Commit or Change target; target '{}' has kind {}",
                label.as_str(),
                target_id,
                target_kind_str()
            )));
        }
        EdgeLabel::FailedOn
            if !matches!(
                target_kind,
                Some(NodeKind::Symbol | NodeKind::File | NodeKind::PatchArtifact)
            ) =>
        {
            return Err(ApiError::bad_request(format!(
                "evidence link relation '{}' requires a Symbol, File, or legacy PatchArtifact target; target '{}' has kind {}",
                label.as_str(),
                target_id,
                target_kind_str()
            )));
        }
        EdgeLabel::ProducedPatch if !matches!(target_kind, Some(NodeKind::PatchArtifact)) => {
            return Err(ApiError::bad_request(format!(
                "evidence link relation '{}' requires a PatchArtifact target; target '{}' has kind {}",
                label.as_str(),
                target_id,
                target_kind_str()
            )));
        }
        EdgeLabel::ProducedEvidence
            if !matches!(target_kind, Some(NodeKind::CommandRun | NodeKind::TestRun)) =>
        {
            return Err(ApiError::bad_request(format!(
                "evidence link relation '{}' requires a CommandRun or TestRun target; target '{}' has kind {}",
                label.as_str(),
                target_id,
                target_kind_str()
            )));
        }
        _ => {}
    }
    Ok(())
}

// Sentinel timestamps used for evidence-edge temporal routing metadata.
// These are valid RFC 3339 but semantically unimportant; agent-memory edges are
// not used in time-range queries.
const EVIDENCE_EDGE_ROUTING_TIMESTAMP: &str = "1970-01-01T00:00:00Z";

/// Agent kinds defined in docs/schema/agent-memory.md.
const VALID_AGENT_KINDS: &[&str] = &[
    "codex",
    "claude-code",
    "vantage",
    "rust-swe-agent",
    "human",
    "other",
];

/// Node kinds that belong to the agent-memory domain.
const AGENT_MEMORY_NODE_KINDS: &[NodeKind] = &[
    NodeKind::Agent,
    NodeKind::AgentSession,
    NodeKind::Observation,
    NodeKind::Task,
    NodeKind::Artifact,
    NodeKind::CommandEvidence,
    // Legacy trajectory importer diagnostics kept ingestible during the same
    // migration window as legacy action/evidence records.
    NodeKind::Diagnostic,
    NodeKind::AgentRun,
    NodeKind::AgentTurn,
    NodeKind::ToolCall,
    // Legacy trajectory importer outputs kept ingestible until the emitter
    // migrates these records into verification/artifact domains.
    NodeKind::CommandRun,
    NodeKind::Verification,
    NodeKind::FileEdit,
    NodeKind::PatchArtifact,
    NodeKind::Failure,
    NodeKind::Decision,
    NodeKind::CostUsage,
];

// Validates that the source and target IDs of a directly submitted agent-memory edge
// are in the domains required by the cross-domain registry (docs/schema/agent-memory.md §6).
#[allow(clippy::too_many_lines)]
fn validate_agent_memory_edge_endpoints(
    edge_id: &str,
    label: EdgeLabel,
    source: &str,
    target: &str,
) -> WriteResult<()> {
    if matches!(
        label,
        EdgeLabel::ClosesAcceptanceCriterion
            | EdgeLabel::OwnedByTask
            | EdgeLabel::ExternalHandle
            | EdgeLabel::TouchesFile
    ) {
        return Err(ApiError::bad_request(format!(
            "agent-memory edge '{edge_id}' label '{}' is project-only; use a project:v1: edge",
            label.as_str()
        )));
    }
    // Source-domain constraints per schema registry.
    match label {
        // Labels that require an agent_memory:v1: source exclusively.
        EdgeLabel::SessionOf
        | EdgeLabel::Observes
        | EdgeLabel::ProducedPatch
        | EdgeLabel::ProducedEvidence
        | EdgeLabel::ValidatedBy
        | EdgeLabel::ExplainsChange
        | EdgeLabel::ReferencesTask
        | EdgeLabel::Supersedes
            if !source.starts_with("agent_memory:v1:") =>
        {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires an agent_memory:v1: source; got source '{source}'",
                label.as_str()
            )));
        }
        // Labels that allow agent_memory:v1: OR verification:v1: sources.
        EdgeLabel::MentionsSymbol
        | EdgeLabel::TouchedFile
        | EdgeLabel::FailedOn
        | EdgeLabel::Contradicts
            if !source.starts_with("agent_memory:v1:")
                && !source.starts_with("verification:v1:") =>
        {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires an agent_memory:v1: or verification:v1: source; got source '{source}'",
                label.as_str()
            )));
        }
        // AUTHORED_BY, HAS_EVIDENCE, RELATES_TO: any source domain is permitted.
        _ => {}
    }
    // Target-domain constraints per schema registry.
    match label {
        // Must target agent_memory exclusively.
        EdgeLabel::SessionOf | EdgeLabel::AuthoredBy | EdgeLabel::Supersedes
            if !target.starts_with("agent_memory:v1:") =>
        {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires an agent_memory:v1: target; got target '{target}'",
                label.as_str()
            )));
        }
        EdgeLabel::ReferencesTask if !target.starts_with("project:v1:") => {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires a project:v1: target; got target '{target}'",
                label.as_str()
            )));
        }
        // ValidatedBy and HAS_EVIDENCE can target agent_memory OR verification.
        EdgeLabel::ValidatedBy | EdgeLabel::HasEvidence
            if !target.starts_with("agent_memory:v1:")
                && !target.starts_with("verification:v1:") =>
        {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires an agent_memory:v1: or verification:v1: target; got target '{target}'",
                label.as_str()
            )));
        }
        EdgeLabel::Observes
        | EdgeLabel::MentionsSymbol
        | EdgeLabel::TouchedFile
        | EdgeLabel::ExplainsChange
            if !target.starts_with("codegraph:") =>
        {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires a codegraph: target; got target '{target}'",
                label.as_str()
            )));
        }
        EdgeLabel::FailedOn
            if !target.starts_with("codegraph:") && !target.starts_with("agent_memory:v1:") =>
        {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires a codegraph: target or legacy agent_memory:v1: PatchArtifact target; got target '{target}'",
                label.as_str()
            )));
        }
        EdgeLabel::ProducedPatch
            if !target.starts_with("artifact:v1:") && !target.starts_with("agent_memory:v1:") =>
        {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires an artifact:v1: target or legacy agent_memory:v1: PatchArtifact target; got target '{target}'",
                label.as_str()
            )));
        }
        EdgeLabel::ProducedEvidence if !target.starts_with("verification:v1:") => {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires a verification:v1: target; got target '{target}'",
                label.as_str()
            )));
        }
        // RELATES_TO: any target domain is permitted.
        _ => {}
    }
    Ok(())
}

// Returns (synthesized_edges, canonical_source_nodes).
// Canonical source nodes are copies of source records that had triple-resolved evidence links,
// with target_record_id filled in from the resolved canonical ID so both the denormalized
// JSON and the traversal edge agree on the target.
#[allow(clippy::too_many_lines)]
fn validate_and_synthesize_evidence_edges(
    records: &[GraphRecord],
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
) -> WriteResult<(Vec<GraphRecord>, Vec<GraphRecord>)> {
    // Phase 1: validate and resolve all targets while holding the read lock.
    let resolved: Vec<ResolvedLink> = {
        let sink_guard = sink
            .read()
            .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
        let mut resolved = Vec::new();
        for record in records {
            if let GraphRecord::Edge { id, label, .. } = record
                && !id.starts_with("user_context:v1:")
                && USER_CONTEXT_ONLY_EDGE_LABELS.contains(label)
            {
                return Err(ApiError::bad_request(format!(
                    "edge '{id}' uses user-context-only label '{}'; use a user_context:v1: edge",
                    label.as_str()
                )));
            }
            // Reject evidence-link labels on codegraph edges — they must go through the
            // agent-memory envelope and its cross-domain checks, not the codegraph path.
            if let GraphRecord::Edge { id, label, .. } = record
                && !id.starts_with("agent_memory:v1:")
                && !(id.starts_with("artifact:v1:") && *label == EdgeLabel::Supersedes)
                && !(id.starts_with("project:v1:") && PROJECT_EDGE_LABELS.contains(label))
                && !(id.starts_with("user_context:v1:") && USER_CONTEXT_EDGE_LABELS.contains(label))
                && label.is_evidence_link_label()
            {
                return Err(ApiError::bad_request(format!(
                    "edge '{id}' uses evidence-link label '{}' but is not an agent_memory:v1:, project:v1:, or user_context:v1: edge; evidence relations are only permitted on agent-memory, project, or user-context edges",
                    label.as_str()
                )));
            }
            if let GraphRecord::Edge { id, label, .. } = record
                && id.starts_with("artifact:v1:")
                && *label != EdgeLabel::Supersedes
            {
                return Err(ApiError::bad_request(format!(
                    "artifact edge '{id}' uses unsupported label '{}'; artifact-domain edges only permit SUPERSEDES",
                    label.as_str()
                )));
            }
            if let GraphRecord::Edge {
                id,
                label: EdgeLabel::Supersedes,
                source,
                target,
                schema_version,
                ..
            } = record
                && id.starts_with("artifact:v1:")
            {
                if *schema_version != ARTIFACT_SCHEMA_VERSION {
                    return Err(ApiError::bad_request(format!(
                        "artifact edge '{id}' has schema_version {schema_version} but only version {ARTIFACT_SCHEMA_VERSION} is accepted"
                    )));
                }
                if !source.starts_with("artifact:v1:") || !target.starts_with("artifact:v1:") {
                    return Err(ApiError::bad_request(format!(
                        "artifact SUPERSEDES edge '{id}' requires artifact:v1: source and target; got source '{source}' target '{target}'"
                    )));
                }
                let source_kind = lookup_node_kind(source, records, &sink_guard)?;
                let target_kind = lookup_node_kind(target, records, &sink_guard)?;
                if !matches!(
                    (source_kind, target_kind),
                    (Some(NodeKind::PatchArtifact), Some(NodeKind::PatchArtifact))
                ) {
                    return Err(ApiError::bad_request(format!(
                        "artifact SUPERSEDES edge '{id}' requires PatchArtifact source and target"
                    )));
                }
            }
            // Validate directly submitted agent-memory edge records.
            if let GraphRecord::Edge {
                id,
                label,
                source,
                target,
                schema_version,
                confidence,
                ..
            } = record
                && id.starts_with("agent_memory:v1:")
            {
                if label.is_codegraph_topology_label() {
                    return Err(ApiError::bad_request(format!(
                        "agent-memory edge '{id}' uses codegraph-topology label '{}'; only evidence-link and agent-memory structural labels are permitted for agent-memory edges",
                        label.as_str()
                    )));
                }
                if *schema_version != AGENT_MEMORY_SCHEMA_VERSION {
                    return Err(ApiError::bad_request(format!(
                        "agent-memory edge '{id}' has schema_version {schema_version} but only version {AGENT_MEMORY_SCHEMA_VERSION} is accepted"
                    )));
                }
                if matches!(
                    label,
                    EdgeLabel::Observes
                        | EdgeLabel::MentionsSymbol
                        | EdgeLabel::ExplainsChange
                        | EdgeLabel::Contradicts
                ) {
                    let valid = confidence
                        .as_deref()
                        .and_then(|s| s.parse::<f64>().ok())
                        .is_some_and(|v| (0.0..=1.0).contains(&v));
                    if !valid {
                        return Err(ApiError::bad_request(format!(
                            "agent-memory edge '{id}' label '{}' requires a numeric confidence in [0.0, 1.0]",
                            label.as_str()
                        )));
                    }
                }
                validate_agent_memory_edge_endpoints(id, *label, source, target)?;
                // Validate node-kind constraints for structural labels where the registry
                // requires specific endpoint kinds beyond domain-prefix checks.
                match label {
                    EdgeLabel::SessionOf => {
                        let source_kind = lookup_node_kind(source, records, &sink_guard)?;
                        let target_kind = lookup_node_kind(target, records, &sink_guard)?;
                        let valid_documented = matches!(source_kind, Some(NodeKind::AgentSession))
                            && matches!(target_kind, Some(NodeKind::Agent));
                        let valid_legacy_traj = matches!(source_kind, Some(NodeKind::AgentRun))
                            && matches!(target_kind, Some(NodeKind::AgentSession));
                        if valid_documented || valid_legacy_traj {
                            continue;
                        }
                        if !matches!(
                            source_kind,
                            Some(NodeKind::AgentSession | NodeKind::AgentRun)
                        ) {
                            return Err(ApiError::bad_request(format!(
                                "agent-memory edge '{id}' SESSION_OF requires an AgentSession source or legacy AgentRun source; got {}",
                                source_kind.map_or_else(
                                    || "unknown".to_owned(),
                                    |k| k.as_str().to_owned()
                                )
                            )));
                        }
                        return Err(ApiError::bad_request(format!(
                            "agent-memory edge '{id}' SESSION_OF requires an Agent target or legacy AgentSession target; got {}",
                            target_kind
                                .map_or_else(|| "unknown".to_owned(), |k| k.as_str().to_owned())
                        )));
                    }
                    EdgeLabel::AuthoredBy => {
                        let target_kind = lookup_node_kind(target, records, &sink_guard)?;
                        if !matches!(
                            target_kind,
                            Some(NodeKind::AgentSession | NodeKind::AgentRun | NodeKind::AgentTurn)
                        ) {
                            return Err(ApiError::bad_request(format!(
                                "agent-memory edge '{id}' AUTHORED_BY requires an AgentSession target or legacy AgentRun/AgentTurn target; got {}",
                                target_kind.map_or_else(
                                    || "unknown".to_owned(),
                                    |k| k.as_str().to_owned()
                                )
                            )));
                        }
                    }
                    // Evidence-link labels: apply the same source/target kind constraints
                    // used by the evidence_links validator so direct-edge submissions cannot
                    // bypass schema endpoint checks.
                    other if other.is_evidence_link_label() => {
                        let source_kind = lookup_node_kind(source, records, &sink_guard)?;
                        let target_kind = lookup_node_kind(target, records, &sink_guard)?;
                        validate_evidence_endpoint_constraints(
                            source_kind,
                            *label,
                            target_kind,
                            target,
                        )?;
                    }
                    _ => {}
                }
            }
            if let GraphRecord::Node {
                id,
                kind,
                schema_version,
                evidence_links,
                name,
                confidence,
                text,
                agent_id,
                agent_kind,
                session_id,
                observed_at,
                ingested_at,
                ..
            } = record
            {
                let links = evidence_links.as_deref().unwrap_or(&[]);
                // Reject evidence links on codegraph nodes — they would produce
                // edges from a non-agent-memory/verification source, bypassing
                // the envelope domain check.
                if !links.is_empty()
                    && !id.starts_with("agent_memory:v1:")
                    && !id.starts_with("verification:v1:")
                {
                    return Err(ApiError::bad_request(format!(
                        "node '{id}' has evidence_links but is not an agent-memory or verification record; evidence links are only supported for agent_memory:v1: and verification:v1: nodes"
                    )));
                }
                // Validate and enforce schema constraints for all agent-memory node kinds.
                if id.starts_with("agent_memory:v1:") {
                    // Reject codegraph node kinds stored under an agent-memory ID.
                    if !AGENT_MEMORY_NODE_KINDS.contains(kind) {
                        return Err(ApiError::bad_request(format!(
                            "node kind '{}' is not permitted under the agent_memory:v1: namespace; use codegraph: IDs for code-graph nodes",
                            kind.as_str()
                        )));
                    }
                    // Schema version must match the published agent-memory v1 contract.
                    if *schema_version != AGENT_MEMORY_SCHEMA_VERSION {
                        return Err(ApiError::bad_request(format!(
                            "agent-memory node '{id}' has schema_version {schema_version} but only version {AGENT_MEMORY_SCHEMA_VERSION} is accepted"
                        )));
                    }
                    if *kind == NodeKind::Observation && links.is_empty() {
                        return Err(ApiError::missing_field(
                            "evidence_links (Observation requires at least one evidence link)",
                        ));
                    }
                    // Required provenance fields. Agent nodes represent a stable identity
                    // and omit session-specific timestamp fields so their payload is
                    // invariant across multiple session registrations for the same agent_id.
                    let session_fields_required = *kind != NodeKind::Agent;
                    let required: &[(&str, bool)] = &[
                        ("agent_id", agent_id.as_ref().is_some_and(|s| !s.is_empty())),
                        (
                            "agent_kind",
                            agent_kind.as_ref().is_some_and(|s| !s.is_empty()),
                        ),
                        (
                            "session_id",
                            !session_fields_required
                                || session_id.as_ref().is_some_and(|s| !s.is_empty()),
                        ),
                        (
                            "observed_at",
                            !session_fields_required
                                || observed_at.as_ref().is_some_and(|s| !s.is_empty()),
                        ),
                        (
                            "ingested_at",
                            !session_fields_required
                                || ingested_at.as_ref().is_some_and(|s| !s.is_empty()),
                        ),
                    ];
                    for (field, present) in required {
                        if !present {
                            return Err(ApiError::missing_field(format!(
                                "{field} (required for agent-memory {} nodes)",
                                kind.as_str()
                            )));
                        }
                    }
                    // Validate agent_kind against the published enum.
                    if let Some(ak) = agent_kind.as_deref().filter(|s| !s.is_empty())
                        && !VALID_AGENT_KINDS.contains(&ak)
                    {
                        return Err(ApiError::bad_request(format!(
                            "agent_kind '{ak}' is not a recognized value; expected one of: {}",
                            VALID_AGENT_KINDS.join(", ")
                        )));
                    }
                    // Validate timestamp format for required timestamp fields.
                    for (ts_field, ts_val) in [
                        ("observed_at", observed_at.as_deref()),
                        ("ingested_at", ingested_at.as_deref()),
                    ] {
                        if let Some(ts) = ts_val.filter(|s| !s.is_empty())
                            && DateTime::parse_from_rfc3339(ts).is_err()
                        {
                            return Err(ApiError::bad_request(format!(
                                "{ts_field} '{ts}' is not a valid RFC 3339 timestamp"
                            )));
                        }
                    }
                    // confidence is required only for Observation nodes; optional for others.
                    if *kind == NodeKind::Observation
                        && confidence.as_ref().is_none_or(String::is_empty)
                    {
                        return Err(ApiError::missing_field(
                            "confidence (required for Observation nodes)",
                        ));
                    }
                    if matches!(kind, NodeKind::ToolCall | NodeKind::FileEdit)
                        && confidence.as_ref().is_some_and(|s| !s.is_empty())
                    {
                        return Err(ApiError::bad_request(format!(
                            "{} nodes must not carry confidence",
                            kind.as_str()
                        )));
                    }
                    // Validate confidence format when present (applies to all kinds).
                    if let Some(conf_str) = confidence.as_deref().filter(|s| !s.is_empty()) {
                        let conf_val: f64 = conf_str.parse().map_err(|_| {
                            ApiError::bad_request(format!(
                                "confidence '{conf_str}' must be a numeric float string"
                            ))
                        })?;
                        if !(0.0..=1.0).contains(&conf_val) {
                            return Err(ApiError::bad_request(format!(
                                "confidence '{conf_str}' must be in the range [0.0, 1.0]"
                            )));
                        }
                    }
                    // text is additionally required for Observation nodes.
                    if *kind == NodeKind::Observation && text.as_ref().is_none_or(String::is_empty)
                    {
                        return Err(ApiError::missing_field(
                            "text (required for Observation nodes)",
                        ));
                    }
                    // name is required for Agent and AgentSession nodes per schema v1.
                    if matches!(kind, NodeKind::Agent | NodeKind::AgentSession)
                        && name.as_ref().is_none_or(String::is_empty)
                    {
                        return Err(ApiError::missing_field(format!(
                            "name (required for {} nodes)",
                            kind.as_str()
                        )));
                    }
                    validate_agent_action_record(record, records, &sink_guard)?;
                }
                for (link_index, link) in links.iter().enumerate() {
                    let was_triple_resolved = link.target_record_id.is_none();
                    let (target_id, routing_commit) =
                        resolve_evidence_target(link, &sink_guard, records)?;
                    if link.confidence.is_empty() {
                        return Err(ApiError::missing_field("evidence_links[].confidence"));
                    }
                    let conf_val: f64 = link.confidence.parse().map_err(|_| {
                        ApiError::bad_request(format!(
                            "evidence_links[].confidence '{}' must be a numeric float string",
                            link.confidence
                        ))
                    })?;
                    if !(0.0..=1.0).contains(&conf_val) {
                        return Err(ApiError::bad_request(format!(
                            "evidence_links[].confidence '{}' must be in the range [0.0, 1.0]",
                            link.confidence
                        )));
                    }
                    // Validate edge label and source/target endpoint constraints.
                    let edge_label = EdgeLabel::from_relation(&link.relation).ok_or_else(|| {
                        ApiError::bad_request(format!(
                            "unknown evidence link relation '{}'",
                            link.relation
                        ))
                    })?;
                    if !edge_label.is_evidence_link_label() {
                        return Err(ApiError::bad_request(format!(
                            "evidence link relation '{}' is a codegraph-internal label and may not be used in evidence links",
                            link.relation
                        )));
                    }
                    // Validate that target_domain matches the registry's TO domain for this relation.
                    match edge_label {
                        EdgeLabel::Observes
                        | EdgeLabel::MentionsSymbol
                        | EdgeLabel::TouchedFile
                        | EdgeLabel::ExplainsChange
                            if link.target_domain != "codegraph" =>
                        {
                            return Err(ApiError::bad_request(format!(
                                "evidence link relation '{}' requires target_domain 'codegraph'; got '{}'",
                                edge_label.as_str(),
                                link.target_domain
                            )));
                        }
                        EdgeLabel::FailedOn
                            if !matches!(
                                link.target_domain.as_str(),
                                "codegraph" | "agent_memory"
                            ) =>
                        {
                            return Err(ApiError::bad_request(format!(
                                "evidence link relation '{}' requires target_domain 'codegraph' or legacy 'agent_memory'; got '{}'",
                                edge_label.as_str(),
                                link.target_domain
                            )));
                        }
                        // ValidatedBy and HasEvidence can target agent_memory OR verification.
                        EdgeLabel::ValidatedBy | EdgeLabel::HasEvidence
                            if !matches!(
                                link.target_domain.as_str(),
                                "agent_memory" | "verification"
                            ) =>
                        {
                            return Err(ApiError::bad_request(format!(
                                "evidence link relation '{}' requires target_domain 'agent_memory' or 'verification'; got '{}'",
                                edge_label.as_str(),
                                link.target_domain
                            )));
                        }
                        // Supersedes: agent_memory only.
                        EdgeLabel::Supersedes if link.target_domain != "agent_memory" => {
                            return Err(ApiError::bad_request(format!(
                                "evidence link relation '{}' requires target_domain 'agent_memory'; got '{}'",
                                edge_label.as_str(),
                                link.target_domain
                            )));
                        }
                        // CONTRADICTS: TO any — no target_domain restriction.
                        EdgeLabel::ReferencesTask if link.target_domain != "project" => {
                            return Err(ApiError::bad_request(format!(
                                "evidence link relation '{}' requires target_domain 'project'; got '{}'",
                                edge_label.as_str(),
                                link.target_domain
                            )));
                        }
                        EdgeLabel::ClosesAcceptanceCriterion
                        | EdgeLabel::OwnedByTask
                        | EdgeLabel::ExternalHandle
                        | EdgeLabel::TouchesFile => {
                            return Err(ApiError::bad_request(format!(
                                "evidence link relation '{}' is project-only and must be written as a project edge",
                                edge_label.as_str()
                            )));
                        }
                        EdgeLabel::ProducedPatch if link.target_domain != "artifact" => {
                            return Err(ApiError::bad_request(format!(
                                "evidence link relation '{}' requires target_domain 'artifact'; got '{}'",
                                edge_label.as_str(),
                                link.target_domain
                            )));
                        }
                        EdgeLabel::ProducedEvidence if link.target_domain != "verification" => {
                            return Err(ApiError::bad_request(format!(
                                "evidence link relation '{}' requires target_domain 'verification'; got '{}'",
                                edge_label.as_str(),
                                link.target_domain
                            )));
                        }
                        // RELATES_TO: any target domain is permitted.
                        _ => {}
                    }
                    let target_kind = lookup_node_kind(&target_id, records, &sink_guard)?;
                    validate_evidence_endpoint_constraints(
                        Some(*kind),
                        edge_label,
                        target_kind,
                        &target_id,
                    )?;
                    resolved.push(ResolvedLink {
                        node_id: id.clone(),
                        link_index,
                        target_id,
                        edge_label,
                        confidence: Some(link.confidence.clone()),
                        routing_commit,
                        was_triple_resolved,
                    });
                }
            }
        }
        drop(sink_guard); // release read lock before Phase 2
        resolved
    };

    // Collect triple-resolution data before Phase 2 moves `resolved`.
    // Maps node_id → (link_index → resolved canonical target_id) for any link that was
    // submitted without a target_record_id and resolved from the (path, span, commit) triple.
    let mut node_resolutions: BTreeMap<String, BTreeMap<usize, String>> = BTreeMap::new();
    for rl in &resolved {
        if rl.was_triple_resolved {
            node_resolutions
                .entry(rl.node_id.clone())
                .or_default()
                .insert(rl.link_index, rl.target_id.clone());
        }
    }

    // Phase 2: synthesize edge records (no lock needed).
    // Dedup key: (edge_id, routing_commit, confidence).  Same all three → skip silently.
    // Same edge_id + same commit but different confidence → conflict error.
    // Same edge_id + different commit → conflict error (ambiguous temporal target).
    let mut seen_edges: BTreeMap<String, (Option<String>, String)> = BTreeMap::new();
    let mut edges = Vec::with_capacity(resolved.len());
    for rl in resolved {
        // edge_label and is_evidence_link_label were already validated in Phase 1.
        let summary = format!(
            "{} {} (from evidence link)",
            rl.node_id,
            rl.edge_label.as_str()
        );
        let conf = rl.confidence.clone().unwrap_or_default();
        let edge = GraphRecord::agent_memory_edge(
            rl.edge_label,
            rl.node_id,
            rl.target_id,
            rl.confidence,
            summary,
        );
        // If the citation anchors a specific commit, attach it as routing-only temporal metadata
        // so write_edge can resolve the correct temporal endpoint when the target has multiple
        // historical observations.
        let edge = if let Some(ref commit) = rl.routing_commit {
            edge.with_temporal(TemporalMetadata {
                git_commit: commit.clone(),
                git_parent_commits: Vec::new(),
                valid_time: EVIDENCE_EDGE_ROUTING_TIMESTAMP.to_owned(),
                observed_at: EVIDENCE_EDGE_ROUTING_TIMESTAMP.to_owned(),
                author_time: None,
                valid_time_source: None,
            })
        } else {
            edge
        };
        let edge_id = edge.id().to_owned();
        match seen_edges.get(&edge_id) {
            Some((existing_commit, existing_conf)) if *existing_commit == rl.routing_commit => {
                if existing_conf.as_str() != conf.as_str() {
                    return Err(ApiError::bad_request(format!(
                        "evidence links for edge '{edge_id}' have conflicting confidence values"
                    )));
                }
                // exact duplicate, skip silently
            }
            Some(_) => {
                return Err(ApiError::bad_request(format!(
                    "evidence links for edge '{edge_id}' have conflicting as_of_commit values"
                )));
            }
            None => {
                seen_edges.insert(edge_id, (rl.routing_commit, conf));
                edges.push(edge);
            }
        }
    }

    // Build canonical source nodes: for any node that had triple-resolved links (submitted
    // without target_record_id), fill in the resolved canonical ID so that the stored
    // evidence_links JSON and the synthesized traversal edge both point to the same target.
    let canonical_nodes: Vec<GraphRecord> = records
        .iter()
        .filter_map(|record| {
            let resolutions = node_resolutions.get(record.id())?;
            if let GraphRecord::Node {
                evidence_links: Some(links),
                ..
            } = record
            {
                let canonical_links: Vec<EvidenceLink> = links
                    .iter()
                    .enumerate()
                    .map(|(i, link)| {
                        resolutions.get(&i).map_or_else(
                            || link.clone(),
                            |resolved_id| EvidenceLink {
                                target_record_id: Some(resolved_id.clone()),
                                ..link.clone()
                            },
                        )
                    })
                    .collect();
                let mut canonical = record.clone();
                if let GraphRecord::Node { evidence_links, .. } = &mut canonical {
                    *evidence_links = Some(canonical_links);
                }
                Some(canonical)
            } else {
                None
            }
        })
        .collect();

    Ok((edges, canonical_nodes))
}

fn has_duplicate_recovery_keys(records: &[GraphRecord]) -> bool {
    let mut seen = BTreeSet::new();
    for record in records {
        if !seen.insert(recovery_key(record)) {
            return true;
        }
    }
    false
}

fn has_ambiguous_recovery_keys(records: &[GraphRecord]) -> bool {
    let mut seen = BTreeMap::<String, &GraphRecord>::new();
    for record in records {
        let key = recovery_key(record);
        if let Some(previous) = seen.insert(key, record)
            && previous != record
        {
            return true;
        }
    }
    false
}

fn recovery_key(record: &GraphRecord) -> String {
    match record {
        GraphRecord::Node {
            id,
            temporal: Some(temporal),
            ..
        } => format!("node\0{}\0{}", id, temporal.git_commit),
        GraphRecord::Node { id, .. } | GraphRecord::Tombstone { id, .. } => id.clone(),
        GraphRecord::Edge { id, temporal, .. } => {
            let commit = temporal
                .as_ref()
                .map_or("", |temporal| temporal.git_commit.as_str());
            let payload = serde_json::to_vec(record).unwrap_or_default();
            format!(
                "edge\0{}\0{}\0{}",
                id,
                commit,
                blake3::hash(&payload).to_hex()
            )
        }
    }
}

fn complete_idempotency_entry(
    idempotency_key: &str,
    payload_hash: &str,
    response: &DaemonIngestResponse,
    idempotency: &Arc<Mutex<IdempotencyStore>>,
) -> WriteResult<()> {
    {
        let mut store = idempotency
            .lock()
            .map_err(|_| ApiError::internal("idempotency store lock poisoned"))?;
        store
            .set_entry_durably(
                idempotency_key.to_owned(),
                IdempotencyEntry::Committed {
                    payload_hash: payload_hash.to_owned(),
                    response: response.clone(),
                },
            )
            .map_err(|error| ApiError::internal(error.to_string()))?;
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, state: Arc<ServerState>) {
    let response = match read_http_request(&mut stream, &state.token) {
        Ok(request) => handle_request(&request, &state),
        Err(error) if error.to_string() == "request body too large" => {
            HttpResponse::error(ApiError::payload_too_large())
        }
        Err(error) => HttpResponse::error(ApiError::bad_request(error.to_string())),
    };
    let _ = write_http_response(&mut stream, &response);
    drop(state);
}

fn handle_request(request: &HttpRequest, state: &ServerState) -> HttpResponse {
    if request.method == "GET" && request.path == "/v1/health" {
        return HttpResponse::json(
            200,
            json!({
                "api_version": "v1",
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "data_dir": state.store_identity.as_str(),
            }),
        );
    }
    if !is_authorized(request, &state.token) {
        return HttpResponse::error(ApiError::unauthorized());
    }
    // Shutdown is handled before the gate so concurrent/retried stop calls
    // succeed even after the flag is set (idempotent drain behavior).
    if request.method == "POST" && request.path == "/v1/admin/shutdown" {
        state.shutdown.store(true, Ordering::SeqCst);
        return HttpResponse::success(None, 200, json!({ "status": "stopping" }));
    }
    if state.shutdown.load(Ordering::SeqCst) {
        return HttpResponse::error(ApiError::shutdown_in_progress());
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/v1/status") => handle_status(state),
        ("POST", "/v1/records/ingest") => handle_ingest(request, state),
        ("POST", "/v1/query") => handle_query(request, state),
        ("POST", "/v1/agents/register") => handle_agent_register(request, state),
        ("POST", "/v1/agents/heartbeat") => handle_agent_heartbeat(request, state),
        ("POST", "/v1/jobs/ingest") => handle_job_ingest(request, state),
        ("POST", "/v1/admin/checkpoint") => handle_checkpoint(state),
        ("GET", "/v1/records") => handle_get_all_records(state),
        _ if request.method == "GET" && request.path.starts_with("/v1/records/") => {
            let record_id = request.path.trim_start_matches("/v1/records/");
            handle_get_record(record_id, state)
        }
        _ if request.method == "GET" && request.path.starts_with("/v1/jobs/") => {
            handle_get_job(&request.path, state)
        }
        _ => HttpResponse::error(ApiError::not_found("unknown daemon endpoint")),
    }
}

fn handle_status(state: &ServerState) -> HttpResponse {
    let jobs = match state.jobs.lock() {
        Ok(jobs) => jobs.len(),
        Err(_) => return HttpResponse::error(ApiError::internal("jobs lock poisoned")),
    };
    let agents = match state.agents.lock() {
        Ok(agents) => agents.len(),
        Err(_) => return HttpResponse::error(ApiError::internal("agents lock poisoned")),
    };
    let idempotency_store_size = match state.idempotency.lock() {
        Ok(store) => store.entries.len(),
        Err(_) => return HttpResponse::error(ApiError::internal("idempotency lock poisoned")),
    };
    HttpResponse::json(
        200,
        json!({
            "api_version": "v1",
            "status": "running",
            "data_dir": state.store_identity.as_str(),
            "jobs": jobs,
            "agents": agents,
            "idempotency_store_size": idempotency_store_size,
            "pressure": state.pressure.snapshot_json(),
        }),
    )
}

fn handle_get_all_records(state: &ServerState) -> HttpResponse {
    let Ok(sink) = state.sink.read() else {
        return HttpResponse::error(ApiError::internal("embedded sink lock poisoned"));
    };
    let snapshot_timestamp = chrono::Utc::now().to_rfc3339();
    let report = match sink.inspect_all_records() {
        Ok(r) => r,
        Err(e) => return HttpResponse::error(adapter_read_error_to_api(e)),
    };
    drop(sink);
    HttpResponse::success(
        None,
        200,
        json!({
            "records": report.records,
            "unknown_schema_versions": report.unknown_schema_versions,
            "snapshot_timestamp": snapshot_timestamp,
        }),
    )
}

fn handle_ingest(request: &HttpRequest, state: &ServerState) -> HttpResponse {
    let envelope = match parse_json::<RequestEnvelope>(&request.body) {
        Ok(envelope) => envelope,
        Err(error) => return HttpResponse::error(error),
    };
    let request_id = match non_empty(envelope.request_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => return HttpResponse::error(ApiError::missing_field("request_id")),
    };
    let agent_id = match non_empty(envelope.agent_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("agent_id"));
        }
    };
    if non_empty(envelope.session_id.as_deref()).is_none() {
        return HttpResponse::error_with_id(&request_id, ApiError::missing_field("session_id"));
    }
    let idempotency_key = match non_empty(envelope.idempotency_key.as_deref()) {
        Some(key) => key.to_owned(),
        None => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::missing_field("idempotency_key"),
            );
        }
    };
    let domain = match non_empty(envelope.domain.as_deref()) {
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("domain"));
        }
        Some(d)
            if !matches!(
                d,
                "codegraph"
                    | "agent_memory"
                    | "verification"
                    | "artifact"
                    | "project"
                    | "semantic"
                    | "user_context"
            ) =>
        {
            return HttpResponse::error_with_id(&request_id, ApiError::invalid_domain());
        }
        Some(d) => d.to_owned(),
    };
    match envelope
        .created_at
        .as_deref()
        .and_then(|s| non_empty(Some(s)))
    {
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("created_at"));
        }
        Some(ts) if DateTime::parse_from_rfc3339(ts).is_err() => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::bad_request("created_at must be RFC 3339"),
            );
        }
        _ => {}
    }
    if envelope.payload.is_null() {
        return HttpResponse::error_with_id(&request_id, ApiError::missing_field("payload"));
    }
    let payload = match serde_json::from_value::<IngestPayload>(envelope.payload) {
        Ok(payload) => payload,
        Err(error) => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::bad_request(error.to_string()),
            );
        }
    };
    if let Some(bad) = payload
        .records
        .iter()
        .find(|r| !record_id_matches_domain(r.id(), &domain))
    {
        return HttpResponse::error_with_id(
            &request_id,
            ApiError::bad_request(format!(
                "record '{}' has ID inconsistent with domain '{domain}'",
                bad.id()
            )),
        );
    }
    let scoped_key = scoped_idempotency_key(&agent_id, "records/ingest", &idempotency_key);
    match enqueue_write(state, scoped_key, payload.records, &request_id) {
        Ok(response) => HttpResponse::success(Some(&request_id), 200, json!(response)),
        Err(error) => HttpResponse::error_with_id(&request_id, error),
    }
}

fn handle_get_record(record_id: &str, state: &ServerState) -> HttpResponse {
    let Ok(sink) = state.sink.read() else {
        return HttpResponse::error(ApiError::internal("embedded sink lock poisoned"));
    };
    match sink.read_back(record_id) {
        Ok(record) => HttpResponse::success(None, 200, json!({ "record": record })),
        Err(error) => HttpResponse::error(adapter_read_error_to_api(error)),
    }
}

const DEFAULT_QUERY_MAX_RESULTS: usize = 5_000;
const DEFAULT_QUERY_TIMEOUT_MS: u64 = 5_000;
const DRIFT_TOP_N_DEFAULT: usize = 10;
const DRIFT_TOP_N_MAX: usize = 100;

/// Returns the current instant as an RFC3339 timestamp for `result.snapshot`.
fn rfc3339_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Builds the standard verb success result: `{ verb, snapshot, records, page }`.
fn verb_success_result(
    verb: &str,
    snapshot: &str,
    records: &[serde_json::Value],
) -> serde_json::Value {
    let returned = records.len() as u64;
    json!({
        "verb": verb,
        "snapshot": snapshot,
        "records": records,
        "page": {
            "cursor": serde_json::Value::Null,
            "has_more": false,
            "returned": returned
        }
    })
}

/// Loads all records from the embedded sink, respecting the read budget.
/// Returns the records filtered to the given domain and the RFC3339 snapshot
/// timestamp captured at read-lock acquisition time.
fn load_all_records_for_verb(
    state: &ServerState,
    started: Instant,
    budget: Option<Duration>,
    domain: &str,
) -> std::result::Result<(Vec<GraphRecord>, String), ApiError> {
    let sink = query_sink_read(state, started, budget)?;
    // Capture the snapshot while the read lock is held.
    let snapshot = rfc3339_now();
    let records = sink.read_all_records().map_err(adapter_read_error_to_api)?;
    drop(sink);
    // Post-read check: the read itself may have overrun the deadline.
    check_query_budget(started, budget)?;
    // Filter to the requested domain.
    let records = records
        .into_iter()
        .filter(|r| record_id_matches_domain(r.id(), domain))
        .collect();
    Ok((records, snapshot))
}

/// Collects the IDs of tombstoned records in the slice.
fn tombstoned_ids_in(records: &[GraphRecord]) -> BTreeSet<&str> {
    records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect()
}

/// Converts a `Symbol` node to the query JSON shape that matches CLI `eg query symbol`.
/// Returns `None` when the record is not a Symbol or has no name.
fn symbol_node_to_query_json(record: &GraphRecord) -> Option<serde_json::Value> {
    let GraphRecord::Node {
        id,
        kind: NodeKind::Symbol,
        schema_version,
        name,
        repo_relative_path,
        span,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    // Use empty string for unnamed symbols to match non-daemon `eg query file` parity.
    let name_str = name.as_deref().unwrap_or("");
    let mut obj = serde_json::Map::new();
    obj.insert("record_id".to_owned(), json!(id.as_str()));
    obj.insert("schema_version".to_owned(), json!(*schema_version));
    obj.insert("name".to_owned(), json!(name_str));
    obj.insert("kind".to_owned(), json!("Symbol"));
    obj.insert(
        "repo_relative_path".to_owned(),
        json!(repo_relative_path.as_deref()),
    );
    obj.insert("span".to_owned(), json!(span));
    if let Some(t) = temporal {
        obj.insert("git_commit".to_owned(), json!(&t.git_commit));
    }
    Some(serde_json::Value::Object(obj))
}

/// Converts a `SemanticDrift` node to the query JSON shape that matches CLI `eg query drift`.
/// Resolves target path/name first from a `DriftsFrom` edge, then from
/// `semantic_drift.target_record_id`, and finally from inline node fields.
fn drift_node_to_query_json(
    record: &GraphRecord,
    all_records: &[GraphRecord],
) -> Option<serde_json::Value> {
    let GraphRecord::Node {
        id,
        kind: NodeKind::SemanticDrift,
        schema_version,
        semantic_drift: Some(drift),
        repo_relative_path: drift_path,
        name: drift_name,
        ..
    } = record
    else {
        return None;
    };

    // Prefer target id from a DriftsFrom edge; fall back to the inline
    // target_record_id on the drift metadata when the edge is absent or filtered.
    let edge_target: Option<&str> = all_records.iter().find_map(|r| {
        if let GraphRecord::Edge {
            source,
            target,
            label: EdgeLabel::DriftsFrom,
            ..
        } = r
            && source == id
        {
            return Some(target.as_str());
        }
        None
    });
    let target_id: &str = edge_target.unwrap_or(drift.target_record_id.as_str());

    let (resolved_path, resolved_name) = all_records.iter().find(|r| r.id() == target_id).map_or(
        (drift_path.as_deref(), drift_name.as_deref()),
        |target| match target {
            GraphRecord::Node {
                repo_relative_path,
                name,
                ..
            } => (
                repo_relative_path.as_deref().or(drift_path.as_deref()),
                name.as_deref().or(drift_name.as_deref()),
            ),
            _ => (drift_path.as_deref(), drift_name.as_deref()),
        },
    );

    let mut obj = serde_json::Map::new();
    obj.insert("record_id".to_owned(), json!(id.as_str()));
    obj.insert("schema_version".to_owned(), json!(*schema_version));
    obj.insert("before_commit".to_owned(), json!(&drift.before_git_commit));
    obj.insert("after_commit".to_owned(), json!(&drift.after_git_commit));
    obj.insert(
        "before_valid_time".to_owned(),
        json!(&drift.before_valid_time),
    );
    obj.insert(
        "after_valid_time".to_owned(),
        json!(&drift.after_valid_time),
    );
    obj.insert("prior_record_id".to_owned(), json!(&drift.prior_record_id));
    obj.insert(
        "target_record_id".to_owned(),
        json!(&drift.target_record_id),
    );
    obj.insert("score".to_owned(), json!(drift.score));
    obj.insert(
        "selection_threshold".to_owned(),
        json!(drift.selection_threshold),
    );
    obj.insert(
        "selection_basis".to_owned(),
        json!(drift.selection_basis.as_str()),
    );
    obj.insert("metric_kind".to_owned(), json!(drift.metric_kind.as_str()));
    obj.insert(
        "embedding_model_provider".to_owned(),
        json!(&drift.embedding_model.provider),
    );
    obj.insert(
        "embedding_model_name".to_owned(),
        json!(&drift.embedding_model.name),
    );
    obj.insert(
        "embedding_model_version".to_owned(),
        json!(&drift.embedding_model.version),
    );
    obj.insert(
        "embedding_model_dim".to_owned(),
        json!(drift.embedding_model.dim),
    );
    obj.insert(
        "embedding_model_content_hash".to_owned(),
        json!(&drift.embedding_model.content_hash),
    );
    if let Some(p) = resolved_path {
        obj.insert("repo_relative_path".to_owned(), json!(p));
    }
    if let Some(n) = resolved_name {
        obj.insert("name".to_owned(), json!(n));
    }
    Some(serde_json::Value::Object(obj))
}

// ── Verb handler: get_records ─────────────────────────────────────────────────

fn handle_verb_get_records(
    request_id: &str,
    params: &serde_json::Value,
    domain: &str,
    limit: usize,
    started: Instant,
    budget: Option<Duration>,
    state: &ServerState,
) -> HttpResponse {
    let record_ids: Vec<String> = match params.get("record_ids") {
        Some(serde_json::Value::Array(arr)) => {
            let mut ids = Vec::with_capacity(arr.len());
            for v in arr {
                match v.as_str() {
                    Some(s) => ids.push(s.to_owned()),
                    None => {
                        return HttpResponse::error_with_id(
                            request_id,
                            ApiError::bad_request("params.record_ids must be an array of strings"),
                        );
                    }
                }
            }
            ids
        }
        Some(_) => {
            return HttpResponse::error_with_id(
                request_id,
                ApiError::bad_request("params.record_ids must be an array"),
            );
        }
        None => vec![],
    };

    let deadline = budget.and_then(|b| started.checked_add(b));
    let mut records: Vec<serde_json::Value> = Vec::new();
    let domain_filtered: Vec<&String> = record_ids
        .iter()
        .filter(|id| record_id_matches_domain(id, domain))
        .collect();

    // Capture snapshot under a brief read lock so it is bound to the store
    // state at the start of the read sequence rather than before any lock.
    let snapshot = {
        let _snap_guard = match query_sink_read(state, started, budget) {
            Ok(g) => g,
            Err(e) => return HttpResponse::error_with_id(request_id, e),
        };
        rfc3339_now()
    };

    for record_id in domain_filtered.iter().take(limit) {
        if let Err(error) = check_query_budget(started, budget) {
            return HttpResponse::error_with_id(request_id, error);
        }
        let result = {
            let sink = match query_sink_read(state, started, budget) {
                Ok(sink) => sink,
                Err(error) => return HttpResponse::error_with_id(request_id, error),
            };
            sink.read_back_until(record_id, deadline)
        };
        match result {
            Ok(Some(record)) => {
                if let Ok(v) = serde_json::to_value(&record) {
                    records.push(v);
                }
            }
            Ok(None) => {}
            Err(AdapterError::TimedOut { .. }) => {
                return HttpResponse::error_with_id(request_id, ApiError::query_timeout());
            }
            Err(error) => {
                return HttpResponse::error_with_id(request_id, adapter_read_error_to_api(error));
            }
        }
        if let Err(error) = check_query_budget(started, budget) {
            return HttpResponse::error_with_id(request_id, error);
        }
    }
    HttpResponse::success(
        Some(request_id),
        200,
        verb_success_result("get_records", &snapshot, &records),
    )
}

// ── Verb handler: symbol_by_name ──────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn handle_verb_symbol_by_name(
    request_id: &str,
    params: &serde_json::Value,
    as_of_valid_time: Option<&str>,
    limit: usize,
    started: Instant,
    budget: Option<Duration>,
    domain: &str,
    state: &ServerState,
) -> HttpResponse {
    let name = match params.get("name").and_then(serde_json::Value::as_str) {
        Some(n) => n.to_owned(),
        None => {
            return HttpResponse::error_with_id(request_id, ApiError::missing_field("params.name"));
        }
    };
    let kind_filter = params
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);

    let (records, snapshot) = match load_all_records_for_verb(state, started, budget, domain) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_with_id(request_id, e),
    };

    let result_records: Vec<serde_json::Value> = if let Some(as_of) = as_of_valid_time {
        match graph_query::symbol_as_of_valid_time(&records, &name, as_of) {
            Ok(Some(record)) => {
                // kind_filter only recognises "Symbol" in v1; anything else → empty.
                // Apply limit: a budget cap of 0 means no results.
                if kind_filter.as_deref().is_some_and(|kf| kf != "Symbol") || limit == 0 {
                    vec![]
                } else {
                    symbol_node_to_query_json(record).into_iter().collect()
                }
            }
            Ok(None) => vec![],
            Err(msg) => {
                return HttpResponse::error_with_id(request_id, ApiError::bad_request(msg));
            }
        }
    } else {
        let deleted = tombstoned_ids_in(&records);
        let mut results: Vec<(serde_json::Value, Option<usize>)> = records
            .iter()
            .filter(|r| match r {
                GraphRecord::Node {
                    id, temporal: None, ..
                } => !deleted.contains(id.as_str()),
                _ => true,
            })
            .filter_map(|r| {
                let GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name: node_name,
                    span,
                    ..
                } = r
                else {
                    return None;
                };
                if node_name.as_deref() != Some(name.as_str()) {
                    return None;
                }
                if kind_filter.as_deref().is_some_and(|kf| kf != "Symbol") {
                    return None;
                }
                let json = symbol_node_to_query_json(r)?;
                let line = span.map(|s| s.start_line);
                Some((json, line))
            })
            .collect();
        // Sort first so that limit truncates the tail, not an arbitrary prefix.
        results.sort_by(|(av, al), (bv, bl)| {
            al.cmp(bl)
                .then_with(|| av["record_id"].as_str().cmp(&bv["record_id"].as_str()))
        });
        results.truncate(limit);
        // Enforce timeout after the in-memory filter/sort phase.
        if let Err(e) = check_query_budget(started, budget) {
            return HttpResponse::error_with_id(request_id, e);
        }
        results.into_iter().map(|(v, _)| v).collect()
    };

    HttpResponse::success(
        Some(request_id),
        200,
        verb_success_result("symbol_by_name", &snapshot, &result_records),
    )
}

// ── Verb handler: symbol_at_commit ────────────────────────────────────────────

fn handle_verb_symbol_at_commit(
    request_id: &str,
    params: &serde_json::Value,
    limit: usize,
    started: Instant,
    budget: Option<Duration>,
    domain: &str,
    state: &ServerState,
) -> HttpResponse {
    let name = match params.get("name").and_then(serde_json::Value::as_str) {
        Some(n) => n.to_owned(),
        None => {
            return HttpResponse::error_with_id(request_id, ApiError::missing_field("params.name"));
        }
    };
    let commit = match params.get("commit").and_then(serde_json::Value::as_str) {
        Some(c) => c.to_owned(),
        None => {
            return HttpResponse::error_with_id(
                request_id,
                ApiError::missing_field("params.commit"),
            );
        }
    };

    let (records, snapshot) = match load_all_records_for_verb(state, started, budget, domain) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_with_id(request_id, e),
    };

    // Check for ambiguous commit prefix
    let matching_commits: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                temporal: Some(t), ..
            }
            | GraphRecord::Edge {
                temporal: Some(t), ..
            } => {
                if t.git_commit.starts_with(commit.as_str()) {
                    Some(t.git_commit.as_str())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    if matching_commits.len() > 1 {
        return HttpResponse::error_with_id(
            request_id,
            ApiError::new(
                ErrorCode::AmbiguousCommitPrefix,
                format!(
                    "ambiguous commit prefix '{}' matches {} distinct commits",
                    commit,
                    matching_commits.len()
                ),
            ),
        );
    }

    // Enforce timeout after the full-scan ambiguity check.
    if let Err(e) = check_query_budget(started, budget) {
        return HttpResponse::error_with_id(request_id, e);
    }

    // Apply the budget limit: limit=0 means no results are wanted.
    let result_records = graph_query::symbol_at_commit(&records, &name, &commit)
        .and_then(symbol_node_to_query_json)
        .into_iter()
        .take(limit)
        .collect::<Vec<_>>();

    HttpResponse::success(
        Some(request_id),
        200,
        verb_success_result("symbol_at_commit", &snapshot, &result_records),
    )
}

// ── Verb handler: file_defines ────────────────────────────────────────────────

/// Collects the symbols defined in `path` as of `as_of_dt`.
/// Returns empty when no `File` node for `path` with `valid_time <= as_of_dt` exists.
/// Deduplicates by `name` for spanned records (history records with the same name
/// are the same logical symbol, even when it moves lines across commits) and by
/// `record_id` for span-absent records to avoid coalescing distinct same-name symbols
/// that have no positional information.
fn file_defines_as_of(
    records: &[GraphRecord],
    path: &str,
    as_of_dt: chrono::DateTime<chrono::FixedOffset>,
    limit: usize,
) -> Vec<serde_json::Value> {
    if !file_node_exists_as_of(records, path, as_of_dt) {
        return vec![];
    }
    // Key: (name, span_key) where span_key is "" for spanned records (dedup by
    // name so the same logical symbol is collapsed across line-moving commits)
    // or "id:<record_id>" for span-absent records.
    #[allow(clippy::type_complexity)]
    let mut best: std::collections::BTreeMap<
        (String, String),
        (
            serde_json::Value,
            Option<usize>,
            chrono::DateTime<chrono::FixedOffset>,
        ),
    > = std::collections::BTreeMap::new();
    for r in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name: node_name,
            repo_relative_path,
            span,
            temporal,
            valid_time,
            ..
        } = r
        else {
            continue;
        };
        if repo_relative_path.as_deref() != Some(path) {
            continue;
        }
        let vt_str = temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref());
        let Some(vt_str) = vt_str else { continue };
        let Ok(vt) = chrono::DateTime::parse_from_rfc3339(vt_str) else {
            continue;
        };
        if vt > as_of_dt {
            continue;
        }
        let Some(name_str) = node_name.as_deref() else {
            continue;
        };
        let Some(json) = symbol_node_to_query_json(r) else {
            continue;
        };
        let line = span.map(|s| s.start_line);
        // For spanned records, dedup by name only: the same logical symbol is
        // coalesced to its most-recent version even when it moves lines across
        // commits (history records with the same name are always the same symbol).
        // For span-absent records, fall back to record_id so distinct same-name
        // symbols without positional info are not incorrectly merged.
        let span_key = line.map_or_else(|| format!("id:{}", id.as_str()), |_| String::new());
        let key = (name_str.to_owned(), span_key);
        let is_better = best.get(&key).is_none_or(|(pv, _, pvt)| {
            vt > *pvt || (vt == *pvt && json["record_id"].as_str() < pv["record_id"].as_str())
        });
        if is_better {
            best.insert(key, (json, line, vt));
        }
    }
    let mut results: Vec<(serde_json::Value, Option<usize>)> =
        best.into_values().map(|(v, l, _)| (v, l)).collect();
    sort_and_truncate_symbol_results(&mut results, limit);
    results.into_iter().map(|(v, _)| v).collect()
}

/// Returns true when a non-tombstoned `File` node for `path` exists in `records`.
fn live_file_node_exists(records: &[GraphRecord], path: &str) -> bool {
    let deleted = tombstoned_ids_in(records);
    records.iter().any(|r| {
        matches!(
            r,
            GraphRecord::Node {
                id,
                kind: NodeKind::File,
                repo_relative_path: Some(p),
                temporal: None,
                ..
            } if p == path && !deleted.contains(id.as_str())
        )
    })
}

/// Returns true when a `File` node for `path` with `valid_time <= as_of_dt` exists in `records`.
fn file_node_exists_as_of(
    records: &[GraphRecord],
    path: &str,
    as_of_dt: chrono::DateTime<chrono::FixedOffset>,
) -> bool {
    records.iter().any(|r| {
        let GraphRecord::Node {
            kind: NodeKind::File,
            repo_relative_path,
            temporal,
            valid_time,
            ..
        } = r
        else {
            return false;
        };
        if repo_relative_path.as_deref() != Some(path) {
            return false;
        }
        let vt_str = temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref());
        vt_str
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|vt| vt <= as_of_dt)
    })
}

/// Collects the current-state symbols defined in `path` (tombstones excluded).
/// Returns empty when no live `File` node exists for `path`, matching non-daemon behaviour.
fn file_defines_current(
    records: &[GraphRecord],
    path: &str,
    limit: usize,
) -> Vec<serde_json::Value> {
    if !live_file_node_exists(records, path) {
        return vec![];
    }
    let deleted = tombstoned_ids_in(records);
    let mut results: Vec<(serde_json::Value, Option<usize>)> = records
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                repo_relative_path,
                span,
                temporal,
                ..
            } = r
            else {
                return None;
            };
            if repo_relative_path.as_deref() != Some(path) {
                return None;
            }
            if temporal.is_none() && deleted.contains(id.as_str()) {
                return None;
            }
            let json = symbol_node_to_query_json(r)?;
            let line = span.map(|s| s.start_line);
            Some((json, line))
        })
        .collect();
    // Sort first so that limit truncates the tail, not an arbitrary prefix.
    sort_and_truncate_symbol_results(&mut results, limit);
    results.into_iter().map(|(v, _)| v).collect()
}

/// Sorts a `(json, start_line)` results list and truncates to `limit`.
fn sort_and_truncate_symbol_results(
    results: &mut Vec<(serde_json::Value, Option<usize>)>,
    limit: usize,
) {
    results.sort_by(|(av, al), (bv, bl)| {
        al.cmp(bl)
            .then_with(|| av["record_id"].as_str().cmp(&bv["record_id"].as_str()))
    });
    results.truncate(limit);
}

#[allow(clippy::too_many_arguments)]
fn handle_verb_file_defines(
    request_id: &str,
    params: &serde_json::Value,
    as_of_valid_time: Option<&str>,
    limit: usize,
    started: Instant,
    budget: Option<Duration>,
    domain: &str,
    state: &ServerState,
) -> HttpResponse {
    let path = match params
        .get("repo_relative_path")
        .and_then(serde_json::Value::as_str)
    {
        Some(p) => p.to_owned(),
        None => {
            return HttpResponse::error_with_id(
                request_id,
                ApiError::missing_field("params.repo_relative_path"),
            );
        }
    };

    let (records, snapshot) = match load_all_records_for_verb(state, started, budget, domain) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_with_id(request_id, e),
    };

    // When as_of_valid_time is set, keep the most-recent-per-symbol-name at or
    // before the given instant. Records without valid_time are excluded (they are
    // untimed current-state records, not part of any historical point-in-time view).
    let result_records: Vec<serde_json::Value> = if let Some(as_of) = as_of_valid_time {
        let as_of_dt = match chrono::DateTime::parse_from_rfc3339(as_of) {
            Ok(dt) => dt,
            Err(e) => {
                return HttpResponse::error_with_id(
                    request_id,
                    ApiError::bad_request(format!("invalid as_of.valid_time: {e}")),
                );
            }
        };
        file_defines_as_of(&records, &path, as_of_dt, limit)
    } else {
        file_defines_current(&records, &path, limit)
    };

    // Enforce timeout after the in-memory filter/sort phase.
    if let Err(e) = check_query_budget(started, budget) {
        return HttpResponse::error_with_id(request_id, e);
    }

    HttpResponse::success(
        Some(request_id),
        200,
        verb_success_result("file_defines", &snapshot, &result_records),
    )
}

// ── Verb handler: drift_top_n ─────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn handle_verb_drift_top_n(
    request_id: &str,
    params: &serde_json::Value,
    as_of_valid_time: Option<&str>,
    budget_limit: usize,
    started: Instant,
    budget: Option<Duration>,
    domain: &str,
    state: &ServerState,
) -> HttpResponse {
    // Effective limit: min(params.limit capped at DRIFT_TOP_N_MAX, budget_limit).
    // Reject non-integer limit values rather than silently coercing to the default.
    let params_limit = match params.get("limit") {
        None => DRIFT_TOP_N_DEFAULT,
        Some(v) => match v.as_u64() {
            Some(n) => usize::try_from(n).unwrap_or(DRIFT_TOP_N_MAX),
            None => {
                return HttpResponse::error_with_id(
                    request_id,
                    ApiError::bad_request("params.limit must be a non-negative integer"),
                );
            }
        },
    }
    .min(DRIFT_TOP_N_MAX);
    let effective_limit = params_limit.min(budget_limit);

    let drift_domain = if domain == "codegraph" {
        "semantic"
    } else {
        domain
    };
    let (mut records, snapshot) =
        match load_all_records_for_verb(state, started, budget, drift_domain) {
            Ok(r) => r,
            Err(e) => return HttpResponse::error_with_id(request_id, e),
        };

    // When as_of_valid_time is set, exclude drift records whose valid_time
    // exceeds the given instant. Records without valid_time are current-state
    // records with no temporal stamp; they are excluded from point-in-time queries.
    if let Some(as_of) = as_of_valid_time {
        let as_of_dt = match chrono::DateTime::parse_from_rfc3339(as_of) {
            Ok(dt) => dt,
            Err(e) => {
                return HttpResponse::error_with_id(
                    request_id,
                    ApiError::bad_request(format!("invalid as_of.valid_time: {e}")),
                );
            }
        };
        records.retain(|r| match r {
            GraphRecord::Node {
                temporal,
                valid_time,
                ..
            } => {
                let vt_str = temporal
                    .as_ref()
                    .map(|t| t.valid_time.as_str())
                    .or(valid_time.as_deref());
                vt_str
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .is_some_and(|vt| vt <= as_of_dt)
            }
            // In an as-of query, keep only edges that have an explicit valid_time
            // at or before as_of_dt.  Untimed current-state edges (temporal: None)
            // are from outside the point-in-time snapshot and must be excluded so
            // drift targets are not resolved using out-of-snapshot metadata.
            GraphRecord::Edge { temporal, .. } => {
                let vt_str = temporal.as_ref().map(|t| t.valid_time.as_str());
                vt_str.is_some_and(|s| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .ok()
                        .is_some_and(|vt| vt <= as_of_dt)
                })
            }
            GraphRecord::Tombstone { .. } => true,
        });
    }

    let drifts = graph_query::largest_semantic_drifts(&records, effective_limit);
    let result_records = drifts
        .into_iter()
        .filter_map(|r| drift_node_to_query_json(r, &records))
        .collect::<Vec<_>>();

    // Enforce timeout after ranking/materialization CPU phase.
    if let Err(e) = check_query_budget(started, budget) {
        return HttpResponse::error_with_id(request_id, e);
    }

    HttpResponse::success(
        Some(request_id),
        200,
        verb_success_result("drift_top_n", &snapshot, &result_records),
    )
}

// ── Verb handler: observations_for_symbol (issue #38) ────────────────────────

/// Loads all records from every domain without any prefix filtering.
///
/// `observations_for_symbol` is a cross-domain query: it needs codegraph
/// Symbol/File records AND `agent_memory` Observation records AND project Task
/// records AND verification Evidence records in a single pass so that
/// [`graph_query::symbol_context`] can traverse cross-domain edges.
fn load_cross_domain_records(
    state: &ServerState,
    started: Instant,
    budget: Option<Duration>,
) -> std::result::Result<(Vec<GraphRecord>, String), ApiError> {
    let sink = query_sink_read(state, started, budget)?;
    let snapshot = rfc3339_now();
    let records = sink.read_all_records().map_err(adapter_read_error_to_api)?;
    drop(sink);
    check_query_budget(started, budget)?;
    Ok((records, snapshot))
}

fn context_source_fact_to_json(record: &GraphRecord) -> serde_json::Value {
    let GraphRecord::Node {
        id,
        kind,
        name,
        repo_relative_path,
        span,
        temporal,
        valid_time,
        language,
        symbol_kind,
        ..
    } = record
    else {
        return json!({ "record_id": record.id() });
    };
    json!({
        "record_id": id,
        "kind": kind.as_str(),
        "name": name,
        "repo_relative_path": repo_relative_path,
        "span": span,
        "git_commit": temporal.as_ref().map(|t| t.git_commit.as_str()),
        "valid_time": valid_time.as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
        "language": language,
        "symbol_kind": symbol_kind,
    })
}

fn context_observation_to_json(record: &GraphRecord) -> serde_json::Value {
    let GraphRecord::Node {
        id,
        kind,
        summary,
        text,
        agent_id,
        session_id,
        observed_at,
        confidence,
        failure_kind,
        exit_code,
        evidence_links,
        ..
    } = record
    else {
        return json!({ "record_id": record.id() });
    };
    let provenance_handle = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => Some(format!("{a}:{s}")),
        (Some(a), None) => Some(a.to_owned()),
        _ => None,
    };
    let links = evidence_links.as_deref().unwrap_or(&[]);
    json!({
        "record_id": id,
        "kind": kind.as_str(),
        "summary": summary,
        "text": text,
        "provenance_handle": provenance_handle,
        "agent_id": agent_id,
        "session_id": session_id,
        "observed_at": observed_at,
        "confidence": confidence,
        "failure_kind": failure_kind,
        "exit_code": exit_code,
        "evidence_links": serde_json::to_value(links).unwrap_or_default(),
    })
}

fn context_linked_item_to_json(record: &GraphRecord) -> serde_json::Value {
    let GraphRecord::Node {
        id,
        kind,
        summary,
        title,
        name,
        text,
        status,
        verification_kind,
        exit_code,
        executed_at,
        evidence_quality,
        repo_relative_path,
        body_handle,
        evidence_links,
        stdout_handle,
        stderr_handle,
        source_artifact_path,
        source_artifact_hash,
        edit_kind,
        before_hash,
        after_hash,
        rename_to,
        hunk_count,
        linked_turn_id,
        linked_patch_id,
        patch_status,
        patch_handle,
        patch_bytes_hash,
        patch_bytes_size,
        target_files,
        validation_summary,
        base_commit,
        unknown_base_reason,
        producer_session_id,
        ..
    } = record
    else {
        return json!({ "record_id": record.id() });
    };
    let links = evidence_links.as_deref().unwrap_or(&[]);
    json!({
        "record_id": id,
        "kind": kind.as_str(),
        "summary": summary,
        "title": title,
        "name": name,
        "text": text,
        "status": status,
        "verification_kind": verification_kind,
        "exit_code": exit_code,
        "executed_at": executed_at,
        "evidence_quality": evidence_quality,
        "repo_relative_path": repo_relative_path,
        "body_handle": body_handle.as_deref().map(|h| serde_json::to_value(h).unwrap_or_default()),
        "stdout_handle": stdout_handle.as_deref().map(|h| serde_json::to_value(h).unwrap_or_default()),
        "stderr_handle": stderr_handle.as_deref().map(|h| serde_json::to_value(h).unwrap_or_default()),
        "source_artifact_path": source_artifact_path,
        "source_artifact_hash": source_artifact_hash,
        "edit_kind": edit_kind,
        "before_hash": before_hash,
        "after_hash": after_hash,
        "rename_to": rename_to,
        "hunk_count": hunk_count,
        "linked_turn_id": linked_turn_id,
        "linked_patch_id": linked_patch_id,
        "patch_status": patch_status,
        "patch_handle": patch_handle.as_deref().map(|h| serde_json::to_value(h).unwrap_or_default()),
        "patch_bytes_hash": patch_bytes_hash,
        "patch_bytes_size": patch_bytes_size,
        "target_files": target_files,
        "validation_summary": validation_summary,
        "base_commit": base_commit,
        "unknown_base_reason": unknown_base_reason,
        "producer_session_id": producer_session_id,
        "evidence_links": serde_json::to_value(links).unwrap_or_default(),
    })
}

struct ContextSections {
    source_facts: Vec<serde_json::Value>,
    topology_edges: Vec<serde_json::Value>,
    observations: Vec<serde_json::Value>,
    project_state: Vec<serde_json::Value>,
    artifacts: Vec<serde_json::Value>,
    verification_evidence: Vec<serde_json::Value>,
    unresolved: Vec<serde_json::Value>,
}

fn build_context_sections(ctx: &graph_query::SymbolContext<'_>, limit: usize) -> ContextSections {
    let mut rem = limit;

    let source_facts: Vec<_> = ctx
        .source_facts
        .iter()
        .take(rem)
        .map(|r| context_source_fact_to_json(r))
        .collect();
    rem = rem.saturating_sub(source_facts.len());

    let topology_edges: Vec<_> = ctx
        .topology_edges
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Edge {
                id,
                label,
                source,
                target,
                summary,
                temporal,
                ..
            } = r
            {
                Some(json!({
                    "record_id": id, "label": label.as_str(),
                    "source_id": source, "target_id": target, "summary": summary,
                    "git_commit": temporal.as_ref().map(|t| t.git_commit.as_str()),
                    "valid_time": temporal.as_ref().map(|t| t.valid_time.as_str()),
                }))
            } else {
                None
            }
        })
        .take(rem)
        .collect();
    rem = rem.saturating_sub(topology_edges.len());

    let observations: Vec<_> = ctx
        .observations
        .iter()
        .take(rem)
        .map(|r| context_observation_to_json(r))
        .collect();
    rem = rem.saturating_sub(observations.len());

    let project_state: Vec<_> = ctx
        .project_state
        .iter()
        .take(rem)
        .map(|r| context_linked_item_to_json(r))
        .collect();
    rem = rem.saturating_sub(project_state.len());

    let artifacts: Vec<_> = ctx
        .artifacts
        .iter()
        .take(rem)
        .map(|r| context_linked_item_to_json(r))
        .collect();
    rem = rem.saturating_sub(artifacts.len());

    let verification_evidence: Vec<_> = ctx
        .verification_evidence
        .iter()
        .take(rem)
        .map(|r| context_linked_item_to_json(r))
        .collect();
    rem = rem.saturating_sub(verification_evidence.len());

    let unresolved: Vec<_> = ctx
        .unresolved
        .iter()
        .take(rem)
        .map(|u| {
            json!({
                "source_record_id": u.source_record_id,
                "target_handle": u.target_handle,
                "relation": u.relation,
                "target_domain": u.target_domain,
                "verification_status": "unresolved",
            })
        })
        .collect();

    ContextSections {
        source_facts,
        topology_edges,
        observations,
        project_state,
        artifacts,
        verification_evidence,
        unresolved,
    }
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::option_if_let_else)]
fn handle_verb_observations_for_symbol(
    request_id: &str,
    params: &serde_json::Value,
    as_of_valid_time: Option<&str>,
    limit: usize,
    started: Instant,
    budget: Option<Duration>,
    state: &ServerState,
) -> HttpResponse {
    let name = match params.get("name").and_then(serde_json::Value::as_str) {
        Some(n) => n.to_owned(),
        None => {
            return HttpResponse::error_with_id(request_id, ApiError::missing_field("params.name"));
        }
    };

    let (mut records, snapshot) = match load_cross_domain_records(state, started, budget) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_with_id(request_id, e),
    };

    // Apply as_of.valid_time: exclude records whose valid_time is after the cutoff.
    // Records with no valid_time are excluded from point-in-time queries (consistent
    // with other temporal verbs).
    if let Some(as_of) = as_of_valid_time {
        let as_of_dt = match chrono::DateTime::parse_from_rfc3339(as_of) {
            Ok(dt) => dt,
            Err(e) => {
                return HttpResponse::error_with_id(
                    request_id,
                    ApiError::bad_request(format!("invalid as_of.valid_time: {e}")),
                );
            }
        };
        // First pass: collect IDs of nodes that fall within the as-of window.
        // Used to decide whether to keep untimed current-state edges.
        let retained_node_ids: BTreeSet<String> = records
            .iter()
            .filter_map(|r| match r {
                GraphRecord::Node {
                    id,
                    temporal,
                    valid_time,
                    ..
                } => {
                    let vt_str = temporal
                        .as_ref()
                        .map(|t| t.valid_time.as_str())
                        .or(valid_time.as_deref());
                    vt_str
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .is_some_and(|vt| vt <= as_of_dt)
                        .then(|| id.clone())
                }
                _ => None,
            })
            .collect();
        // Second pass: apply the actual filter.
        // - Nodes: keep if their valid_time is <= as_of_dt.
        // - Timed edges: keep if their valid_time is <= as_of_dt.
        // - Untimed edges: keep only if both endpoints are in the retained slice
        //   (current-state project MENTIONS_SYMBOL / DEFINES edges are still valid).
        // - Tombstones: drop — they have no valid_time so we cannot determine whether
        //   the deletion occurred before or after as_of_dt; retaining them would
        //   erroneously hide records that existed at the requested instant.
        records.retain(|r| match r {
            GraphRecord::Node {
                temporal,
                valid_time,
                ..
            } => {
                let vt_str = temporal
                    .as_ref()
                    .map(|t| t.valid_time.as_str())
                    .or(valid_time.as_deref());
                vt_str
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .is_some_and(|vt| vt <= as_of_dt)
            }
            GraphRecord::Edge {
                source,
                target,
                temporal,
                ..
            } => temporal
                .as_ref()
                .map(|t| t.valid_time.as_str())
                .map_or_else(
                    || {
                        retained_node_ids.contains(source.as_str())
                            && retained_node_ids.contains(target.as_str())
                    },
                    |vt_str| {
                        chrono::DateTime::parse_from_rfc3339(vt_str)
                            .ok()
                            .is_some_and(|vt| vt <= as_of_dt)
                    },
                ),
            GraphRecord::Tombstone { .. } => false,
        });
    }

    let ctx = graph_query::symbol_context(&records, &name);

    // Recheck budget after BFS traversal (potentially expensive for large stores).
    if let Err(e) = check_query_budget(started, budget) {
        return HttpResponse::error_with_id(request_id, e);
    }

    if ctx.is_no_match() {
        return HttpResponse::error_with_id(
            request_id,
            ApiError::not_found(format!("no records found for symbol '{name}'")),
        );
    }

    let s = build_context_sections(&ctx, limit);

    HttpResponse::success(
        Some(request_id),
        200,
        json!({
            "verb": "observations_for_symbol",
            "snapshot": snapshot,
            "symbol_name": name,
            "source_facts": s.source_facts,
            "topology_edges": s.topology_edges,
            "observations": s.observations,
            "project_state": s.project_state,
            "artifacts": s.artifacts,
            "verification_evidence": s.verification_evidence,
            "unresolved": s.unresolved,
        }),
    )
}

// ── Main query handler ────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn handle_query(request: &HttpRequest, state: &ServerState) -> HttpResponse {
    let started = Instant::now();
    let query = match parse_json::<QueryVerbRequest>(&request.body) {
        Ok(query) => query,
        Err(error) => return HttpResponse::error(error),
    };

    let request_id = match non_empty(query.request_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => return HttpResponse::error(ApiError::missing_field("request_id")),
    };

    // Check temporal reservations before any verb dispatch
    if let Some(as_of) = &query.as_of {
        if as_of.transaction_time.is_some() {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::new(
                    ErrorCode::NotImplemented,
                    "as_of.transaction_time is reserved; transaction-time axis is not yet wired up",
                ),
            );
        }
        if as_of.since.is_some() {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::new(
                    ErrorCode::NotImplemented,
                    "as_of.since is reserved; range queries are not yet implemented",
                ),
            );
        }
    }

    if non_empty(query.domain.as_deref()).is_some_and(|d| {
        !matches!(
            d,
            "codegraph" | "agent_memory" | "verification" | "artifact" | "project" | "semantic"
        )
    }) {
        return HttpResponse::error_with_id(&request_id, ApiError::invalid_domain());
    }

    let domain = non_empty(query.domain.as_deref())
        .unwrap_or("codegraph")
        .to_owned();
    let as_of_valid_time = query
        .as_of
        .as_ref()
        .and_then(|a| a.valid_time.as_deref())
        .map(str::to_owned);

    let (limit, timeout_ms) =
        query
            .budget
            .as_ref()
            .map_or((DEFAULT_QUERY_MAX_RESULTS, None), |b| {
                (
                    b.max_results.unwrap_or(DEFAULT_QUERY_MAX_RESULTS),
                    b.timeout_ms,
                )
            });
    let limit = limit.min(DEFAULT_QUERY_MAX_RESULTS);
    let budget = timeout_ms
        .or(Some(DEFAULT_QUERY_TIMEOUT_MS))
        .map(Duration::from_millis);

    if budget == Some(Duration::ZERO) {
        return HttpResponse::error_with_id(&request_id, ApiError::query_timeout());
    }

    let verb = match non_empty(query.verb.as_deref()) {
        Some(v) => v.to_owned(),
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("verb"));
        }
    };

    let params = query
        .params
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    if !params.is_object() {
        return HttpResponse::error_with_id(
            &request_id,
            ApiError::bad_request("params must be a JSON object"),
        );
    }

    match verb.as_str() {
        "get_records" => {
            handle_verb_get_records(&request_id, &params, &domain, limit, started, budget, state)
        }
        "symbol_by_name" => handle_verb_symbol_by_name(
            &request_id,
            &params,
            as_of_valid_time.as_deref(),
            limit,
            started,
            budget,
            &domain,
            state,
        ),
        "symbol_at_commit" => handle_verb_symbol_at_commit(
            &request_id,
            &params,
            limit,
            started,
            budget,
            &domain,
            state,
        ),
        "file_defines" => handle_verb_file_defines(
            &request_id,
            &params,
            as_of_valid_time.as_deref(),
            limit,
            started,
            budget,
            &domain,
            state,
        ),
        "drift_top_n" => handle_verb_drift_top_n(
            &request_id,
            &params,
            as_of_valid_time.as_deref(),
            limit,
            started,
            budget,
            &domain,
            state,
        ),
        "observations_for_symbol" => handle_verb_observations_for_symbol(
            &request_id,
            &params,
            as_of_valid_time.as_deref(),
            limit,
            started,
            budget,
            state,
        ),
        "drift" | "agent_sessions_for_repo" | "criteria_for_task" => HttpResponse::error_with_id(
            &request_id,
            ApiError::new(
                ErrorCode::NotImplemented,
                format!("verb '{verb}' is reserved and not yet implemented"),
            ),
        ),
        _ => HttpResponse::error_with_id(
            &request_id,
            ApiError::bad_request_field(format!("unknown verb '{verb}'"), "verb"),
        ),
    }
}

fn check_query_budget(
    started: Instant,
    budget: Option<Duration>,
) -> std::result::Result<(), ApiError> {
    if budget.is_some_and(|budget| started.elapsed() >= budget) {
        return Err(ApiError::query_timeout());
    }
    Ok(())
}

fn query_sink_read(
    state: &ServerState,
    started: Instant,
    budget: Option<Duration>,
) -> std::result::Result<RwLockReadGuard<'_, EmbeddedAletheiaSink>, ApiError> {
    if budget.is_none() {
        return state
            .sink
            .read()
            .map_err(|_| ApiError::internal("embedded sink lock poisoned"));
    }

    loop {
        check_query_budget(started, budget)?;
        match state.sink.try_read() {
            Ok(sink) => return Ok(sink),
            Err(TryLockError::Poisoned(_)) => {
                return Err(ApiError::internal("embedded sink lock poisoned"));
            }
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(1)),
        }
    }
}

fn handle_agent_register(request: &HttpRequest, state: &ServerState) -> HttpResponse {
    let registration = match parse_json::<AgentRegisterRequest>(&request.body) {
        Ok(registration) => registration,
        Err(error) => return HttpResponse::error(error),
    };
    let request_id = match non_empty(registration.request_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => return HttpResponse::error(ApiError::missing_field("request_id")),
    };
    let agent_id = match non_empty(registration.agent_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("agent_id"));
        }
    };
    let session_id = match non_empty(registration.session_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("session_id"));
        }
    };
    let agent_kind = match non_empty(registration.agent_kind.as_deref()) {
        Some(kind) if VALID_AGENT_KINDS.contains(&kind) => kind.to_owned(),
        Some(kind) => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::bad_request(format!(
                    "agent_kind '{kind}' is not a recognized value; expected one of: {}",
                    VALID_AGENT_KINDS.join(", ")
                )),
            );
        }
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("agent_kind"));
        }
    };
    let project_scope = match non_empty(registration.project_scope.as_deref()) {
        Some(scope) => scope.to_owned(),
        None => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::missing_field("project_scope"),
            );
        }
    };
    let registered_at = match non_empty(registration.created_at.as_deref()) {
        Some(ts) if DateTime::parse_from_rfc3339(ts).is_err() => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::bad_request("created_at must be RFC 3339"),
            );
        }
        Some(ts) => ts.to_owned(),
        // created_at is required so that registration retries for the same
        // (agent_id, session_id) pair produce hash-stable records and correctly
        // hit the idempotency cache.
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("created_at"));
        }
    };

    let agent_status = AgentStatus {
        agent_id: agent_id.clone(),
        session_id: session_id.clone(),
        agent_kind: agent_kind.clone(),
        project_scope: project_scope.clone(),
        last_seen_unix_ms: unix_ms(),
    };
    if let Ok(mut agents) = state.agents.lock() {
        agents.insert(
            AgentSessionKey::new(agent_id.clone(), session_id.clone()),
            agent_status,
        );
    } else {
        return HttpResponse::error_with_id(
            &request_id,
            ApiError::internal("agents lock poisoned"),
        );
    }

    let reg = AgentRegisterFull {
        agent_id,
        session_id,
        agent_kind,
        project_scope,
        registered_at,
    };
    let records = agent_registration_records(&reg);
    let idempotency_key = stable_pair_key("agent-register", &reg.agent_id, &reg.session_id);
    match enqueue_write(state, idempotency_key, records, &request_id) {
        Ok(response) => HttpResponse::success(
            Some(&request_id),
            200,
            json!({
                "status": "registered",
                "record_ids": response.record_ids,
                "node_kinds": ["Agent", "AgentSession"],
            }),
        ),
        Err(error) => HttpResponse::error_with_id(&request_id, error),
    }
}

fn handle_agent_heartbeat(request: &HttpRequest, state: &ServerState) -> HttpResponse {
    let heartbeat = match parse_json::<AgentHeartbeatRequest>(&request.body) {
        Ok(heartbeat) => heartbeat,
        Err(error) => return HttpResponse::error(error),
    };
    let request_id = match non_empty(heartbeat.request_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => return HttpResponse::error(ApiError::missing_field("request_id")),
    };
    let agent_id = match non_empty(heartbeat.agent_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("agent_id"));
        }
    };
    let session_id = match non_empty(heartbeat.session_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("session_id"));
        }
    };
    let key = AgentSessionKey::new(agent_id, session_id);
    let Ok(mut agents) = state.agents.lock() else {
        return HttpResponse::error_with_id(
            &request_id,
            ApiError::internal("agents lock poisoned"),
        );
    };
    if let Some(agent) = agents.get_mut(&key) {
        agent.last_seen_unix_ms = unix_ms();
    }
    drop(agents);
    HttpResponse::success(Some(&request_id), 200, json!({ "status": "ok" }))
}

#[allow(clippy::too_many_lines)]
fn handle_job_ingest(request: &HttpRequest, state: &ServerState) -> HttpResponse {
    let envelope = match parse_json::<RequestEnvelope>(&request.body) {
        Ok(envelope) => envelope,
        Err(error) => return HttpResponse::error(error),
    };
    let request_id = match non_empty(envelope.request_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => return HttpResponse::error(ApiError::missing_field("request_id")),
    };
    let agent_id = match non_empty(envelope.agent_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("agent_id"));
        }
    };
    if non_empty(envelope.session_id.as_deref()).is_none() {
        return HttpResponse::error_with_id(&request_id, ApiError::missing_field("session_id"));
    }
    let idempotency_key = match non_empty(envelope.idempotency_key.as_deref()) {
        Some(key) => key.to_owned(),
        None => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::missing_field("idempotency_key"),
            );
        }
    };
    let domain = match non_empty(envelope.domain.as_deref()) {
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("domain"));
        }
        Some(d)
            if !matches!(
                d,
                "codegraph"
                    | "agent_memory"
                    | "verification"
                    | "artifact"
                    | "project"
                    | "semantic"
                    | "user_context"
            ) =>
        {
            return HttpResponse::error_with_id(&request_id, ApiError::invalid_domain());
        }
        Some(d) => d.to_owned(),
    };
    match envelope
        .created_at
        .as_deref()
        .and_then(|s| non_empty(Some(s)))
    {
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("created_at"));
        }
        Some(ts) if DateTime::parse_from_rfc3339(ts).is_err() => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::bad_request("created_at must be RFC 3339"),
            );
        }
        _ => {}
    }
    if envelope.payload.is_null() {
        return HttpResponse::error_with_id(&request_id, ApiError::missing_field("payload"));
    }
    let payload = match serde_json::from_value::<IngestPayload>(envelope.payload) {
        Ok(payload) => payload,
        Err(error) => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::bad_request(error.to_string()),
            );
        }
    };
    if let Some(bad) = payload
        .records
        .iter()
        .find(|r| !record_id_matches_domain(r.id(), &domain))
    {
        return HttpResponse::error_with_id(
            &request_id,
            ApiError::bad_request(format!(
                "record '{}' has ID inconsistent with domain '{domain}'",
                bad.id()
            )),
        );
    }
    let scoped_key = scoped_idempotency_key(&agent_id, "jobs/ingest", &idempotency_key);
    let payload_hash = match records_hash(&payload.records) {
        Ok(hash) => hash,
        Err(error) => {
            return HttpResponse::error_with_id(&request_id, ApiError::internal(error.to_string()));
        }
    };

    let job_id = stable_job_id(&scoped_key);
    let job = JobStatus {
        job_id: job_id.clone(),
        status: "queued".to_owned(),
        report: None,
        events: vec!["queued".to_owned()],
        payload_hash: payload_hash.clone(),
    };

    // Check persisted idempotency first to handle post-restart replays.
    let persisted = match state.idempotency.lock() {
        Ok(store) => store.entries.get(&scoped_key).cloned(),
        Err(_) => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::internal("idempotency lock poisoned"),
            );
        }
    };
    if let Some(entry) = persisted {
        if entry.payload_hash() != payload_hash {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::conflict("idempotency key reused with different payload"),
            );
        }
        // Rehydrate job into state.jobs so GET /v1/jobs/{id} works after restart.
        match &entry {
            IdempotencyEntry::Committed { response, .. } => {
                if let Ok(mut jobs) = state.jobs.lock() {
                    jobs.entry(job_id.clone()).or_insert_with(|| JobStatus {
                        job_id: job_id.clone(),
                        status: "completed".to_owned(),
                        report: Some(response.clone()),
                        events: vec![
                            "queued".to_owned(),
                            "started".to_owned(),
                            "completed".to_owned(),
                        ],
                        payload_hash: payload_hash.clone(),
                    });
                }
            }
            IdempotencyEntry::Pending { records, .. } => {
                if let Ok(mut jobs) = state.jobs.lock() {
                    jobs.entry(job_id.clone()).or_insert_with(|| JobStatus {
                        job_id: job_id.clone(),
                        status: "queued".to_owned(),
                        report: None,
                        events: vec!["queued".to_owned()],
                        payload_hash: payload_hash.clone(),
                    });
                }
                // Recover the uncommitted write in the background.
                let state_clone = state.clone();
                let records_clone = records.clone();
                let job_id_thread = job_id.clone();
                let scoped_key_thread = scoped_key;
                let request_id_thread = request_id.clone();
                thread::spawn(move || {
                    update_job(&state_clone, &job_id_thread, "running", "started", None);
                    let response = enqueue_write(
                        &state_clone,
                        scoped_key_thread,
                        records_clone,
                        &request_id_thread,
                    );
                    match response {
                        Ok(report) => update_job(
                            &state_clone,
                            &job_id_thread,
                            "completed",
                            "completed",
                            Some(report),
                        ),
                        Err(error) => {
                            let report = DaemonIngestResponse {
                                attempted: 0,
                                succeeded: 0,
                                failed: 1,
                                failures: vec![DaemonIngestFailure {
                                    record_id: job_id_thread.clone(),
                                    message: error.message,
                                }],
                                record_ids: Vec::new(),
                                idempotent: false,
                            };
                            update_job(
                                &state_clone,
                                &job_id_thread,
                                "failed",
                                "failed",
                                Some(report),
                            );
                        }
                    }
                });
            }
        }
        return HttpResponse::success(
            Some(&request_id),
            200,
            json!({ "job_id": job_id, "status": "queued" }),
        );
    }

    match state.jobs.lock() {
        Ok(mut jobs) => {
            if let Some(existing) = jobs.get(&job_id) {
                if existing.payload_hash != payload_hash {
                    return HttpResponse::error_with_id(
                        &request_id,
                        ApiError::conflict("idempotency key reused with different payload"),
                    );
                }
                // Return the original accepted status, not the current mutable status.
                return HttpResponse::success(
                    Some(&request_id),
                    200,
                    json!({ "job_id": existing.job_id, "status": "queued" }),
                );
            }
            jobs.insert(job_id.clone(), job);
        }
        Err(_) => {
            return HttpResponse::error_with_id(
                &request_id,
                ApiError::internal("jobs lock poisoned"),
            );
        }
    }

    let state = state.clone();
    let job_id_for_thread = job_id.clone();
    let request_id_for_thread = request_id.clone();
    thread::spawn(move || {
        update_job(&state, &job_id_for_thread, "running", "started", None);
        let response = enqueue_write(&state, scoped_key, payload.records, &request_id_for_thread);
        match response {
            Ok(report) => update_job(
                &state,
                &job_id_for_thread,
                "completed",
                "completed",
                Some(report),
            ),
            Err(error) => {
                let report = DaemonIngestResponse {
                    attempted: 0,
                    succeeded: 0,
                    failed: 1,
                    failures: vec![DaemonIngestFailure {
                        record_id: job_id_for_thread.clone(),
                        message: error.message,
                    }],
                    record_ids: Vec::new(),
                    idempotent: false,
                };
                update_job(&state, &job_id_for_thread, "failed", "failed", Some(report));
            }
        }
    });

    HttpResponse::success(
        Some(&request_id),
        202,
        json!({ "job_id": job_id, "status": "queued" }),
    )
}

fn handle_get_job(path: &str, state: &ServerState) -> HttpResponse {
    let suffix = path.trim_start_matches("/v1/jobs/");
    let (job_id, events_only) = suffix
        .strip_suffix("/events")
        .map_or((suffix, false), |job_id| (job_id, true));
    let Ok(jobs) = state.jobs.lock() else {
        return HttpResponse::error(ApiError::internal("jobs lock poisoned"));
    };
    let Some(job) = jobs.get(job_id) else {
        return HttpResponse::error(ApiError::not_found("job not found"));
    };
    let result = if events_only {
        json!({ "job_id": job.job_id, "events": job.events })
    } else {
        json!(job)
    };
    drop(jobs);
    HttpResponse::success(None, 200, result)
}

fn handle_checkpoint(state: &ServerState) -> HttpResponse {
    let Ok(sink) = state.sink.read() else {
        return HttpResponse::error(ApiError::internal("embedded sink lock poisoned"));
    };
    match sink.persist_indexes() {
        Ok(()) => HttpResponse::success(None, 200, json!({ "status": "checkpointed" })),
        Err(error) => HttpResponse::error(ApiError::internal(error.to_string())),
    }
}

fn enqueue_write(
    state: &ServerState,
    idempotency_key: String,
    records: Vec<GraphRecord>,
    request_id: &str,
) -> WriteResult {
    let payload_hash =
        records_hash(&records).map_err(|error| ApiError::internal(error.to_string()))?;
    let (response_tx, response_rx) = mpsc::channel();
    let command = WriteCommand {
        idempotency_key,
        payload_hash,
        records,
        response_tx,
    };
    // Count the in-flight write before the worker can observe it, so the
    // worker's matching `on_complete` can never underflow the depth counter.
    state.pressure.on_enqueue();
    match state.write_tx.try_send(command) {
        Ok(()) => response_rx.recv().map_err(|_| {
            ApiError::internal(format!("write worker dropped request {request_id}"))
        })?,
        Err(mpsc::TrySendError::Full(_)) => {
            state
                .pressure
                .on_reject_after_rollback("records/ingest", request_id);
            Err(ApiError::overloaded())
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            state.pressure.rollback_enqueue();
            Err(ApiError::internal("write worker disconnected"))
        }
    }
}

fn update_job(
    state: &ServerState,
    job_id: &str,
    status: &str,
    event: &str,
    report: Option<DaemonIngestResponse>,
) {
    if let Ok(mut jobs) = state.jobs.lock()
        && let Some(job) = jobs.get_mut(job_id)
    {
        status.clone_into(&mut job.status);
        job.events.push(event.to_owned());
        if let Some(report) = report {
            job.report = Some(report);
        }
    }
}

fn agent_registration_records(registration: &AgentRegisterFull) -> Vec<GraphRecord> {
    // Agent node ID is derived from (agent_id, agent_kind, project_scope) so the payload
    // is identical on every registration for the same combination.  Different agent_kind or
    // project_scope values produce different Agent node identities.
    let agent_node_id = agent_memory_stable_id(&[
        "node",
        "agent",
        &registration.agent_id,
        &registration.agent_kind,
        &registration.project_scope,
    ]);
    let session_node_id = agent_memory_stable_id(&[
        "node",
        "agent_session",
        &registration.agent_id,
        &registration.session_id,
    ]);
    let now = registration.registered_at.clone();
    let mut agent_node = GraphRecord::node(
        agent_node_id.clone(),
        NodeKind::Agent,
        None,
        None,
        Some(registration.agent_id.clone()),
        format!(
            "Agent {} ({}) scoped to {}",
            registration.agent_id, registration.agent_kind, registration.project_scope,
        ),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        agent_kind,
        confidence,
        ..
    } = &mut agent_node
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some(registration.agent_id.clone());
        *agent_kind = Some(registration.agent_kind.clone());
        // The Agent node represents a stable identity, so no session-specific or
        // time-varying fields are stored here; the payload must be identical on
        // every registration that shares the same agent_id.
        *confidence = Some("1.0".to_owned());
    }
    let mut session_node = GraphRecord::node(
        session_node_id.clone(),
        NodeKind::AgentSession,
        None,
        None,
        Some(registration.session_id.clone()),
        format!(
            "Session {} for agent {}",
            registration.session_id, registration.agent_id
        ),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        agent_kind,
        session_id,
        confidence,
        observed_at,
        ingested_at,
        ..
    } = &mut session_node
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some(registration.agent_id.clone());
        *agent_kind = Some(registration.agent_kind.clone());
        *session_id = Some(registration.session_id.clone());
        *confidence = Some("1.0".to_owned());
        *observed_at = Some(now.clone());
        *ingested_at = Some(now);
    }
    vec![
        agent_node,
        session_node,
        GraphRecord::agent_memory_edge(
            EdgeLabel::SessionOf,
            session_node_id,
            agent_node_id,
            Some("explicit".to_owned()),
            "Agent session belongs to agent".to_owned(),
        ),
    ]
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &[u8]) -> std::result::Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|error| ApiError::bad_request(error.to_string()))
}

fn is_authorized(request: &HttpRequest, token: &str) -> bool {
    headers_authorized(&request.headers, token)
}

fn headers_authorized(headers: &HashMap<String, String>, token: &str) -> bool {
    headers
        .get("authorization")
        .is_some_and(|header| header == &format!("Bearer {token}"))
}

fn read_http_request(stream: &mut TcpStream, token: &str) -> io::Result<HttpRequest> {
    let started = Instant::now();
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        set_request_read_timeout(stream, started)?;
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before headers",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > REQUEST_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        if let Some(index) = find_header_end(&buffer) {
            break index;
        }
    };

    let header_text = String::from_utf8(buffer[..header_end].to_vec())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing method"))?
        .to_owned();
    let path = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing path"))?
        .to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<HashMap<_, _>>();
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    if !headers_authorized(&headers, token) {
        return Ok(HttpRequest {
            method,
            path,
            headers,
            body: Vec::new(),
        });
    }
    if content_length > REQUEST_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request body too large",
        ));
    }
    let body_start = header_end + 4;
    let mut body = buffer[body_start..].to_vec();
    while body.len() < content_length {
        set_request_read_timeout(stream, started)?;
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before body",
            ));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn set_request_read_timeout(stream: &TcpStream, started: Instant) -> io::Result<()> {
    let remaining = REQUEST_READ_TIMEOUT
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(request_read_timed_out)?;
    stream.set_read_timeout(Some(remaining))
}

fn request_read_timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "request read timed out")
}

fn write_http_response(stream: &mut TcpStream, response: &HttpResponse) -> io::Result<()> {
    let body = serde_json::to_string(&response.body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let status_text = status_text(response.status);
    let response_text = format!(
        "HTTP/1.1 {} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        response.status,
        body.len()
    );
    stream.write_all(response_text.as_bytes())
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

const fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn parse_http_response(response: &str) -> Result<(u16, String)> {
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("invalid HTTP response"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow!("missing HTTP status"))?
        .parse::<u16>()
        .context("invalid HTTP status")?;
    Ok((status, body.to_owned()))
}

fn wait_until_running(data_dir: &Path) -> Result<DaemonMetadata> {
    let start = Instant::now();
    loop {
        if let Some(metadata) = active_metadata(data_dir)? {
            return Ok(metadata);
        }
        if start.elapsed() > START_TIMEOUT {
            return Err(anyhow!("daemon failed to start for {}", data_dir.display()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_until_stopped(data_dir: &Path) -> Result<()> {
    let start = Instant::now();
    loop {
        if active_metadata(data_dir)?.is_none() && store_is_unleased(data_dir)? {
            return Ok(());
        }
        if start.elapsed() > START_TIMEOUT {
            return Err(anyhow!("daemon did not stop for {}", data_dir.display()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn read_metadata(data_dir: &Path) -> Result<DaemonMetadata> {
    let path = metadata_path(data_dir);
    reject_runtime_symlink_components(&path, "runtime file")?;
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let metadata = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    validate_daemon_runtime_schema(&metadata, &path)?;
    Ok(metadata)
}

fn validate_daemon_runtime_schema(metadata: &DaemonMetadata, path: &Path) -> Result<()> {
    if metadata.schema_version != DAEMON_RUNTIME_SCHEMA_VERSION {
        return Err(anyhow!(
            "unsupported daemon runtime schema_version {} in {}; only {} is supported",
            metadata.schema_version,
            path.display(),
            DAEMON_RUNTIME_SCHEMA_VERSION
        ));
    }
    Ok(())
}

fn write_metadata(data_dir: &Path, metadata: &DaemonMetadata) -> Result<()> {
    let path = metadata_path(data_dir);
    let json = serde_json::to_vec_pretty(metadata)?;
    atomic_write(&path, &json)
}

fn metadata_path(data_dir: &Path) -> PathBuf {
    runtime_dir(data_dir).join(METADATA_FILE)
}

/// Reads daemon metadata without staleness validation.
///
/// Returns `None` if the metadata file does not exist.
/// Unlike `read_metadata`, this does not reject stale or crashed metadata.
/// It is intended for diagnostic and repair use only.
///
/// # Errors
///
/// Returns an error if the metadata file exists but cannot be read or parsed.
pub fn try_read_raw_metadata(data_dir: &Path) -> Result<Option<DaemonMetadata>> {
    let path = metadata_path(data_dir);
    reject_runtime_symlink_components(&path, "runtime file")?;
    match fs::read_to_string(&path) {
        Ok(contents) => {
            let metadata = serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Returns the v1 runtime sidecar directory for a daemon data directory.
#[must_use]
pub fn runtime_dir_for_data_dir(data_dir: &Path) -> PathBuf {
    runtime_dir(data_dir)
}

/// Returns true when metadata exists but the daemon lock is not held.
///
/// Clients use this as the cheap stale-file check before trusting
/// `egregored.json` connection fields.
///
/// # Errors
///
/// Returns an error if the runtime lock file cannot be opened or inspected.
pub fn runtime_metadata_is_stale(data_dir: &Path) -> Result<bool> {
    let metadata_path = metadata_path(data_dir);
    reject_runtime_symlink_components(&metadata_path, "runtime file")?;
    if !metadata_path.exists() {
        return Ok(false);
    }
    let path = lock_path(data_dir)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    reject_runtime_symlink(&path, "runtime file")?;
    let file = options
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    enforce_runtime_file_permissions(&path)?;
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(true)
        }
        Err(error) if lock_error_is_contention(&error) => Ok(false),
        Err(error) => Err(io::Error::from(error))
            .with_context(|| format!("failed to inspect runtime lock {}", path.display())),
    }
}

const fn lock_error_is_contention(error: &FileTryLockError) -> bool {
    matches!(error, FileTryLockError::WouldBlock)
}

/// Discovers the runtime dir for a client that starts with a working directory.
///
/// `data_dir_env` represents the already-read `EGREGORE_DATA_DIR` override.
/// Passing `None` runs the walk-up discovery algorithm.
///
/// # Errors
///
/// Returns an error when no runtime directory is found. The error includes the
/// candidate paths that were tried.
pub fn discover_runtime_dir_for_working_dir(
    working_dir: &Path,
    data_dir_env: Option<&Path>,
) -> Result<PathBuf> {
    let mut tried = Vec::new();
    if let Some(data_dir) = data_dir_env {
        let runtime = runtime_dir(data_dir);
        tried.push(runtime.clone());
        if runtime_dir_is_plain_dir(&runtime) {
            return Ok(runtime);
        }
        return Err(no_runtime_dir_error(working_dir, &tried));
    }

    let mut cursor = if working_dir.is_dir() {
        working_dir.to_path_buf()
    } else {
        working_dir
            .parent()
            .map_or_else(|| working_dir.to_path_buf(), Path::to_path_buf)
    };

    loop {
        let direct_runtime = runtime_dir(&cursor);
        tried.push(direct_runtime.clone());
        if runtime_dir_is_plain_dir(&direct_runtime) {
            return Ok(direct_runtime);
        }

        let default_data_dir = cursor.join(".egregore");
        let default_runtime = runtime_dir(&default_data_dir);
        tried.push(default_runtime.clone());
        if runtime_dir_is_plain_dir(&default_runtime) {
            return Ok(default_runtime);
        }

        let Some(parent) = cursor.parent() else {
            break;
        };
        if !same_mount(&cursor, parent)? {
            break;
        }
        cursor = parent.to_path_buf();
    }

    Err(no_runtime_dir_error(working_dir, &tried))
}

/// Discovers the runtime dir using `EGREGORE_DATA_DIR` and then walk-up rules.
///
/// # Errors
///
/// Returns an error when no daemon runtime dir is discoverable.
pub fn discover_runtime_dir(working_dir: &Path) -> Result<PathBuf> {
    let env_data_dir = std::env::var_os(EGREGORE_DATA_DIR_ENV).map(PathBuf::from);
    discover_runtime_dir_for_working_dir(working_dir, env_data_dir.as_deref())
}

fn no_runtime_dir_error(working_dir: &Path, tried: &[PathBuf]) -> anyhow::Error {
    let tried = tried
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow!(
        "no daemon for this directory {}; tried runtime dirs: {}",
        working_dir.display(),
        tried
    )
}

fn stale_metadata_error(data_dir: &Path) -> anyhow::Error {
    anyhow!(
        "daemon metadata is stale for {}; runtime lock is not held",
        data_dir.display()
    )
}

fn runtime_dir(data_dir: &Path) -> PathBuf {
    let data_dir = store_identity_dir(data_dir);
    data_dir.file_name().map_or_else(
        || data_dir.join(RUNTIME_DIR_SUFFIX),
        |file_name| {
            let mut runtime_name = file_name.to_os_string();
            runtime_name.push(RUNTIME_DIR_SUFFIX);
            data_dir.with_file_name(runtime_name)
        },
    )
}

fn store_identity_dir(data_dir: &Path) -> PathBuf {
    if let Ok(canonical) = data_dir.canonicalize() {
        return canonical;
    }
    if let (Some(parent), Some(file_name)) = (data_dir.parent(), data_dir.file_name())
        && let Ok(canonical_parent) = parent.canonicalize()
    {
        return canonical_parent.join(file_name);
    }
    data_dir.to_path_buf()
}

fn store_identity_text(data_dir: &Path) -> String {
    store_identity_dir(data_dir).to_string_lossy().into_owned()
}

fn runtime_dir_is_plain_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn reject_runtime_symlink_components(path: &Path, kind: &str) -> Result<()> {
    let mut ancestors = path.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(anyhow!(
                        "runtime_permissions_unsafe: {kind} {} contains symlink component {}",
                        path.display(),
                        ancestor.display()
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", ancestor.display()));
            }
        }
    }
    Ok(())
}

fn reject_runtime_symlink(path: &Path, kind: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(anyhow!(
            "runtime_permissions_unsafe: {kind} {} is a symlink",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn ensure_runtime_dir(data_dir: &Path) -> Result<PathBuf> {
    let runtime_dir = runtime_dir(data_dir);
    reject_runtime_symlink_components(&runtime_dir, "runtime dir")?;
    fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("failed to create {}", runtime_dir.display()))?;
    enforce_runtime_dir_permissions(&runtime_dir)?;
    Ok(runtime_dir)
}

#[cfg(unix)]
fn enforce_runtime_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    reject_runtime_symlink(path, "runtime dir")?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "runtime_permissions_unsafe: failed to restrict runtime dir {}",
            path.display()
        )
    })?;
    let mode = fs::metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o700 {
        return Err(anyhow!(
            "runtime_permissions_unsafe: {} has mode {:o}, expected 700",
            path.display(),
            mode
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
fn enforce_runtime_dir_permissions(path: &Path) -> Result<()> {
    reject_runtime_symlink(path, "runtime dir")?;
    // TODO(windows-acl-runtime-permissions): set the runtime directory ACL to
    // the current user only, then fail startup with runtime_permissions_unsafe
    // if the ACL cannot be enforced. The Windows conformance test asserts this
    // named gap so the bearer-token leak surface is not silent.
    Ok(())
}

#[cfg(unix)]
fn enforce_runtime_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    reject_runtime_symlink(path, "runtime file")?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "runtime_permissions_unsafe: failed to restrict runtime file {}",
            path.display()
        )
    })?;
    let mode = fs::metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(anyhow!(
            "runtime_permissions_unsafe: {} has mode {:o}, expected 600",
            path.display(),
            mode
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
fn enforce_runtime_file_permissions(path: &Path) -> Result<()> {
    reject_runtime_symlink(path, "runtime file")?;
    // TODO(windows-acl-runtime-permissions): set the lock, metadata, and
    // idempotency journal ACL to the current user only, then fail startup with
    // runtime_permissions_unsafe if the ACL cannot be enforced.
    Ok(())
}

#[cfg(windows)]
/// Explicit Windows ACL enforcement gap for issue #18.
pub const WINDOWS_RUNTIME_ACL_TODO: &str =
    "TODO(windows-acl-runtime-permissions): restrict egregored runtime files to the current user";

#[cfg(unix)]
fn same_mount(child: &Path, parent: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let child_dev = fs::metadata(child)
        .with_context(|| format!("failed to inspect {}", child.display()))?
        .dev();
    let parent_dev = fs::metadata(parent)
        .with_context(|| format!("failed to inspect {}", parent.display()))?
        .dev();
    Ok(child_dev == parent_dev)
}

#[cfg(windows)]
fn same_mount(child: &Path, parent: &Path) -> Result<bool> {
    let child_canonical = child
        .canonicalize()
        .with_context(|| format!("failed to inspect {}", child.display()))?;
    let parent_canonical = parent
        .canonicalize()
        .with_context(|| format!("failed to inspect {}", parent.display()))?;
    Ok(same_mount_canonical_paths(
        &child_canonical,
        &parent_canonical,
    ))
}

#[cfg(windows)]
fn same_mount_canonical_paths(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

#[cfg(not(any(unix, windows)))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
fn same_mount(_child: &Path, _parent: &Path) -> Result<bool> {
    Ok(true)
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        reject_runtime_symlink_components(parent, "runtime dir")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        enforce_runtime_dir_permissions(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    reject_runtime_symlink(path, "runtime file")?;
    reject_runtime_symlink(&tmp, "runtime file")?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", tmp.display()))?;
    }
    enforce_runtime_file_permissions(&tmp)?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} to {}", tmp.display(), path.display()))?;
    enforce_runtime_file_permissions(path)
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(token, "{byte:02x}");
    }
    token
}

fn records_hash(records: &[GraphRecord]) -> Result<String> {
    let bytes = serde_json::to_vec(records)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn request_id(prefix: &str, value: &str) -> String {
    let input = format!("{prefix}:{value}:{}", unix_ms());
    format!("{prefix}-{}", blake3::hash(input.as_bytes()).to_hex())
}

fn stable_job_id(scoped_idempotency_key: &str) -> String {
    format!(
        "job-{}",
        blake3::hash(scoped_idempotency_key.as_bytes()).to_hex()
    )
}

fn scoped_idempotency_key(agent_id: &str, route: &str, idempotency_key: &str) -> String {
    stable_triple_key("idempotency", agent_id, route, idempotency_key)
}

fn stable_triple_key(prefix: &str, first: &str, second: &str, third: &str) -> String {
    let mut key =
        String::with_capacity(prefix.len() + first.len() + second.len() + third.len() + 48);
    let _ = write!(
        &mut key,
        "{prefix}:{}:{}:{}:",
        first.len(),
        second.len(),
        third.len()
    );
    key.push_str(first);
    key.push_str(second);
    key.push_str(third);
    key
}

fn stable_pair_key(prefix: &str, left: &str, right: &str) -> String {
    let mut key = String::with_capacity(prefix.len() + left.len() + right.len() + 32);
    let _ = write!(&mut key, "{prefix}:{}:{}:", left.len(), right.len());
    key.push_str(left);
    key.push_str(right);
    key
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_read_uses_total_deadline_for_slow_headers() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").context("test listener should bind")?;
        let address = listener
            .local_addr()
            .context("test listener should have a local address")?;
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            let started = Instant::now();
            let result = read_http_request(&mut stream, "test-token");
            (started.elapsed(), result.map_err(|error| error.kind()))
        });
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("client should connect");
            for byte in b"POST /v1/" {
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(400));
            }
        });

        let (elapsed, result) = server
            .join()
            .expect("server request reader should finish cleanly");
        client
            .join()
            .expect("slow client writer should finish cleanly");

        assert!(
            matches!(result, Err(io::ErrorKind::TimedOut)),
            "slow request should time out, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "slow request headers should be bounded by a total read deadline"
        );
        Ok(())
    }

    #[test]
    fn query_timeout_includes_waiting_for_sink_read_lock() -> Result<()> {
        let temp = tempfile::tempdir().context("temp dir should be created")?;
        let sink = Arc::new(RwLock::new(
            EmbeddedAletheiaSink::open(temp.path()).map_err(|error| anyhow!(error.to_string()))?,
        ));
        let write_guard = sink
            .write()
            .map_err(|_| anyhow!("embedded sink lock poisoned"))?;
        let (write_tx, _write_rx) = mpsc::sync_channel(1);
        let idempotency_path = temp.path().join("idempotency.json");
        let idempotency = Arc::new(Mutex::new(
            IdempotencyStore::load(idempotency_path).context("idempotency store")?,
        ));
        let state = ServerState {
            token: "test-token".to_owned(),
            store_identity: store_identity_text(temp.path()),
            sink: Arc::clone(&sink),
            write_tx,
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            agents: Arc::new(Mutex::new(BTreeMap::new())),
            idempotency,
            shutdown: Arc::new(AtomicBool::new(false)),
            pressure: Arc::new(PressureTracker::new(1)),
        };
        let request = HttpRequest {
            method: "POST".to_owned(),
            path: "/v1/query".to_owned(),
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "request_id": "locked-query",
                "agent_id": "test-agent",
                "verb": "get_records",
                "params": { "record_ids": ["codegraph:v3:missing"] },
                "budget": { "timeout_ms": 1_u64 }
            }))?,
        };

        let started = Instant::now();
        let response = handle_query(&request, &state);
        assert_eq!(response.status, 408);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "query timeout should include waiting for the read lock"
        );
        drop(write_guard);
        Ok(())
    }

    #[test]
    fn committed_idempotency_replay_precedes_schema_version_validation() -> Result<()> {
        let temp = tempfile::tempdir().context("temp dir should be created")?;
        let sink = Arc::new(RwLock::new(
            EmbeddedAletheiaSink::open(temp.path()).map_err(|error| anyhow!(error.to_string()))?,
        ));
        let record = GraphRecord::node(
            "codegraph:v3:committed-schema-replay".to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some("repo".to_owned()),
            "committed replay from a future schema".to_owned(),
        )
        .with_domain("codegraph", crate::ir::SCHEMA_VERSION + 1);
        let payload_hash = records_hash(std::slice::from_ref(&record))?;
        let cached_response = DaemonIngestResponse {
            attempted: 1,
            succeeded: 1,
            failed: 0,
            failures: Vec::new(),
            record_ids: vec![record.id().to_owned()],
            idempotent: false,
        };
        let idempotency = Arc::new(Mutex::new(IdempotencyStore {
            path: temp.path().join("idempotency.json"),
            entries: BTreeMap::from([(
                "committed-key".to_owned(),
                IdempotencyEntry::Committed {
                    payload_hash: payload_hash.clone(),
                    response: cached_response,
                },
            )]),
        }));
        let (response_tx, response_rx) = mpsc::channel();
        let command = WriteCommand {
            idempotency_key: "committed-key".to_owned(),
            payload_hash,
            records: vec![record],
            response_tx,
        };

        let response =
            apply_write(&command, &sink, &idempotency).map_err(|error| anyhow!(error.message))?;

        assert_eq!(response.succeeded, 1);
        assert!(response.idempotent);
        drop(response_rx);
        Ok(())
    }

    #[test]
    fn read_routes_preserve_unknown_schema_version_errors() -> Result<()> {
        let temp = tempfile::tempdir().context("temp dir should be created")?;
        let record_id = "codegraph:v3:future-read-node".to_owned();
        let record = GraphRecord::node(
            record_id.clone(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some("future_read_node".to_owned()),
            "read path future schema fixture".to_owned(),
        );
        let mut raw_sink =
            EmbeddedAletheiaSink::open(temp.path()).map_err(|error| anyhow!(error.to_string()))?;
        let report = ingest_records(std::slice::from_ref(&record), &mut raw_sink);
        assert!(report.is_success(), "{report:?}");
        raw_sink
            .force_latest_node_schema_version_for_test(&record_id, crate::ir::SCHEMA_VERSION + 1)
            .map_err(|error| anyhow!(error.to_string()))?;
        let sink = Arc::new(RwLock::new(raw_sink));
        let (write_tx, _write_rx) = mpsc::sync_channel(1);
        let idempotency = Arc::new(Mutex::new(IdempotencyStore {
            path: temp.path().join("idempotency.json"),
            entries: BTreeMap::new(),
        }));
        let state = ServerState {
            token: "test-token".to_owned(),
            store_identity: store_identity_text(temp.path()),
            sink,
            write_tx,
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            agents: Arc::new(Mutex::new(BTreeMap::new())),
            idempotency,
            shutdown: Arc::new(AtomicBool::new(false)),
            pressure: Arc::new(PressureTracker::new(1)),
        };

        let read_response = handle_get_record(&record_id, &state);
        assert_eq!(read_response.status, 422);
        assert_eq!(
            read_response.body["error"]["code"],
            UNKNOWN_SCHEMA_VERSION_CODE
        );

        let query_request = HttpRequest {
            method: "POST".to_owned(),
            path: "/v1/query".to_owned(),
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "request_id": "future-read-query",
                "agent_id": "test-agent",
                "verb": "get_records",
                "params": { "record_ids": [record_id] }
            }))?,
        };
        let query_response = handle_query(&query_request, &state);
        assert_eq!(query_response.status, 422);
        assert_eq!(
            query_response.body["error"]["code"],
            UNKNOWN_SCHEMA_VERSION_CODE
        );
        Ok(())
    }

    #[test]
    fn lock_contention_classifier_does_not_hide_other_io_errors() {
        assert!(lock_error_is_contention(&FileTryLockError::WouldBlock));
        assert!(!lock_error_is_contention(&FileTryLockError::Error(
            io::Error::from(io::ErrorKind::PermissionDenied)
        )));
        assert!(!lock_error_is_contention(&FileTryLockError::Error(
            io::Error::from(io::ErrorKind::Unsupported)
        )));
    }

    #[test]
    fn stopped_metadata_is_not_rewritten_as_crashed_during_start_preflight() -> Result<()> {
        let temp = tempfile::tempdir().context("temp dir should be created")?;
        let data_dir = temp.path().join("store");
        let metadata = DaemonMetadata {
            schema_version: DAEMON_RUNTIME_SCHEMA_VERSION,
            pid: 999_990,
            address: "127.0.0.1:9".to_owned(),
            token: "stopped-token".to_owned(),
            data_dir: store_identity_dir(&data_dir),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            started_at_unix_ms: 0,
            state: DaemonState::Stopped,
            api_version: None,
            transports: None,
            token_expires_at_unix_ms: None,
            daemons_index_url: None,
        };
        write_metadata(&data_dir, &metadata)?;

        assert!(mark_metadata_crashed_if_store_unleased(&data_dir)?);

        let metadata = read_metadata(&data_dir)?;
        assert_eq!(
            metadata.state,
            DaemonState::Stopped,
            "gracefully stopped metadata must remain stopped during preflight"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn foreground_daemon_preserves_non_contention_lease_errors() -> Result<()> {
        let temp = tempfile::tempdir().context("temp dir should be created")?;
        let data_dir = temp.path().join("store");
        let metadata = DaemonMetadata {
            schema_version: DAEMON_RUNTIME_SCHEMA_VERSION,
            pid: 999_989,
            address: "127.0.0.1:9".to_owned(),
            token: "unsafe-lock-token".to_owned(),
            data_dir: store_identity_dir(&data_dir),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            started_at_unix_ms: 0,
            state: DaemonState::Running,
            api_version: None,
            transports: None,
            token_expires_at_unix_ms: None,
            daemons_index_url: None,
        };
        write_metadata(&data_dir, &metadata)?;
        let runtime_dir = runtime_dir(&data_dir);
        let target = temp.path().join("target-lock");
        fs::write(&target, "target").context("lock target should write")?;
        std::os::unix::fs::symlink(&target, runtime_dir.join(LOCK_FILE))
            .context("lock symlink should be created")?;

        let error = run_foreground(&DaemonConfig::new(data_dir))
            .expect_err("unsafe runtime lock error should abort foreground startup");
        assert!(
            error.to_string().contains("runtime_permissions_unsafe"),
            "foreground startup should preserve the lock acquisition error, got {error:#}"
        );
        assert!(
            !error.to_string().contains("daemon already running for"),
            "non-contention lock errors must not be rewritten as already-running: {error:#}"
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_mount_identity_detects_canonical_parent_boundary() {
        assert!(same_mount_canonical_paths(
            Path::new(r"C:\repo\mounted\work"),
            Path::new(r"C:\repo\mounted")
        ));
        assert!(!same_mount_canonical_paths(
            Path::new(r"D:\mounted-target\work"),
            Path::new(r"C:\repo")
        ));
    }

    #[test]
    fn cross_key_exact_current_node_replay_does_not_duplicate_observation() -> Result<()> {
        let temp = tempfile::tempdir().context("temp dir should be created")?;
        let sink = Arc::new(RwLock::new(
            EmbeddedAletheiaSink::open(temp.path()).map_err(|error| anyhow!(error.to_string()))?,
        ));
        let idempotency = Arc::new(Mutex::new(IdempotencyStore {
            path: temp.path().join("idempotency.json"),
            entries: BTreeMap::new(),
        }));
        let record = GraphRecord::node(
            "codegraph:v3:cross-key-current-node".to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some("repo".to_owned()),
            "same current node".to_owned(),
        );

        for idempotency_key in ["first-key", "second-key"] {
            let (response_tx, response_rx) = mpsc::channel();
            let command = WriteCommand {
                idempotency_key: idempotency_key.to_owned(),
                payload_hash: records_hash(std::slice::from_ref(&record))?,
                records: vec![record.clone()],
                response_tx,
            };
            let response = apply_write(&command, &sink, &idempotency)
                .map_err(|error| anyhow!(error.message))?;
            assert_eq!(response.succeeded, 1);
            drop(response_rx);
        }

        {
            let sink = sink
                .read()
                .map_err(|_| anyhow!("embedded sink lock poisoned"))?;
            assert_eq!(
                sink.node_observation_count_for_test("codegraph:v3:cross-key-current-node"),
                1
            );
            drop(sink);
        }
        Ok(())
    }

    #[test]
    fn cross_key_exact_edge_replay_does_not_duplicate_observation() -> Result<()> {
        let temp = tempfile::tempdir().context("temp dir should be created")?;
        let sink = Arc::new(RwLock::new(
            EmbeddedAletheiaSink::open(temp.path()).map_err(|error| anyhow!(error.to_string()))?,
        ));
        let idempotency = Arc::new(Mutex::new(IdempotencyStore {
            path: temp.path().join("idempotency.json"),
            entries: BTreeMap::new(),
        }));
        let file_id = "codegraph:v3:cross-key-edge-file".to_owned();
        let symbol_id = "codegraph:v3:cross-key-edge-symbol".to_owned();
        let edge = GraphRecord::edge(
            EdgeLabel::Defines,
            file_id.clone(),
            symbol_id.clone(),
            Some("1.0".to_owned()),
            "same current edge".to_owned(),
        );
        let records = vec![
            GraphRecord::node(
                file_id,
                NodeKind::File,
                Some("src/lib.rs".to_owned()),
                None,
                Some("src/lib.rs".to_owned()),
                "file endpoint".to_owned(),
            ),
            GraphRecord::node(
                symbol_id,
                NodeKind::Symbol,
                Some("src/lib.rs".to_owned()),
                None,
                Some("stable".to_owned()),
                "symbol endpoint".to_owned(),
            ),
            edge.clone(),
        ];

        for idempotency_key in ["first-key", "second-key"] {
            let (response_tx, response_rx) = mpsc::channel();
            let command = WriteCommand {
                idempotency_key: idempotency_key.to_owned(),
                payload_hash: records_hash(&records)?,
                records: records.clone(),
                response_tx,
            };
            let response = apply_write(&command, &sink, &idempotency)
                .map_err(|error| anyhow!(error.message))?;
            assert_eq!(response.succeeded, 3);
            drop(response_rx);
        }

        {
            let sink = sink
                .read()
                .map_err(|_| anyhow!("embedded sink lock poisoned"))?;
            assert_eq!(sink.edge_observation_count_for_test(edge.id()), 1);
            drop(sink);
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn build_pressure_test_state(
        temp: &Path,
        capacity: usize,
    ) -> Result<(
        ServerState,
        mpsc::SyncSender<WriteCommand>,
        mpsc::Receiver<WriteCommand>,
    )> {
        let sink = Arc::new(RwLock::new(
            EmbeddedAletheiaSink::open(temp).map_err(|error| anyhow!(error.to_string()))?,
        ));
        let (write_tx, write_rx) = mpsc::sync_channel(capacity);
        let idempotency = Arc::new(Mutex::new(IdempotencyStore {
            path: temp.join("idempotency.json"),
            entries: BTreeMap::new(),
        }));
        let state = ServerState {
            token: "test-token".to_owned(),
            store_identity: store_identity_text(temp),
            sink,
            write_tx: write_tx.clone(),
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            agents: Arc::new(Mutex::new(BTreeMap::new())),
            idempotency,
            shutdown: Arc::new(AtomicBool::new(false)),
            pressure: Arc::new(PressureTracker::new(capacity)),
        };
        // The caller keeps `write_rx` alive so the bounded channel reports
        // `Full` (not `Disconnected`) once its buffer fills.
        Ok((state, write_tx, write_rx))
    }

    fn dummy_write_command() -> WriteCommand {
        let (response_tx, _response_rx) = mpsc::channel();
        WriteCommand {
            idempotency_key: "queue-filler".to_owned(),
            payload_hash: "queue-filler-hash".to_owned(),
            records: Vec::new(),
            response_tx,
        }
    }

    #[test]
    fn pressure_tracker_reports_idle_busy_saturated_and_recovers() {
        let tracker = PressureTracker::new(2); // recovery watermark = 1
        assert_eq!(tracker.state(), PressureState::Idle);

        // Fill the queue to capacity so a rejection reflects a genuinely full
        // queue (in flight above the recovery watermark).
        tracker.on_enqueue();
        tracker.on_enqueue();
        assert_eq!(tracker.state(), PressureState::Busy);

        // First rejection raises saturation and emits exactly one entry event.
        tracker.on_reject("records/ingest", "req-1");
        assert_eq!(tracker.state(), PressureState::Saturated);
        // A second rejection while already saturated bumps the counter but does
        // not emit a duplicate transition event.
        tracker.on_reject("records/ingest", "req-2");
        assert_eq!(tracker.total_rejections.load(Ordering::SeqCst), 2);
        assert_eq!(tracker.saturation_transitions.load(Ordering::SeqCst), 1);

        let entry_events: Vec<_> = tracker
            .events_snapshot()
            .into_iter()
            .filter(|event| event.transition == "entered_saturation")
            .collect();
        assert_eq!(
            entry_events.len(),
            1,
            "exactly one entry event per saturation"
        );
        assert_eq!(entry_events[0].code, "queue_full");
        assert_eq!(entry_events[0].operation, "records/ingest");
        assert_eq!(entry_events[0].request_id, "req-1");

        // Drain below the recovery watermark: saturation clears and one exit
        // event is emitted.
        tracker.on_complete();
        assert_ne!(tracker.state(), PressureState::Saturated);
        let exit_events = tracker
            .events_snapshot()
            .into_iter()
            .filter(|event| event.transition == "exited_saturation")
            .count();
        assert_eq!(exit_events, 1, "exactly one exit event per recovery");
        assert!(tracker.last_recovered_at_unix_ms.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn rejection_after_queue_drains_does_not_stick_saturated() {
        // Models the race where the worker finishes the last admitted writes in
        // the gap between a failed `try_send` and recording the rejection: the
        // drain's `on_complete` sees `saturated == false`, then the late
        // rejection sets `saturated` with nothing in flight. Status must not
        // stay `saturated` with a drained queue.
        let tracker = PressureTracker::new(2);
        tracker.on_enqueue(); // admitted write A
        tracker.on_enqueue(); // admitted write B (queue full)
        tracker.on_enqueue(); // rejected write C's speculative count
        // Worker drains A and B before C's rejection is recorded.
        tracker.on_complete();
        tracker.on_complete();
        // C's rejection is now recorded (rolling back its speculative count).
        tracker.on_reject_after_rollback("records/ingest", "req-late");

        assert_eq!(
            tracker.state(),
            PressureState::Idle,
            "a drained queue must not remain saturated after a late rejection"
        );
        assert_eq!(
            tracker.total_rejections.load(Ordering::SeqCst),
            1,
            "the rejection is still counted even though pressure recovered"
        );
    }

    #[test]
    fn pressure_event_request_handle_is_length_bounded() {
        let tracker = PressureTracker::new(2);
        // Fill to capacity so the rejection keeps the daemon saturated and the
        // entry event is retained.
        tracker.on_enqueue();
        tracker.on_enqueue();
        let huge = "x".repeat(10_000);
        tracker.on_reject("records/ingest", &huge);

        let events = tracker.events_snapshot();
        let entry = events
            .iter()
            .find(|event| event.transition == "entered_saturation")
            .expect("rejection must record an entry event");
        assert!(
            entry.request_id.len() < huge.len(),
            "oversized request handle must be truncated"
        );
        assert!(
            entry.request_id.len() <= PRESSURE_REQUEST_HANDLE_MAX_BYTES + "…".len(),
            "retained request handle must stay bounded, got {} bytes",
            entry.request_id.len()
        );
    }

    #[test]
    fn pressure_event_buffer_is_bounded() {
        let tracker = PressureTracker::new(2);
        for index in 0..(PRESSURE_EVENT_CAPACITY * 2) {
            // Force a full enter/exit cycle each iteration.
            tracker.on_enqueue();
            tracker.on_reject("records/ingest", &format!("req-{index}"));
            tracker.on_complete();
        }
        assert!(
            tracker.events_snapshot().len() <= PRESSURE_EVENT_CAPACITY,
            "pressure event buffer must stay bounded"
        );
    }

    #[test]
    fn pressure_contract_observed_in_one_local_run() -> Result<()> {
        // Single local run, no network: idle status, induced saturation, an
        // overloaded-write rejection, a structured diagnostic event with no
        // payload, and recovery back to a non-saturated state.
        let temp = tempfile::tempdir().context("temp dir should be created")?;
        let capacity = 2;
        let (state, write_tx, _write_rx) = build_pressure_test_state(temp.path(), capacity)?;

        // (1) Idle status surfaces api_version, store identity, and pressure.
        let idle = handle_status(&state);
        assert_eq!(idle.status, 200);
        assert_eq!(idle.body["api_version"], "v1");
        assert_eq!(idle.body["data_dir"], store_identity_text(temp.path()));
        assert_eq!(idle.body["pressure"]["state"], "idle");
        assert_eq!(idle.body["pressure"]["alive"], true);
        assert_eq!(idle.body["pressure"]["queue_capacity"], capacity);

        // Fill the bounded queue so the next admission is rejected.
        for _ in 0..capacity {
            state.pressure.on_enqueue();
            write_tx
                .try_send(dummy_write_command())
                .map_err(|_| anyhow!("queue filler should buffer"))?;
        }
        assert_eq!(handle_status(&state).body["pressure"]["state"], "busy");

        // (3) An overloaded write is rejected with queue_full + retry_after_ms.
        let payload_summary = "SECRET-PAYLOAD-BODY-must-not-leak";
        let record = GraphRecord::node(
            "codegraph:v3:pressure-overflow-node".to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some("repo".to_owned()),
            payload_summary.to_owned(),
        );
        let rejection = enqueue_write(
            &state,
            "pressure-overflow-key".to_owned(),
            vec![record],
            "req-overload",
        )
        .expect_err("overloaded write must be rejected");
        assert_eq!(rejection.code, ErrorCode::QueueFull);
        assert_eq!(rejection.status, 429);
        assert!(
            rejection.retry_after_ms.is_some_and(|ms| ms > 0),
            "queue_full must carry a positive retry_after_ms"
        );

        // (2) Saturated status is observable within 500 ms of the rejection.
        let measured = Instant::now();
        let saturated = handle_status(&state);
        assert!(
            measured.elapsed() < Duration::from_millis(500),
            "saturated status must classify within 500 ms"
        );
        assert_eq!(saturated.body["pressure"]["state"], "saturated");
        assert_eq!(saturated.body["pressure"]["alive"], true);
        assert!(
            saturated.body["pressure"]["retry_after_ms"]
                .as_u64()
                .is_some_and(|ms| ms > 0),
            "saturated status must advertise retry guidance"
        );
        assert_eq!(saturated.body["pressure"]["total_rejections"], 1);

        // (5) A structured diagnostic event marks the saturation transition and
        // carries no submitted payload body.
        let events = state.pressure.events_snapshot();
        let entry = events
            .iter()
            .find(|event| event.transition == "entered_saturation")
            .expect("entry into saturation must emit a diagnostic event");
        assert_eq!(entry.code, "queue_full");
        assert_eq!(entry.operation, "records/ingest");
        assert_eq!(entry.request_id, "req-overload");
        let serialized = serde_json::to_string(&saturated.body)?;
        assert!(
            !serialized.contains(payload_summary),
            "no diagnostic output may echo submitted payload bodies"
        );

        // (7) Recovery: draining the backlog returns to a non-saturated state.
        for _ in 0..capacity {
            state.pressure.on_complete();
        }
        let recovered = handle_status(&state);
        assert_eq!(recovered.body["pressure"]["state"], "idle");
        assert!(
            state
                .pressure
                .events_snapshot()
                .iter()
                .any(|event| event.transition == "exited_saturation"),
            "recovery must emit an exit diagnostic event"
        );
        Ok(())
    }
}
