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
    ir::{AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, GraphRecord, NodeKind, agent_memory_stable_id},
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
        }
    }

    const fn http_status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::BadRequest | Self::MissingField | Self::InvalidDomain => 400,
            Self::IdempotencyConflict => 409,
            Self::NotFound => 404,
            Self::PayloadTooLarge => 413,
            Self::QueueFull => 429,
            Self::QueryTimeout => 408,
            Self::InternalError => 500,
            Self::NotImplemented => 501,
            Self::ShutdownInProgress => 503,
            Self::RedactionRequired | Self::UnresolvedEvidenceTarget => 422,
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

    fn invalid_domain() -> Self {
        Self::new(
            ErrorCode::InvalidDomain,
            r#"domain must be "codegraph" for v1 writes"#,
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

#[derive(Debug, Deserialize)]
struct QueryBudget {
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct QueryPayload {
    #[serde(default)]
    budget: Option<QueryBudget>,
    #[serde(default)]
    record_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct QueryRequest {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    payload: Option<QueryPayload>,
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.trim().is_empty())
}

struct AgentRegisterFull {
    agent_id: String,
    session_id: String,
    agent_kind: String,
    project_scope: String,
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

fn apply_write(
    command: &WriteCommand,
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
    idempotency: &Arc<Mutex<IdempotencyStore>>,
) -> WriteResult {
    validate_unique_recovery_keys(&command.records)?;
    validate_evidence_links(&command.records, sink)?;
    let record_ids = command
        .records
        .iter()
        .map(|record| record.id().to_owned())
        .collect::<Vec<_>>();
    let pending_records = {
        let mut store = idempotency
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
                IdempotencyEntry::Pending { records, .. } => Some(records.clone()),
            }
        } else {
            store
                .set_entry_durably(
                    command.idempotency_key.clone(),
                    IdempotencyEntry::Pending {
                        payload_hash: command.payload_hash.clone(),
                        record_ids: record_ids.clone(),
                        records: command.records.clone(),
                    },
                )
                .map_err(|error| ApiError::internal(error.to_string()))?;
            None
        }
    };
    if let Some(pending_records) = pending_records
        && let Some(response) = recover_pending_write(
            &command.idempotency_key,
            &command.payload_hash,
            &pending_records,
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
        let report = ingest_records(&command.records, &mut *sink);
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

fn validate_unique_recovery_keys(records: &[GraphRecord]) -> WriteResult<()> {
    if has_duplicate_recovery_keys(records) || has_ambiguous_recovery_keys(records) {
        return Err(ApiError::conflict(
            "ingest payload has duplicate record IDs that are not idempotently recoverable",
        ));
    }
    Ok(())
}

fn validate_evidence_links(
    records: &[GraphRecord],
    sink: &Arc<RwLock<EmbeddedAletheiaSink>>,
) -> WriteResult<()> {
    let sink_guard = sink
        .read()
        .map_err(|_| ApiError::internal("embedded sink lock poisoned"))?;
    for record in records {
        if let GraphRecord::Node {
            evidence_links: Some(links),
            ..
        } = record
        {
            for link in links {
                match sink_guard.read_back(&link.target_record_id) {
                    Ok(None) => {
                        return Err(ApiError::new(
                            ErrorCode::UnresolvedEvidenceTarget,
                            format!(
                                "evidence link target '{}' not found in store",
                                link.target_record_id
                            ),
                        ));
                    }
                    Ok(Some(_)) => {}
                    Err(error) => return Err(ApiError::internal(error.to_string())),
                }
            }
        }
    }
    Ok(())
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
    match non_empty(envelope.domain.as_deref()) {
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("domain"));
        }
        Some(d) if !matches!(d, "codegraph" | "agent_memory") => {
            return HttpResponse::error_with_id(&request_id, ApiError::invalid_domain());
        }
        _ => {}
    }
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

fn handle_query(request: &HttpRequest, state: &ServerState) -> HttpResponse {
    let started = Instant::now();
    let query = match parse_json::<QueryRequest>(&request.body) {
        Ok(query) => query,
        Err(error) => return HttpResponse::error(error),
    };
    let request_id = match non_empty(query.request_id.as_deref()) {
        Some(id) => id.to_owned(),
        None => return HttpResponse::error(ApiError::missing_field("request_id")),
    };
    if non_empty(query.agent_id.as_deref()).is_none() {
        return HttpResponse::error_with_id(&request_id, ApiError::missing_field("agent_id"));
    }
    if non_empty(query.session_id.as_deref()).is_none() {
        return HttpResponse::error_with_id(&request_id, ApiError::missing_field("session_id"));
    }
    if non_empty(query.domain.as_deref()).is_some_and(|d| !matches!(d, "codegraph" | "agent_memory")) {
        return HttpResponse::error_with_id(&request_id, ApiError::invalid_domain());
    }
    let agent_id = query.agent_id;
    let session_id = query.session_id;
    let Some(payload) = query.payload else {
        return HttpResponse::error_with_id(&request_id, ApiError::missing_field("payload"));
    };

    let (limit, timeout_ms) =
        payload
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
    let deadline = budget.and_then(|b| started.checked_add(b));
    let mut records = Vec::new();
    for record_id in payload.record_ids.iter().take(limit) {
        if let Err(error) = check_query_budget(started, budget) {
            return HttpResponse::error_with_id(&request_id, error);
        }
        let result = {
            let sink = match query_sink_read(state, started, budget) {
                Ok(sink) => sink,
                Err(error) => return HttpResponse::error_with_id(&request_id, error),
            };
            sink.read_back_until(record_id, deadline)
        };
        match result {
            Ok(Some(record)) => records.push(record),
            Ok(None) => {}
            Err(AdapterError::TimedOut { .. }) => {
                return HttpResponse::error_with_id(&request_id, ApiError::query_timeout());
            }
            Err(error) => {
                return HttpResponse::error_with_id(
                    &request_id,
                    ApiError::internal(error.to_string()),
                );
            }
        }
        if let Err(error) = check_query_budget(started, budget) {
            return HttpResponse::error_with_id(&request_id, error);
        }
    }
    HttpResponse::success(
        Some(&request_id),
        200,
        json!({
            "agent_id": agent_id,
            "session_id": session_id,
            "domain": "codegraph",
            "records": records,
            "snapshot": unix_ms().to_string(),
        }),
    )
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
        Some(kind) => kind.to_owned(),
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
    match non_empty(envelope.domain.as_deref()) {
        None => {
            return HttpResponse::error_with_id(&request_id, ApiError::missing_field("domain"));
        }
        Some(d) if !matches!(d, "codegraph" | "agent_memory") => {
            return HttpResponse::error_with_id(&request_id, ApiError::invalid_domain());
        }
        _ => {}
    }
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
    let agent_node_id = agent_memory_stable_id(&["node", "agent", &registration.agent_id]);
    let session_node_id = agent_memory_stable_id(&[
        "node",
        "agent_session",
        &registration.agent_id,
        &registration.session_id,
    ]);
    let mut agent_node = GraphRecord::node(
        agent_node_id.clone(),
        NodeKind::Agent,
        None,
        None,
        Some(registration.agent_id.clone()),
        format!(
            "Agent {} ({}) scoped to {}",
            registration.agent_id, registration.agent_kind, registration.project_scope
        ),
    );
    if let GraphRecord::Node { schema_version, .. } = &mut agent_node {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
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
    if let GraphRecord::Node { schema_version, .. } = &mut session_node {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
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
                "session_id": "test-session",
                "payload": {
                    "budget": { "timeout_ms": 1_u64 },
                    "record_ids": ["codegraph:v1:missing"]
                }
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
            "codegraph:v1:cross-key-current-node".to_owned(),
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
                sink.node_observation_count_for_test("codegraph:v1:cross-key-current-node"),
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
        let file_id = "codegraph:v1:cross-key-edge-file".to_owned();
        let symbol_id = "codegraph:v1:cross-key-edge-symbol".to_owned();
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
