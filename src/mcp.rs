//! MCP server for Egregore tools — issue #53.
//!
//! Implements the Model Context Protocol using the [`rmcp`] crate and exposes
//! five read-only tools backed by the existing daemon query, symbol-context,
//! task-evidence, and failure-history contracts, plus one write tool:
//!
//! - **`inspect_store`** — store-inspection summary (record counts, domain breakdown).
//! - **`symbol_context`** — evidence-backed symbol context, trust-separated by domain.
//! - **`task_evidence`** — evidence-backed task context, trust-separated by domain.
//! - **`store_freshness`** — store-freshness verdict for the whole store (issue #220).
//! - **`failure_history`** — prior failed attempts for a symbol, file, or task
//!   handle, trust-separated into runtime failures, agent failures, and
//!   superseding successes (issue #188).
//! - **`record_observation`** — records an evidence-backed agent observation
//!   into the `agent_memory` domain (issue #183). The first write tool on this
//!   transport: it enforces the same provenance contract as
//!   `eg write observation` (session id, body, confidence, at least one
//!   evidence target with its domain), stamps an MCP-origin producer envelope,
//!   and can never write into the deterministic `codegraph` domain.
//!
//! All tool responses carry machine-readable structured output with record IDs and
//! citation handles. Successful *read* responses additionally carry a `freshness`
//! object (issue #220): the store-freshness verdict from the #186 contract
//! (`fresh` / `stale_head` / `stale_dirty` / `unknown`), the stored
//! source-snapshot identity the answer was derived from, and the working-tree
//! state it was compared against — a trust signal, never suppression. The
//! write tool's success response carries the new `record_id` instead.
//!
//! ## Transport
//!
//! [`run_stdio`] starts the rmcp stdio server and blocks until the client
//! disconnects. All JSON-RPC 2.0 framing (initialize, ping, tools/list,
//! tools/call) is handled by rmcp.
//!
//! ## Daemon discovery
//!
//! Each tool call discovers the running daemon from the `data_dir` argument
//! (default `.egregore`) using the existing `DaemonClient::from_data_dir`
//! contract. When the daemon is missing or stale the tool returns a stable
//! machine-readable error rather than falling back to shell commands or
//! embedded direct reads.
//!
//! ## Redaction
//!
//! Tool output never includes raw transcript text, patch hunks, issue bodies,
//! bearer tokens, or other protected artifact payloads. All structured output
//! is produced using the same field-level filtering applied by the CLI query
//! commands.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    GraphRecord, NodeKind,
    adapters::DanglingCitationPolicy,
    daemon::DaemonClient,
    evidence::{
        EvidenceProvenance, ObservationRequest, ProvenanceError,
        build_observation_records_with_producer, mcp_observation_producer, now_rfc3339,
        validate_observation_request,
    },
    ir::{EdgeLabel, EvidenceLink},
    mcp_contract::MCP_CONTRACT_VERSION,
    query,
    schema_version::{UnknownSchemaVersion, record_version, validate_record_version},
};

// ── Tool parameter types ──────────────────────────────────────────────────────

/// Parameters for the `inspect_store` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectStoreArgs {
    /// `AletheiaDB` data directory (default: `.egregore`).
    pub data_dir: Option<String>,
    /// Working-tree path the store freshness verdict is computed against
    /// (default: the MCP server's current directory, mirroring
    /// `eg freshness`'s default `.`).
    pub repo_path: Option<String>,
}

/// Parameters for the `symbol_context` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolContextArgs {
    /// Exact symbol name to look up.
    pub symbol_name: String,
    /// Disambiguation selector (issue #192): pass one candidate's `record_id`
    /// or `file:span` handle from an `ambiguous_symbol` response to get
    /// context scoped to exactly that symbol. When present, `symbol_name`
    /// may be empty — the candidate alone identifies the symbol.
    pub candidate: Option<String>,
    /// `AletheiaDB` data directory (default: `.egregore`).
    pub data_dir: Option<String>,
    /// Working-tree path the store freshness verdict is computed against
    /// (default: the MCP server's current directory, mirroring
    /// `eg freshness`'s default `.`).
    pub repo_path: Option<String>,
}

/// Parameters for the `task_evidence` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskEvidenceArgs {
    /// Task record ID, `GitHub` URL, `GitHub` short handle, or local JSONL handle.
    pub id_or_handle: String,
    /// `AletheiaDB` data directory (default: `.egregore`).
    pub data_dir: Option<String>,
    /// Working-tree path the store freshness verdict is computed against
    /// (default: the MCP server's current directory, mirroring
    /// `eg freshness`'s default `.`).
    pub repo_path: Option<String>,
}

/// Parameters for the `store_freshness` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StoreFreshnessArgs {
    /// Working-tree path the store freshness verdict is computed against
    /// (default: the MCP server's current directory, mirroring
    /// `eg freshness`'s default `.`).
    pub repo_path: Option<String>,
    /// `AletheiaDB` data directory (default: `.egregore`).
    pub data_dir: Option<String>,
}

/// Parameters for the `failure_history` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FailureHistoryArgs {
    /// Handle resolving to the failure-history target, in `eg query failures`
    /// order: canonical code record ID, task/task-source handle, repo-relative
    /// file path, exact symbol name, or source/provenance handle.
    pub handle: String,
    /// `AletheiaDB` data directory (default: `.egregore`).
    pub data_dir: Option<String>,
    /// Working-tree path the store freshness verdict is computed against
    /// (default: the MCP server's current directory, mirroring
    /// `eg freshness`'s default `.`).
    pub repo_path: Option<String>,
}

/// One evidence target cited by a `record_observation` call.
///
/// Mirrors the CLI's `--evidence-target` / `--evidence-domain` pair
/// (`eg write observation`): only `codegraph` (cited via `OBSERVES`) and
/// `verification` (cited via `VALIDATED_BY`) targets are accepted — the same
/// restriction the CLI enforces, because the daemon rejects any other
/// relation/domain combination.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordObservationEvidenceTarget {
    /// Stable record ID of the cited graph record (must be non-empty).
    pub target_record_id: Option<String>,
    /// Domain of the cited record: `codegraph` or `verification`
    /// (must be non-empty).
    pub target_domain: Option<String>,
}

/// Parameters for the `record_observation` write tool (issue #183).
///
/// Every required provenance field is `Option`-typed so that an *absent* field
/// and an *empty* field are rejected identically with the structured
/// `{"ok": false, "error": ...}` envelope — the tool never writes a partial
/// or unprovenanced record. Fields the CLI defaults (`agent_kind`,
/// `observed_at`) stay optional here too and default the same way.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordObservationArgs {
    /// Stable agent identity; required, must be non-empty.
    pub agent_id: Option<String>,
    /// Agent kind from the published enum (`other`, `claude-code`,
    /// `vantage`, `codex`, `rust-swe-agent`, `human`); defaults to `other`.
    pub agent_kind: Option<String>,
    /// Active session identifier; required, must be non-empty.
    pub session_id: Option<String>,
    /// RFC 3339 observation timestamp; defaults to now when absent.
    pub observed_at: Option<String>,
    /// Citable source artifact path or hash; required, must be non-empty.
    pub source_handle: Option<String>,
    /// Observation body text; required, must be non-empty.
    pub text: Option<String>,
    /// Extraction confidence in `[0.0, 1.0]`; required.
    pub confidence: Option<f64>,
    /// Evidence targets anchoring the claim; at least one is required, each
    /// with a non-empty record ID and a `codegraph`/`verification` domain.
    pub evidence: Option<Vec<RecordObservationEvidenceTarget>>,
    /// `AletheiaDB` data directory (default: `.egregore`).
    pub data_dir: Option<String>,
}

// ── MCP server ────────────────────────────────────────────────────────────────

