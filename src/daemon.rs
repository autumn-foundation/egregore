//! Local daemon for shared Egregore store access.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, RwLock, RwLockReadGuard, TryLockError,
        atomic::{AtomicBool, Ordering},
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
        IdentitySource, NodeKind, OutputHandle, TemporalMetadata, VERIFICATION_SCHEMA_VERSION,
        agent_memory_stable_id,
    },
    query as graph_query,
};

const RUNTIME_DIR_SUFFIX: &str = ".egregore-runtime";
const LOCK_FILE: &str = "egregored.lock";
const METADATA_FILE: &str = "egregored.json";
const IDEMPOTENCY_FILE: &str = "idempotency.json";
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

/// Metadata written by a running daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonMetadata {
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
        let entries = match fs::read_to_string(&path) {
            Ok(contents) => {
                serde_json::from_str::<IdempotencyFile>(&contents)
                    .with_context(|| format!("failed to parse {}", path.display()))?
                    .entries
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        Ok(Self { path, entries })
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
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file, path })),
            Err(_) => Ok(None),
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
    let runtime_dir = runtime_dir(data_dir);
    fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("failed to create {}", runtime_dir.display()))?;
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
}

struct WriteCommand {
    idempotency_key: String,
    payload_hash: String,
    records: Vec<GraphRecord>,
    response_tx: mpsc::Sender<WriteResult>,
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
        }
    }

    const fn http_status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
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
            Self::InternalError => 500,
            Self::NotImplemented => 501,
            Self::ShutdownInProgress => 503,
            Self::RedactionRequired
            | Self::UnresolvedEvidenceTarget
            | Self::LocalPathIdentityUnsupported
            | Self::MissingEvidenceHandle
            | Self::PatchStatusPinned => 422,
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
            r#"domain must be "codegraph", "agent_memory", "verification", or "artifact""#,
        )
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
            retry_after_ms: Some(500),
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
    if let Some(metadata) = active_metadata(&config.data_dir) {
        return Err(anyhow!("daemon already running at {}", metadata.address));
    }
    let metadata_path = metadata_path(&config.data_dir);
    if metadata_path.exists() {
        if !remove_metadata_if_store_unleased(&config.data_dir)? {
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
    let mut lease = StoreLease::acquire(&config.data_dir)
        .with_context(|| format!("daemon already running for {}", config.data_dir.display()))?;
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
        pid: std::process::id(),
        address,
        token: token.clone(),
        data_dir: config.data_dir.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at_unix_ms: unix_ms(),
    };
    write_metadata(&config.data_dir, &metadata)?;
    lease.write_metadata(&metadata)?;

    let sink = Arc::new(RwLock::new(sink));
    let (write_tx, write_rx) = mpsc::sync_channel(config.write_queue_capacity);
    let idempotency_path = runtime_dir(&config.data_dir).join(IDEMPOTENCY_FILE);
    let idempotency = Arc::new(Mutex::new(IdempotencyStore::load(idempotency_path)?));
    let shutdown = Arc::new(AtomicBool::new(false));
    let state = Arc::new(ServerState {
        token,
        store_identity: store_identity_text(&config.data_dir),
        sink: Arc::clone(&sink),
        write_tx,
        jobs: Arc::new(Mutex::new(BTreeMap::new())),
        agents: Arc::new(Mutex::new(BTreeMap::new())),
        idempotency: Arc::clone(&idempotency),
        shutdown: Arc::clone(&shutdown),
    });
    let worker = spawn_write_worker(write_rx, sink, idempotency);

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
    let _ = fs::remove_file(metadata_path(&config.data_dir));
    Ok(())
}

/// Returns active daemon metadata for a data directory if the daemon responds.
#[must_use]
pub fn active_metadata(data_dir: &Path) -> Option<DaemonMetadata> {
    let metadata = read_metadata(data_dir).ok()?;
    let client = DaemonClient::for_data_dir(metadata.clone(), data_dir);
    client.health().ok()?;
    Some(metadata)
}