/// MCP server that exposes the Egregore evidence-query tools plus the
/// `record_observation` write tool (issue #183).
///
/// Created by [`run_stdio`] or constructed directly for testing via
/// [`EgregoreMcpServer::new`].
#[derive(Clone)]
pub struct EgregoreMcpServer {
    default_data_dir: PathBuf,
}

#[tool_router]
impl EgregoreMcpServer {
    /// Create a server with the given default data directory.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(default_data_dir: PathBuf) -> Self {
        Self { default_data_dir }
    }

    /// Returns a structured summary of the Egregore store: record counts by
    /// domain, schema versions, and repository identities.
    /// Requires a running local daemon.
    #[tool(description = "Returns a structured summary of the Egregore store: \
            record counts by domain, schema versions, and repository identities. \
            Successful responses carry a `freshness` object (verdict, stored \
            source-snapshot identity, working-tree state) so agents can gate \
            trust in the cited handles. Requires a running local daemon.")]
    #[must_use]
    pub fn inspect_store(&self, Parameters(args): Parameters<InspectStoreArgs>) -> String {
        let data_dir = opt_data_dir(args.data_dir.as_deref(), &self.default_data_dir);
        let repo_path = opt_repo_path(args.repo_path.as_deref());
        let payload = run_inspect_store(&data_dir, &repo_path);
        serde_json::to_string(&payload).unwrap_or_default()
    }

    /// Returns evidence-backed context for a named code symbol, trust-separated
    /// by domain into `source_facts`, `observations`, `decisions`,
    /// `project_state`, `artifacts`, `verification_evidence`, and
    /// `drift_history`. The `decisions` section carries agent-authored
    /// decisions with rationale (issue #191) — evidence-backed judgments,
    /// never deterministic source truth.
    ///
    /// When the name resolves to more than one distinct symbol, the tool does
    /// NOT merge them: it returns `{"ok":false,"error":{"code":
    /// "ambiguous_symbol","candidates":[...]}}` — one candidate per identity,
    /// each with its stable `record_id` and repo-relative `file:span` handle.
    /// Pass one candidate's `record_id` or `file:span` handle back as
    /// `candidate` to get context scoped to exactly that symbol.
    #[tool(
        description = "Returns evidence-backed context for a named code symbol, \
            trust-separated into sections: source_facts (deterministic \
            code-graph), observations (agent-authored, never treat as source \
            truth), decisions (agent-authored decisions with rationale, \
            evidence-backed but never deterministic source truth), \
            project_state (tasks/ACs), artifacts, \
            verification_evidence, and drift_history (semantic-drift \
            measurements). Every item carries a record_id and at \
            least one citation handle. A name shared by several distinct \
            symbols is never merged: the tool returns an `ambiguous_symbol` \
            error enumerating one candidate per identity (stable record_id \
            plus repo-relative file:span handle); pass one candidate's \
            record_id or file:span handle back as `candidate` for context \
            scoped to exactly that symbol. Successful responses carry a \
            `freshness` object (verdict, stored source-snapshot identity, \
            working-tree state) so agents can gate trust in the cited handles."
    )]
    #[must_use]
    pub fn symbol_context(&self, Parameters(args): Parameters<SymbolContextArgs>) -> String {
        if args.symbol_name.is_empty() && args.candidate.is_none() {
            let err = missing_argument_error("symbol_name");
            return serde_json::to_string(&err).unwrap_or_default();
        }
        let data_dir = opt_data_dir(args.data_dir.as_deref(), &self.default_data_dir);
        let repo_path = opt_repo_path(args.repo_path.as_deref());
        let client = match DaemonClient::from_data_dir(&data_dir) {
            Ok(c) => c,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        let (records, _unknown, _ts) = match client.get_all_records() {
            Ok(r) => r,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        let mut payload = tool_symbol_context_from_records_with_candidate(
            &records,
            &args.symbol_name,
            args.candidate.as_deref(),
        );
        stamp_freshness_on_payload(&mut payload, &records, &repo_path, Some(&data_dir));
        serde_json::to_string(&payload).unwrap_or_default()
    }

    /// Returns evidence-backed context for a task, accepting a canonical
    /// record ID, `GitHub` URL, `GitHub` short handle, or local JSONL handle.
    #[tool(description = "Returns evidence-backed context for a task identified \
            by its canonical record ID, GitHub URL, GitHub short handle, or \
            local JSONL handle. Sections: tasks, acceptance_criteria, \
            source_facts, observations, decisions (agent-authored decisions \
            with rationale, evidence-backed but never deterministic source \
            truth), artifacts, verification_evidence, \
            reviews, external_links, unresolved. Successful responses carry a \
            `freshness` object (verdict, stored source-snapshot identity, \
            working-tree state) so agents can gate trust in the cited handles.")]
    #[must_use]
    pub fn task_evidence(&self, Parameters(args): Parameters<TaskEvidenceArgs>) -> String {
        let data_dir = opt_data_dir(args.data_dir.as_deref(), &self.default_data_dir);
        let repo_path = opt_repo_path(args.repo_path.as_deref());
        let client = match DaemonClient::from_data_dir(&data_dir) {
            Ok(c) => c,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        let (records, _unknown, _ts) = match client.get_all_records() {
            Ok(r) => r,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        let mut payload = tool_task_evidence_from_records(&records, &args.id_or_handle);
        stamp_freshness_on_payload(&mut payload, &records, &repo_path, Some(&data_dir));
        serde_json::to_string(&payload).unwrap_or_default()
    }

    /// Returns the store-freshness verdict for the whole store — the same
    /// verdict the per-tool `freshness` objects carry — without requiring a
    /// symbol or task argument. Requires a running local daemon.
    #[tool(description = "Returns the store-freshness verdict for the whole \
            store: verdict (fresh / stale_head / stale_dirty / unknown), the \
            stored source-snapshot identity, and the working-tree state it was \
            compared against. Same verdict the inspect_store / symbol_context / \
            task_evidence `freshness` objects carry. Requires a running local \
            daemon.")]
    #[must_use]
    pub fn store_freshness(&self, Parameters(args): Parameters<StoreFreshnessArgs>) -> String {
        let data_dir = opt_data_dir(args.data_dir.as_deref(), &self.default_data_dir);
        let repo_path = opt_repo_path(args.repo_path.as_deref());
        let client = match DaemonClient::from_data_dir(&data_dir) {
            Ok(c) => c,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        let (records, _unknown, _ts) = match client.get_all_records() {
            Ok(r) => r,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        let payload = json!({
            "ok": true,
            "freshness": tool_freshness_stamp(&records, &repo_path, Some(&data_dir)),
        });
        serde_json::to_string(&payload).unwrap_or_default()
    }

    /// Returns prior failed attempts for a symbol, file, or task handle —
    /// trust-separated into `runtime_failures` (trust `verification_evidence`),
    /// `agent_failures` (trust `agent_authored`), and `superseding_successes`
    /// (later passing verifications, which never hide the older failures).
    /// Every failure carries a stable `record_id`/evidence handle and a
    /// read-time `resolution_status` (`still_failing` | `since_resolved`);
    /// sections are canonically ordered oldest-first. Handles resolve in the
    /// same order as `eg query failures`: canonical code record ID,
    /// task/task-source handle, repo-relative file path, exact symbol name,
    /// source/provenance handle. Unresolvable handles surface structured
    /// `no_match` / `stale_handle` / `ambiguous_handle` / `unsupported_handle`
    /// errors — never a silent empty success. A resolved target with no
    /// recorded failures returns a successful explicitly-empty answer with a
    /// `safety_note`: absence of recorded failure is not evidence of safety,
    /// and no failure cause is ever inferred. Successful responses carry a
    /// `freshness` object (verdict, stored source-snapshot identity,
    /// working-tree state) so agents can gate trust in the cited handles.
    #[tool(description = "Returns prior failed attempts for a symbol, file, \
            or task handle, trust-separated into runtime_failures (trust \
            verification_evidence), agent_failures (trust agent_authored), \
            and superseding_successes (later passing verifications, which \
            never hide the older failures). Every failure carries a stable \
            record_id/evidence handle and a read-time resolution_status \
            (still_failing | since_resolved); sections are canonically \
            ordered oldest-first. Handles resolve in the same order as `eg \
            query failures`: canonical code record ID, task/task-source \
            handle, repo-relative file path, exact symbol name, \
            source/provenance handle. Unresolvable handles surface structured \
            no_match / stale_handle / ambiguous_handle / unsupported_handle \
            errors. A resolved target with no recorded failures returns a \
            successful explicitly-empty answer with a safety_note: absence \
            of recorded failure is not evidence of safety. Successful \
            responses carry a `freshness` object (verdict, stored \
            source-snapshot identity, working-tree state) so agents can gate \
            trust in the cited handles.")]
    #[must_use]
    pub fn failure_history(&self, Parameters(args): Parameters<FailureHistoryArgs>) -> String {
        if args.handle.is_empty() {
            let err = missing_argument_error("handle");
            return serde_json::to_string(&err).unwrap_or_default();
        }
        let data_dir = opt_data_dir(args.data_dir.as_deref(), &self.default_data_dir);
        let repo_path = opt_repo_path(args.repo_path.as_deref());
        let client = match DaemonClient::from_data_dir(&data_dir) {
            Ok(c) => c,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        let (records, _unknown, _ts) = match client.get_all_records() {
            Ok(r) => r,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        let mut payload = tool_failure_history_from_records(&records, &args.handle);
        stamp_freshness_on_payload(&mut payload, &records, &repo_path, Some(&data_dir));
        serde_json::to_string(&payload).unwrap_or_default()
    }

    /// Records an evidence-backed agent observation into the `agent_memory`
    /// domain of the store named by `data_dir` (issue #183).
    ///
    /// This is the first write tool on the MCP transport, closing the agent
    /// learning loop (query → learn → persist with provenance) without leaving
    /// the transport. It enforces the same provenance contract as
    /// `eg write observation`: `agent_id`, `session_id`, `text`, `confidence`
    /// in `[0.0, 1.0]`, `source_handle`, and at least one evidence target with
    /// a `codegraph`/`verification` domain. Any missing or invalid field — or
    /// zero evidence targets — is rejected with the structured
    /// `{"ok": false, "error": ...}` envelope and nothing is written.
    ///
    /// The persisted records carry an MCP-origin producer envelope
    /// (`producer_kind: observation_writer`, `producer_components.transport:
    /// "mcp"`), distinct from CLI-originated writes. The write is ingested
    /// under the `agent_memory` domain only — the tool has no domain parameter
    /// and the daemon validates record IDs against the ingest domain, so
    /// agent-authored observations can never land in the deterministic
    /// `codegraph` domain. A dangling evidence target rejects the whole batch
    /// atomically (`RejectBatch`): no partial record is ever persisted.
    /// Requires a running local daemon.
    #[tool(description = "Records an evidence-backed agent observation into \
            the agent_memory domain of the Egregore store. Required: \
            agent_id, session_id, text (observation body), confidence in \
            [0.0, 1.0], source_handle (citable source artifact path or hash), \
            and at least one evidence target, each with a target_record_id \
            and a target_domain of codegraph (cited via OBSERVES) or \
            verification (cited via VALIDATED_BY). Optional: agent_kind \
            (defaults to other), observed_at in RFC 3339 (defaults to now). \
            A call missing any required field or citing zero evidence targets \
            is rejected with a structured ok:false error and writes nothing. \
            The write can never target the deterministic codegraph domain, \
            and a dangling evidence target rejects the whole batch \
            atomically. On success the response carries the new record_id — \
            the citable handle for the observation. Requires a running local \
            daemon.")]
    #[must_use]
    pub fn record_observation(
        &self,
        Parameters(args): Parameters<RecordObservationArgs>,
    ) -> String {
        let req = match record_observation_request_from_args(&args) {
            Ok(req) => req,
            Err(error) => {
                return serde_json::to_string(&provenance_error_payload(&error))
                    .unwrap_or_default();
            }
        };
        let data_dir = opt_data_dir(args.data_dir.as_deref(), &self.default_data_dir);
        let client = match DaemonClient::from_data_dir(&data_dir) {
            Ok(c) => c,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        // The builder re-validates every provenance field: the CLI and the
        // MCP tool share one enforcement point, so the contract cannot drift.
        let outcome = match build_observation_records_with_producer(
            &req,
            &mcp_observation_producer(&req.provenance.observed_at),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                return serde_json::to_string(&provenance_error_payload(&error))
                    .unwrap_or_default();
            }
        };
        // The observation's content-addressed record ID is the idempotency
        // key: an identical retry replays the daemon's cached response
        // instead of duplicating records.
        let idempotency_key = outcome.record_id.clone();
        // The ingest domain is hardcoded to `agent_memory` inside
        // `ingest_agent_memory_records` — the tool has no domain parameter,
        // so observations can never land in the deterministic `codegraph`
        // domain.
        match client.ingest_agent_memory_records(
            &outcome.records,
            &req.provenance.agent_id,
            &req.provenance.session_id,
            &idempotency_key,
            DanglingCitationPolicy::RejectBatch,
        ) {
            Ok(response) if response.failed == 0 => {
                let payload = json!({
                    "ok": true,
                    "record_id": outcome.record_id,
                    "records_written": response.succeeded,
                });
                serde_json::to_string(&payload).unwrap_or_default()
            }
            Ok(response) => {
                let detail = response
                    .failures
                    .first()
                    .map(|failure| failure.message.clone())
                    .unwrap_or_default();
                serde_json::to_string(&write_rejected_error(&detail)).unwrap_or_default()
            }
            Err(error) => {
                serde_json::to_string(&write_failed_error(&error.to_string())).unwrap_or_default()
            }
        }
    }
}

#[tool_handler]
impl ServerHandler for EgregoreMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("egregore", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "Egregore knowledge graph tools: five read-only evidence-query \
                tools plus `record_observation`, which records an \
                evidence-backed agent observation into the `agent_memory` \
                domain (it enforces the same provenance contract as \
                `eg write observation` and can never write into the \
                deterministic codegraph domain). \
                Connect to a running local daemon (`eg daemon`) to query the \
                code-graph, agent observations, and task evidence. \
                All tools return structured JSON with `ok`, `error`, and \
                trust-separated data sections. \
                mcp_contract_version={MCP_CONTRACT_VERSION} (frozen tool I/O \
                contract, issue #194; see docs/schema/mcp.md)."
            ))
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Starts the MCP stdio server using the rmcp transport layer.
///
/// Blocks until the client disconnects. All MCP JSON-RPC 2.0 framing,
/// initialize, ping, tools/list, and tools/call routing is handled by rmcp.
///
/// # Errors
///
/// Returns an error if the tokio runtime cannot be created or if the
/// transport encounters an unrecoverable IO error.
pub fn run_stdio(default_data_dir: &Path) -> anyhow::Result<()> {
    let server = EgregoreMcpServer::new(default_data_dir.to_path_buf());
    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    rt.block_on(async move {
        server
            .serve(rmcp::transport::stdio())
            .await
            .context("MCP transport error")?
            .waiting()
            .await
            .map(|_| ())
            .context("MCP server error")
    })
}

/// Builds a structured store-inspection summary from a record slice.
///
/// Used by the `inspect_store` tool after fetching records from the daemon, and
/// directly by tests that supply a fixture slice.
///
/// The `snapshot_timestamp` field is forwarded verbatim; pass an RFC 3339 string.
///
/// The returned payload does NOT include the `freshness` object — the tool
/// method stamps it via [`stamp_freshness_on_payload`] once the working-tree
/// comparison path (`repo_path`) is known.
#[must_use]
pub fn tool_inspect_store_from_records(
    records: &[GraphRecord],
    unknown_versions: &[UnknownSchemaVersion],
    snapshot_timestamp: &str,
) -> Value {
    let mut total = 0_usize;
    let mut nodes = 0_usize;
    let mut edges = 0_usize;
    let mut tombstones = 0_usize;
    let mut diagnostics = 0_usize;
    let mut schema_versions: BTreeMap<String, usize> = BTreeMap::new();
    let mut unknown_schema_map: BTreeMap<String, usize> = BTreeMap::new();
    let mut domain_counts: BTreeMap<&'static str, BTreeMap<String, usize>> = BTreeMap::new();
    let mut repositories: Vec<Value> = Vec::new();

    for uv in unknown_versions {
        total += 1;
        let key = format!(
            "{}:{}:{}",
            uv.version.domain, uv.version.kind, uv.version.version
        );
        *unknown_schema_map.entry(key).or_default() += 1;
    }

    for record in records {
        total += 1;

        if let Err(uv) = validate_record_version(record) {
            let key = format!(
                "{}:{}:{}",
                uv.version.domain, uv.version.kind, uv.version.version
            );
            *unknown_schema_map.entry(key).or_default() += 1;
            continue;
        }

        let rv = record_version(record);
        let sv_key = format!("{}:{}:{}", rv.domain, rv.kind, rv.version);
        *schema_versions.entry(sv_key.clone()).or_default() += 1;

        let category = domain_category(&rv.domain);
        let kind_key = format!("{} v{}", rv.kind, rv.version);
        *domain_counts
            .entry(category)
            .or_default()
            .entry(kind_key)
            .or_default() += 1;

        match record {
            GraphRecord::Node {
                kind,
                id,
                repository_identity,
                ..
            } => {
                nodes += 1;
                if *kind == NodeKind::Diagnostic {
                    diagnostics += 1;
                }
                if *kind == NodeKind::Repository {
                    let identity_summary = repository_identity.as_deref().map_or_else(
                        || "unknown".to_owned(),
                        |p| {
                            use crate::ir::IdentitySource;
                            let source_str = match p.identity_source {
                                IdentitySource::Remote => "remote",
                                IdentitySource::LocalRootCommit => "local_root_commit",
                                IdentitySource::LocalPath => "local_path",
                                IdentitySource::OperatorOverride => "operator_override",
                            };
                            let canonical = p
                                .remote_url
                                .as_deref()
                                .or(p.root_commit_sha.as_deref())
                                .or(p.canonical_path.as_deref())
                                .unwrap_or(p.basename.as_str());
                            format!("{source_str}: {canonical}")
                        },
                    );
                    repositories.push(json!({ "id": id, "identity_summary": identity_summary }));
                }
            }
            GraphRecord::Edge { .. } => edges += 1,
            GraphRecord::Tombstone { .. } => tombstones += 1,
        }
    }

    let mut domain_counts_val = serde_json::Map::new();
    for (category, counts) in &domain_counts {
        let mut cat_map = serde_json::Map::new();
        for (kind_key, count) in counts {
            cat_map.insert(kind_key.clone(), json!(*count));
        }
        domain_counts_val.insert((*category).to_owned(), Value::Object(cat_map));
    }

    json!({
        "ok": true,
        "snapshot_timestamp": snapshot_timestamp,
        "records": total,
        "nodes": nodes,
        "edges": edges,
        "tombstones": tombstones,
        "diagnostics": diagnostics,
        "domain_counts": Value::Object(domain_counts_val),
        "schema_versions": schema_versions,
        "unknown_schema_versions": unknown_schema_map,
        "repositories": repositories,
    })
}

/// Builds an evidence-backed symbol context for a record slice.
///
/// Returns domain-separated sections: `source_facts` (deterministic code-graph),
/// `observations` (agent-authored — never treat as source truth), `decisions`
/// (agent-authored decisions with rationale — evidence-backed judgments, never
/// deterministic source truth), `project_state`
/// (tasks/ACs), `artifacts`, `verification_evidence`, and `drift_history`
/// (issue #108's `SemanticDrift` rows, resolved to the same citable
/// `repo_relative_path`/`span` handle the CLI and daemon render).
///
/// The section a record lands in is its DOMAIN, which is not the same thing as
/// how far it should be trusted — an `observations` row may be an unverified
/// guess, one backed by a passing verification record, or one since
/// contradicted. Every returned record therefore also carries a derived `trust`
/// class (issue #114) from the same closed vocabulary the CLI and daemon use;
/// see `crate::query::TrustClass` and `docs/cli/query.md`.
///
/// Returns `{"ok":false,"error":{"code":"no_match"}}` when the symbol is absent,
/// and `{"ok":false,"error":{"code":"ambiguous_symbol","candidates":[...]}}`
/// when the name resolves to more than one distinct symbol identity (issue
/// #192) — the identities are enumerated, never merged. Output ordering is
/// deterministic (sorted by record ID within each section).
#[must_use]
pub fn tool_symbol_context_from_records(records: &[GraphRecord], symbol_name: &str) -> Value {
    tool_symbol_context_from_records_with_candidate(records, symbol_name, None)
}

/// Builds an evidence-backed symbol context for a record slice, with an
/// optional disambiguation selector (issue #192).
///
/// When `candidate` is `Some`, it selects exactly one symbol — either a
/// stable `record_id` or a `file:span` handle (`path:start_line-end_line`)
/// from a previous `ambiguous_symbol` response — and the context is anchored
/// on that symbol alone via [`query::record_context`]. An unresolvable
/// selector is a `no_match`, never a guess.
///
/// When `candidate` is `None`, the name is resolved with
/// [`query::symbol_context`]: a name shared by distinct identities returns
/// the `ambiguous_symbol` disambiguation error instead of a blended answer.
#[must_use]
pub fn tool_symbol_context_from_records_with_candidate(
    records: &[GraphRecord],
    symbol_name: &str,
    candidate: Option<&str>,
) -> Value {
    // Disambiguated re-query: anchor on exactly one symbol identity.
    if let Some(selector) = candidate {
        let anchor = records
            .iter()
            .find(|r| {
                r.id() == selector
                    && matches!(
                        r,
                        GraphRecord::Node {
                            kind: NodeKind::Symbol,
                            ..
                        }
                    )
            })
            .or_else(|| query::resolve_symbol_file_span_handle(records, selector));
        let Some(anchor) = anchor else {
            return json!({
                "ok": false,
                "error": { "code": "no_match", "candidate": selector }
            });
        };
        let ctx = query::record_context(records, anchor.id());
        if ctx.is_no_match() {
            return json!({
                "ok": false,
                "error": { "code": "no_match", "candidate": selector }
            });
        }
        let name = match anchor {
            GraphRecord::Node { name: Some(n), .. } => n.as_str(),
            _ => selector,
        };
        return symbol_context_payload(records, &ctx, name);
    }

    let ctx = query::symbol_context(records, symbol_name);

    // Issue #192: a name shared by distinct symbols is reported, never merged.
    // The error code is the symbol-domain analogue of `task_evidence`'s
    // `ambiguous_handle`.
    if ctx.is_ambiguous() {
        let candidates: Vec<Value> = ctx
            .candidates
            .iter()
            .map(|c| {
                json!({
                    "record_id": c.record_id,
                    "symbol_name": c.symbol_name,
                    "repo_relative_path": c.repo_relative_path,
                    "span": c.span,
                    "file_span_handle": c.file_span_handle(),
                })
            })
            .collect();
        return json!({
            "ok": false,
            "error": {
                "code": "ambiguous_symbol",
                "symbol_name": symbol_name,
                "message": "the name matches more than one distinct symbol; pass one candidate's record_id or file:span handle as `candidate`",
                "candidates": candidates,
            }
        });
    }

    if ctx.is_no_match() {
        return json!({
            "ok": false,
            "error": { "code": "no_match", "symbol_name": symbol_name }
        });
    }

    symbol_context_payload(records, &ctx, symbol_name)
}

/// Renders the `ok:true` symbol-context payload from a resolved
/// [`query::SymbolContext`], shared by the name-recall and the disambiguated
/// re-query paths so both lanes emit the identical envelope shape.
fn symbol_context_payload(
    records: &[GraphRecord],
    ctx: &query::SymbolContext<'_>,
    symbol_name: &str,
) -> Value {
    let trust = query::TrustIndex::build(records);

    let source_facts: Vec<Value> = ctx
        .source_facts
        .iter()
        .filter_map(|r| record_to_source_fact(r, &trust))
        .collect();
    let observations: Vec<Value> = ctx
        .observations
        .iter()
        .filter_map(|r| record_to_observation(r, &trust))
        .collect();
    // Section contract (issue #191): decisions surface in their own
    // section and never in `observations`.
    let decisions: Vec<Value> = ctx
        .decisions
        .iter()
        .filter_map(|r| record_to_decision(r, records, &trust))
        .collect();
    let project_state: Vec<Value> = ctx
        .project_state
        .iter()
        .filter_map(|r| record_to_linked_item(r, &trust))
        .collect();
    let artifacts: Vec<Value> = ctx
        .artifacts
        .iter()
        .filter_map(|r| record_to_linked_item(r, &trust))
        .collect();
    let verification_evidence: Vec<Value> = ctx
        .verification_evidence
        .iter()
        .filter_map(|r| record_to_linked_item(r, &trust))
        .collect();
    let topology_edges: Vec<Value> = ctx
        .topology_edges
        .iter()
        .filter_map(|r| record_to_topology_edge(r, &trust))
        .collect();
    let resolved_drift_targets = query::resolve_drift_targets(records, &ctx.drift_history);
    let drift_history: Vec<Value> = ctx
        .drift_history
        .iter()
        .zip(resolved_drift_targets)
        .filter_map(|(r, resolved)| record_to_drift(r, resolved, &trust))
        .collect();
    let unresolved: Vec<Value> = ctx.unresolved.iter().map(unresolved_to_json).collect();

    json!({
        "ok": true,
        "symbol_name": symbol_name,
        "source_facts": source_facts,
        "topology_edges": topology_edges,
        "observations": observations,
        "decisions": decisions,
        "project_state": project_state,
        "artifacts": artifacts,
        "verification_evidence": verification_evidence,
        "drift_history": drift_history,
        "unresolved": unresolved,
        // Issue #196: store-level domain presence. The twins always receive
        // the full record slice (`get_all_records`), so `from_records` reads
        // the whole store.
        "store_coverage": crate::query::StoreCoverage::from_records(records),
    })
}

/// Builds an evidence-backed task context for a record slice.
///
/// Accepts a canonical task ID, GitHub URL, GitHub short handle, or local JSONL handle.
/// Returns trust-separated sections.
///
/// Error codes:
/// - `no_match` — no task with the given handle exists
/// - `ambiguous_handle` — the handle matched more than one task
/// - `unsupported_handle` — the handle format is unrecognized
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn tool_task_evidence_from_records(records: &[GraphRecord], id_or_handle: &str) -> Value {
    let resolved_ids = match query::resolve_task_ids(records, id_or_handle) {
        Ok(ids) => ids,
        Err(query::TaskResolveError::Ambiguous { handle, candidates }) => {
            return json!({
                "ok": false,
                "error": {
                    "code": "ambiguous_handle",
                    "handle": handle,
                    "candidates": candidates,
                }
            });
        }
        Err(query::TaskResolveError::Unsupported { handle, message }) => {
            return json!({
                "ok": false,
                "error": {
                    "code": "unsupported_handle",
                    "handle": handle,
                    "message": message,
                }
            });
        }
    };

    let Some(task_id) = resolved_ids.iter().next() else {
        return json!({
            "ok": false,
            "error": { "code": "no_match", "id_or_handle": id_or_handle }
        });
    };
    let ctx = query::task_evidence_context(records, task_id);

    if ctx.is_no_match() {
        return json!({
            "ok": false,
            "error": { "code": "no_match", "id_or_handle": id_or_handle }
        });
    }

    let trust = query::TrustIndex::build(records);

    let tasks: Vec<Value> = ctx
        .tasks
        .iter()
        .filter_map(|r| record_to_linked_item(r, &trust))
        .collect();
    let acceptance_criteria: Vec<Value> = ctx
        .acceptance_criteria
        .iter()
        .filter_map(|r| {
            let mut item = record_to_linked_item(r, &trust)?;
            if item["status"].as_str() == Some("verified") {
                let GraphRecord::Node {
                    verification_link_id,
                    ..
                } = r
                else {
                    return Some(item);
                };
                let ver_id = verification_link_id.as_deref().or_else(|| {
                    records.iter().find_map(|edge| {
                        if let GraphRecord::Edge {
                            label: EdgeLabel::ClosesAcceptanceCriterion,
                            source,
                            target,
                            ..
                        } = edge
                        {
                            if source == r.id() {
                                Some(target.as_str())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                });
                if let Some(ver) = ver_id
                    .and_then(|vid| records.iter().find(|c| c.id() == vid))
                    .and_then(|r| record_to_linked_item(r, &trust))
                {
                    item["verification_record"] = ver;
                }
            }
            Some(item)
        })
        .collect();

    // Reuses the index built above: each build scans the whole graph and
    // allocates the node/edge/tombstone maps plus a temporal resolver, so a
    // second one would double that cost per request for no benefit.
    let source_facts: Vec<Value> = ctx
        .source_facts
        .iter()
        .filter_map(|r| record_to_source_fact(r, &trust))
        .collect();
    let observations: Vec<Value> = ctx
        .observations
        .iter()
        .filter_map(|r| record_to_observation(r, &trust))
        .collect();
    // Section contract (issue #191): decisions surface in their own
    // section and never in `observations`.
    let decisions: Vec<Value> = ctx
        .decisions
        .iter()
        .filter_map(|r| record_to_decision(r, records, &trust))
        .collect();
    let artifacts: Vec<Value> = ctx
        .artifacts
        .iter()
        .filter_map(|r| record_to_linked_item(r, &trust))
        .collect();
    let verification_evidence: Vec<Value> = ctx
        .verification_evidence
        .iter()
        .filter_map(|r| record_to_linked_item(r, &trust))
        .collect();
    let reviews: Vec<Value> = ctx
        .reviews
        .iter()
        .filter_map(|r| record_to_linked_item(r, &trust))
        .collect();
    let external_links: Vec<Value> = ctx
        .external_links
        .iter()
        .filter_map(|r| record_to_linked_item(r, &trust))
        .collect();
    let unresolved: Vec<Value> = ctx.unresolved.iter().map(unresolved_to_json).collect();

    json!({
        "ok": true,
        "task_id": task_id,
        "tasks": tasks,
        "acceptance_criteria": acceptance_criteria,
        "source_facts": source_facts,
        "observations": observations,
        "decisions": decisions,
        "artifacts": artifacts,
        "verification_evidence": verification_evidence,
        "reviews": reviews,
        "external_links": external_links,
        "unresolved": unresolved,
        // Issue #196: store-level domain presence; see the symbol twin.
        "store_coverage": crate::query::StoreCoverage::from_records(records),
    })
}

/// Builds a prior-failed-attempt history for a record slice (issue #188).
///
/// Resolves `handle` through the same documented order as `eg query failures`
/// (canonical code record ID → task/task-source handle → repo-relative file
/// path → exact symbol name → source/provenance handle) and returns the same
/// trust-separated sections: `runtime_failures` (trust
/// `verification_evidence`), `agent_failures` (trust `agent_authored`), and
/// `superseding_successes` (later passing verifications, which never hide the
/// older failures), plus the reached `patch_artifacts` and stable
/// `diagnostics`.
///
/// Every failure row carries a stable `record_id`/evidence handle and a
/// read-time `resolution_status` (`still_failing` | `since_resolved`).
/// Sections are canonically ordered oldest-first, so identical stores yield
/// byte-identical output.
///
/// Error codes (the same distinctions as the CLI exit codes, never a silent
/// empty success):
/// - `unsupported_handle` — malformed handle, same inputs the CLI rejects at exit 1
/// - `ambiguous_handle` — cross-repository collision, with a `candidates` list
/// - `no_match` — handle resolved to nothing live, same inputs the CLI rejects at exit 2
/// - `stale_handle` — handle named only tombstoned records
///
/// A resolved target with no recorded failures is NOT an error: it returns
/// `ok: true` with empty sections and a `safety_note`, because absence of
/// recorded failure is not evidence of safety and no failure cause is ever
/// inferred from the answer.
///
/// Rows reuse the CLI's redaction-safe serializers — no raw transcript text,
/// command stdout/stderr, or patch hunks appear anywhere; only hashes,
/// handles, bounded summaries, and redaction markers.
#[must_use]
pub fn tool_failure_history_from_records(records: &[GraphRecord], handle: &str) -> Value {
    let repo_index = query::RepositoryIndex::build(records);
    let target = match query::resolve_failure_handle(records, handle, &repo_index, None) {
        Ok(target) => target,
        Err(query::FailureHandleError::Ambiguous { handle, candidates }) => {
            return json!({
                "ok": false,
                "error": {
                    "code": "ambiguous_handle",
                    "handle": handle,
                    "candidates": candidates,
                }
            });
        }
        Err(query::FailureHandleError::Unsupported { handle, message }) => {
            return json!({
                "ok": false,
                "error": {
                    "code": "unsupported_handle",
                    "handle": handle,
                    "message": message,
                }
            });
        }
    };

    // A handle that resolved to nothing live in the store is a structured
    // no-match (or a stale handle when it named a tombstoned record). This is
    // distinct from a resolved target that simply has no recorded failures,
    // which is a genuine ok:true empty answer below (AC4).
    if target.is_empty() {
        let code = if target.stale {
            "stale_handle"
        } else {
            "no_match"
        };
        return json!({
            "ok": false,
            "error": { "code": code, "handle": handle },
        });
    }

    let ctx = query::failure_history_context(records, &target);

    // The CLI's redaction-safe serializers (issue #188, AC6): every row
    // carries only record IDs, hashes, handles, spans, and redaction
    // markers — never raw payloads.
    let runtime_failures: Vec<Value> = ctx
        .runtime_failures
        .iter()
        .map(|attempt| {
            serde_json::to_value(crate::cli::failure_attempt_json(attempt)).unwrap_or(Value::Null)
        })
        .collect();
    let agent_failures: Vec<Value> = ctx
        .agent_failures
        .iter()
        .map(|attempt| {
            serde_json::to_value(crate::cli::failure_attempt_json(attempt)).unwrap_or(Value::Null)
        })
        .collect();
    let superseding_successes: Vec<Value> = ctx
        .superseding_successes
        .iter()
        .map(|item| serde_json::to_value(crate::cli::audit_item(item)).unwrap_or(Value::Null))
        .collect();
    let patch_artifacts: Vec<Value> = ctx
        .patch_artifacts
        .iter()
        .map(|item| serde_json::to_value(crate::cli::audit_item(item)).unwrap_or(Value::Null))
        .collect();
    let diagnostics: Vec<Value> = ctx
        .diagnostics
        .iter()
        .map(|diag| {
            let mut obj = serde_json::Map::new();
            obj.insert("code".to_owned(), json!(diag.code));
            obj.insert("source_record_id".to_owned(), json!(diag.source_record_id));
            obj.insert("target_handle".to_owned(), json!(diag.target_handle));
            if !diag.relation.is_empty() {
                obj.insert("relation".to_owned(), json!(diag.relation));
            }
            if !diag.target_domain.is_empty() {
                obj.insert("target_domain".to_owned(), json!(diag.target_domain));
            }
            Value::Object(obj)
        })
        .collect();

    json!({
        "ok": true,
        "handle": handle,
        "target_type": ctx.target_kind,
        "target_ids": ctx.target_ids,
        "runtime_failures": runtime_failures,
        "agent_failures": agent_failures,
        "superseding_successes": superseding_successes,
        "patch_artifacts": patch_artifacts,
        "diagnostics": diagnostics,
        // AC4: absence of recorded failure is not evidence of safety, and no
        // failure cause is ever inferred — the answer states the limit.
        "safety_note": "No failure cause is inferred from this answer. A target with no recorded failures may still be unsafe: absence of recorded failure is not evidence of safety.",
    })
}

// ── Tool runners (daemon I/O) ─────────────────────────────────────────────────

/// Builds the machine-readable `freshness` object stamped onto MCP tool
/// responses (issue #220).
///
/// Reuses the store-freshness contract from #186 — no new vocabulary:
/// - `verdict` / `fresh`: the stable code from
///   [`freshness::Freshness::code`](crate::freshness::Freshness) (`fresh` /
///   `stale_head` / `stale_dirty` / `unknown`), computed by the exact code path
///   `eg freshness` uses, so the verdict always agrees with the CLI for the
///   same store and working-tree state.
/// - `stored_snapshot`: the source-snapshot identity the answer was derived
///   from, reusing the on-disk [`SourceSnapshotPayload`](crate::ir::SourceSnapshotPayload)
///   serialization (`head.state`: `commit` + `sha`, `no_git`, or `unborn_head`);
///   a store that predates snapshot stamping carries the explicit
///   `{"state": "pre_stamping"}` marker instead of a silent absence — never a
///   false `fresh`.
/// - `current_head` / `current_dirty`: the working-tree state the stored
///   snapshot was compared against, probed read-only at `repo_path`.
/// - `repository_id` / `repo_path` / `message`: which repository the verdict
///   was computed for, which tree it was compared against, and the
///   human-readable explanation from the #186 contract.
///
/// The probe is strictly read-only and offline: `git rev-parse` / `git status`
/// with `GIT_OPTIONAL_LOCKS=0` (the index is never refreshed), no network, and
/// no store writes. For a fixed store + working-tree state the object is
/// byte-identical across calls (the stored `scanned_at` is record data, fixed
/// for a fixed store).
#[must_use]
pub fn tool_freshness_stamp(
    records: &[GraphRecord],
    repo_path: &Path,
    data_dir: Option<&Path>,
) -> Value {
    let report = crate::cli::assess_freshness(repo_path, records, None, data_dir, None);
    // Explicit marker — a pre-stamping store must never read as a silent
    // absence, and must never classify `fresh`.
    let stored_snapshot = report.stored_snapshot.as_ref().map_or_else(
        || json!({"state": "pre_stamping"}),
        |snapshot| serde_json::to_value(snapshot).unwrap_or(Value::Null),
    );
    json!({
        "verdict": report.freshness,
        "fresh": report.fresh,
        "repository_id": report.repository_id,
        "repo_path": repo_path.display().to_string(),
        "stored_snapshot": stored_snapshot,
        "current_head": report.current_head,
        "current_dirty": report.current_dirty,
        "message": report.message,
    })
}

/// Stamps the [`tool_freshness_stamp`] object onto a successful tool payload
/// (issue #220).
///
/// Freshness is a trust signal, never suppression: the stamp is purely additive
/// and the full answer payload is preserved. Error payloads (`ok: false`) are
/// left untouched — freshness annotates answers, not failures.
pub fn stamp_freshness_on_payload(
    payload: &mut Value,
    records: &[GraphRecord],
    repo_path: &Path,
    data_dir: Option<&Path>,
) {
    if payload.get("ok").and_then(Value::as_bool) != Some(true) {
        return;
    }
    payload["freshness"] = tool_freshness_stamp(records, repo_path, data_dir);
}

fn run_inspect_store(data_dir: &Path, repo_path: &Path) -> Value {
    let client = match DaemonClient::from_data_dir(data_dir) {
        Ok(c) => c,
        Err(e) => return daemon_error(&e.to_string()),
    };
    let (records, unknown_versions, snapshot_timestamp) = match client.get_all_records() {
        Ok(r) => r,
        Err(e) => return daemon_error(&e.to_string()),
    };
    let mut payload =
        tool_inspect_store_from_records(&records, &unknown_versions, &snapshot_timestamp);
    stamp_freshness_on_payload(&mut payload, &records, repo_path, Some(data_dir));
    payload
}

// ── Record → JSON helpers ─────────────────────────────────────────────────────

fn record_to_source_fact(record: &GraphRecord, trust: &query::TrustIndex<'_>) -> Option<Value> {
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
        return None;
    };
    Some(json!({
        "record_id": id,
        "kind": kind.as_str(),
        "trust": trust.classify(record).as_str(),
        "name": name,
        "repo_relative_path": repo_relative_path,
        "span": span,
        "git_commit": temporal.as_ref().map(|t| t.git_commit.as_str()),
        "valid_time": valid_time.as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
        "language": language,
        "symbol_kind": symbol_kind,
    }))
}

fn record_to_observation(record: &GraphRecord, trust: &query::TrustIndex<'_>) -> Option<Value> {
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
        return None;
    };
    let provenance_handle = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => Some(format!("{a}:{s}")),
        (Some(a), None) => Some(a.to_owned()),
        _ => None,
    };
    let links: Vec<Value> = evidence_links
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|l| {
            json!({
                "target_record_id": l.target_record_id,
                "target_domain": l.target_domain,
                "relation": l.relation,
                "confidence": l.confidence,
            })
        })
        .collect();
    Some(json!({
        "record_id": id,
        "kind": kind.as_str(),
        "trust": trust.classify(record).as_str(),
        "summary": summary,
        "text": text,
        "provenance_handle": provenance_handle,
        "agent_id": agent_id,
        "session_id": session_id,
        "observed_at": observed_at,
        "confidence": confidence,
        "failure_kind": failure_kind,
        "exit_code": exit_code,
        "evidence_links": links,
    }))
}

/// Returns only citation metadata from an `OutputHandle`, stripping any inlined payload.
fn output_handle_citation(h: &crate::ir::OutputHandle) -> Value {
    json!({ "hash": h.hash, "bytes": h.bytes })
}

/// Builds the MCP JSON row for one agent-authored `Decision` record
/// (issue #191): `decision_text` plus the non-empty `rationale_summary`,
/// provenance, confidence, and resolved evidence handles. Reuses the
/// shared [`query::context_decision`] builder so the MCP lane emits the
/// same row shape as the CLI.
fn record_to_decision(
    record: &GraphRecord,
    records: &[GraphRecord],
    trust: &query::TrustIndex<'_>,
) -> Option<Value> {
    query::context_decision(record, records, trust).and_then(|row| serde_json::to_value(row).ok())
}

/// Returns only citation metadata from a `PatchHandle`, stripping any inlined bytes.
fn patch_handle_citation(h: &crate::ir::PatchHandle) -> Value {
    json!({ "path": h.path })
}

fn record_to_linked_item(record: &GraphRecord, trust: &query::TrustIndex<'_>) -> Option<Value> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        title,
        text,
        summary,
        status,
        verification_kind,
        exit_code,
        executed_at,
        evidence_quality,
        source_artifact_path,
        source_artifact_hash,
        repo_relative_path,
        edit_kind,
        patch_status,
        patch_bytes_hash,
        patch_bytes_size,
        patch_handle,
        target_files,
        validation_summary,
        base_commit,
        producer_session_id,
        author,
        evidence_links,
        url,
        system_native_id,
        body_handle,
        stdout_handle,
        stderr_handle,
        ..
    } = record
    else {
        return None;
    };
    let redacted_validation = validation_summary
        .as_deref()
        .map(|s| if s.is_empty() { s } else { "<summarized>" });
    let links: Vec<Value> = evidence_links
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|l| {
            json!({
                "target_record_id": l.target_record_id,
                "target_domain": l.target_domain,
                "relation": l.relation,
                "confidence": l.confidence,
            })
        })
        .collect();
    Some(json!({
        "record_id": id,
        "kind": kind.as_str(),
        "trust": trust.classify(record).as_str(),
        "summary": summary,
        "title": title,
        "name": name,
        "text": text,
        "status": status,
        "verification_kind": verification_kind,
        "exit_code": exit_code,
        "executed_at": executed_at,
        "evidence_quality": evidence_quality,
        "source_artifact_path": source_artifact_path,
        "source_artifact_hash": source_artifact_hash,
        "stdout_handle": stdout_handle.as_deref().map(output_handle_citation),
        "stderr_handle": stderr_handle.as_deref().map(output_handle_citation),
        "repo_relative_path": repo_relative_path,
        "edit_kind": edit_kind,
        "patch_status": patch_status,
        "patch_handle": patch_handle.as_deref().map(patch_handle_citation),
        "patch_bytes_hash": patch_bytes_hash,
        "patch_bytes_size": patch_bytes_size,
        "target_files": target_files,
        "validation_summary": redacted_validation,
        "base_commit": base_commit,
        "producer_session_id": producer_session_id,
        "body_handle": body_handle.as_deref().map(output_handle_citation),
        "author": author,
        "url": url,
        "system_native_id": system_native_id,
        "evidence_links": links,
    }))
}

fn record_to_topology_edge(record: &GraphRecord, trust: &query::TrustIndex<'_>) -> Option<Value> {
    let GraphRecord::Edge {
        id,
        label,
        source,
        target,
        summary,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    Some(json!({
        "record_id": id,
        "trust": trust.classify(record).as_str(),
        "label": label.as_str(),
        "source_id": source,
        "target_id": target,
        "summary": summary,
        "git_commit": temporal.as_ref().map(|t| t.git_commit.as_str()),
        "valid_time": temporal.as_ref().map(|t| t.valid_time.as_str()),
    }))
}

/// Builds one `drift_history` row from a `SemanticDrift` record (issue #108),
/// matching the field set the CLI's `ContextDrift` and the daemon's
/// `context_drift_to_json` both emit, given its already-resolved target
/// `(repo_relative_path, name, span)` — the same shape
/// `query::resolve_drift_target`/`resolve_drift_targets` return — so a row
/// carries the same citable handle across every transport. Callers resolve
/// targets for the whole `drift_history` slice in one batched pass via
/// `query::resolve_drift_targets` (issue #497 Codex review: resolving one row
/// at a time made serialization O(D×N) for D drift rows). Returns `None` for
/// a non-drift record — `ctx.drift_history` only ever contains `SemanticDrift`
/// nodes by construction, but a stub row would otherwise silently diverge from
/// the CLI/daemon shape if that invariant were ever broken.
fn record_to_drift(
    record: &GraphRecord,
    resolved: (Option<&str>, Option<&str>, Option<crate::ir::SourceSpan>),
    trust: &query::TrustIndex<'_>,
) -> Option<Value> {
    let GraphRecord::Node {
        id,
        semantic_drift: Some(drift),
        ..
    } = record
    else {
        return None;
    };
    let (resolved_path, _resolved_name, resolved_span) = resolved;
    let mut obj = serde_json::Map::new();
    obj.insert("record_id".to_owned(), json!(id.as_str()));
    obj.insert("trust".to_owned(), json!(trust.classify(record).as_str()));
    obj.insert("score".to_owned(), json!(drift.score));
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
    obj.insert("embedding_model".to_owned(), json!(&drift.embedding_model));
    if let Some(p) = resolved_path {
        obj.insert("repo_relative_path".to_owned(), json!(p));
    }
    if let Some(s) = resolved_span {
        obj.insert("span".to_owned(), json!(s));
    }
    Some(Value::Object(obj))
}

fn unresolved_to_json(u: &query::UnresolvedRef) -> Value {
    json!({
        "source_record_id": u.source_record_id,
        "target_handle": u.target_handle,
        "relation": u.relation,
        "target_domain": u.target_domain,
        "verification_status": "unresolved",
    })
}

// ── Error helpers ─────────────────────────────────────────────────────────────

/// Builds the stable `missing_argument` error payload for an empty required
/// tool argument (issue #194: the shape is part of the frozen contract).
#[must_use]
pub fn missing_argument_error(field: &str) -> Value {
    json!({
        "ok": false,
        "error": {
            "code": "missing_argument",
            "field": field,
            "message": format!("{field} is required and must be non-empty")
        }
    })
}

fn daemon_error(msg: &str) -> Value {
    let code = if msg.to_lowercase().contains("stale") || msg.to_lowercase().contains("metadata") {
        "daemon_stale"
    } else {
        "daemon_not_running"
    };
    json!({
        "ok": false,
        "error": { "code": code, "message": msg }
    })
}

/// Builds the [`ObservationRequest`] for the `record_observation` tool.
///
/// Pure: performs no I/O and contacts no daemon. An absent or empty required
/// field is a [`ProvenanceError::missing`]; a target domain outside
/// `codegraph`/`verification` is [`ProvenanceError::invalid`] (mirroring the
/// CLI's `--evidence-domain` pre-check in `eg write observation`). The built
/// request then validates through [`validate_observation_request`] — the same
/// function the record builder uses — so malformed timestamps, unknown agent
/// kinds, out-of-range confidence, and inconsistent evidence links are
/// rejected here, before any daemon contact. The CLI and the MCP tool share
/// one provenance contract, enforced by one function, so the two transports
/// cannot drift apart.
///
/// The evidence relation is derived from the target domain exactly as the CLI
/// derives it: `codegraph` → `OBSERVES`, `verification` → `VALIDATED_BY`.
/// Each link carries the observation's confidence, also matching the CLI.
///
/// # Errors
///
/// Returns a [`ProvenanceError`] naming the first missing or invalid field:
/// an absent or empty required field is `missing`, a target domain outside
/// `codegraph`/`verification` is `invalid`, and anything the shared
/// [`validate_observation_request`] contract rejects (malformed
/// `observed_at`, unknown `agent_kind`, out-of-range confidence, inconsistent
/// evidence links) is returned unchanged.
pub fn record_observation_request_from_args(
    args: &RecordObservationArgs,
) -> Result<ObservationRequest, ProvenanceError> {
    fn required(field: &str, value: Option<&str>) -> Result<String, ProvenanceError> {
        match value {
            Some(text) if !text.is_empty() => Ok(text.to_owned()),
            _ => Err(ProvenanceError::missing(field)),
        }
    }
    let confidence = args
        .confidence
        .ok_or_else(|| ProvenanceError::missing("confidence"))?;
    let targets = args.evidence.as_deref().unwrap_or_default();
    if targets.is_empty() {
        return Err(ProvenanceError::missing("evidence"));
    }
    let evidence_links = targets
        .iter()
        .map(|target| record_observation_evidence_link(target, confidence))
        .collect::<Result<Vec<_>, _>>()?;
    let req = ObservationRequest {
        provenance: EvidenceProvenance {
            agent_id: required("agent_id", args.agent_id.as_deref())?,
            agent_kind: args.agent_kind.clone().unwrap_or_default(),
            session_id: required("session_id", args.session_id.as_deref())?,
            observed_at: args.observed_at.clone().unwrap_or_else(now_rfc3339),
            source_handle: Some(required("source_handle", args.source_handle.as_deref())?),
        },
        text: required("text", args.text.as_deref())?,
        confidence,
        evidence_links,
        supersession: None,
    };
    // One shared provenance contract: the CLI's record builder validates
    // through this same function.
    validate_observation_request(&req)?;
    Ok(req)
}

/// Builds one [`EvidenceLink`] for a `record_observation` evidence target.
///
/// Only `codegraph` and `verification` domains are accepted — the CLI rejects
/// anything else before building, because the daemon rejects `OBSERVES` on
/// non-codegraph targets and `VALIDATED_BY` on non-verification targets.
fn record_observation_evidence_link(
    target: &RecordObservationEvidenceTarget,
    confidence: f64,
) -> Result<EvidenceLink, ProvenanceError> {
    let target_record_id = match target.target_record_id.as_deref() {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => return Err(ProvenanceError::missing("evidence.target_record_id")),
    };
    let target_domain = match target.target_domain.as_deref() {
        Some(domain) if !domain.is_empty() => domain,
        _ => return Err(ProvenanceError::missing("evidence.target_domain")),
    };
    let relation = match target_domain {
        "codegraph" => EdgeLabel::Observes.as_str(),
        "verification" => EdgeLabel::ValidatedBy.as_str(),
        _ => return Err(ProvenanceError::invalid("evidence.target_domain")),
    };
    Ok(EvidenceLink {
        target_record_id: Some(target_record_id),
        target_domain: target_domain.to_owned(),
        relation: relation.to_owned(),
        confidence: confidence.to_string(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    })
}

/// Maps a [`ProvenanceError`] to the frozen structured error envelope.
///
/// The envelope names the offending field but never echoes payload text: the
/// message carries only the stable code and the field path.
#[must_use]
pub fn provenance_error_payload(error: &ProvenanceError) -> Value {
    let message = match error.code {
        "missing_field" => format!("{} is required", error.field),
        _ => format!("{} is invalid", error.field),
    };
    json!({
        "ok": false,
        "error": {
            "code": error.code,
            "field": error.field,
            "message": message,
        }
    })
}

/// Structured error for a write the daemon refused after accepting the
/// request (e.g. a dangling evidence target under `RejectBatch`).
///
/// The batch is atomic: when this error fires, nothing was persisted. `detail`
/// carries the daemon's first per-record diagnostic (stable handles only —
/// never payload text).
fn write_rejected_error(detail: &str) -> Value {
    json!({
        "ok": false,
        "error": {
            "code": "write_rejected",
            "message": "the store rejected the write; nothing was persisted",
            "detail": detail,
        }
    })
}

/// Structured error for a write that failed in transport (daemon unreachable
/// mid-call, ingest HTTP failure, unparseable response).
///
/// `message` is the daemon client's own error text, which never contains
/// payload values.
fn write_failed_error(message: &str) -> Value {
    json!({
        "ok": false,
        "error": { "code": "write_failed", "message": message }
    })
}

// ── Argument helpers ──────────────────────────────────────────────────────────

fn opt_data_dir(data_dir: Option<&str>, default: &Path) -> PathBuf {
    data_dir.map_or_else(|| default.to_path_buf(), PathBuf::from)
}

/// Resolves the working-tree path a freshness verdict is computed against.
///
/// Defaults to the MCP server process's current directory, mirroring
/// `eg freshness`'s default `repo_path` of `.`. A missing/unresolvable default
/// degrades to an empty path, whose working-tree probe yields `no_git` and
/// therefore an `unknown` verdict — never a false `fresh`.
fn opt_repo_path(repo_path: Option<&str>) -> PathBuf {
    repo_path.map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    )
}

// ── Domain category mapping (matches the existing CLI inspect output) ─────────

#[allow(clippy::missing_const_for_fn)]
fn domain_category(domain: &str) -> &'static str {
    match domain {
        "codegraph" => "Deterministic Source Facts",
        "semantic" => "Derived Measurements",
        "agent_memory" => "Agent-Authored Claims",
        "project" => "Project/Work State",
        "artifact" => "Artifacts",
        "verification" => "Verification Evidence",
        "user_context" => "User Context",
        "log" => "Runtime Observations",
        _ => "Unknown Domain",
    }
}