/// Stops the running daemon for a data directory.
///
/// # Errors
///
/// Returns an error if metadata is missing or the shutdown request fails.
pub fn stop(data_dir: &Path) -> Result<()> {
    let metadata = read_metadata(data_dir)
        .with_context(|| format!("no daemon metadata found for {}", data_dir.display()))?;
    let client = DaemonClient::for_data_dir(metadata, data_dir);
    if let Err(error) = client.shutdown() {
        if active_metadata(data_dir).is_none() {
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
        Ok(Self::for_data_dir(read_metadata(data_dir)?, data_dir))
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
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(command) = write_rx.recv() {
            let result = apply_write(&command, &sink, &idempotency);
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

    // Consult the idempotency cache BEFORE running evidence-link validation so that
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

    validate_no_local_path_identity_in_shared_store(&command.records, sink)?;
    validate_verification_domain_records(&command.records)?;
    validate_artifact_domain_records(&command.records, sink)?;

    let (synthesized_edges, canonical_nodes) =
        validate_and_synthesize_evidence_edges(&command.records, sink)?;

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
        if base_commit.as_deref().is_none_or(str::is_empty)
            && unknown_base_reason.as_deref() != Some("unknown_base")
        {
            return Err(ApiError::missing_field(
                "unknown_base_reason (required when base_commit is null)",
            ));
        }
        if target_files.is_none() {
            return Err(ApiError::missing_field("target_files"));
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
        required_str(producer_session_id.as_deref(), "producer_session_id")?;
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

    let sink = sink
        .read()
        .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
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
        match sink.read_back(id) {
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
        _ => true,
    }
}

// Validates that a resolved target ID is consistent with the declared target_domain.
// For "codegraph" and "agent_memory", enforces the expected ID prefix.
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
        // "project" and any other declared domain: no universal prefix requirement.
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
        EdgeLabel::Observes | EdgeLabel::ExplainsChange | EdgeLabel::ValidatedBy => {
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
        EdgeLabel::ProducedPatch => {
            if let Some(sk) = source_kind
                && !matches!(sk, NodeKind::FileEdit | NodeKind::AgentTurn)
            {
                return Err(ApiError::bad_request(format!(
                    "evidence link relation '{}' requires a FileEdit or AgentTurn source node, not {}",
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
                    NodeKind::FileEdit | NodeKind::ToolCall | NodeKind::CommandRun
                )
            {
                return Err(ApiError::bad_request(format!(
                    "evidence link relation '{}' requires a FileEdit, ToolCall, or CommandRun source node, not {}",
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
        EdgeLabel::FailedOn if !matches!(target_kind, Some(NodeKind::Symbol | NodeKind::File)) => {
            return Err(ApiError::bad_request(format!(
                "evidence link relation '{}' requires a Symbol or File target; target '{}' has kind {}",
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
    NodeKind::CommandEvidence,
    NodeKind::AgentRun,
    NodeKind::AgentTurn,
    NodeKind::ToolCall,
    NodeKind::FileEdit,
    NodeKind::Failure,
    NodeKind::Decision,
];

// Validates that the source and target IDs of a directly submitted agent-memory edge
// are in the domains required by the cross-domain registry (docs/schema/agent-memory.md §6).
fn validate_agent_memory_edge_endpoints(
    edge_id: &str,
    label: EdgeLabel,
    source: &str,
    target: &str,
) -> WriteResult<()> {
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
        EdgeLabel::SessionOf
        | EdgeLabel::AuthoredBy
        | EdgeLabel::ReferencesTask
        | EdgeLabel::Supersedes
            if !target.starts_with("agent_memory:v1:") =>
        {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires an agent_memory:v1: target; got target '{target}'",
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
        | EdgeLabel::FailedOn
        | EdgeLabel::ExplainsChange
            if !target.starts_with("codegraph:") =>
        {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires a codegraph: target; got target '{target}'",
                label.as_str()
            )));
        }
        EdgeLabel::ProducedPatch if !target.starts_with("artifact:v1:") => {
            return Err(ApiError::bad_request(format!(
                "agent-memory edge '{edge_id}' label '{}' requires an artifact:v1: target; got target '{target}'",
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
            // Reject evidence-link labels on codegraph edges — they must go through the
            // agent-memory envelope and its cross-domain checks, not the codegraph path.
            if let GraphRecord::Edge { id, label, .. } = record
                && !id.starts_with("agent_memory:v1:")
                && label.is_evidence_link_label()
            {
                return Err(ApiError::bad_request(format!(
                    "edge '{id}' uses evidence-link label '{}' but is not an agent_memory:v1: edge; evidence relations are only permitted on agent-memory edges",
                    label.as_str()
                )));
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
                        if !matches!(source_kind, Some(NodeKind::AgentSession)) {
                            return Err(ApiError::bad_request(format!(
                                "agent-memory edge '{id}' SESSION_OF requires an AgentSession source; got {}",
                                source_kind.map_or_else(
                                    || "unknown".to_owned(),
                                    |k| k.as_str().to_owned()
                                )
                            )));
                        }
                        if !matches!(target_kind, Some(NodeKind::Agent)) {
                            return Err(ApiError::bad_request(format!(
                                "agent-memory edge '{id}' SESSION_OF requires an Agent target; got {}",
                                target_kind.map_or_else(
                                    || "unknown".to_owned(),
                                    |k| k.as_str().to_owned()
                                )
                            )));
                        }
                    }
                    EdgeLabel::AuthoredBy => {
                        let target_kind = lookup_node_kind(target, records, &sink_guard)?;
                        if !matches!(target_kind, Some(NodeKind::AgentSession)) {
                            return Err(ApiError::bad_request(format!(
                                "agent-memory edge '{id}' AUTHORED_BY requires an AgentSession target; got {}",
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
                        | EdgeLabel::FailedOn
                        | EdgeLabel::ExplainsChange
                            if link.target_domain != "codegraph" =>
                        {
                            return Err(ApiError::bad_request(format!(
                                "evidence link relation '{}' requires target_domain 'codegraph'; got '{}'",
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
                        // REFERENCES_TASK: the schema registry documents the TO domain as
                        // "project", but Task nodes currently live in agent_memory.
                        // Accept both to cover clients using the documented domain name.
                        EdgeLabel::ReferencesTask
                            if !matches!(
                                link.target_domain.as_str(),
                                "project" | "agent_memory"
                            ) =>
                        {
                            return Err(ApiError::bad_request(format!(
                                "evidence link relation '{}' requires target_domain 'project' or 'agent_memory'; got '{}'",
                                edge_label.as_str(),
                                link.target_domain
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
            "jobs": jobs,
            "agents": agents,
            "idempotency_store_size": idempotency_store_size,
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
                "codegraph" | "agent_memory" | "verification" | "artifact"
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
        Err(error) => HttpResponse::error(ApiError::internal(error.to_string())),
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
    let records = sink
        .read_all_records()
        .map_err(|e| ApiError::internal(e.to_string()))?;
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
    obj.insert("before_commit".to_owned(), json!(&drift.before_git_commit));
    obj.insert("after_commit".to_owned(), json!(&drift.after_git_commit));
    obj.insert("score".to_owned(), json!(&drift.score));
    obj.insert("model_id".to_owned(), json!(&drift.model_id));
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
                return HttpResponse::error_with_id(
                    request_id,
                    ApiError::internal(error.to_string()),
                );
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

    let (mut records, snapshot) = match load_all_records_for_verb(state, started, budget, domain) {
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
            "codegraph" | "agent_memory" | "verification" | "artifact"
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
        "observations_for_symbol" | "agent_sessions_for_repo" => HttpResponse::error_with_id(
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
                "codegraph" | "agent_memory" | "verification" | "artifact"
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
    match state.write_tx.try_send(command) {
        Ok(()) => response_rx.recv().map_err(|_| {
            ApiError::internal(format!("write worker dropped request {request_id}"))
        })?,
        Err(mpsc::TrySendError::Full(_)) => Err(ApiError::overloaded()),
        Err(mpsc::TrySendError::Disconnected(_)) => {
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
        if let Some(metadata) = active_metadata(data_dir) {
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
        if active_metadata(data_dir).is_none() && remove_metadata_if_store_unleased(data_dir)? {
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
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_metadata(data_dir: &Path, metadata: &DaemonMetadata) -> Result<()> {
    let path = metadata_path(data_dir);
    let json = serde_json::to_vec_pretty(metadata)?;
    atomic_write(&path, &json)
}

fn metadata_path(data_dir: &Path) -> PathBuf {
    runtime_dir(data_dir).join(METADATA_FILE)
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

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, data).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} to {}", tmp.display(), path.display()))
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
}
