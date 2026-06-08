//! Command-line interface for Egregore.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::{
    adapters::{DryRunSink, ingest_records, records_from_jsonl},
    evidence::{
        ArtifactRequest, CommandEvidenceRequest, EvidenceProvenance, ObservationRequest,
        VerificationRequest, build_artifact_records, build_command_evidence_records,
        build_observation_records, build_verification_records,
    },
    ir::{
        EdgeLabel, EvidenceLink, Graph, GraphRecord, NodeKind, SemanticDriftMetadata, SourceSpan,
    },
    link_evidence::{self, LinkOptions},
    local_project, query, scan_repository_history_with_override, scan_repository_with_override,
    schema_version::{RecordVersion, record_version},
    traj::{self, ImportOptions},
};

#[cfg(feature = "embedded-aletheiadb")]
use crate::adapters::EmbeddedAletheiaSink;
#[cfg(feature = "embeddings")]
use crate::adapters::SemanticMatch;
#[cfg(feature = "embedded-aletheiadb")]
use crate::daemon::{DaemonClient, DaemonConfig};
#[cfg(feature = "embedded-aletheiadb")]
use crate::repair;

#[derive(Debug, Parser)]
#[command(
    name = "egregore",
    about = "Manage agentic SWE knowledge graphs on AletheiaDB"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Scan a repository and write graph JSONL.
    Scan {
        /// Repository path to scan.
        repo_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
        /// Override the auto-detected repository identity.
        ///
        /// Forces `identity_source = operator_override`. Use for fixture-stable
        /// tests or when the auto-detected remote is wrong (e.g. a mirror).
        #[arg(long)]
        repo_id_override: Option<String>,
    },
    /// Replay Git history and write temporal graph JSONL.
    ScanHistory {
        /// Git repository path to scan.
        repo_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
        /// Override the auto-detected repository identity.
        ///
        /// Forces `identity_source = operator_override`. Use for fixture-stable
        /// tests or when the auto-detected remote is wrong (e.g. a mirror).
        #[arg(long)]
        repo_id_override: Option<String>,
    },
    /// Inspect a graph JSONL file or a running daemon.
    Inspect {
        /// Graph JSONL path to inspect.
        graph: Option<PathBuf>,
        /// Route the inspection through the running daemon (requires --data-dir, conflicts with graph).
        #[cfg(feature = "embedded-aletheiadb")]
        #[arg(long, requires = "data_dir", conflicts_with = "graph")]
        daemon: bool,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, conflicts_with = "graph")]
        data_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "text")]
        format: OutputFormat,
    },
    /// Ingest graph JSONL through a storage adapter.
    Ingest {
        /// Graph JSONL path to ingest.
        graph: PathBuf,
        /// Adapter to use for ingestion.
        #[arg(long, default_value = "dry-run")]
        adapter: IngestAdapter,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Agent ID for daemon-backed writes.
        #[arg(long, default_value = "egregore-cli")]
        agent_id: String,
        /// Session ID for daemon-backed writes.
        #[arg(long, default_value = "egregore-cli")]
        session_id: String,
        /// Idempotency key for daemon-backed writes.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Generate and store semantic embeddings for file and symbol nodes.
        #[cfg(feature = "embeddings")]
        #[arg(long)]
        embed: bool,
    },
    /// Import a rust-swe-agent .traj trajectory file into agent-memory JSONL.
    ImportTraj {
        /// Path to the `.traj` trajectory file.
        traj_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Import a Codex session or rollout JSONL into agent-memory JSONL.
    ImportCodex {
        /// Path to the Codex session or rollout JSONL file.
        codex_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Import a Claude Code transcript JSONL into agent-memory JSONL.
    ///
    /// Documented in `docs/cli/claude-code-import.md`.
    ImportClaudeCode {
        /// Path to the Claude Code session transcript JSONL file.
        transcript_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Import local project/task JSONL into project-graph JSONL.
    ImportLocalTasks {
        /// Directory containing `.jsonl` task files, or path to a single `.jsonl` file.
        tasks_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
        /// Repository root used to compute repo-relative source handles.
        /// Defaults to the current working directory.
        #[arg(long)]
        repo_root: Option<PathBuf>,
        /// Fixed RFC 3339 transaction time for deterministic output (useful for tests).
        /// Defaults to the current wall-clock instant.
        #[arg(long)]
        transaction_time: Option<String>,
    },
    /// Import work items from an external source into project-graph JSONL.
    ///
    /// Documented in `docs/cli/github-import.md` and the policy in
    /// `docs/schema/import-github.md`.
    Import {
        /// Import source.
        #[command(subcommand)]
        source: ImportSource,
    },
    /// Link imported agent evidence to code-graph facts.
    ///
    /// Reads a code-graph JSONL (from `scan`) and an agent-evidence JSONL
    /// (from `import-traj` or `import-codex`) and resolves every unambiguous
    /// repo-relative file or symbol handle to a stable code-graph record ID.
    ///
    /// Resolved handles emit `TOUCHED_FILE`, `FAILED_ON`, or `MENTIONS_SYMBOL`
    /// edges in the output JSONL.  Unresolved handles are written to stderr as
    /// machine-readable JSON diagnostics (one object per line).
    ///
    /// Documented in `docs/cli/link-evidence.md`.
    LinkEvidence {
        /// Code-graph JSONL produced by `scan`.
        #[arg(long)]
        code_graph: PathBuf,
        /// Agent-evidence JSONL produced by `import-traj` or `import-codex`.
        #[arg(long)]
        evidence: PathBuf,
        /// Output JSONL path for resolved cross-domain edges.
        #[arg(long)]
        out: PathBuf,
    },
    /// Query an existing graph JSONL for symbols, files, or drift records.
    Query {
        /// Query subcommand.
        #[command(subcommand)]
        subcommand: QuerySubcommand,
    },
    /// Write typed evidence records (observations, command evidence, artifacts, verification).
    ///
    /// This is the default contract for interactive clients, SDKs, and the future MCP surface.
    /// Use `eg ingest` for batch importers that already produce well-formed JSONL.
    ///
    /// Rejected writes exit with code 1 and print a machine-readable error to stderr:
    ///   `{"code":"missing_field","field":"agent_id"}`
    ///
    /// Example (accepted observation):
    ///   eg write observation \
    ///     --agent-id my-agent --session-id sess-001 \
    ///     --observed-at 2026-05-30T10:00:00Z \
    ///     --source-handle "src/lib.rs:sha256:abc123" \
    ///     --text "function f has high complexity" \
    ///     --confidence 0.9 \
    ///     --evidence-target codegraph:v4:deadbeef \
    ///     --out evidence.jsonl
    ///
    /// Example (rejected — missing agent-id):
    ///   eg write observation --session-id sess-001 ...
    ///   # exits 1, stderr: `{"code":"missing_field","field":"agent_id"}`
    ///
    /// Example (read-back to verify handles survived):
    ///   eg inspect evidence.jsonl
    Write {
        /// Write subcommand.
        #[command(subcommand)]
        kind: WriteKind,
    },
    /// Run the semantic code search relevance evaluation against a corpus file.
    ///
    /// Requires a pre-built embedded store (`eg ingest --adapter embedded`).
    /// Exits 0 if top-3 recall meets the threshold, 1 with a diagnostic if missed.
    EvalSemantic {
        /// Path to the semantic relevance corpus JSON file.
        corpus: PathBuf,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long)]
        data_dir: PathBuf,
        /// Number of top results to retrieve per query.
        #[arg(long, default_value = "3")]
        top_k: usize,
        /// Minimum top-3 recall fraction required to pass (0.0–1.0, default 0.80).
        #[arg(long, default_value = "0.8", value_parser = parse_threshold)]
        threshold: f64,
        /// Minimum cosine score for an ambiguous-query result to count as a false positive (0.0–1.0, default 0.50).
        #[arg(long, default_value = "0.5", value_parser = parse_threshold)]
        fp_threshold: f64,
    },
    /// Run the semantic drift calibration against a corpus file.
    EvalDrift {
        /// Path to the drift calibration corpus JSON file.
        #[arg(long, default_value = "corpus/drift_calibration_corpus.json")]
        corpus: PathBuf,
        /// Selection threshold for drift evaluation (0.0–1.0, default 0.20).
        #[arg(long, default_value = "0.20", value_parser = parse_threshold)]
        threshold: f64,
    },
    /// Manage the local Egregore daemon.
    #[cfg(feature = "embedded-aletheiadb")]
    Daemon {
        /// Daemon action.
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Record an operator decision for a promotion candidate.
    Decide {
        /// Candidate ID to decide on.
        candidate_id: String,
        /// Outcome of the decision.
        #[arg(long)]
        outcome: String,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Graph JSONL path.
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Output JSONL path for the generated decision records.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Edited rule text (required if outcome is `edited_then_approved`).
        #[arg(long)]
        edited_rule_text: Option<String>,
        /// Rationale for the decision.
        #[arg(long)]
        rationale: Option<String>,
        /// Operator who decided.
        #[arg(long, default_value = "operator")]
        decided_by: String,
        /// Surface where prompt was shown.
        #[arg(long, default_value = "cli")]
        prompt_surface: String,
        /// Operator the prompt was shown to.
        #[arg(long, default_value = "operator")]
        prompted_to: String,
    },
    /// Offline repair workflow for Egregore stores.
    ///
    /// Use `repair preflight` first to inspect ownership, then `repair run --confirm`
    /// to perform the repair. Both commands emit machine-readable JSON to stdout.
    ///
    /// Documented workflow:
    ///   1. eg repair preflight --data-dir .egregore
    ///   2. eg daemon stop --data-dir .egregore   (if verdict is live)
    ///   3. eg repair run --data-dir .egregore --confirm
    ///   4. eg repair preflight --data-dir .egregore   (verify clean state)
    ///   5. eg daemon start --data-dir .egregore
    #[cfg(feature = "embedded-aletheiadb")]
    Repair {
        /// Repair action.
        #[command(subcommand)]
        action: RepairCliAction,
    },
    /// Start an MCP stdio server exposing read-only evidence-query tools.
    ///
    /// Processes JSON-RPC 2.0 messages from stdin (one per line) and writes
    /// responses to stdout.  Tools: `inspect_store`, `symbol_context`,
    /// `task_evidence`.  Requires a running `eg daemon`.
    ///
    /// Configure as an MCP server in Claude Code:
    ///   `eg mcp --data-dir .egregore`
    #[cfg(feature = "embedded-aletheiadb")]
    Mcp {
        /// `AletheiaDB` data directory (default: `.egregore`).
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
    },
}

/// Subcommands for `import`.
#[derive(Debug, Subcommand)]
enum ImportSource {
    /// Import one GitHub repository's issues, pull requests, comments, and reviews.
    ///
    /// Local-first and explicit: fetches over the REST API, writes a JSONL
    /// handoff plus an idempotency state file, then exits. Never runs in the
    /// daemon, polls, subscribes to webhooks, or crawls beyond `<owner>/<repo>`.
    ///
    /// Auth (closed enumeration): `GH_TOKEN`, then `GITHUB_TOKEN`, then
    /// `--token-file`, then `gh auth token`. Failures exit non-zero with a
    /// stable `{"code":"github_..."}` diagnostic that never echoes the token.
    Github {
        /// `<owner>/<repo>` to import.
        repo: String,
        /// Output JSONL handoff path.
        #[arg(long)]
        out: PathBuf,
        /// Idempotency state file. Defaults to `<out-dir>/.github-import-state.json`.
        #[arg(long)]
        state_file: Option<PathBuf>,
        /// Seeded code-graph JSONL (from `scan`) for `TOUCHES_FILE` resolution.
        #[arg(long)]
        code_graph: Option<PathBuf>,
        /// Operator-managed token file (read at import time; never logged).
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// API base URL override (for testing against a local mock server).
        #[arg(long)]
        api_base: Option<String>,
        /// Fixed RFC 3339 transaction time for deterministic output (tests).
        #[arg(long)]
        transaction_time: Option<String>,
        /// Skip rate-limit/retry backoff sleeps (tests only).
        #[arg(long, hide = true)]
        no_backoff: bool,
    },
}

/// Output format for query results.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, clap::ValueEnum)]
enum OutputFormat {
    /// Newline-delimited JSON objects (default, machine-readable).
    #[default]
    Json,
    /// Human-readable one-line-per-result form.
    Text,
}

/// Subcommands for `query`.
#[derive(Debug, Subcommand)]
enum QuerySubcommand {
    /// Find symbol nodes by name.
    Symbol {
        /// Symbol name to look up.
        name: String,
        /// Graph JSONL path (mutually exclusive with --data-dir / --daemon).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Route the query through the running daemon (requires --data-dir, conflicts with --graph).
        #[cfg(feature = "embedded-aletheiadb")]
        #[arg(long, requires = "data_dir", conflicts_with = "graph")]
        daemon: bool,
        /// Restrict to the record at this commit SHA or unique prefix.
        /// Shorthand for --as-of keyed by a Git SHA. Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Return the symbol state at the most recent commit at or before this
        /// RFC 3339 instant (valid-time axis). Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Transaction-time selector (reserved, not yet implemented).
        /// Returns a `not_implemented` error envelope rather than silently ignoring the flag.
        #[arg(long)]
        tx_as_of: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List symbols defined in a file via DEFINES edges.
    File {
        /// Repository-relative file path.
        path: String,
        /// Graph JSONL path (mutually exclusive with --data-dir / --daemon).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Route the query through the running daemon (requires --data-dir, conflicts with --graph).
        #[cfg(feature = "embedded-aletheiadb")]
        #[arg(long, requires = "data_dir", conflicts_with = "graph")]
        daemon: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Find semantic drift nodes ranked by score descending.
    Drift {
        /// Graph JSONL path (mutually exclusive with --data-dir / --daemon).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Route the query through the running daemon (requires --data-dir, conflicts with --graph).
        #[cfg(feature = "embedded-aletheiadb")]
        #[arg(long, requires = "data_dir", conflicts_with = "graph")]
        daemon: bool,
        /// Maximum number of results (default 10).
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Find similar code by semantic embedding.
    #[cfg(feature = "embeddings")]
    Semantic {
        /// Query text (symbol name, description, or code snippet).
        query: String,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long)]
        data_dir: PathBuf,
        /// Route the query through the running daemon instead of opening the store directly.
        #[arg(long)]
        daemon: bool,
        /// Maximum number of results (default 10).
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Retrieve evidence-backed context for a named symbol.
    ///
    /// Returns a structured JSON object with five trust-separated sections:
    /// `source_facts` (code-graph), `observations` (agent-authored),
    /// `project_state` (tasks/ACs), `artifacts`, and `verification_evidence`.
    /// Missing evidence links are surfaced as `unresolved` items.
    ///
    /// On no-match: emits `{"ok":false,"error":{"code":"no_match",...}}` to
    /// stdout and exits with code 2. No synthesized prose; no hallucinated
    /// fallback records.
    Context {
        /// Symbol name to look up.
        name: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Retrieve evidence-backed context for a task.
    Task {
        /// Task record ID or source handle (URL, `system_native_id`, local handle).
        id_or_handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Route the query through the running daemon (requires --data-dir, conflicts with --graph).
        #[cfg(feature = "embedded-aletheiadb")]
        #[arg(long, requires = "data_dir", conflicts_with = "graph")]
        daemon: bool,
    },
    /// List pending promotion candidates.
    Candidates {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List active approved policies (`Preference`, `WorkflowRule`, `NamingDecision`, `Constraint`).
    Policy {
        /// Optional repository filter.
        #[arg(long)]
        repo: Option<String>,
        /// Optional path glob filter.
        #[arg(long)]
        path_glob: Option<String>,
        /// Optional language filter.
        #[arg(long)]
        language: Option<String>,
        /// Optional lifecycle phase filter.
        #[arg(long)]
        lifecycle_phase: Option<String>,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Trace the audit trail back from durable policies to observations.
    Audit {
        /// Durable policy record ID.
        id: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually `exclusive_with` --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Audit the evidence behind one agent-authored memory claim.
    ///
    /// Starts from a memory record and returns its provenance, supporting,
    /// contradicting, and superseding evidence, related code and project
    /// handles, and verification evidence — separated by trust class so an
    /// agent-authored claim is never presented as source truth. Output never
    /// includes raw transcript text, command output, or protected payloads;
    /// only bounded summaries, hashes, handles, and redaction markers.
    ///
    /// "No evidence found" is not evidence that the claim is true.
    ///
    /// Documented in `docs/cli/memory-audit.md`.
    Memory {
        /// Memory record ID (`agent_memory:v1:...`) or source artifact/session handle.
        id_or_handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Exclude unverified observations; report each as an `excluded` diagnostic.
        #[arg(long)]
        verified_only: bool,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, clap::ValueEnum)]
enum IngestAdapter {
    /// Validate ingest ordering and read-back without writing external storage.
    DryRun,
    /// Write into an embedded `AletheiaDB` store.
    #[cfg(feature = "embedded-aletheiadb")]
    Embedded,
    /// Write through a running local Egregore daemon.
    #[cfg(feature = "embedded-aletheiadb")]
    Daemon,
}

#[cfg(feature = "embedded-aletheiadb")]
#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Start the daemon in the background.
    Start {
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
        /// Loopback host to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// TCP port. Use 0 to ask the OS to choose one.
        #[arg(long, default_value_t = 37_383)]
        port: u16,
        /// Bounded write queue capacity.
        #[arg(long, default_value_t = 64)]
        write_queue_capacity: usize,
    },
    /// Run the daemon in the current process.
    #[command(hide = true)]
    Run {
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
        /// Loopback host to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// TCP port. Use 0 to ask the OS to choose one.
        #[arg(long, default_value_t = 37_383)]
        port: u16,
        /// Bounded write queue capacity.
        #[arg(long, default_value_t = 64)]
        write_queue_capacity: usize,
    },
    /// Report daemon status.
    Status {
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
    },
    /// Stop the daemon.
    Stop {
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
    },
}

/// Subcommands for `repair`.
#[cfg(feature = "embedded-aletheiadb")]
#[derive(Debug, Subcommand)]
enum RepairCliAction {
    /// Inspect ownership verdict with zero mutations (the documented first step).
    ///
    /// Always safe to run: never creates, removes, or modifies any file.
    /// Output is machine-readable JSON; pipe to `jq` for interactive inspection.
    ///
    /// Verdicts: `live` | `stopped` | `stale_no_owner` | `ambiguous`.
    /// When `allow` is false, `refusal_reasons` contains stable codes explaining why.
    Preflight {
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
    },
    /// Run an offline repair session (requires `--confirm` or `--dry-run`).
    ///
    /// Dry-run proves zero mutations while returning the same allow/refuse verdict.
    /// Confirmed repair removes stale metadata and writes a recovery report.
    ///
    /// Supported actions: `stale_metadata_cleanup`, `recovery_report_generation`.
    /// Unsupported: graph-record deletion, store rewrite, compaction, migration.
    Run {
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
        /// Confirm the repair: actually perform filesystem changes.
        /// Mutually exclusive with `--dry-run`.
        #[arg(long, conflicts_with = "dry_run")]
        confirm: bool,
        /// Simulate the repair: return the verdict with zero mutations.
        /// Mutually exclusive with `--confirm`.
        #[arg(long, conflicts_with = "confirm")]
        dry_run: bool,
    },
}

/// Subcommands for `write`.
#[derive(Debug, Subcommand)]
enum WriteKind {
    /// Write a typed Observation node backed by at least one evidence link.
    ///
    /// Writes fail immediately when any required provenance field is absent.
    ///
    /// The error is a machine-readable JSON object on stderr naming the field;
    /// it never echoes the observation text or any other payload value.
    Observation {
        /// Stable agent identity; required.
        #[arg(long, default_value = "")]
        agent_id: String,
        /// Agent kind (`other`, `claude-code`, `vantage`, `codex`, `rust-swe-agent`, `human`).
        #[arg(long, default_value = "other")]
        agent_kind: String,
        /// Active session identifier; required.
        #[arg(long, default_value = "")]
        session_id: String,
        /// RFC 3339 observation timestamp; required.
        #[arg(long, default_value = "")]
        observed_at: String,
        /// Citable source artifact path or hash; required.
        #[arg(long, default_value = "")]
        source_handle: String,
        /// Observation body text; required.
        #[arg(long, default_value = "")]
        text: String,
        /// Confidence in `[0.0, 1.0]`.
        #[arg(long, default_value_t = 0.9)]
        confidence: f64,
        /// Stable record IDs to cite as evidence (space-separated or repeated).
        /// At least one is required.
        #[arg(long, num_args = 0..)]
        evidence_target: Vec<String>,
        /// Domain of the evidence targets (`codegraph`, `verification`, etc.).
        #[arg(long, default_value = "codegraph")]
        evidence_domain: String,
        /// Output JSONL path for the produced records.
        #[arg(long, required = true)]
        out: PathBuf,
    },
    /// Write a typed `CommandRun` (command evidence) record in the verification domain.
    CommandEvidence {
        /// Stable agent identity; required.
        #[arg(long, default_value = "")]
        agent_id: String,
        /// Agent kind.
        #[arg(long, default_value = "other")]
        agent_kind: String,
        /// Active session identifier; required.
        #[arg(long, default_value = "")]
        session_id: String,
        /// RFC 3339 observation timestamp; required.
        #[arg(long, default_value = "")]
        observed_at: String,
        /// Citable source artifact path or hash.
        #[arg(long)]
        source_handle: Option<String>,
        /// RFC 3339 execution timestamp; required.
        #[arg(long, default_value = "")]
        executed_at: String,
        /// Shell exit code; required.
        #[arg(long)]
        exit_code: Option<i64>,
        /// Captured stdout text.
        #[arg(long)]
        stdout: Option<String>,
        /// Captured stderr text.
        #[arg(long)]
        stderr: Option<String>,
        /// Evidence quality: `verbatim`, `summarized`, or `referenced_only`.
        #[arg(long, default_value = "verbatim")]
        evidence_quality: String,
        /// Source artifact path; at least one of `--source-artifact-path` or
        /// `--source-artifact-hash` is required.
        #[arg(long, default_value = "")]
        source_artifact_path: String,
        /// Source artifact hash.
        #[arg(long, default_value = "")]
        source_artifact_hash: String,
        /// Output JSONL path for the produced records.
        #[arg(long, required = true)]
        out: PathBuf,
    },
    /// Write a typed `PatchArtifact` record in the artifact domain.
    Artifact {
        /// Stable agent identity; required.
        #[arg(long, default_value = "")]
        agent_id: String,
        /// Agent kind.
        #[arg(long, default_value = "other")]
        agent_kind: String,
        /// Active session identifier; required.
        #[arg(long, default_value = "")]
        session_id: String,
        /// RFC 3339 observation timestamp; required.
        #[arg(long, default_value = "")]
        observed_at: String,
        /// Citable source artifact path or hash.
        #[arg(long)]
        source_handle: Option<String>,
        /// Path to the raw patch file (unified diff).
        #[arg(long, required = true)]
        patch_file: PathBuf,
        /// Repository-relative target files touched by the patch (repeated).
        #[arg(long, num_args = 0..)]
        target_file: Vec<String>,
        /// Patch validation status.
        #[arg(long, default_value = "unverified")]
        patch_status: String,
        /// Git commit SHA the patch was authored against.
        #[arg(long)]
        base_commit: Option<String>,
        /// Source artifact path.
        #[arg(long, default_value = "")]
        source_artifact_path: String,
        /// Source artifact hash.
        #[arg(long, default_value = "")]
        source_artifact_hash: String,
        /// Human-readable summary of any validation performed on this artifact.
        #[arg(long, default_value = "")]
        validation_summary: String,
        /// Output JSONL path for the produced records.
        #[arg(long, required = true)]
        out: PathBuf,
    },
    /// Write a typed Verification record in the verification domain.
    Verification {
        /// Stable agent identity; required.
        #[arg(long, default_value = "")]
        agent_id: String,
        /// Agent kind.
        #[arg(long, default_value = "other")]
        agent_kind: String,
        /// Active session identifier; required.
        #[arg(long, default_value = "")]
        session_id: String,
        /// RFC 3339 observation timestamp; required.
        #[arg(long, default_value = "")]
        observed_at: String,
        /// Citable source artifact path or hash.
        #[arg(long)]
        source_handle: Option<String>,
        /// RFC 3339 execution timestamp; required.
        #[arg(long, default_value = "")]
        executed_at: String,
        /// Verification outcome: `pass`, `fail`, `skip`, `error`, or `timeout`.
        #[arg(long, default_value = "")]
        status: String,
        /// Verification kind: `test_run`, `command_run`, `ci_status`, etc.
        #[arg(long, default_value = "command_run")]
        verification_kind: String,
        /// Captured stdout text.
        #[arg(long)]
        stdout: Option<String>,
        /// Evidence quality: `high`, `medium`, or `low`.
        #[arg(long, default_value = "high")]
        evidence_quality: String,
        /// Source artifact path.
        #[arg(long, default_value = "")]
        source_artifact_path: String,
        /// Source artifact hash.
        #[arg(long, default_value = "")]
        source_artifact_hash: String,
        /// Stable ID of a `CommandRun` record that produced this result.
        #[arg(long)]
        linked_command_evidence_id: Option<String>,
        /// Output JSONL path for the produced records.
        #[arg(long, required = true)]
        out: PathBuf,
    },
}

/// Parses process arguments and runs the CLI.
///
/// # Errors
///
/// Returns an error if scanning, serialization, file IO, or inspection fails.
pub fn run() -> Result<()> {
    run_cli(Cli::parse())
}

#[allow(clippy::too_many_lines)]
fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Scan {
            repo_path,
            out,
            repo_id_override,
        } => scan(&repo_path, &out, repo_id_override.as_deref()),
        Commands::ScanHistory {
            repo_path,
            out,
            repo_id_override,
        } => scan_history(&repo_path, &out, repo_id_override.as_deref()),
        Commands::Inspect {
            graph,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            data_dir,
            format,
        } => {
            #[cfg(not(feature = "embedded-aletheiadb"))]
            let daemon = false;
            inspect(graph.as_deref(), daemon, data_dir.as_deref(), format)
        }
        Commands::Ingest {
            graph,
            adapter,
            data_dir,
            agent_id,
            session_id,
            idempotency_key,
            #[cfg(feature = "embeddings")]
            embed,
        } => ingest(
            &graph,
            adapter,
            data_dir.as_deref(),
            &agent_id,
            &session_id,
            idempotency_key.as_deref(),
            #[cfg(feature = "embeddings")]
            embed,
        ),
        Commands::ImportTraj { traj_path, out } => import_traj_cmd(&traj_path, &out),
        Commands::ImportCodex { codex_path, out } => import_codex_cmd(&codex_path, &out),
        Commands::ImportClaudeCode {
            transcript_path,
            out,
        } => import_claude_code_cmd(&transcript_path, &out),
        Commands::ImportLocalTasks {
            tasks_path,
            out,
            repo_root,
            transaction_time,
        } => import_local_tasks_cmd(
            &tasks_path,
            &out,
            repo_root.as_deref(),
            transaction_time.as_deref(),
        ),
        Commands::Import { source } => match source {
            ImportSource::Github {
                repo,
                out,
                state_file,
                code_graph,
                token_file,
                api_base,
                transaction_time,
                no_backoff,
            } => import_github_cmd(
                &repo,
                &out,
                state_file.as_deref(),
                code_graph.as_deref(),
                token_file.as_deref(),
                api_base.as_deref(),
                transaction_time.as_deref(),
                no_backoff,
            ),
        },
        Commands::LinkEvidence {
            code_graph,
            evidence,
            out,
        } => link_evidence_cmd(&code_graph, &evidence, &out),
        Commands::Query { subcommand } => query_cmd(subcommand),
        Commands::Write { kind } => write_evidence(kind),
        Commands::EvalSemantic {
            corpus,
            data_dir,
            top_k,
            threshold,
            fp_threshold,
        } => {
            #[cfg(feature = "embeddings")]
            return eval_semantic_cmd(&corpus, &data_dir, top_k, threshold, fp_threshold);
            #[cfg(not(feature = "embeddings"))]
            {
                let _ = (corpus, data_dir, top_k, threshold, fp_threshold);
                anyhow::bail!("eval-semantic requires the 'embeddings' feature")
            }
        }
        Commands::EvalDrift { corpus, threshold } => {
            #[cfg(feature = "embeddings")]
            return eval_drift_cmd(&corpus, threshold);
            #[cfg(not(feature = "embeddings"))]
            {
                let _ = (corpus, threshold);
                anyhow::bail!("eval-drift requires the 'embeddings' feature")
            }
        }
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::Daemon { action } => daemon(action),
        Commands::Decide {
            candidate_id,
            outcome,
            data_dir,
            graph,
            out,
            edited_rule_text,
            rationale,
            decided_by,
            prompt_surface,
            prompted_to,
        } => decide_cmd(
            candidate_id,
            outcome,
            data_dir,
            graph,
            out,
            edited_rule_text,
            rationale,
            decided_by,
            prompt_surface,
            prompted_to,
        ),
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::Repair { action } => repair_cmd(action),
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::Mcp { data_dir } => crate::mcp::run_stdio(&data_dir),
    }
}

/// Prints a machine-readable JSON error envelope to stderr and exits with code 1.
///
/// Callers that detect a [`crate::evidence::ProvenanceError`] use this instead of
/// propagating the error so that `main` does not emit a second human-readable line
/// after the JSON envelope has already been written.
fn write_evidence_error(e: &crate::evidence::ProvenanceError) -> ! {
    eprintln!(r#"{{"code":"{}", "field":"{}"}}"#, e.code, e.field);
    process::exit(1);
}

/// Handles `eg write <kind>` subcommands.
///
/// On provenance failure the function writes a JSON error to stderr
/// (`{"code":"missing_field","field":"<name>"}`) and returns an error.
#[allow(clippy::too_many_lines)]
fn write_evidence(kind: WriteKind) -> Result<()> {
    match kind {
        WriteKind::Observation {
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            source_handle,
            text,
            confidence,
            evidence_target,
            evidence_domain,
            out,
        } => {
            // Only codegraph (OBSERVES) and verification (VALIDATED_BY) are supported.
            // The daemon rejects OBSERVES on non-codegraph targets and VALIDATED_BY on
            // non-verification targets, so any other domain would produce invalid JSONL.
            if evidence_domain != "codegraph" && evidence_domain != "verification" {
                write_evidence_error(&crate::evidence::ProvenanceError::invalid(
                    "evidence_domain",
                ));
            }
            let relation = if evidence_domain == "verification" {
                EdgeLabel::ValidatedBy.as_str().to_owned()
            } else {
                EdgeLabel::Observes.as_str().to_owned()
            };
            let evidence_links: Vec<EvidenceLink> = evidence_target
                .into_iter()
                .map(|target_id| EvidenceLink {
                    target_record_id: Some(target_id),
                    target_domain: evidence_domain.clone(),
                    relation: relation.clone(),
                    confidence: confidence.to_string(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                })
                .collect();
            let req = ObservationRequest {
                provenance: EvidenceProvenance {
                    agent_id,
                    agent_kind,
                    session_id,
                    observed_at,
                    source_handle: Some(source_handle),
                },
                text,
                confidence,
                evidence_links,
            };
            let outcome =
                build_observation_records(&req).unwrap_or_else(|e| write_evidence_error(&e));
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id)
        }
        WriteKind::CommandEvidence {
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            source_handle,
            executed_at,
            exit_code,
            stdout,
            stderr,
            evidence_quality,
            source_artifact_path,
            source_artifact_hash,
            out,
        } => {
            let exit_code = exit_code.unwrap_or_else(|| {
                write_evidence_error(&crate::evidence::ProvenanceError::missing("exit_code"))
            });
            let req = CommandEvidenceRequest {
                provenance: EvidenceProvenance {
                    agent_id,
                    agent_kind,
                    session_id,
                    observed_at,
                    source_handle,
                },
                executed_at,
                exit_code,
                stdout,
                stderr,
                evidence_quality,
                source_artifact_path,
                source_artifact_hash,
            };
            let outcome =
                build_command_evidence_records(&req).unwrap_or_else(|e| write_evidence_error(&e));
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id)
        }
        WriteKind::Artifact {
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            source_handle,
            patch_file,
            target_file,
            patch_status,
            base_commit,
            source_artifact_path,
            source_artifact_hash,
            validation_summary,
            out,
        } => {
            let patch_bytes = fs::read(&patch_file)
                .with_context(|| format!("failed to read patch file {}", patch_file.display()))?;
            let req = ArtifactRequest {
                provenance: EvidenceProvenance {
                    agent_id,
                    agent_kind,
                    session_id,
                    observed_at,
                    source_handle,
                },
                patch_bytes,
                target_files: target_file,
                patch_status,
                base_commit,
                source_artifact_path,
                source_artifact_hash,
                validation_summary,
            };
            let outcome = build_artifact_records(&req).unwrap_or_else(|e| write_evidence_error(&e));
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id)
        }
        WriteKind::Verification {
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            source_handle,
            executed_at,
            status,
            verification_kind,
            stdout,
            evidence_quality,
            source_artifact_path,
            source_artifact_hash,
            linked_command_evidence_id,
            out,
        } => {
            let req = VerificationRequest {
                provenance: EvidenceProvenance {
                    agent_id,
                    agent_kind,
                    session_id,
                    observed_at,
                    source_handle,
                },
                executed_at,
                status,
                verification_kind,
                stdout,
                evidence_quality,
                source_artifact_path,
                source_artifact_hash,
                linked_command_evidence_id,
            };
            let outcome =
                build_verification_records(&req).unwrap_or_else(|e| write_evidence_error(&e));
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id)
        }
    }
}

/// Serializes evidence records to JSONL and prints the evidence handle.
fn write_evidence_outcome(
    records: &[GraphRecord],
    out: &Path,
    evidence_handle: &str,
) -> Result<()> {
    let mut graph = Graph::new();
    for record in records {
        graph.push(record.clone());
    }
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize evidence JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write evidence JSONL to {}", out.display()))?;
    println!(
        r#"{{"ok":true,"evidence_handle":"{}","records":{}}}"#,
        evidence_handle,
        records.len()
    );
    Ok(())
}

fn import_local_tasks_cmd(
    tasks_path: &Path,
    out: &Path,
    repo_root: Option<&Path>,
    transaction_time: Option<&str>,
) -> Result<()> {
    let repo_root = match repo_root {
        Some(r) => r.to_path_buf(),
        None => std::env::current_dir().context("failed to determine current directory")?,
    };
    let opts = local_project::ImportOptions {
        transaction_time: transaction_time.map(str::to_owned),
        ..local_project::ImportOptions::default()
    };
    let result = local_project::import_local_tasks(tasks_path, &repo_root, &opts)
        .with_context(|| format!("failed to import local tasks from {}", tasks_path.display()))?;
    let jsonl = result
        .graph
        .to_jsonl()
        .context("failed to serialize project-graph JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    println!(
        "imported {} records ({} diagnostics) from {}",
        result.graph.records().len(),
        result.diagnostic_count,
        tasks_path.display()
    );
    Ok(())
}

/// Handles `eg import github <owner>/<repo>`.
///
/// On failure, prints a scrubbed machine-readable `{"code":"github_..."}` line
/// to stderr and exits non-zero. The handoff JSONL and state file are written
/// only on success, so an auth/rate-limit failure leaves no partial state
/// (`docs/schema/import-github.md` §5).
#[allow(clippy::too_many_arguments)]
fn import_github_cmd(
    repo: &str,
    out: &Path,
    state_file: Option<&Path>,
    code_graph: Option<&Path>,
    token_file: Option<&Path>,
    api_base: Option<&str>,
    transaction_time: Option<&str>,
    no_backoff: bool,
) -> Result<()> {
    use crate::github::{
        client::scrub_line,
        error::GithubError,
        import::{ImportOptions, run_import},
        state::State,
    };

    // Validate the repo argument shape before any network work.
    if repo.split('/').filter(|s| !s.is_empty()).count() != 2 || repo.matches('/').count() != 1 {
        let e = GithubError::InvalidRepoArg {
            arg: repo.to_owned(),
        };
        eprintln!("{}", scrub_line(&format!(r#"{{"code":"{}"}}"#, e.code())));
        eprintln!("{}", scrub_line(&e.to_string()));
        process::exit(1);
    }

    let api_base = api_base.map_or_else(
        || crate::github::client::DEFAULT_API_BASE.to_owned(),
        str::to_owned,
    );

    // Default state-file path: alongside the handoff output.
    let default_state = out
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".github-import-state.json");
    let state_path = state_file.map_or(default_state, Path::to_path_buf);

    let prior_state = State::load_or_fresh(&state_path, repo, &api_base);

    let opts = ImportOptions {
        source_repo: repo,
        api_base,
        token_file,
        code_graph,
        transaction_time: transaction_time.map(str::to_owned),
        no_backoff,
    };

    match run_import(&opts, prior_state) {
        Ok(outcome) => {
            fs::write(out, &outcome.jsonl)
                .with_context(|| format!("failed to write handoff JSONL to {}", out.display()))?;
            outcome
                .state
                .save(&state_path)
                .with_context(|| format!("failed to write state file {}", state_path.display()))?;
            let q = outcome
                .summary
                .quota_remaining
                .map_or_else(|| "unknown".to_owned(), |v| v.to_string());
            // Per-run summary (scrubbed) to stderr per §4.
            eprintln!(
                "{}",
                scrub_line(&format!(
                    "egregore-github-import: requests={} quota_remaining={} elapsed={:.2}s",
                    outcome.summary.requests, q, outcome.summary.elapsed_secs
                ))
            );
            println!(
                "imported {} records from {} into {}",
                outcome.summary.emitted_records,
                repo,
                out.display()
            );
            Ok(())
        }
        Err(e) => {
            // Machine-readable, token-scrubbed diagnostic; no partial state write.
            eprintln!("{}", scrub_line(&format!(r#"{{"code":"{}"}}"#, e.code())));
            eprintln!("{}", scrub_line(&e.to_string()));
            process::exit(1);
        }
    }
}

fn import_codex_cmd(codex_path: &Path, out: &Path) -> Result<()> {
    let opts = crate::codex::ImportOptions::default();
    let graph = crate::codex::import_codex(codex_path, &opts)
        .with_context(|| format!("failed to import Codex JSONL from {}", codex_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize agent-memory JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    println!(
        "imported {} records from {}",
        graph.records().len(),
        codex_path.display()
    );
    Ok(())
}

fn import_claude_code_cmd(transcript_path: &Path, out: &Path) -> Result<()> {
    let opts = crate::claude_code::ImportOptions::default();
    let graph =
        crate::claude_code::import_claude_code(transcript_path, &opts).with_context(|| {
            format!(
                "failed to import Claude Code transcript from {}",
                transcript_path.display()
            )
        })?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize agent-memory JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    println!(
        "imported {} records from {}",
        graph.records().len(),
        transcript_path.display()
    );
    Ok(())
}

fn import_traj_cmd(traj_path: &Path, out: &Path) -> Result<()> {
    let opts = ImportOptions::default();
    let graph = traj::import_traj(traj_path, &opts)
        .with_context(|| format!("failed to import .traj from {}", traj_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize agent-memory JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    println!(
        "imported {} records from {}",
        graph.records().len(),
        traj_path.display()
    );
    Ok(())
}

fn link_evidence_cmd(code_graph_path: &Path, evidence_path: &Path, out: &Path) -> Result<()> {
    let cg_jsonl = fs::read_to_string(code_graph_path).with_context(|| {
        format!(
            "failed to read code-graph JSONL from {}",
            code_graph_path.display()
        )
    })?;
    let ev_jsonl = fs::read_to_string(evidence_path).with_context(|| {
        format!(
            "failed to read evidence JSONL from {}",
            evidence_path.display()
        )
    })?;

    let code_graph = records_from_jsonl(&cg_jsonl).with_context(|| {
        format!(
            "failed to parse code-graph JSONL from {}",
            code_graph_path.display()
        )
    })?;
    let evidence = records_from_jsonl(&ev_jsonl).with_context(|| {
        format!(
            "failed to parse evidence JSONL from {}",
            evidence_path.display()
        )
    })?;

    let link_output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    // Write diagnostics to stderr as JSON lines (machine-readable, per AC5).
    for diag in &link_output.diagnostics {
        let line = serde_json::to_string(diag).context("failed to serialize diagnostic")?;
        eprintln!("{line}");
    }

    // Serialize resolved edges as JSONL.
    let mut lines: Vec<String> = Vec::with_capacity(link_output.edges.len());
    for edge in &link_output.edges {
        let line = serde_json::to_string(edge).context("failed to serialize linked edge")?;
        lines.push(line);
    }
    let content = if lines.is_empty() {
        String::new()
    } else {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    };

    fs::write(out, content)
        .with_context(|| format!("failed to write output to {}", out.display()))?;

    println!(
        "linked: {} edges resolved, {} diagnostics",
        link_output.edges.len(),
        link_output.diagnostics.len()
    );
    Ok(())
}

fn scan(repo_path: &Path, out: &Path, repo_id_override: Option<&str>) -> Result<()> {
    let graph = scan_repository_with_override(repo_path, repo_id_override)
        .with_context(|| format!("failed to scan repository {}", repo_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write graph JSONL to {}", out.display()))?;
    Ok(())
}

fn scan_history(repo_path: &Path, out: &Path, repo_id_override: Option<&str>) -> Result<()> {
    let graph = scan_repository_history_with_override(repo_path, repo_id_override)
        .with_context(|| format!("failed to scan Git history for {}", repo_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize history graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write history graph JSONL to {}", out.display()))?;
    Ok(())
}

fn print_counts_text(counts: &InspectCounts) {
    println!("records: {}", counts.records);
    println!("nodes: {}", counts.nodes);
    println!("edges: {}", counts.edges);
    println!("tombstones: {}", counts.tombstones);
    println!("diagnostics: {}", counts.diagnostics);

    // Group counts by domain category
    let mut domain_groups: BTreeMap<&str, Vec<(&RecordVersion, usize)>> = BTreeMap::new();
    for (version, count) in &counts.schema_versions {
        domain_groups
            .entry(version.domain.as_str())
            .or_default()
            .push((version, *count));
    }

    let ordered_domains = vec![
        ("codegraph", "Deterministic Source Facts (codegraph)"),
        ("semantic", "Derived Measurements (semantic)"),
        ("agent_memory", "Agent-Authored Claims (agent_memory)"),
        ("project", "Project/Work State (project)"),
        ("artifact", "Artifacts (artifact)"),
        ("verification", "Verification Evidence (verification)"),
        ("user_context", "User Context (user_context)"),
    ];

    for (dom_name, category) in ordered_domains {
        if let Some(mut items) = domain_groups.remove(dom_name) {
            println!("{category}:");
            items.sort_by_key(|(v, _)| (&v.kind, v.version));
            for (version, count) in items {
                println!("  {} v{}: {}", version.kind, version.version, count);
            }
        }
    }

    for (dom_name, mut items) in domain_groups {
        println!("{dom_name} (unknown domain):");
        items.sort_by_key(|(v, _)| (&v.kind, v.version));
        for (version, count) in items {
            println!("  {} v{}: {}", version.kind, version.version, count);
        }
    }

    if !counts.unknown_schema_versions.is_empty() {
        println!("unknown schema versions:");
        let mut unknown_items: Vec<_> = counts.unknown_schema_versions.iter().collect();
        unknown_items.sort_by_key(|(v, _)| (&v.domain, &v.kind, v.version));
        for (version, count) in unknown_items {
            println!(
                "  {} {} v{}: {}",
                version.domain, version.kind, version.version, count
            );
        }
    }

    for repo in &counts.repositories {
        println!("repository: {} ({})", repo.id, repo.identity_summary);
    }
    for (kind, count) in &counts.producer_kinds {
        println!("producer_kind {kind}: {count}");
    }
    for (version, count) in &counts.egregore_versions {
        println!("egregore_version {version}: {count}");
    }
}

fn inspect(
    graph: Option<&Path>,
    daemon: bool,
    data_dir: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    #[cfg(not(feature = "embedded-aletheiadb"))]
    let _ = data_dir;
    #[cfg(feature = "embedded-aletheiadb")]
    if daemon {
        let default_path = PathBuf::from(".egregore");
        let data_dir = data_dir.unwrap_or(&default_path);
        let client = DaemonClient::from_data_dir(data_dir).with_context(|| {
            format!(
                "failed to inspect data-dir {}: daemon metadata is missing or invalid",
                data_dir.display()
            )
        })?;
        client.health().with_context(|| {
            format!(
                "failed to inspect data-dir {}: daemon is not running or unresponsive",
                data_dir.display()
            )
        })?;
        let (records, unknown_versions, snapshot_timestamp) =
            client.get_all_records().with_context(|| {
                format!(
                "failed to inspect data-dir {}: invalid authorization token or daemon read error",
                data_dir.display()
            )
            })?;

        let counts = InspectCounts::from_records(&records, &unknown_versions);

        match format {
            OutputFormat::Json => {
                let json_val = counts.to_json(&snapshot_timestamp);
                println!("{}", serde_json::to_string_pretty(&json_val)?);
            }
            OutputFormat::Text => {
                print_counts_text(&counts);
            }
        }
        return Ok(());
    }

    #[cfg(not(feature = "embedded-aletheiadb"))]
    if daemon {
        anyhow::bail!("daemon inspection requires 'embedded-aletheiadb' feature");
    }

    let graph = graph.ok_or_else(|| anyhow::anyhow!("graph file path or --daemon is required"))?;
    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    let counts = InspectCounts::from_jsonl(&jsonl)?;
    let snapshot_timestamp = chrono::Utc::now().to_rfc3339();

    match format {
        OutputFormat::Json => {
            let json_val = counts.to_json(&snapshot_timestamp);
            println!("{}", serde_json::to_string_pretty(&json_val)?);
        }
        OutputFormat::Text => {
            print_counts_text(&counts);
        }
    }
    Ok(())
}

fn ingest(
    graph: &Path,
    adapter: IngestAdapter,
    data_dir: Option<&Path>,
    agent_id: &str,
    session_id: &str,
    idempotency_key: Option<&str>,
    #[cfg(feature = "embeddings")] embed: bool,
) -> Result<()> {
    #[cfg(not(feature = "embedded-aletheiadb"))]
    let _ = (data_dir, agent_id, session_id, idempotency_key);

    #[cfg(feature = "embeddings")]
    if embed && adapter != IngestAdapter::Embedded {
        anyhow::bail!("--embed requires --adapter embedded");
    }

    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    let records = records_from_jsonl(&jsonl).context("failed to parse graph JSONL")?;

    let report = match adapter {
        IngestAdapter::DryRun => {
            let mut sink = DryRunSink::default();
            ingest_records(&records, &mut sink)
        }
        #[cfg(feature = "embedded-aletheiadb")]
        IngestAdapter::Embedded => {
            let data_dir = data_dir.map_or_else(|| PathBuf::from(".egregore"), Path::to_path_buf);
            #[cfg(feature = "embeddings")]
            let mut sink = if embed {
                let (vectors, dimensions) = generate_embeddings(&records)?;
                EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, dimensions)
                    .with_context(|| {
                        format!("failed to open embedded store {}", data_dir.display())
                    })?
            } else {
                EmbeddedAletheiaSink::open(&data_dir).with_context(|| {
                    format!("failed to open embedded store {}", data_dir.display())
                })?
            };
            #[cfg(not(feature = "embeddings"))]
            let mut sink = EmbeddedAletheiaSink::open(&data_dir)
                .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
            let report = ingest_records(&records, &mut sink);
            if report.is_success() {
                sink.persist_indexes().with_context(|| {
                    format!("failed to persist embedded store {}", data_dir.display())
                })?;
            }
            report
        }
        #[cfg(feature = "embedded-aletheiadb")]
        IngestAdapter::Daemon => {
            let data_dir = data_dir.map_or_else(|| PathBuf::from(".egregore"), Path::to_path_buf);
            let idempotency_key =
                idempotency_key.context("--idempotency-key is required for --adapter daemon")?;
            let client = DaemonClient::from_data_dir(&data_dir)
                .with_context(|| format!("failed to load daemon for {}", data_dir.display()))?;
            let response =
                client.ingest_records(&records, agent_id, session_id, idempotency_key)?;
            println!("attempted: {}", response.attempted);
            println!("succeeded: {}", response.succeeded);
            println!("failed: {}", response.failed);
            println!("idempotent: {}", response.idempotent);
            if response.failed == 0 {
                return Ok(());
            }
            for failure in &response.failures {
                eprintln!("{}: {}", failure.record_id, failure.message);
            }
            anyhow::bail!("ingest failed for {} records", response.failed);
        }
    };

    println!("attempted: {}", report.attempted);
    println!("succeeded: {}", report.succeeded);
    println!("failed: {}", report.failed);

    if report.is_success() {
        Ok(())
    } else {
        for failure in &report.failures {
            eprintln!("{}: {}", failure.record_id, failure.message);
        }
        anyhow::bail!("ingest failed for {} records", report.failed);
    }
}

#[cfg(feature = "embedded-aletheiadb")]
fn daemon(action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start {
            data_dir,
            host,
            port,
            write_queue_capacity,
        } => {
            let mut config = DaemonConfig::new(data_dir);
            config.host = host;
            config.port = port;
            config.write_queue_capacity = write_queue_capacity;
            let metadata = crate::daemon::start_background(&config)?;
            println!("daemon started at {}", metadata.address);
            Ok(())
        }
        DaemonAction::Run {
            data_dir,
            host,
            port,
            write_queue_capacity,
        } => {
            let mut config = DaemonConfig::new(data_dir);
            config.host = host;
            config.port = port;
            config.write_queue_capacity = write_queue_capacity;
            crate::daemon::run_foreground(&config)
        }
        DaemonAction::Status { data_dir } => {
            let Some(metadata) = crate::daemon::active_metadata(&data_dir)? else {
                anyhow::bail!("daemon not running for {}", data_dir.display());
            };
            println!("daemon running at {}", metadata.address);
            let client = crate::daemon::DaemonClient::new(metadata);
            let status = client.status()?;
            render_daemon_pressure(&status);
            Ok(())
        }
        DaemonAction::Stop { data_dir } => {
            crate::daemon::stop(&data_dir)?;
            println!("daemon stopped");
            Ok(())
        }
    }
}

/// Handles `eg repair preflight` and `eg repair run`.
#[cfg(feature = "embedded-aletheiadb")]
fn repair_cmd(action: RepairCliAction) -> Result<()> {
    match action {
        RepairCliAction::Preflight { data_dir } => {
            let report = repair::preflight(&data_dir)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        RepairCliAction::Run {
            data_dir,
            confirm,
            dry_run,
        } => {
            let report = repair::run_repair(&data_dir, dry_run, confirm)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

/// Renders the daemon write-admission pressure block in human-readable form.
///
/// The HTTP `GET /v1/status` JSON remains the stable machine-readable contract;
/// this rendering is for operators reading the terminal.
#[cfg(feature = "embedded-aletheiadb")]
fn render_daemon_pressure(status: &serde_json::Value) {
    let pressure = &status["pressure"];
    let state = pressure["state"].as_str().unwrap_or("unknown");
    let depth = pressure["queue_depth"].as_u64().unwrap_or(0);
    let capacity = pressure["queue_capacity"].as_u64().unwrap_or(0);
    let rejections = pressure["total_rejections"].as_u64().unwrap_or(0);
    println!("pressure: {state} (write queue {depth}/{capacity}, {rejections} rejected)");
    match state {
        "saturated" => {
            let retry_after_ms = pressure["retry_after_ms"].as_u64().unwrap_or(500);
            println!(
                "  daemon is alive but backpressuring: wait at least {retry_after_ms} ms, then retry the same write with its original idempotency key."
            );
            println!("  do not bypass the daemon with direct embedded writes.");
        }
        "busy" => {
            println!("  daemon is admitting writes; no retry action needed.");
        }
        _ => {
            println!("  daemon is idle and accepting writes.");
        }
    }
}

// ---------------------------------------------------------------------------
// Query output types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SymbolResult<'a> {
    record_id: &'a str,
    schema_version: u32,
    name: &'a str,
    kind: &'static str,
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
}

#[derive(Serialize)]
struct DriftResult<'a> {
    record_id: &'a str,
    schema_version: u32,
    before_commit: &'a str,
    after_commit: &'a str,
    before_valid_time: &'a str,
    after_valid_time: &'a str,
    embedding_model_provider: &'a str,
    embedding_model_name: &'a str,
    embedding_model_version: &'a str,
    embedding_model_dim: u32,
    embedding_model_content_hash: &'a str,
    metric_kind: &'static str,
    prior_record_id: &'a str,
    target_record_id: &'a str,
    score: f64,
    selection_threshold: f64,
    selection_basis: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    status: &'static str,
}

/// Output row for a semantic similarity result.
#[cfg(feature = "embeddings")]
#[derive(Serialize)]
struct SemanticResult<'a> {
    record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
}

#[cfg(feature = "embeddings")]
impl<'a> From<&'a SemanticMatch> for SemanticResult<'a> {
    fn from(m: &'a SemanticMatch) -> Self {
        Self {
            record_id: &m.record_id,
            name: m.name.as_deref(),
            repo_relative_path: m.repo_relative_path.as_deref(),
            score: m.score,
            span: m.span,
        }
    }
}

// ---------------------------------------------------------------------------
// Context query output types (issue #38)
// ---------------------------------------------------------------------------

/// One item in the `source_facts` section of a context query result.
///
/// Every field that was present on the source record is forwarded directly so
/// the output is fully citable. Per AC2 from issue #38, every item must
/// include `record_id` plus at least one of `repo_relative_path`,
/// `git_commit`, or `valid_time`.
#[derive(Serialize)]
struct ContextSourceFact<'a> {
    record_id: &'a str,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
}

/// One item in the `observations` section.
///
/// Per AC3 from issue #38, every observation must include `record_id`,
/// `provenance_handle` (or `agent_id`/`session_id`), `observed_at`,
/// `confidence`, and the evidence links.
#[derive(Serialize)]
struct ContextObservation<'a> {
    record_id: &'a str,
    /// Node kind (`"Observation"`, `"Decision"`, or `"Failure"`).
    /// Lets consumers distinguish subjective observation types without
    /// re-inspecting the raw graph.
    kind: &'static str,
    /// Agent-facing summary from the raw `GraphRecord`. Always present —
    /// `Decision` records use this field for their human-readable content
    /// rather than `text`, so consumers must not rely on `text` alone.
    summary: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    /// Provenance handle composed from `agent_id:session_id` when both are present.
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<&'a str>,
    /// Failure classification for `Failure` records (e.g. `"command_failure"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_kind: Option<&'a str>,
    /// Shell exit code for `Failure` or `CommandRun` records that represent a failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i64>,
    /// Supporting evidence links from the observation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    evidence_links: Vec<&'a crate::ir::EvidenceLink>,
}

/// One item in the `project_state`, `artifacts`, or `verification_evidence` sections.
#[derive(Serialize)]
struct ContextLinkedItem<'a> {
    record_id: &'a str,
    kind: &'static str,
    /// Agent-facing summary from the raw `GraphRecord`. Populated for all records
    /// so consumers can understand the item without reloading the graph — especially
    /// for `Artifact` records where `title`/`name`/`text`/`status` may all be absent.
    summary: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    /// Human-readable criterion text for `AcceptanceCriterion` nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_kind: Option<&'a str>,
    /// Shell exit code for `CommandRun` verification records.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i64>,
    /// RFC 3339 timestamp when the command was executed (`CommandRun`).
    #[serde(skip_serializing_if = "Option::is_none")]
    executed_at: Option<&'a str>,
    /// Evidence capture quality: `"verbatim"`, `"summarized"`, or `"referenced_only"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_quality: Option<&'a str>,
    /// Captured stdout from a `CommandRun` / `TestRun` (hash + optional inline).
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout_handle: Option<&'a crate::ir::OutputHandle>,
    /// Captured stderr from a `CommandRun` / `TestRun`.
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr_handle: Option<&'a crate::ir::OutputHandle>,
    /// Repo-relative path to the script, config, or CI definition that was run.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_artifact_path: Option<&'a str>,
    /// BLAKE3 hex of the artefact at `source_artifact_path` at run time.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_artifact_hash: Option<&'a str>,
    /// File path for `FileEdit` nodes (the file that was edited).
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    /// Edit operation for `FileEdit` nodes (e.g. `"modify"`, `"rename"`, `"delete"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    edit_kind: Option<&'a str>,
    /// BLAKE3 hash of the file content before the edit (`FileEdit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    before_hash: Option<&'a str>,
    /// BLAKE3 hash of the file content after the edit (`FileEdit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    after_hash: Option<&'a str>,
    /// New path when the file was renamed (`FileEdit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    rename_to: Option<&'a str>,
    /// Number of diff hunks in the edit (`FileEdit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    hunk_count: Option<u32>,
    /// Agent turn that produced this edit (`FileEdit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    linked_turn_id: Option<&'a str>,
    /// Patch artifact this edit belongs to (`FileEdit`, optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    linked_patch_id: Option<&'a str>,
    // ── PatchArtifact-specific fields ────────────────────────────────────────
    /// Patch validation status (`"valid"`, `"invalid"`, `"pending"`) for `PatchArtifact`.
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_status: Option<&'a str>,
    /// Storage handle for raw patch bytes (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_handle: Option<&'a crate::ir::PatchHandle>,
    /// BLAKE3 hash of the raw patch bytes (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_bytes_hash: Option<&'a str>,
    /// Repo-relative file paths touched by the patch (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    target_files: Option<&'a [String]>,
    /// Human-readable validation summary, redacted by policy (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    validation_summary: Option<&'a str>,
    /// Git SHA the patch was authored against (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    base_commit: Option<&'a str>,
    /// Reason `base_commit` is absent (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    unknown_base_reason: Option<&'a str>,
    /// Raw patch byte length (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_bytes_size: Option<u64>,
    /// `AgentSession` record ID that produced this patch (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    producer_session_id: Option<&'a str>,
    /// Redacted body handle for `Task` nodes (required field per project schema).
    #[serde(skip_serializing_if = "Option::is_none")]
    body_handle: Option<&'a crate::ir::OutputHandle>,
    /// Evidence links that connect this item to the queried symbol.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    evidence_links: Vec<&'a crate::ir::EvidenceLink>,
    /// The verification record that closed this acceptance criterion (only populated for verified ACs).
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_record: Option<Box<Self>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<&'a str>,
}

/// One unresolved evidence link target, surfaced per AC5.
#[derive(Serialize)]
struct ContextUnresolved<'a> {
    source_record_id: &'a str,
    target_handle: &'a str,
    relation: &'a str,
    target_domain: &'a str,
    verification_status: &'static str,
}

/// One codegraph topology edge in the context response.
#[derive(Serialize)]
struct ContextTopologyEdge<'a> {
    record_id: &'a str,
    label: &'static str,
    source_id: &'a str,
    target_id: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    summary: &'a str,
    /// Git commit SHA for scan-history edges (None for current-tree edges).
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    /// Bitemporal `valid_time` for scan-history edges.
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
}

/// Full context query response envelope.
#[derive(Serialize)]
struct ContextResponse<'a> {
    ok: bool,
    symbol_name: &'a str,
    source_facts: Vec<ContextSourceFact<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topology_edges: Vec<ContextTopologyEdge<'a>>,
    observations: Vec<ContextObservation<'a>>,
    project_state: Vec<ContextLinkedItem<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
}

/// Full task context query response envelope.
#[derive(Serialize)]
struct TaskContextResponse<'a> {
    ok: bool,
    task_id: &'a str,
    tasks: Vec<ContextLinkedItem<'a>>,
    acceptance_criteria: Vec<ContextLinkedItem<'a>>,
    source_facts: Vec<ContextSourceFact<'a>>,
    observations: Vec<ContextObservation<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reviews: Vec<ContextLinkedItem<'a>>,
    external_links: Vec<ContextLinkedItem<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
}

// ---------------------------------------------------------------------------
// memory evidence audit (issue #64)
// ---------------------------------------------------------------------------

/// The agent-authored memory claim under audit.
///
/// Tagged `trust_class: "agent_authored"` so it is never presented as source
/// truth or proof by itself (AC3). The raw `text` body is never emitted — for a
/// `Failure` or imported claim it may hold a command-output excerpt — only the
/// bounded `summary`, a `text_hash` handle, and redaction metadata (AC9).
#[derive(Serialize)]
struct AuditClaim<'a> {
    record_id: &'a str,
    kind: &'static str,
    trust_class: &'static str,
    /// A redaction-safe structured label. The stored summary embeds a prefix of
    /// the observation text, so it is never forwarded verbatim — only this label
    /// and `summary_hash` are emitted (AC9).
    summary: String,
    /// BLAKE3 hash of the stored summary, citable without emitting its bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_hash: Option<String>,
    /// BLAKE3 hash of the post-redaction body, so the body is citable as a
    /// handle without emitting its bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    text_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<&'a str>,
    /// True when the claim carries a redaction marker or policy version.
    redacted: bool,
}

/// Direct provenance for the claim. Every field is a citable handle (AC4).
#[derive(Serialize)]
struct AuditProvenance<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ingested_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_handle: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_artifact_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_artifact_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    redaction_policy_version: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    agent_session_ids: Vec<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    agent_ids: Vec<&'a str>,
}

/// One safe, bounded evidence item. Carries only record IDs, hashes, handles,
/// spans, and redaction markers — never raw payloads (AC9).
#[derive(Serialize)]
struct AuditItem<'a> {
    record_id: &'a str,
    kind: &'static str,
    trust_class: &'static str,
    /// Relation that connected this item to the claim (e.g. `CONTRADICTS`).
    relation: String,
    /// A non-empty citable handle, guaranteeing AC4 for every item.
    citable_handle: String,
    /// A redaction-safe label. For agent-authored items (whose stored summary
    /// embeds observation text) this is synthesized from typed fields; the
    /// original is exposed only via `summary_hash` (AC9).
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_artifact_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_artifact_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_bytes_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body_handle_hash: Option<&'a str>,
    /// BLAKE3 hash handle for a `Review` record's protected diff hunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    diff_hunk_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<&'a str>,
    /// True when a protected raw payload (patch bytes, command output, body) is
    /// referenced by hash but withheld from this response (AC9).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    protected: bool,
}

/// One stable, machine-readable audit diagnostic (AC6).
#[derive(Serialize)]
struct AuditDiagnostic<'a> {
    code: &'a str,
    source_record_id: &'a str,
    target_handle: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    relation: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    target_domain: &'a str,
}

/// One record excluded by `--verified-only`, reported not dropped (AC5).
#[derive(Serialize)]
struct AuditExcluded<'a> {
    record_id: &'a str,
    kind: &'static str,
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_handle: Option<&'a str>,
}

/// Deterministic pagination block (AC8). v1 returns full pages only.
#[derive(Serialize)]
struct AuditPage {
    cursor: Option<()>,
    has_more: bool,
    returned: usize,
}

/// Full memory evidence audit response envelope.
#[derive(Serialize)]
struct MemoryAuditResponse<'a> {
    ok: bool,
    memory_id: &'a str,
    verified_only: bool,
    memory_claim: Vec<AuditClaim<'a>>,
    direct_provenance: AuditProvenance<'a>,
    supporting_evidence: Vec<AuditItem<'a>>,
    contradicting_evidence: Vec<AuditItem<'a>>,
    superseding_records: Vec<AuditItem<'a>>,
    related_code_handles: Vec<AuditItem<'a>>,
    related_project_handles: Vec<AuditItem<'a>>,
    verification_evidence: Vec<AuditItem<'a>>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
    excluded: Vec<AuditExcluded<'a>>,
    page: AuditPage,
}

// ---------------------------------------------------------------------------
// query_cmd — dispatch
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn query_cmd(subcommand: QuerySubcommand) -> Result<()> {
    match subcommand {
        QuerySubcommand::Symbol {
            name,
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            at,
            as_of,
            tx_as_of,
            format,
        } => {
            if let Some(tx) = tx_as_of.as_deref() {
                // --at keys the valid-time axis to a commit; combining it with a
                // transaction-time selector is an unsupported workflow (AC6).
                if at.is_some() {
                    print_tx_error(
                        "unsupported_combination",
                        "--at cannot be combined with --tx-as-of; use --as-of for the \
                         valid-time axis alongside --tx-as-of",
                    )?;
                    std::process::exit(1);
                }
                #[cfg(feature = "embedded-aletheiadb")]
                if daemon {
                    let dir = data_dir
                        .as_deref()
                        .expect("clap requires --data-dir with --daemon");
                    return query_symbol_tx_via_daemon(&name, dir, tx, as_of.as_deref(), format);
                }
                // Validate the temporal selectors before touching the local store
                // so a malformed instant returns the `invalid_timestamp` envelope
                // without reading (or failing on) a large/missing/unhealthy store —
                // matching the daemon path, which validates before connecting.
                if let Err(e) = chrono::DateTime::parse_from_rfc3339(tx) {
                    print_tx_error(
                        "invalid_timestamp",
                        &format!("invalid --tx-as-of timestamp '{tx}': {e}"),
                    )?;
                    std::process::exit(1);
                }
                if let Some(vt) = as_of.as_deref()
                    && let Err(e) = chrono::DateTime::parse_from_rfc3339(vt)
                {
                    print_tx_error(
                        "invalid_timestamp",
                        &format!("invalid --as-of timestamp '{vt}': {e}"),
                    )?;
                    std::process::exit(1);
                }
                let records = load_query_records_history(graph.as_deref(), data_dir.as_deref())?;
                return query_symbol_tx_as_of(&records, &name, tx, as_of.as_deref(), format);
            }
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_symbol_via_daemon(
                    &name,
                    dir,
                    at.as_deref(),
                    as_of.as_deref(),
                    format,
                );
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            as_of.map_or_else(
                || {
                    at.map_or_else(
                        || query_symbol_all(&records, &name, format),
                        |prefix| query_symbol_at(&records, &name, &prefix, format),
                    )
                },
                |instant| query_symbol_as_of(&records, &name, &instant, format),
            )
        }
        QuerySubcommand::File {
            path,
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            format,
        } => {
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_file_via_daemon(&path, dir, format);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_file(&records, &path, format)
        }
        QuerySubcommand::Drift {
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            limit,
            format,
        } => {
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_drift_via_daemon(dir, limit, format);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_drift(&records, limit, format)
        }
        #[cfg(feature = "embeddings")]
        QuerySubcommand::Semantic {
            query,
            data_dir,
            daemon,
            limit,
            format,
        } => {
            if daemon {
                query_semantic_via_daemon(&query, &data_dir, limit, format)
            } else {
                query_semantic(&query, &data_dir, limit, format)
            }
        }
        QuerySubcommand::Context {
            name,
            graph,
            data_dir,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_context_cmd(&records, &name)
        }
        QuerySubcommand::Task {
            id_or_handle,
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
        } => {
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_task_via_daemon(&id_or_handle, dir);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_task_cmd(&records, &id_or_handle)
        }
        QuerySubcommand::Candidates {
            graph,
            data_dir,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_candidates_cmd(&records, format)
        }
        QuerySubcommand::Policy {
            repo,
            path_glob,
            language,
            lifecycle_phase,
            graph,
            data_dir,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let scope = crate::UserContextScope {
                repo,
                path_glob,
                language,
                lifecycle_phase,
            };
            query_policy_cmd(&records, &scope, format)
        }
        QuerySubcommand::Audit {
            id,
            graph,
            data_dir,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_audit_cmd(&records, &id, format)
        }
        QuerySubcommand::Memory {
            id_or_handle,
            graph,
            data_dir,
            verified_only,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_memory_cmd(&records, &id_or_handle, verified_only)
        }
    }
}

#[cfg(feature = "embedded-aletheiadb")]
fn query_symbol_via_daemon(
    name: &str,
    data_dir: &Path,
    at: Option<&str>,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let (verb, params) = at.map_or_else(
        || ("symbol_by_name", serde_json::json!({ "name": name })),
        |commit| {
            (
                "symbol_at_commit",
                serde_json::json!({ "name": name, "commit": commit }),
            )
        },
    );
    let records = client.query_verb(verb, &params, as_of)?;
    if records.is_empty() {
        eprintln!("error: no match found for symbol `{name}`");
        std::process::exit(2);
    }
    for rec in &records {
        print_daemon_symbol_record(rec, format)?;
    }
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
fn query_file_via_daemon(path: &str, data_dir: &Path, format: OutputFormat) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let params = serde_json::json!({ "repo_relative_path": path });
    let records = client.query_verb("file_defines", &params, None)?;
    if records.is_empty() {
        eprintln!("error: no match found for file `{path}`");
        std::process::exit(2);
    }
    for rec in &records {
        print_daemon_symbol_record(rec, format)?;
    }
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
fn query_drift_via_daemon(data_dir: &Path, limit: usize, format: OutputFormat) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let params = serde_json::json!({ "limit": limit as u64 });
    let records = client.query_verb("drift_top_n", &params, None)?;
    if records.is_empty() {
        eprintln!("error: no match found — no SemanticDrift nodes in graph");
        std::process::exit(2);
    }
    for rec in &records {
        print_daemon_drift_record(rec, format)?;
    }
    Ok(())
}

/// Prints a daemon symbol/file record (`serde_json::Value`) in the requested format.
#[cfg(feature = "embedded-aletheiadb")]
fn print_daemon_symbol_record(rec: &serde_json::Value, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(rec)?),
        OutputFormat::Text => {
            let name = rec["name"].as_str().unwrap_or("(unknown)");
            let kind = rec["kind"].as_str().unwrap_or("Symbol");
            let path = rec["repo_relative_path"].as_str().unwrap_or("(unknown)");
            let line = rec["span"]["start_line"].as_u64().unwrap_or(0);
            let commit = rec["git_commit"]
                .as_str()
                .map_or(String::new(), |c| format!(" [{c}]"));
            println!("{name} ({kind}) @ {path}:{line}{commit}");
        }
    }
    Ok(())
}

/// Prints a daemon drift record (`serde_json::Value`) in the requested format.
#[cfg(feature = "embedded-aletheiadb")]
fn print_daemon_drift_record(rec: &serde_json::Value, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(rec)?),
        OutputFormat::Text => {
            let name = rec["name"].as_str().unwrap_or("(unknown)");
            // score may be a JSON string ("0.92") or a JSON number (0.92);
            // accept both so text output is correct regardless of serialisation path.
            let score_owned = rec["score"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| rec["score"].as_f64().map(|f| f.to_string()))
                .unwrap_or_else(|| "?".to_owned());
            let score = score_owned.as_str();
            let before = rec["before_commit"].as_str().unwrap_or("?");
            let after = rec["after_commit"].as_str().unwrap_or("?");
            let path = rec["repo_relative_path"].as_str().unwrap_or("(unknown)");
            println!("{name} score={score} {before}..{after} @ {path}");
        }
    }
    Ok(())
}

fn load_query_records(graph: Option<&Path>, data_dir: Option<&Path>) -> Result<Vec<GraphRecord>> {
    match (graph, data_dir) {
        (Some(path), None) => load_records_from_jsonl(path),
        (None, Some(dir)) => load_records_from_db(dir),
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
    }
}

fn load_records_from_jsonl(graph: &Path) -> Result<Vec<GraphRecord>> {
    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    crate::adapters::records_from_jsonl(&jsonl)
        .map_err(|e| anyhow::anyhow!("failed to parse graph JSONL: {e}"))
}

#[cfg(feature = "embedded-aletheiadb")]
fn validate_existing_embedded_store(data_dir: &Path) -> Result<()> {
    match fs::read_dir(data_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!(
                "error: embedded store not found at {} - \
                 run `eg ingest --adapter embedded --data-dir <path>` first",
                data_dir.display()
            );
        }
        Ok(mut entries) => {
            if entries.next().is_none() {
                anyhow::bail!(
                    "error: embedded store at {} is empty - \
                     run `eg ingest --adapter embedded --data-dir <path>` first",
                    data_dir.display()
                );
            }
        }
        Err(_) => {}
    }
    Ok(())
}

fn load_records_from_db(data_dir: &Path) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
        sink.read_all_records()
            .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// Loads query records for a transaction-time query (issue #66).
///
/// The `--graph` JSONL path already preserves every written line, so it is used
/// unchanged. The embedded `--data-dir` path additionally surfaces superseded
/// non-temporal versions so a prior store view can be reconstructed.
fn load_query_records_history(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<Vec<GraphRecord>> {
    match (graph, data_dir) {
        (Some(path), None) => load_records_from_jsonl(path),
        (None, Some(dir)) => load_records_from_db_history(dir),
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
    }
}

fn load_records_from_db_history(data_dir: &Path) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
        sink.read_all_records_including_superseded()
            .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

// ---------------------------------------------------------------------------
// Embedding helpers
// ---------------------------------------------------------------------------

/// Generates dense embeddings for all file and symbol candidates in `records`.
///
/// Returns a `(record_id → vector, dimension)` map ready for
/// `EmbeddedAletheiaSink::open_with_embeddings`.
#[cfg(feature = "embeddings")]
fn generate_embeddings(
    records: &[GraphRecord],
) -> Result<(crate::embeddings::EmbeddingVectorMap, usize)> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        DEFAULT_EMBEDDING_MODEL_NAME, EmbeddingVectorKey, EmbeddingVectorMap, aletheia_embeddings,
        embedding_candidates,
    };

    let candidates = embedding_candidates(records);
    if candidates.is_empty() {
        return Ok((
            EmbeddingVectorMap::new(),
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        ));
    }

    eprintln!(
        "Generating embeddings for {} file/symbol nodes…",
        candidates.len()
    );

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let texts: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let embed_data = rt
        .block_on(aletheia_embeddings::embed_query(&texts, &embedder, None))
        .context("embedding generation failed")?;

    let dense: Vec<Vec<f32>> = aletheia_embeddings::embed_data_to_dense_iter(embed_data, None)
        .collect::<Result<Vec<_>, _>>()
        .context("embedding result was not dense")?
        .into_iter()
        .map(|d| d.embedding)
        .collect();

    let dimensions = dense.first().map_or(0, Vec::len);
    if dimensions == 0 {
        anyhow::bail!("embedding model returned zero-dimension vectors");
    }
    let map = candidates
        .into_iter()
        .zip(dense)
        .map(|(candidate, vector)| (EmbeddingVectorKey::from_candidate(&candidate), vector))
        .collect();

    Ok((map, dimensions))
}

/// Embeds a natural-language query into a dense vector using the default local
/// model. Shared by the embedded and daemon-backed semantic search paths so
/// both produce identical query vectors (and therefore identical rankings).
///
/// The model is loaded from the local Hugging Face cache; no remote embedding
/// service is contacted at query time.
#[cfg(feature = "embeddings")]
fn embed_query_text(query: &str) -> Result<Vec<f32>> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_NAME, aletheia_embeddings,
    };

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let embed_data = rt
        .block_on(aletheia_embeddings::embed_query(&[query], &embedder, None))
        .context("failed to embed query")?;

    aletheia_embeddings::embed_data_to_dense_iter(embed_data, Some(1))
        .next()
        .context("no embedding returned for query")?
        .context("embedding result was not dense")
        .map(|dense| dense.embedding)
}

/// Semantic similarity search against an embedded store.
#[cfg(feature = "embeddings")]
fn query_semantic(query: &str, data_dir: &Path, limit: usize, format: OutputFormat) -> Result<()> {
    validate_existing_embedded_store(data_dir)?;

    let query_vector = embed_query_text(query)?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let matches = sink
        .semantic_search(&query_vector, limit)
        .with_context(|| "semantic search failed — was the store ingested with --embed?")?;

    if matches.is_empty() {
        eprintln!("no results — store may not have embeddings (re-run ingest with --embed)");
        std::process::exit(2);
    }

    for m in matches {
        print_result(&SemanticResult::from(&m), format)?;
    }
    Ok(())
}

/// Semantic similarity search routed through the running daemon (issue #59).
///
/// Connects to the daemon first (so a missing or stale daemon fails fast,
/// before the model is loaded), embeds the query locally, then dispatches the
/// `semantic_search` verb. Results are the same retrieval-lead rows the
/// embedded path emits; the daemon owns the shared store, token, and snapshot.
#[cfg(feature = "embeddings")]
fn query_semantic_via_daemon(
    query: &str,
    data_dir: &Path,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;

    let query_vector = embed_query_text(query)?;
    let params = serde_json::json!({
        "query_vector": query_vector,
        "limit": limit as u64,
    });
    let records = client.query_verb("semantic_search", &params, None)?;

    if records.is_empty() {
        eprintln!("no results — store may not have embeddings (re-run ingest with --embed)");
        std::process::exit(2);
    }

    for rec in &records {
        print_daemon_semantic_record(rec, format)?;
    }
    Ok(())
}

/// Prints a daemon semantic result row (`serde_json::Value`) in the requested
/// format. JSON output forwards the row verbatim; text output renders the
/// bounded handle fields only.
#[cfg(feature = "embeddings")]
fn print_daemon_semantic_record(rec: &serde_json::Value, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(rec)?),
        OutputFormat::Text => {
            let record_id = rec["record_id"].as_str().unwrap_or("(unknown)");
            let score = rec["score"].as_f64().unwrap_or(0.0);
            let path = rec["repo_relative_path"].as_str().unwrap_or("(unknown)");
            let line = rec["span"]["start_line"].as_u64();
            let location = line.map_or_else(
                || path.to_owned(),
                |start_line| format!("{path}:{start_line}"),
            );
            println!("{record_id} score={score:.4} @ {location}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// eval-semantic command
// ---------------------------------------------------------------------------

/// Runs the semantic relevance corpus evaluation against an embedded store.
///
/// Reads each query from the corpus, embeds it with the default model, runs
/// semantic search, computes aggregate metrics, and prints the report.
fn parse_threshold(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a valid number"))?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(format!("threshold must be between 0.0 and 1.0, got {v}"))
    }
}

/// Exits 1 with a diagnostic if the top-3 recall threshold is missed.
#[cfg(feature = "embeddings")]
#[allow(clippy::too_many_lines)]
fn eval_semantic_cmd(
    corpus_path: &Path,
    data_dir: &Path,
    top_k: usize,
    threshold: f64,
    fp_threshold: f64,
) -> Result<()> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_NAME, aletheia_embeddings,
    };
    use crate::semantic_eval::{
        SearchHit, SemanticRelevanceCorpus, build_report, evaluate_query, format_diagnostic,
        print_report,
    };

    validate_existing_embedded_store(data_dir)?;

    let corpus = SemanticRelevanceCorpus::from_json_file(corpus_path)?;

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

    let mut results = Vec::new();
    for query in &corpus.queries {
        let embed_data = rt
            .block_on(aletheia_embeddings::embed_query(
                &[query.text.as_str()],
                &embedder,
                None,
            ))
            .with_context(|| format!("failed to embed query {}", query.id))?;

        let query_vector = aletheia_embeddings::embed_data_to_dense_iter(embed_data, Some(1))
            .next()
            .with_context(|| format!("no embedding returned for query {}", query.id))?
            .with_context(|| format!("embedding result not dense for query {}", query.id))?
            .embedding;

        let matches = sink
            .semantic_search(&query_vector, top_k.max(3))
            .with_context(|| {
                format!(
                    "semantic search failed for query {} — was the store ingested with --embed?",
                    query.id
                )
            })?;

        let hits: Vec<SearchHit> = matches.iter().map(SearchHit::from).collect();
        #[allow(clippy::cast_possible_truncation)]
        results.push(evaluate_query(query, &hits, fp_threshold as f32));
    }

    let report = build_report(results, threshold);
    print_report(&report, std::io::stdout())?;

    if !report.passed {
        eprintln!("{}", format_diagnostic(&report));
        process::exit(1);
    }

    Ok(())
}

/// Run semantic drift calibration evaluation.
#[cfg(feature = "embeddings")]
#[allow(clippy::too_many_lines)]
fn eval_drift_cmd(corpus_path: &Path, threshold: f64) -> Result<()> {
    use crate::embeddings::{
        CandidateVector, DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_NAME,
        aletheia_embeddings, embedding_candidates, semantic_drift_records,
    };
    use std::collections::HashSet;
    use std::io::Write as _;
    use std::process::Command;

    #[derive(Debug, Clone, serde::Deserialize)]
    struct DriftCalibrationCorpus {
        #[allow(dead_code)]
        pub corpus_version: String,
        #[allow(dead_code)]
        pub description: String,
        pub scenarios: Vec<DriftScenario>,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct DriftScenario {
        pub id: String,
        pub class: String,
        pub file_path: String,
        pub before: String,
        pub after: String,
    }

    #[allow(clippy::struct_excessive_bools)]
    struct ScenarioEvalResult {
        pub scenario_id: String,
        pub class: String,
        pub drift_detected: bool,
        pub max_score: f64,
        pub drift_details: Vec<DriftDetails>,
        pub git_diff_detected: bool,
        pub git_log_s_detected: bool,
        pub rg_detected: bool,
    }

    struct DriftDetails {
        #[allow(dead_code)]
        pub before_commit: String,
        #[allow(dead_code)]
        pub after_commit: String,
        pub file_path: String,
        pub span: Option<SourceSpan>,
        pub score: f64,
        pub selection_threshold: f64,
        pub model_name: String,
    }

    let corpus_text = std::fs::read_to_string(corpus_path)
        .with_context(|| format!("failed to read corpus file {}", corpus_path.display()))?;
    let corpus: DriftCalibrationCorpus = serde_json::from_str(&corpus_text)
        .with_context(|| format!("failed to parse corpus JSON from {}", corpus_path.display()))?;

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

    let mut scanned_scenarios = Vec::new();

    for scenario in &corpus.scenarios {
        let temp_dir = tempfile::tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();

        let run_git = |args: &[&str]| -> Result<()> {
            let output = Command::new("git")
                .args(args)
                .current_dir(&temp_path)
                .output()?;
            if !output.status.success() {
                anyhow::bail!(
                    "git command failed: git {:?} in {}. stderr: {}",
                    args,
                    temp_path.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(())
        };

        run_git(&["init"])?;
        run_git(&["config", "user.name", "Test User"])?;
        run_git(&["config", "user.email", "test@example.com"])?;
        run_git(&["config", "commit.gpgsign", "false"])?;

        let file_path = temp_path.join(&scenario.file_path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&file_path, &scenario.before)?;

        run_git(&["add", "."])?;

        let run_git_with_env = |args: &[&str], envs: &[(&str, &str)]| -> Result<()> {
            let mut cmd = Command::new("git");
            cmd.args(args).current_dir(&temp_path);
            for (k, v) in envs {
                cmd.env(k, v);
            }
            let output = cmd.output()?;
            if !output.status.success() {
                anyhow::bail!(
                    "git command failed with envs: git {:?} in {}. stderr: {}",
                    args,
                    temp_path.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(())
        };

        let before_date = "2026-01-01T00:00:00Z";
        let after_date = "2026-01-02T00:00:00Z";

        run_git_with_env(
            &["commit", "--allow-empty", "-m", "before"],
            &[
                ("GIT_AUTHOR_DATE", before_date),
                ("GIT_COMMITTER_DATE", before_date),
            ],
        )?;

        std::fs::write(&file_path, &scenario.after)?;

        run_git(&["add", "."])?;
        run_git_with_env(
            &["commit", "--allow-empty", "-m", "after"],
            &[
                ("GIT_AUTHOR_DATE", after_date),
                ("GIT_COMMITTER_DATE", after_date),
            ],
        )?;

        let graph = scan_repository_history_with_override(&temp_path, None)?;
        let scenario_records = graph.into_records();

        let before_words: HashSet<&str> = scenario.before.split_whitespace().collect();
        let after_words: HashSet<&str> = scenario.after.split_whitespace().collect();
        let added_token = after_words
            .iter()
            .find(|w| !before_words.contains(*w))
            .copied();
        let removed_token = before_words
            .iter()
            .find(|w| !after_words.contains(*w))
            .copied();
        let diff_token = added_token.or(removed_token).map(|s| {
            s.trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_owned()
        });

        let has_git_diff = !Command::new("git")
            .args(["diff", "--quiet", "HEAD~1", "HEAD"])
            .current_dir(&temp_path)
            .status()?
            .success();

        let has_git_log_s = if let Some(ref token) = diff_token {
            if token.is_empty() {
                false
            } else {
                let out = Command::new("git")
                    .args(["log", &format!("-S{token}")])
                    .current_dir(&temp_path)
                    .output()?;
                out.status.success() && !out.stdout.is_empty()
            }
        } else {
            false
        };

        let has_rg = if let Some(ref token) = diff_token {
            if token.is_empty() {
                false
            } else {
                Command::new("git")
                    .args(["grep", "-q", token])
                    .current_dir(&temp_path)
                    .status()?
                    .success()
            }
        } else {
            false
        };

        scanned_scenarios.push((
            scenario.id.clone(),
            scenario.class.clone(),
            scenario_records,
            has_git_diff,
            has_git_log_s,
            has_rg,
            temp_dir,
        ));
    }

    let mut all_records = Vec::new();
    for (_, _, records, _, _, _, _) in &scanned_scenarios {
        all_records.extend(records.clone());
    }

    let candidates = embedding_candidates(&all_records);
    if candidates.is_empty() {
        anyhow::bail!("no embedding candidates found in scanned graphs");
    }

    let texts: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();
    let embed_data = rt
        .block_on(aletheia_embeddings::embed_query(&texts, &embedder, None))
        .context("embedding generation failed")?;

    let dense: Vec<Vec<f32>> = aletheia_embeddings::embed_data_to_dense_iter(embed_data, None)
        .collect::<Result<Vec<_>, _>>()
        .context("embedding result was not dense")?
        .into_iter()
        .map(|d| d.embedding)
        .collect();

    let candidate_vectors: Vec<CandidateVector> = candidates
        .into_iter()
        .zip(dense)
        .map(|(candidate, vector)| CandidateVector { candidate, vector })
        .collect();

    let mut scenario_results = Vec::new();

    for (scenario_id, class, scenario_records, has_git_diff, has_git_log_s, has_rg, _) in
        scanned_scenarios
    {
        let scenario_record_ids: HashSet<&str> =
            scenario_records.iter().map(GraphRecord::id).collect();
        let scenario_commits: HashSet<String> = scenario_records
            .iter()
            .filter_map(|r| {
                if let GraphRecord::Node {
                    temporal: Some(t), ..
                } = r
                {
                    Some(t.git_commit.clone())
                } else if let GraphRecord::Edge {
                    temporal: Some(t), ..
                } = r
                {
                    Some(t.git_commit.clone())
                } else {
                    None
                }
            })
            .collect();

        let scenario_candidate_vectors: Vec<CandidateVector> = candidate_vectors
            .iter()
            .filter(|cv| {
                scenario_record_ids.contains(cv.candidate.record_id.as_str())
                    && cv
                        .candidate
                        .temporal
                        .as_ref()
                        .is_none_or(|t| scenario_commits.contains(&t.git_commit))
            })
            .cloned()
            .collect();

        let scenario_drifts = semantic_drift_records(
            &scenario_candidate_vectors,
            DEFAULT_EMBEDDING_MODEL_NAME,
            threshold,
        );

        let drift_detected = !scenario_drifts.is_empty();
        let mut max_score = 0.0;
        let mut drift_details = Vec::new();

        for record in &scenario_drifts {
            if let GraphRecord::Node {
                id,
                semantic_drift: Some(drift),
                repo_relative_path: drift_path,
                name: drift_name,
                ..
            } = record
            {
                if drift.score > max_score {
                    max_score = drift.score;
                }

                let (resolved_path, _resolved_name, resolved_span) = resolve_drift_target(
                    &all_records,
                    id,
                    drift,
                    drift_path.as_deref(),
                    drift_name.as_deref(),
                );

                drift_details.push(DriftDetails {
                    before_commit: drift.before_git_commit.clone(),
                    after_commit: drift.after_git_commit.clone(),
                    file_path: resolved_path.unwrap_or("").to_owned(),
                    span: resolved_span,
                    score: drift.score,
                    selection_threshold: drift.selection_threshold,
                    model_name: drift.embedding_model.name.clone(),
                });
            }
        }

        scenario_results.push(ScenarioEvalResult {
            scenario_id,
            class,
            drift_detected,
            max_score,
            drift_details,
            git_diff_detected: has_git_diff,
            git_log_s_detected: has_git_log_s,
            rg_detected: has_rg,
        });
    }

    let mut tp = 0;
    let mut fp = 0;
    let mut fn_count = 0;
    let mut unchanged_fp = 0;

    for r in &scenario_results {
        if r.class == "meaning_changed" {
            if r.drift_detected {
                tp += 1;
            } else {
                fn_count += 1;
            }
        } else if r.drift_detected {
            fp += 1;
            if r.class == "unchanged" {
                unchanged_fp += 1;
            }
        }
    }

    let precision = if tp + fp > 0 {
        f64::from(tp) / f64::from(tp + fp)
    } else {
        0.0
    };

    let recall = if tp + fn_count > 0 {
        f64::from(tp) / f64::from(tp + fn_count)
    } else {
        0.0
    };

    let precision_pass = precision >= 0.75;
    let recall_pass = recall >= 0.70;
    let unchanged_pass = unchanged_fp == 0;
    let passed = precision_pass && recall_pass && unchanged_pass;

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();

    writeln!(handle, "Semantic Drift Calibration Report")?;
    writeln!(handle, "=================================")?;
    writeln!(handle, "Evaluation threshold: {threshold:.2}")?;
    writeln!(handle, "Total scenarios: {}", scenario_results.len())?;
    writeln!(handle, "Metrics:")?;
    writeln!(
        handle,
        "  Precision: {precision:.4} (pass: {precision_pass})"
    )?;
    writeln!(handle, "  Recall:    {recall:.4} (pass: {recall_pass})")?;
    writeln!(
        handle,
        "  Unchanged false positives: {unchanged_fp} (pass: {unchanged_pass})"
    )?;
    writeln!(
        handle,
        "  Status:    {}",
        if passed { "PASS" } else { "FAIL" }
    )?;
    writeln!(handle)?;

    writeln!(handle, "Scenario Details:")?;
    writeln!(handle, "-----------------")?;
    for r in &scenario_results {
        writeln!(
            handle,
            "  [{}] class={:<22} detected={:<5} max_score={:.4} | Baselines: diff={:<5} log_s={:<5} rg={:<5}",
            r.scenario_id,
            r.class,
            r.drift_detected,
            r.max_score,
            r.git_diff_detected,
            r.git_log_s_detected,
            r.rg_detected
        )?;

        if r.drift_detected {
            for d in &r.drift_details {
                let span_str = d
                    .span
                    .map_or_else(String::new, |s| format!(" {}:{}", s.start_line, s.end_line));
                writeln!(
                    handle,
                    "         - model={} score={:.4} thresh={:.2} path={}{} status=\"drift is a lead, not proof\"",
                    d.model_name, d.score, d.selection_threshold, d.file_path, span_str
                )?;
            }
        }
    }

    if !passed {
        anyhow::bail!("Calibration metrics did not meet the required gates.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tombstone helpers
// ---------------------------------------------------------------------------

/// Returns the set of record IDs that have been tombstoned and not superseded.
/// Used to exclude deleted records from current-state queries (but not --at queries).
fn current_deleted_ids(records: &[GraphRecord]) -> std::collections::BTreeSet<&str> {
    let mut deleted: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for record in records {
        if let GraphRecord::Tombstone { deleted_id, .. } = record {
            deleted.insert(deleted_id.as_str());
        }
    }
    deleted
}

// ---------------------------------------------------------------------------
// query symbol (all matching)
// ---------------------------------------------------------------------------

fn query_symbol_all(records: &[GraphRecord], name: &str, format: OutputFormat) -> Result<()> {
    let deleted = current_deleted_ids(records);
    let mut results: Vec<SymbolResult<'_>> = records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node {
                id, temporal: None, ..
            } = r
            {
                !deleted.contains(id.as_str())
            } else {
                true
            }
        })
        .filter_map(|r| symbol_result(r, name))
        .collect();

    if results.is_empty() {
        eprintln!("error: no match found for symbol `{name}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| (r.span.map(|s| s.start_line), r.record_id));
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

fn symbol_result<'a>(record: &'a GraphRecord, name: &str) -> Option<SymbolResult<'a>> {
    let GraphRecord::Node {
        id,
        kind: NodeKind::Symbol,
        schema_version,
        name: node_name,
        repo_relative_path,
        span,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    if node_name.as_deref() != Some(name) {
        return None;
    }
    Some(SymbolResult {
        record_id: id,
        schema_version: *schema_version,
        name: node_name.as_deref().unwrap_or(""),
        kind: "Symbol",
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
    })
}

// ---------------------------------------------------------------------------
// query symbol --as-of <instant>
// ---------------------------------------------------------------------------

fn query_symbol_as_of(
    records: &[GraphRecord],
    name: &str,
    as_of: &str,
    format: OutputFormat,
) -> Result<()> {
    match query::symbol_as_of_valid_time(records, name, as_of) {
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
        Ok(None) => {
            eprintln!("error: no match found for symbol `{name}` at or before `{as_of}`");
            std::process::exit(2);
        }
        Ok(Some(record)) => {
            if let Some(result) = symbol_result(record, name) {
                print_result(&result, format)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query symbol --tx-as-of <instant> (issue #66)
// ---------------------------------------------------------------------------

/// A single transaction-time query result row.
///
/// Carries only redaction-safe handles (AC8): record ID, schema version, trust
/// and domain class, valid-time fields, the transaction-time handle, and a
/// citable source handle (`repo_relative_path` + `span`). No summaries, bodies,
/// or raw payloads are emitted.
#[derive(Serialize)]
struct TxSymbolRow<'a> {
    record_id: &'a str,
    schema_version: u32,
    name: &'a str,
    kind: &'static str,
    domain: &'a str,
    trust_class: &'static str,
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time_source: Option<&'a str>,
    transaction_time: &'a str,
}

/// Response envelope for `eg query symbol --tx-as-of`.
#[derive(Serialize)]
struct TxSymbolEnvelope<'a> {
    ok: bool,
    verb: &'static str,
    name: &'a str,
    tx_as_of: &'a str,
    // Always serialized (as `null` when no valid-time axis) so the JSON schema
    // matches the daemon path and the documented envelope.
    as_of: Option<&'a str>,
    snapshot: &'a str,
    records: Vec<TxSymbolRow<'a>>,
    diagnostics: Vec<query::TxDiagnostic>,
    page: TxPage,
}

#[derive(Serialize)]
struct TxPage {
    cursor: Option<()>,
    has_more: bool,
    returned: usize,
}

/// Builds a redaction-safe result row from a selected Symbol record.
fn tx_symbol_row(record: &GraphRecord) -> Option<TxSymbolRow<'_>> {
    let GraphRecord::Node {
        id,
        kind: NodeKind::Symbol,
        schema_version,
        name,
        repo_relative_path,
        span,
        temporal,
        valid_time,
        valid_time_source,
        domain,
        ..
    } = record
    else {
        return None;
    };
    // Prefer the explicit node `domain` field; otherwise fall back to the
    // kind-derived domain (a `'static str`).
    let domain_str = domain
        .as_deref()
        .unwrap_or_else(|| crate::schema_version::domain_for_node_kind("Symbol"));
    Some(TxSymbolRow {
        record_id: id,
        schema_version: *schema_version,
        name: name.as_deref().unwrap_or(""),
        kind: "Symbol",
        domain: domain_str,
        trust_class: trust_class_for(record),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        valid_time: temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref()),
        valid_time_source: temporal
            .as_ref()
            .and_then(|t| t.valid_time_source.as_deref())
            .or(valid_time_source.as_deref()),
        transaction_time: query::record_transaction_time(record).unwrap_or(""),
    })
}

/// Prints a `{ "ok": false, "error": { code, message } }` envelope to stdout.
fn print_tx_error(code: &str, message: &str) -> Result<()> {
    let envelope = serde_json::json!({
        "ok": false,
        "error": { "code": code, "message": message }
    });
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}

fn query_symbol_tx_as_of(
    records: &[GraphRecord],
    name: &str,
    tx_as_of: &str,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // The CLI loads the entire `--graph` file, so the store-wide range is just
    // the full record set: pass `None` to let the resolver derive it.
    match query::symbol_as_of_transaction_time(records, name, tx_as_of, as_of, None) {
        Err(err) => {
            print_tx_error(&err.code, &err.message)?;
            std::process::exit(1);
        }
        Ok(result) => {
            let rows: Vec<TxSymbolRow<'_>> = result
                .records
                .iter()
                .filter_map(|r| tx_symbol_row(r))
                .collect();
            let envelope = TxSymbolEnvelope {
                ok: true,
                verb: "symbol",
                name,
                tx_as_of,
                as_of,
                // The view's transaction-time handle is the requested instant.
                snapshot: tx_as_of,
                page: TxPage {
                    cursor: None,
                    has_more: false,
                    returned: rows.len(),
                },
                records: rows,
                diagnostics: result.diagnostics,
            };
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => {
                    for row in &envelope.records {
                        let path = row.repo_relative_path.unwrap_or("(unknown)");
                        let line = row.span.map_or(0, |s| s.start_line);
                        println!(
                            "{} (Symbol) @ {path}:{line} tx={}",
                            row.name, row.transaction_time
                        );
                    }
                    for diag in &envelope.diagnostics {
                        println!("# {}: {}", diag.code, diag.message);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
fn query_symbol_tx_via_daemon(
    name: &str,
    data_dir: &Path,
    tx_as_of: &str,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // Validate timestamps client-side first so a malformed instant produces the
    // same `invalid_timestamp` envelope as the non-daemon path, before connecting.
    if let Err(e) = chrono::DateTime::parse_from_rfc3339(tx_as_of) {
        print_tx_error(
            "invalid_timestamp",
            &format!("invalid --tx-as-of timestamp '{tx_as_of}': {e}"),
        )?;
        std::process::exit(1);
    }
    if let Some(vt) = as_of
        && let Err(e) = chrono::DateTime::parse_from_rfc3339(vt)
    {
        print_tx_error(
            "invalid_timestamp",
            &format!("invalid --as-of timestamp '{vt}': {e}"),
        )?;
        std::process::exit(1);
    }

    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let mut as_of_obj = serde_json::Map::new();
    as_of_obj.insert("transaction_time".to_owned(), serde_json::json!(tx_as_of));
    if let Some(vt) = as_of {
        as_of_obj.insert("valid_time".to_owned(), serde_json::json!(vt));
    }
    let as_of_value = serde_json::Value::Object(as_of_obj);
    // Translate any daemon-side rejection into the documented machine-readable
    // CLI error envelope on stdout instead of an anyhow string on stderr.
    let result = match client.query_verb_raw_with_as_of(
        "symbol_by_name",
        &serde_json::json!({ "name": name }),
        Some(&as_of_value),
    ) {
        Ok(r) => r,
        Err(e) => {
            print_tx_error("daemon_query_error", &e.to_string())?;
            std::process::exit(1);
        }
    };
    // `query_verb_raw_with_as_of` returns only the daemon `result` object, whose
    // record rows already carry the tx handles. Reconstruct the same CLI
    // `--tx-as-of` envelope the non-daemon path emits (top-level `ok`, `verb`,
    // `name`, `as_of`, `snapshot`) so JSON consumers see one shape regardless of
    // `--daemon`.
    let empty_records = serde_json::json!([]);
    let records = result.get("records").unwrap_or(&empty_records);
    let empty_diags = serde_json::json!([]);
    let diagnostics = result.get("diagnostics").unwrap_or(&empty_diags);
    let default_page = serde_json::json!({ "cursor": null, "has_more": false, "returned": 0 });
    let page = result.get("page").unwrap_or(&default_page);
    let envelope = serde_json::json!({
        "ok": true,
        "verb": "symbol",
        "name": name,
        "tx_as_of": tx_as_of,
        "as_of": as_of,
        "snapshot": tx_as_of,
        "records": records,
        "diagnostics": diagnostics,
        "page": page,
    });
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string(&envelope)?);
        }
        OutputFormat::Text => {
            if let Some(records) = records.as_array() {
                for rec in records {
                    let name = rec.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let path = rec
                        .get("repo_relative_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(unknown)");
                    let tx = rec
                        .get("transaction_time")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    println!("{name} (Symbol) @ {path} tx={tx}");
                }
            }
            // Mirror the non-daemon text path: surface diagnostics so the
            // no-silent-fallback signal is not lost for empty diagnostic-bearing
            // results (e.g. before_first_transaction, no_named_symbol).
            if let Some(diags) = diagnostics.as_array() {
                for diag in diags {
                    let code = diag.get("code").and_then(|v| v.as_str()).unwrap_or("");
                    let message = diag.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    println!("# {code}: {message}");
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query symbol --at <commit>
// ---------------------------------------------------------------------------

fn query_symbol_at(
    records: &[GraphRecord],
    name: &str,
    prefix: &str,
    format: OutputFormat,
) -> Result<()> {
    let matching_commits: std::collections::BTreeSet<&str> = records
        .iter()
        .filter_map(|r| temporal_commit_if_prefix(r, prefix))
        .collect();

    if matching_commits.len() > 1 {
        eprintln!(
            "error: ambiguous commit prefix `{prefix}` matches {} commits",
            matching_commits.len()
        );
        std::process::exit(1);
    }

    match query::symbol_at_commit(records, name, prefix) {
        None => {
            eprintln!("error: no match found for symbol `{name}` at commit `{prefix}`");
            std::process::exit(2);
        }
        Some(record) => {
            if let Some(result) = symbol_result(record, name) {
                print_result(&result, format)?;
            }
        }
    }
    Ok(())
}

fn temporal_commit_if_prefix<'a>(record: &'a GraphRecord, prefix: &str) -> Option<&'a str> {
    let commit = match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        }
        | GraphRecord::Edge {
            temporal: Some(t), ..
        } => t.git_commit.as_str(),
        _ => return None,
    };
    if commit.starts_with(prefix) {
        Some(commit)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// query file
// ---------------------------------------------------------------------------

fn query_file(records: &[GraphRecord], path: &str, format: OutputFormat) -> Result<()> {
    let deleted = current_deleted_ids(records);

    let file_exists = records.iter().any(|r| {
        let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path,
            ..
        } = r
        else {
            return false;
        };
        repo_relative_path.as_deref() == Some(path) && !deleted.contains(id.as_str())
    });

    if !file_exists {
        eprintln!("error: no match found for file `{path}`");
        std::process::exit(2);
    }

    let mut results: Vec<SymbolResult<'_>> = records
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                schema_version,
                name,
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
            Some(SymbolResult {
                record_id: id,
                schema_version: *schema_version,
                name: name.as_deref().unwrap_or(""),
                kind: "Symbol",
                repo_relative_path: repo_relative_path.as_deref(),
                span: *span,
                git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
            })
        })
        .collect();

    if results.is_empty() {
        eprintln!("error: no match found for file `{path}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| (r.span.map(|s| s.start_line), r.record_id));
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query drift
// ---------------------------------------------------------------------------

fn query_drift(records: &[GraphRecord], limit: usize, format: OutputFormat) -> Result<()> {
    let drifts = query::largest_semantic_drifts(records, limit);

    if drifts.is_empty() {
        eprintln!("error: no match found — no SemanticDrift nodes in graph");
        std::process::exit(2);
    }

    for record in drifts {
        let GraphRecord::Node {
            id,
            schema_version,
            semantic_drift: Some(drift),
            repo_relative_path: drift_path,
            name: drift_name,
            ..
        } = record
        else {
            continue;
        };

        let (resolved_path, resolved_name, resolved_span) = resolve_drift_target(
            records,
            id,
            drift,
            drift_path.as_deref(),
            drift_name.as_deref(),
        );

        let result = DriftResult {
            record_id: id,
            schema_version: *schema_version,
            before_commit: &drift.before_git_commit,
            after_commit: &drift.after_git_commit,
            before_valid_time: &drift.before_valid_time,
            after_valid_time: &drift.after_valid_time,
            embedding_model_provider: &drift.embedding_model.provider,
            embedding_model_name: &drift.embedding_model.name,
            embedding_model_version: &drift.embedding_model.version,
            embedding_model_dim: drift.embedding_model.dim,
            embedding_model_content_hash: &drift.embedding_model.content_hash,
            metric_kind: drift.metric_kind.as_str(),
            prior_record_id: &drift.prior_record_id,
            target_record_id: &drift.target_record_id,
            score: drift.score,
            selection_threshold: drift.selection_threshold,
            selection_basis: drift.selection_basis.as_str(),
            repo_relative_path: resolved_path,
            name: resolved_name,
            span: resolved_span,
            status: "drift is a lead, not proof",
        };
        print_result(&result, format)?;
    }
    Ok(())
}

fn resolve_drift_target<'a>(
    records: &'a [GraphRecord],
    drift_id: &str,
    drift: &'a SemanticDriftMetadata,
    drift_path: Option<&'a str>,
    drift_name: Option<&'a str>,
) -> (Option<&'a str>, Option<&'a str>, Option<SourceSpan>) {
    // Follow DriftsFrom edge first (stable contract per CLI docs); fall back to
    // target_record_id when no edge is present in this slice.
    let target_id = records
        .iter()
        .find_map(|r| {
            let GraphRecord::Edge {
                label: EdgeLabel::DriftsFrom,
                source,
                target,
                ..
            } = r
            else {
                return None;
            };
            if source == drift_id {
                Some(target.as_str())
            } else {
                None
            }
        })
        .unwrap_or(drift.target_record_id.as_str());

    if let Some(GraphRecord::Node {
        repo_relative_path,
        name,
        span,
        ..
    }) = records.iter().rfind(|r| {
        if r.id() != target_id {
            return false;
        }
        if let GraphRecord::Node {
            temporal: Some(t), ..
        } = r
        {
            t.git_commit == drift.after_git_commit
        } else {
            true
        }
    }) {
        return (repo_relative_path.as_deref(), name.as_deref(), *span);
    }
    (drift_path, drift_name, None)
}

// ---------------------------------------------------------------------------
// query context (issue #38)
// ---------------------------------------------------------------------------

fn query_context_cmd(records: &[GraphRecord], symbol_name: &str) -> Result<()> {
    let ctx = query::symbol_context(records, symbol_name);

    if ctx.is_no_match() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "symbol_name": symbol_name
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let source_facts: Vec<ContextSourceFact<'_>> = ctx
        .source_facts
        .iter()
        .filter_map(|r| context_source_fact(r))
        .collect();

    let observations: Vec<ContextObservation<'_>> = ctx
        .observations
        .iter()
        .filter_map(|r| context_observation(r))
        .collect();

    let project_state: Vec<ContextLinkedItem<'_>> = ctx
        .project_state
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let artifacts: Vec<ContextLinkedItem<'_>> = ctx
        .artifacts
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let verification_evidence: Vec<ContextLinkedItem<'_>> = ctx
        .verification_evidence
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let unresolved: Vec<ContextUnresolved<'_>> = ctx
        .unresolved
        .iter()
        .map(|u| ContextUnresolved {
            source_record_id: &u.source_record_id,
            target_handle: &u.target_handle,
            relation: &u.relation,
            target_domain: &u.target_domain,
            verification_status: "unresolved",
        })
        .collect();

    let topology_edges: Vec<ContextTopologyEdge<'_>> = ctx
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
                Some(ContextTopologyEdge {
                    record_id: id,
                    label: label.as_str(),
                    source_id: source,
                    target_id: target,
                    summary,
                    git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
                    valid_time: temporal.as_ref().map(|t| t.valid_time.as_str()),
                })
            } else {
                None
            }
        })
        .collect();

    let response = ContextResponse {
        ok: true,
        symbol_name,
        source_facts,
        topology_edges,
        observations,
        project_state,
        artifacts,
        verification_evidence,
        unresolved,
    };

    let output = serde_json::to_string_pretty(&response).context("failed to serialize context")?;
    println!("{output}");
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn query_task_cmd(records: &[GraphRecord], id_or_handle: &str) -> Result<()> {
    // Resolve task ID
    let resolved_ids = match query::resolve_task_ids(records, id_or_handle) {
        Ok(ids) => ids,
        Err(query::TaskResolveError::Ambiguous { handle, candidates }) => {
            let err_json =
                serde_json::to_string(&query::TaskResolveError::Ambiguous { handle, candidates })?;
            eprintln!("{err_json}");
            std::process::exit(1);
        }
        Err(query::TaskResolveError::Unsupported { handle, message }) => {
            let err_json =
                serde_json::to_string(&query::TaskResolveError::Unsupported { handle, message })?;
            eprintln!("{err_json}");
            std::process::exit(1);
        }
    };

    if resolved_ids.is_empty() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "task_id": id_or_handle
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let task_id = resolved_ids.iter().next().unwrap();
    let ctx = query::task_evidence_context(records, task_id);

    if ctx.is_no_match() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "task_id": id_or_handle
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let tasks: Vec<ContextLinkedItem<'_>> = ctx
        .tasks
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let acceptance_criteria: Vec<ContextLinkedItem<'_>> = ctx
        .acceptance_criteria
        .iter()
        .filter_map(|r| {
            let mut ac = context_linked_item(r)?;
            if ac.status == Some("verified") {
                let GraphRecord::Node {
                    verification_link_id,
                    ..
                } = r
                else {
                    return Some(ac);
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

                if let Some(ver_record) =
                    ver_id.and_then(|vid| records.iter().rfind(|cand| cand.id() == vid))
                {
                    ac.verification_record = context_linked_item(ver_record).map(Box::new);
                }
            }
            Some(ac)
        })
        .collect();

    let source_facts: Vec<ContextSourceFact<'_>> = ctx
        .source_facts
        .iter()
        .filter_map(|r| context_source_fact(r))
        .collect();

    let observations: Vec<ContextObservation<'_>> = ctx
        .observations
        .iter()
        .filter_map(|r| context_observation(r))
        .collect();

    let artifacts: Vec<ContextLinkedItem<'_>> = ctx
        .artifacts
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let verification_evidence: Vec<ContextLinkedItem<'_>> = ctx
        .verification_evidence
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let reviews: Vec<ContextLinkedItem<'_>> = ctx
        .reviews
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let external_links: Vec<ContextLinkedItem<'_>> = ctx
        .external_links
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let unresolved: Vec<ContextUnresolved<'_>> = ctx
        .unresolved
        .iter()
        .map(|u| ContextUnresolved {
            source_record_id: &u.source_record_id,
            target_handle: &u.target_handle,
            relation: &u.relation,
            target_domain: &u.target_domain,
            verification_status: "unresolved",
        })
        .collect();

    let response = TaskContextResponse {
        ok: true,
        task_id,
        tasks,
        acceptance_criteria,
        source_facts,
        observations,
        artifacts,
        verification_evidence,
        reviews,
        external_links,
        unresolved,
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize task context")?;
    println!("{output}");
    Ok(())
}

// ---------------------------------------------------------------------------
// query memory — memory evidence audit (issue #64)
// ---------------------------------------------------------------------------

/// Maps a node kind to its trust class so an agent claim is never labelled as
/// source truth (AC3).
fn trust_class_for(record: &GraphRecord) -> &'static str {
    let Some(kind) = record.node_kind_name() else {
        return "other";
    };
    match kind {
        "Observation" | "Decision" | "Failure" | "Lesson" => "agent_authored",
        "Verification" | "CommandEvidence" | "CommandRun" | "TestRun" | "CIStatus"
        | "BenchmarkRun" | "CoverageReport" | "ProofResult" => "verification_evidence",
        "File" | "Symbol" | "Module" | "Import" | "Commit" | "Change" | "Repository" => {
            "source_fact"
        }
        "Task"
        | "AcceptanceCriterion"
        | "LocalTask"
        | "GitHubIssue"
        | "PR"
        | "Review"
        | "ExternalLink"
        | "Product"
        | "Project"
        | "Plan" => "project_state",
        "Artifact" | "PatchArtifact" | "FileEdit" => "artifact",
        _ => "other",
    }
}

/// Returns a redaction-safe summary plus an optional hash of the stored one.
///
/// For agent-authored records the stored summary embeds a prefix of the
/// observation text (see `build_observation_records`), so it is never forwarded
/// verbatim. We synthesize a structured label from typed fields and expose the
/// original only as a BLAKE3 hash (AC9). Structured records (code, verification,
/// project, artifact) keep their templated summary, which carries no free text.
fn safe_summary(record: &GraphRecord) -> (String, Option<String>) {
    let GraphRecord::Node {
        kind,
        summary,
        agent_id,
        session_id,
        ..
    } = record
    else {
        return (String::new(), None);
    };
    if trust_class_for(record) == "agent_authored" {
        let who = match (agent_id.as_deref(), session_id.as_deref()) {
            (Some(a), Some(s)) => format!("{a}:{s}"),
            (Some(a), None) => a.to_owned(),
            _ => "unknown".to_owned(),
        };
        let label = format!("{} by {who}", kind.as_str());
        let hash = format!("blake3:{}", blake3::hash(summary.as_bytes()).to_hex());
        (label, Some(hash))
    } else {
        (summary.clone(), None)
    }
}

/// Builds the claim view, never presenting it as source truth (AC3).
fn audit_claim(record: &GraphRecord) -> Option<AuditClaim<'_>> {
    let GraphRecord::Node {
        id,
        kind,
        text,
        confidence,
        superseded_by,
        redaction_policy_version,
        ..
    } = record
    else {
        return None;
    };
    let redacted = redaction_policy_version.is_some()
        || text.as_deref().is_some_and(|t| t.contains("<REDACTED:"));
    let text_hash = text
        .as_deref()
        .map(|t| format!("blake3:{}", blake3::hash(t.as_bytes()).to_hex()));
    let (summary, summary_hash) = safe_summary(record);
    Some(AuditClaim {
        record_id: id,
        kind: kind.as_str(),
        trust_class: "agent_authored",
        summary,
        summary_hash,
        text_hash,
        confidence: confidence.as_deref(),
        superseded_by: superseded_by.as_deref(),
        redacted,
    })
}

/// Computes a non-empty citable handle for an item, guaranteeing AC4.
fn citable_handle(record: &GraphRecord) -> String {
    let GraphRecord::Node {
        repo_relative_path,
        span,
        source_artifact_path,
        source_artifact_hash,
        stdout_handle,
        stderr_handle,
        patch_bytes_hash,
        body_handle,
        url,
        source_handle,
        agent_id,
        session_id,
        verification_kind,
        title,
        name,
        id,
        ..
    } = record
    else {
        return record.id().to_owned();
    };
    if let Some(path) = repo_relative_path.as_deref() {
        return span.map_or_else(
            || path.to_owned(),
            |s| format!("{path}:{}-{}", s.start_line, s.end_line),
        );
    }
    if let Some(p) = source_artifact_path.as_deref() {
        return p.to_owned();
    }
    if let Some(h) = source_artifact_hash.as_deref() {
        return h.to_owned();
    }
    if let Some(h) = stdout_handle.as_ref().map(|o| o.hash.as_str()) {
        return h.to_owned();
    }
    if let Some(h) = stderr_handle.as_ref().map(|o| o.hash.as_str()) {
        return h.to_owned();
    }
    if let Some(h) = patch_bytes_hash.as_deref() {
        return h.to_owned();
    }
    if let Some(h) = body_handle.as_ref().map(|o| o.hash.as_str()) {
        return h.to_owned();
    }
    if let Some(u) = url.as_deref() {
        return u.to_owned();
    }
    if let Some(s) = source_handle.as_deref() {
        return s.to_owned();
    }
    match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => return format!("{a}:{s}"),
        (Some(a), None) => return a.to_owned(),
        _ => {}
    }
    if let Some(v) = verification_kind.as_deref() {
        return v.to_owned();
    }
    title
        .as_deref()
        .or(name.as_deref())
        .map_or_else(|| id.clone(), ToOwned::to_owned)
}

/// Serializes one evidence item to a bounded, payload-free view (AC9).
#[allow(clippy::too_many_lines)]
fn audit_item<'a>(item: &query::MemoryEvidenceItem<'a>) -> AuditItem<'a> {
    let record = item.record;
    let handle = citable_handle(record);
    let trust = trust_class_for(record);
    let (summary, summary_hash) = safe_summary(record);
    let GraphRecord::Node {
        id,
        kind,
        name,
        title,
        repo_relative_path,
        span,
        status,
        verification_kind,
        exit_code,
        source_artifact_path,
        source_artifact_hash,
        stdout_handle,
        stderr_handle,
        patch_status,
        patch_bytes_hash,
        body_handle,
        author,
        agent_id,
        session_id,
        observed_at,
        confidence,
        patch_handle,
        diff_hunk_handle,
        arguments_handle,
        result_handle,
        ..
    } = record
    else {
        // Edges/tombstones never reach here; produce a minimal safe item.
        return AuditItem {
            record_id: record.id(),
            kind: "Unknown",
            trust_class: "other",
            relation: item.relation.clone(),
            citable_handle: handle,
            summary,
            summary_hash,
            name: None,
            title: None,
            repo_relative_path: None,
            span: None,
            status: None,
            verification_kind: None,
            exit_code: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            stdout_hash: None,
            stderr_hash: None,
            patch_status: None,
            patch_bytes_hash: None,
            body_handle_hash: None,
            diff_hunk_hash: None,
            author: None,
            agent_id: None,
            session_id: None,
            observed_at: None,
            confidence: None,
            protected: false,
        };
    };
    let protected = patch_handle.is_some()
        || stdout_handle.as_ref().is_some_and(|o| o.bytes > 0)
        || stderr_handle.as_ref().is_some_and(|o| o.bytes > 0)
        || body_handle.is_some()
        || diff_hunk_handle.is_some()
        || arguments_handle.is_some()
        || result_handle.is_some();
    AuditItem {
        record_id: id,
        kind: kind.as_str(),
        trust_class: trust,
        relation: item.relation.clone(),
        citable_handle: handle,
        summary,
        summary_hash,
        name: name.as_deref(),
        title: title.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        status: status.as_deref(),
        verification_kind: verification_kind.as_deref(),
        exit_code: *exit_code,
        source_artifact_path: source_artifact_path.as_deref(),
        source_artifact_hash: source_artifact_hash.as_deref(),
        stdout_hash: stdout_handle.as_ref().map(|o| o.hash.as_str()),
        stderr_hash: stderr_handle.as_ref().map(|o| o.hash.as_str()),
        patch_status: patch_status.as_deref(),
        patch_bytes_hash: patch_bytes_hash.as_deref(),
        body_handle_hash: body_handle.as_ref().map(|o| o.hash.as_str()),
        diff_hunk_hash: diff_hunk_handle.as_ref().map(|o| o.hash.as_str()),
        author: author.as_deref(),
        agent_id: agent_id.as_deref(),
        session_id: session_id.as_deref(),
        observed_at: observed_at.as_deref(),
        confidence: confidence.as_deref(),
        protected,
    }
}

/// Emits a `protected_payload` diagnostic for each withheld raw payload (AC6).
fn protected_payload_diagnostics<'a>(record: &'a GraphRecord, out: &mut Vec<AuditDiagnostic<'a>>) {
    let GraphRecord::Node {
        id,
        stdout_handle,
        stderr_handle,
        patch_bytes_hash,
        patch_handle,
        body_handle,
        diff_hunk_handle,
        arguments_handle,
        result_handle,
        ..
    } = record
    else {
        return;
    };
    if let Some(o) = stdout_handle.as_ref().filter(|o| o.bytes > 0) {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "stdout",
            target_domain: "verification",
        });
    }
    if let Some(o) = stderr_handle.as_ref().filter(|o| o.bytes > 0) {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "stderr",
            target_domain: "verification",
        });
    }
    if patch_handle.is_some()
        && let Some(h) = patch_bytes_hash.as_deref()
    {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: h,
            relation: "patch_bytes",
            target_domain: "artifact",
        });
    }
    if let Some(o) = body_handle.as_ref() {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "body",
            target_domain: "project",
        });
    }
    if let Some(o) = diff_hunk_handle.as_ref() {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "diff_hunk",
            target_domain: "project",
        });
    }
    if let Some(o) = arguments_handle.as_ref() {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "tool_arguments",
            target_domain: "agent_memory",
        });
    }
    if let Some(o) = result_handle.as_ref() {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "tool_result",
            target_domain: "agent_memory",
        });
    }
}

#[allow(clippy::too_many_lines)]
fn query_memory_cmd(
    records: &[GraphRecord],
    id_or_handle: &str,
    verified_only: bool,
) -> Result<()> {
    let resolved = match query::resolve_memory_ids(records, id_or_handle) {
        Ok(res) => res,
        Err(
            err @ (query::MemoryResolveError::Ambiguous { .. }
            | query::MemoryResolveError::Unsupported { .. }),
        ) => {
            eprintln!("{}", serde_json::to_string(&err)?);
            std::process::exit(1);
        }
    };

    if resolved.matched.is_empty() {
        // The handle named only deleted records — either the canonical ID is a
        // tombstone target, or a source/session handle matched a now-tombstoned
        // claim (`tombstoned_only`). Either way it is stale, not missing (AC6).
        let is_tombstoned = resolved.tombstoned_only
            || records.iter().any(|r| {
                matches!(r, GraphRecord::Tombstone { deleted_id, .. } if deleted_id == id_or_handle)
            });
        let code = if is_tombstoned {
            "stale_handle"
        } else {
            "no_match"
        };
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "memory_handle": id_or_handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    // `resolve_memory_ids` has already dropped tombstoned (deleted) IDs, so a
    // resolved ID is always a live claim.
    let memory_id = resolved.matched.iter().next().expect("non-empty");

    let ctx = query::memory_audit_context(records, memory_id, verified_only);
    if ctx.is_no_match() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": "no_match", "memory_handle": id_or_handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let memory_claim: Vec<AuditClaim<'_>> = ctx
        .memory_claim
        .iter()
        .filter_map(|r| audit_claim(r))
        .collect();

    // Direct provenance from the first claim node.
    let provenance = ctx.memory_claim.first().map_or_else(
        || AuditProvenance {
            provenance_handle: None,
            agent_id: None,
            agent_kind: None,
            session_id: None,
            observed_at: None,
            ingested_at: None,
            source_handle: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            redaction_policy_version: None,
            agent_session_ids: Vec::new(),
            agent_ids: Vec::new(),
        },
        |claim| {
            let GraphRecord::Node {
                agent_id,
                agent_kind,
                session_id,
                observed_at,
                ingested_at,
                source_handle,
                source_artifact_path,
                source_artifact_hash,
                redaction_policy_version,
                ..
            } = claim
            else {
                unreachable!("claim is a node");
            };
            let provenance_handle = match (agent_id.as_deref(), session_id.as_deref()) {
                (Some(a), Some(s)) => Some(format!("{a}:{s}")),
                (Some(a), None) => Some(a.to_owned()),
                _ => None,
            };
            AuditProvenance {
                provenance_handle,
                agent_id: agent_id.as_deref(),
                agent_kind: agent_kind.as_deref(),
                session_id: session_id.as_deref(),
                observed_at: observed_at.as_deref(),
                ingested_at: ingested_at.as_deref(),
                source_handle: source_handle.as_deref(),
                source_artifact_path: source_artifact_path.as_deref(),
                source_artifact_hash: source_artifact_hash.as_deref(),
                redaction_policy_version: redaction_policy_version.as_deref(),
                agent_session_ids: ctx.agent_sessions.iter().map(|r| r.id()).collect(),
                agent_ids: ctx.agents.iter().map(|r| r.id()).collect(),
            }
        },
    );

    let supporting_evidence: Vec<AuditItem<'_>> =
        ctx.supporting_evidence.iter().map(audit_item).collect();
    let contradicting_evidence: Vec<AuditItem<'_>> =
        ctx.contradicting_evidence.iter().map(audit_item).collect();
    let superseding_records: Vec<AuditItem<'_>> =
        ctx.superseding_records.iter().map(audit_item).collect();
    let related_code_handles: Vec<AuditItem<'_>> =
        ctx.related_code_handles.iter().map(audit_item).collect();
    let related_project_handles: Vec<AuditItem<'_>> =
        ctx.related_project_handles.iter().map(audit_item).collect();
    let verification_evidence: Vec<AuditItem<'_>> =
        ctx.verification_evidence.iter().map(audit_item).collect();

    // Diagnostics: context (unresolved links) + protected payloads + redaction.
    let mut diagnostics: Vec<AuditDiagnostic<'_>> = ctx
        .diagnostics
        .iter()
        .map(|d| AuditDiagnostic {
            code: &d.code,
            source_record_id: &d.source_record_id,
            target_handle: &d.target_handle,
            relation: &d.relation,
            target_domain: &d.target_domain,
        })
        .collect();
    for claim in &ctx.memory_claim {
        protected_payload_diagnostics(claim, &mut diagnostics);
        if let GraphRecord::Node {
            id,
            redaction_policy_version: Some(ver),
            ..
        } = claim
        {
            diagnostics.push(AuditDiagnostic {
                code: "redacted_payload",
                source_record_id: id,
                target_handle: ver,
                relation: "redaction_policy_version",
                target_domain: "agent_memory",
            });
        }
    }
    for item in ctx
        .supporting_evidence
        .iter()
        .chain(&ctx.contradicting_evidence)
        .chain(&ctx.superseding_records)
        .chain(&ctx.related_project_handles)
        .chain(&ctx.verification_evidence)
    {
        protected_payload_diagnostics(item.record, &mut diagnostics);
    }
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.source_record_id.cmp(b.source_record_id))
            .then_with(|| a.target_handle.cmp(b.target_handle))
            .then_with(|| a.relation.cmp(b.relation))
            .then_with(|| a.target_domain.cmp(b.target_domain))
    });
    // Keep `target_domain` in the dedup key: two unresolved links sharing source,
    // handle, and relation but pointing at different domains are distinct
    // unresolved facts and must both be surfaced (AC6).
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
            && a.target_domain == b.target_domain
    });

    let excluded: Vec<AuditExcluded<'_>> = ctx
        .excluded
        .iter()
        .filter_map(|item| {
            let GraphRecord::Node {
                id,
                kind,
                source_handle,
                ..
            } = item.record
            else {
                return None;
            };
            Some(AuditExcluded {
                record_id: id,
                kind: kind.as_str(),
                reason: "unverified_observation",
                source_handle: source_handle.as_deref(),
            })
        })
        .collect();

    let returned = memory_claim.len()
        + supporting_evidence.len()
        + contradicting_evidence.len()
        + superseding_records.len()
        + related_code_handles.len()
        + related_project_handles.len()
        + verification_evidence.len();

    let response = MemoryAuditResponse {
        ok: true,
        memory_id,
        verified_only,
        memory_claim,
        direct_provenance: provenance,
        supporting_evidence,
        contradicting_evidence,
        superseding_records,
        related_code_handles,
        related_project_handles,
        verification_evidence,
        diagnostics,
        excluded,
        page: AuditPage {
            cursor: None,
            has_more: false,
            returned,
        },
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize memory audit")?;
    println!("{output}");
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
fn query_task_via_daemon(id_or_handle: &str, data_dir: &Path) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let params = serde_json::json!({ "task_id": id_or_handle });
    match client.query_verb_raw("criteria_for_task", &params, None) {
        Ok(result) => {
            let output = serde_json::to_string_pretty(&result)
                .context("failed to serialize task query result")?;
            println!("{output}");
            Ok(())
        }
        Err(e) => {
            let err_msg = e.to_string();
            if err_msg.contains("not_found") || err_msg.contains("no records found") {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": "no_match",
                        "task_id": id_or_handle
                    }
                });
                println!("{}", serde_json::to_string(&envelope)?);
                std::process::exit(2);
            } else if err_msg.contains("ambiguous") || err_msg.contains("unsupported") {
                eprintln!("{err_msg}");
                std::process::exit(1);
            }
            Err(e)
        }
    }
}

fn context_source_fact(record: &GraphRecord) -> Option<ContextSourceFact<'_>> {
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
    Some(ContextSourceFact {
        record_id: id,
        kind: kind.as_str(),
        name: name.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        // For current-tree records valid_time is at the node level; for
        // scan-history records it is in temporal.valid_time. Prefer the
        // node-level field and fall back to the temporal block.
        valid_time: valid_time
            .as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
        language: language.as_deref(),
        symbol_kind: symbol_kind.as_deref(),
    })
}

fn context_observation(record: &GraphRecord) -> Option<ContextObservation<'_>> {
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
    Some(ContextObservation {
        record_id: id,
        kind: kind.as_str(),
        summary,
        text: text.as_deref(),
        provenance_handle,
        agent_id: agent_id.as_deref(),
        session_id: session_id.as_deref(),
        observed_at: observed_at.as_deref(),
        confidence: confidence.as_deref(),
        failure_kind: failure_kind.as_deref(),
        exit_code: *exit_code,
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
    })
}

fn context_linked_item(record: &GraphRecord) -> Option<ContextLinkedItem<'_>> {
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
        stdout_handle,
        stderr_handle,
        source_artifact_path,
        source_artifact_hash,
        repo_relative_path,
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
        body_handle,
        evidence_links,
        author,
        ..
    } = record
    else {
        return None;
    };
    Some(ContextLinkedItem {
        record_id: id,
        kind: kind.as_str(),
        summary,
        title: title.as_deref(),
        name: name.as_deref(),
        text: text.as_deref(),
        status: status.as_deref(),
        verification_kind: verification_kind.as_deref(),
        exit_code: *exit_code,
        executed_at: executed_at.as_deref(),
        evidence_quality: evidence_quality.as_deref(),
        stdout_handle: stdout_handle.as_deref(),
        stderr_handle: stderr_handle.as_deref(),
        source_artifact_path: source_artifact_path.as_deref(),
        source_artifact_hash: source_artifact_hash.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        edit_kind: edit_kind.as_deref(),
        before_hash: before_hash.as_deref(),
        after_hash: after_hash.as_deref(),
        rename_to: rename_to.as_deref(),
        hunk_count: *hunk_count,
        linked_turn_id: linked_turn_id.as_deref(),
        linked_patch_id: linked_patch_id.as_deref(),
        patch_status: patch_status.as_deref(),
        patch_handle: patch_handle.as_deref(),
        patch_bytes_hash: patch_bytes_hash.as_deref(),
        patch_bytes_size: *patch_bytes_size,
        target_files: target_files.as_deref(),
        validation_summary: validation_summary.as_deref(),
        base_commit: base_commit.as_deref(),
        unknown_base_reason: unknown_base_reason.as_deref(),
        producer_session_id: producer_session_id.as_deref(),
        body_handle: body_handle.as_deref(),
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
        verification_record: None,
        author: author.as_deref(),
    })
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn print_result<T: Serialize + PrintText>(result: &T, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => {
            let line = serde_json::to_string(result).context("failed to serialize query result")?;
            println!("{line}");
        }
        OutputFormat::Text => {
            println!("{}", result.as_text());
        }
    }
    Ok(())
}

trait PrintText {
    fn as_text(&self) -> String;
}

impl PrintText for SymbolResult<'_> {
    fn as_text(&self) -> String {
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        let line = self.span.map_or(0, |s| s.start_line);
        let commit = self.git_commit.map_or(String::new(), |c| format!(" [{c}]"));
        format!("{} ({}) @ {path}:{line}{commit}", self.name, self.kind)
    }
}

impl PrintText for DriftResult<'_> {
    fn as_text(&self) -> String {
        let name = self.name.unwrap_or("(unknown)");
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        format!(
            "{name} score={:.6} {}..{} @ {path}",
            self.score, self.before_commit, self.after_commit
        )
    }
}

#[cfg(feature = "embeddings")]
impl PrintText for SemanticResult<'_> {
    fn as_text(&self) -> String {
        let name = self.name.unwrap_or("(unknown)");
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        let line = self.span.map_or(0, |s| s.start_line);
        format!("{name} score={:.4} @ {path}:{line}", self.score)
    }
}

// ---------------------------------------------------------------------------

fn domain_category(domain: &str) -> &'static str {
    match domain {
        "codegraph" => "Deterministic Source Facts",
        "semantic" => "Derived Measurements",
        "agent_memory" => "Agent-Authored Claims",
        "project" => "Project/Work State",
        "artifact" => "Artifacts",
        "verification" => "Verification Evidence",
        "user_context" => "User Context",
        _ => "Unknown Domain",
    }
}

#[derive(Debug, Serialize, Clone)]
struct RepositorySummary {
    id: String,
    identity_summary: String,
}

#[derive(Debug, Default)]
struct InspectCounts {
    records: usize,
    nodes: usize,
    edges: usize,
    tombstones: usize,
    diagnostics: usize,
    schema_versions: BTreeMap<RecordVersion, usize>,
    unknown_schema_versions: BTreeMap<RecordVersion, usize>,
    repositories: Vec<RepositorySummary>,
    /// Per-`producer_kind` breakdown; legacy records use key `"legacy_pre_v1"`.
    producer_kinds: BTreeMap<String, usize>,
    /// Per-`egregore_version` breakdown; legacy records use key `"legacy_pre_v1"`.
    egregore_versions: BTreeMap<String, usize>,
}

impl InspectCounts {
    fn from_records(
        records: &[GraphRecord],
        unknown_schema_versions: &[crate::schema_version::UnknownSchemaVersion],
    ) -> Self {
        let mut counts = Self::default();
        for unknown in unknown_schema_versions {
            counts.records += 1;
            *counts
                .unknown_schema_versions
                .entry(unknown.version.clone())
                .or_default() += 1;
        }
        for record in records {
            counts.records += 1;
            if let Err(unknown) = crate::schema_version::validate_record_version(record) {
                *counts
                    .unknown_schema_versions
                    .entry(unknown.version.clone())
                    .or_default() += 1;
                continue;
            }
            *counts
                .schema_versions
                .entry(record_version(record))
                .or_default() += 1;
            match record {
                GraphRecord::Node {
                    id,
                    kind,
                    repository_identity,
                    ..
                } => {
                    counts.nodes += 1;
                    if *kind == NodeKind::Diagnostic {
                        counts.diagnostics += 1;
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
                        counts.repositories.push(RepositorySummary {
                            id: id.clone(),
                            identity_summary,
                        });
                    }
                }
                GraphRecord::Edge { .. } => counts.edges += 1,
                GraphRecord::Tombstone { .. } => counts.tombstones += 1,
            }
            // Producer breakdown — legacy records (no `producer` field) go under "legacy_pre_v1".
            let (kind_key, version_key) = record.producer().map_or_else(
                || ("legacy_pre_v1".to_owned(), "legacy_pre_v1".to_owned()),
                |p| {
                    (
                        p.producer_kind.as_str().to_owned(),
                        p.egregore_version.clone(),
                    )
                },
            );
            *counts.producer_kinds.entry(kind_key).or_default() += 1;
            *counts.egregore_versions.entry(version_key).or_default() += 1;
        }
        counts
    }

    fn from_jsonl(jsonl: &str) -> Result<Self> {
        let report = crate::adapters::records_from_jsonl_report(jsonl)?;
        Ok(Self::from_records(
            &report.records,
            &report.unknown_schema_versions,
        ))
    }

    fn to_json(&self, snapshot_timestamp: &str) -> serde_json::Value {
        let mut schema_versions_obj = serde_json::Map::new();
        for (version, count) in &self.schema_versions {
            let key = format!("{}:{}:{}", version.domain, version.kind, version.version);
            schema_versions_obj.insert(key, serde_json::Value::from(*count));
        }

        let mut unknown_schema_versions_obj = serde_json::Map::new();
        for (version, count) in &self.unknown_schema_versions {
            let key = format!("{}:{}:{}", version.domain, version.kind, version.version);
            unknown_schema_versions_obj.insert(key, serde_json::Value::from(*count));
        }

        let mut structured_counts = serde_json::Map::new();
        for (version, count) in &self.schema_versions {
            let category = domain_category(&version.domain);
            let category_obj = structured_counts
                .entry(category.to_owned())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let serde_json::Value::Object(map) = category_obj {
                let key = format!("{} v{}", version.kind, version.version);
                map.insert(key, serde_json::Value::from(*count));
            }
        }

        serde_json::json!({
            "snapshot_timestamp": snapshot_timestamp,
            "records": self.records,
            "nodes": self.nodes,
            "edges": self.edges,
            "tombstones": self.tombstones,
            "diagnostics": self.diagnostics,
            "domain_counts": structured_counts,
            "schema_versions": schema_versions_obj,
            "unknown_schema_versions": unknown_schema_versions_obj,
            "repositories": self.repositories.iter().map(|r| serde_json::json!({
                "id": r.id,
                "identity_summary": r.identity_summary
            })).collect::<Vec<_>>(),
            "producer_kinds": self.producer_kinds,
            "egregore_versions": self.egregore_versions
        })
    }
}

fn query_candidates_cmd(records: &[GraphRecord], format: OutputFormat) -> Result<()> {
    let list = crate::query::pending_candidates(records, None);
    let mut cli_candidates = Vec::new();
    for rec in list {
        if let GraphRecord::Node {
            id,
            user_context,
            confidence,
            ..
        } = rec
        {
            let suppressed = crate::query::is_candidate_suppressed(records, id);
            cli_candidates.push(serde_json::json!({
                "id": id,
                "proposed_rule_text": user_context.proposed_rule_text,
                "proposed_rule_kind": user_context.proposed_rule_kind,
                "confidence": confidence.as_deref().and_then(|c| c.parse::<f64>().ok()),
                "scope": user_context.scope,
                "supporting_evidence": user_context.supporting_evidence,
                "suppressed": suppressed,
            }));
        }
    }

    match format {
        OutputFormat::Json => {
            let out = serde_json::json!({
                "ok": true,
                "candidates": cli_candidates,
            });
            println!("{}", serde_json::to_string(&out)?);
        }
        OutputFormat::Text => {
            for c in cli_candidates {
                let id = c["id"].as_str().unwrap_or("");
                let rule_text = c["proposed_rule_text"].as_str().unwrap_or("");
                let kind = c["proposed_rule_kind"].as_str().unwrap_or("");
                let conf = c["confidence"].as_f64().unwrap_or(0.0);
                let supp_str = c["suppressed"]
                    .as_str()
                    .map(|s| format!(" [suppressed: {s}]"))
                    .unwrap_or_default();
                println!("{id}: [{kind}] {rule_text} (confidence: {conf}){supp_str}");
            }
        }
    }
    Ok(())
}

fn query_policy_cmd(
    records: &[GraphRecord],
    scope: &crate::UserContextScope,
    format: OutputFormat,
) -> Result<()> {
    let scope_filter = if scope.repo.is_none()
        && scope.path_glob.is_none()
        && scope.language.is_none()
        && scope.lifecycle_phase.is_none()
    {
        None
    } else {
        Some(scope)
    };

    let list = crate::query::active_policy(records, scope_filter);
    let mut cli_policy = Vec::new();
    for rec in list {
        if let GraphRecord::Node {
            id,
            kind,
            user_context,
            ..
        } = rec
        {
            cli_policy.push(serde_json::json!({
                "id": id,
                "kind": kind.as_str(),
                "rule_text": user_context.rule_text,
                "constraint_text": user_context.constraint_text,
                "canonical_name": user_context.canonical_name,
                "entity_kind": user_context.entity_kind,
                "approval_decision_id": user_context.approval_decision_id,
                "scope": user_context.scope,
                "active_from": user_context.active_from,
                "triggers": user_context.triggers,
                "action_summary": user_context.action_summary,
                "alternatives_rejected": user_context.alternatives_rejected,
                "enforcement_level": user_context.enforcement_level,
            }));
        }
    }

    match format {
        OutputFormat::Json => {
            let out = serde_json::json!({
                "ok": true,
                "policy": cli_policy,
            });
            println!("{}", serde_json::to_string(&out)?);
        }
        OutputFormat::Text => {
            for p in cli_policy {
                let id = p["id"].as_str().unwrap_or("");
                let kind = p["kind"].as_str().unwrap_or("");
                let body = p["rule_text"]
                    .as_str()
                    .or_else(|| p["constraint_text"].as_str())
                    .or_else(|| p["canonical_name"].as_str())
                    .unwrap_or("");
                println!("{id}: [{kind}] {body}");
            }
        }
    }
    Ok(())
}

fn query_audit_cmd(records: &[GraphRecord], durable_id: &str, format: OutputFormat) -> Result<()> {
    let durable = records
        .iter()
        .rfind(|r| r.id() == durable_id)
        .ok_or_else(|| anyhow::anyhow!("Durable record '{durable_id}' not found"))?;
    match crate::query::audit_trail(records, durable) {
        Ok(chain) => {
            match format {
                OutputFormat::Json => {
                    let out = serde_json::json!({
                        "ok": true,
                        "audit_chain": chain,
                    });
                    println!("{}", serde_json::to_string(&out)?);
                }
                OutputFormat::Text => {
                    println!("Audit trail for policy record: {durable_id}");
                    for (i, rec) in chain.iter().enumerate() {
                        if let GraphRecord::Node { id, kind, .. } = rec {
                            println!("  Step {i}: [{kind:?}] {id}");
                        }
                    }
                }
            }
            Ok(())
        }
        Err(e) => {
            match format {
                OutputFormat::Json => {
                    let out = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": "audit_trail_failed",
                            "message": e,
                        }
                    });
                    println!("{}", serde_json::to_string(&out)?);
                }
                OutputFormat::Text => {
                    eprintln!("error: audit trail failed: {e}");
                }
            }
            anyhow::bail!("audit trail failed: {e}")
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]
fn decide_cmd(
    candidate_id: String,
    outcome: String,
    data_dir: Option<PathBuf>,
    graph: Option<PathBuf>,
    out: Option<PathBuf>,
    edited_rule_text: Option<String>,
    rationale: Option<String>,
    decided_by: String,
    prompt_surface: String,
    prompted_to: String,
) -> Result<()> {
    if out.is_none() && data_dir.is_none() {
        anyhow::bail!("Either --out or --data-dir must be specified to write the decision records");
    }

    #[cfg(feature = "embedded-aletheiadb")]
    let mut sink_opt = None;

    #[allow(unused_mut)]
    let mut records = if let Some(path) = &graph {
        load_records_from_jsonl(path)?
    } else if let Some(dir) = &data_dir {
        #[cfg(feature = "embedded-aletheiadb")]
        {
            validate_existing_embedded_store(dir)?;
            let sink = EmbeddedAletheiaSink::open(dir)
                .with_context(|| format!("failed to open embedded store {}", dir.display()))?;
            let db_recs = sink
                .read_all_records()
                .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
            sink_opt = Some(sink);
            db_recs
        }
        #[cfg(not(feature = "embedded-aletheiadb"))]
        {
            let _ = dir;
            anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
        }
    } else {
        anyhow::bail!("provide --graph <path> or --data-dir <path>");
    };

    if graph.is_some()
        && let Some(dir) = &data_dir
    {
        #[cfg(feature = "embedded-aletheiadb")]
        {
            let sink = if let Some(s) = sink_opt.take() {
                s
            } else {
                EmbeddedAletheiaSink::open(dir)
                    .with_context(|| format!("failed to open embedded store {}", dir.display()))?
            };

            let store_exists = fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some());
            let db_records = if store_exists {
                sink.read_all_records()
                    .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?
            } else {
                Vec::new()
            };
            sink_opt = Some(sink);

            if !db_records.is_empty() {
                let mut merged = db_records;
                for r in records {
                    if !merged.contains(&r) {
                        merged.push(r);
                    }
                }
                records = merged;
            }
        }
        #[cfg(not(feature = "embedded-aletheiadb"))]
        {
            let _ = dir;
            anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
        }
    }

    let req = crate::decide::DecideRequest {
        candidate_id,
        outcome,
        edited_rule_text,
        rationale,
        decided_by,
        prompt_surface,
        prompted_to,
        transaction_time: None,
    };

    let generated = crate::decide::decide_candidate(&records, &req)?;

    if let Some(out_path) = out {
        let mut lines = String::new();
        for rec in &generated {
            let json = serde_json::to_string(rec)?;
            lines.push_str(&json);
            lines.push('\n');
        }
        fs::write(&out_path, lines).with_context(|| {
            format!("failed to write decision records to {}", out_path.display())
        })?;
    }

    if let Some(dir) = data_dir {
        #[cfg(feature = "embedded-aletheiadb")]
        {
            let mut sink = sink_opt.take().unwrap();
            let edges = crate::decide::synthesize_user_context_edges(&records, &generated);

            let mut source_records_to_persist = Vec::new();
            let mut seen_ids = std::collections::HashSet::new();
            for g in &generated {
                seen_ids.insert(g.id().to_owned());
            }
            let mut validation_edges = Vec::new();
            if let Some(cand) = records.iter().rfind(|r| r.id() == req.candidate_id) {
                let val_edges =
                    crate::daemon::validate_promote_candidate_for_cli(cand, &records, &sink)
                        .context("Candidate validation failed")?;
                validation_edges = val_edges;

                source_records_to_persist.push(cand.clone());
                seen_ids.insert(cand.id().to_owned());
            }

            let mut idx = 0;
            while idx < source_records_to_persist.len() {
                let rec = source_records_to_persist[idx].clone();
                idx += 1;

                if let GraphRecord::Node {
                    kind,
                    user_context,
                    evidence_links,
                    superseded_by,
                    ..
                } = &rec
                {
                    match kind {
                        NodeKind::PromoteCandidate => {
                            if let Some(evidence) = &user_context.supporting_evidence {
                                for link in evidence {
                                    if let Some(ref_id) = &link.target_record_id
                                        && seen_ids.insert(ref_id.clone())
                                        && let Some(evidence_rec) =
                                            records.iter().rfind(|r| r.id() == *ref_id)
                                    {
                                        if let GraphRecord::Node {
                                            kind: ev_kind,
                                            evidence_links: ev_links,
                                            ..
                                        } = evidence_rec
                                            && *ev_kind == NodeKind::Observation
                                            && ev_links
                                                .as_deref()
                                                .is_none_or(<[EvidenceLink]>::is_empty)
                                        {
                                            anyhow::bail!(
                                                "Observation '{ref_id}' must have at least one evidence link"
                                            );
                                        }
                                        source_records_to_persist.push(evidence_rec.clone());
                                    }
                                }
                            }
                            if let Some(evidence) = &user_context.contradicting_evidence {
                                for link in evidence {
                                    if let Some(ref_id) = &link.target_record_id {
                                        if user_context.proposed_rule_kind.as_deref()
                                            == Some("revocation")
                                            && let Some(target_rec) =
                                                records.iter().rfind(|r| r.id() == *ref_id)
                                            && let Ok(target_chain) =
                                                crate::query::audit_trail(&records, target_rec)
                                        {
                                            for chain_rec in target_chain {
                                                if seen_ids.insert(chain_rec.id().to_owned()) {
                                                    source_records_to_persist
                                                        .push(chain_rec.clone());
                                                }
                                            }
                                        }
                                        if seen_ids.insert(ref_id.clone())
                                            && let Some(evidence_rec) =
                                                records.iter().rfind(|r| r.id() == *ref_id)
                                        {
                                            source_records_to_persist.push(evidence_rec.clone());
                                        }
                                    }
                                }
                            }
                            if let Some(rej_id) = superseded_by {
                                if seen_ids.insert(rej_id.clone())
                                    && let Some(rej_cand) =
                                        records.iter().rfind(|r| r.id() == *rej_id)
                                {
                                    source_records_to_persist.push(rej_cand.clone());
                                }
                                for r in &records {
                                    if let GraphRecord::Node {
                                        kind: NodeKind::PromotionDecision,
                                        user_context: dec_uc,
                                        ..
                                    } = r
                                        && dec_uc.candidate_id.as_deref() == Some(rej_id)
                                        && dec_uc.outcome.as_deref() == Some("rejected")
                                        && seen_ids.insert(r.id().to_owned())
                                    {
                                        source_records_to_persist.push(r.clone());
                                    }
                                }
                            }
                        }
                        NodeKind::PromotionDecision => {
                            if let Some(p_id) = &user_context.prompt_id
                                && seen_ids.insert(p_id.clone())
                                && let Some(prompt_rec) = records.iter().rfind(|r| r.id() == *p_id)
                            {
                                source_records_to_persist.push(prompt_rec.clone());
                            }
                        }
                        _ => {
                            if let Some(links) = evidence_links {
                                for ev_link in links {
                                    if let Some(target_id) = &ev_link.target_record_id
                                        && seen_ids.insert(target_id.clone())
                                        && let Some(target_rec) =
                                            records.iter().rfind(|r| r.id() == *target_id)
                                    {
                                        source_records_to_persist.push(target_rec.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let generated_ids: std::collections::HashSet<String> =
                generated.iter().map(|g| g.id().to_owned()).collect();
            let mut copied_synthesized_edges = Vec::new();
            let mut seen_memory_edges: std::collections::HashMap<String, (Option<String>, String)> =
                std::collections::HashMap::new();
            for rec in &source_records_to_persist {
                if generated_ids.contains(rec.id()) {
                    continue;
                }
                if rec.id().starts_with("agent_memory:v1:") {
                    crate::daemon::validate_agent_memory_record_for_cli(rec, &records, &sink)
                        .context("Copied evidence record validation failed")?;
                    if let GraphRecord::Node {
                        id,
                        evidence_links: Some(links),
                        ..
                    } = rec
                    {
                        for link in links {
                            if let Some(target_id) = &link.target_record_id
                                && let Some(label) =
                                    crate::ir::EdgeLabel::from_relation(&link.relation)
                            {
                                let summary =
                                    format!("{} {} (from evidence link)", id, label.as_str());
                                let mut edge = GraphRecord::agent_memory_edge(
                                    label,
                                    id.clone(),
                                    target_id.clone(),
                                    Some(link.confidence.clone()),
                                    summary,
                                );
                                if let Some(ref commit) = link.as_of_commit {
                                    edge = edge.with_temporal(crate::ir::TemporalMetadata {
                                        git_commit: commit.clone(),
                                        git_parent_commits: Vec::new(),
                                        valid_time: "1970-01-01T00:00:00Z".to_owned(),
                                        author_time: None,
                                        observed_at: "1970-01-01T00:00:00Z".to_owned(),
                                        valid_time_source: None,
                                    });
                                }
                                let edge_id = edge.id().to_owned();
                                match seen_memory_edges.get(&edge_id) {
                                    Some((existing_commit, existing_conf))
                                        if *existing_commit == link.as_of_commit =>
                                    {
                                        if existing_conf != &link.confidence {
                                            anyhow::bail!(
                                                "evidence links for edge '{edge_id}' have conflicting confidence values"
                                            );
                                        }
                                        // exact duplicate, skip silently
                                    }
                                    Some(_) => {
                                        anyhow::bail!(
                                            "evidence links for edge '{edge_id}' have conflicting as_of_commit values"
                                        );
                                    }
                                    None => {
                                        seen_memory_edges.insert(
                                            edge_id,
                                            (link.as_of_commit.clone(), link.confidence.clone()),
                                        );
                                        copied_synthesized_edges.push(edge);
                                    }
                                }
                            }
                        }
                    }
                } else if rec.id().starts_with("user_context:v1:") {
                    let edges =
                        crate::daemon::validate_user_context_record_for_cli(rec, &records, &sink)
                            .context("Copied user-context record validation failed")?;
                    copied_synthesized_edges.extend(edges);
                }

                crate::redaction::validate_record(rec).map_err(|e| {
                    anyhow::anyhow!(
                        "Redaction check failed for source record '{}': {}",
                        rec.id(),
                        e
                    )
                })?;

                if let GraphRecord::Node {
                    id,
                    kind,
                    user_context,
                    ..
                } = rec
                {
                    if *kind == NodeKind::PromotionDecision {
                        if let Some(c_id) = &user_context.candidate_id {
                            let edge_id = crate::ir::user_context_stable_id(&[
                                "edge",
                                "DECIDED_ON",
                                id,
                                c_id,
                            ]);
                            copied_synthesized_edges.push(GraphRecord::Edge {
                                id: edge_id,
                                schema_version: crate::ir::USER_CONTEXT_SCHEMA_VERSION,
                                label: crate::ir::EdgeLabel::DecidedOn,
                                source: id.clone(),
                                target: c_id.clone(),
                                confidence: None,
                                temporal: None,
                                summary: "PromotionDecision decided on PromoteCandidate".to_owned(),
                                producer: None,
                            });
                        }
                        if let Some(mat_id) = &user_context.materialized_record_id {
                            let outcome_str = user_context.outcome.as_deref().unwrap_or("");
                            if outcome_str == "approved" || outcome_str == "edited_then_approved" {
                                let is_revocation =
                                    user_context.candidate_id.as_ref().is_some_and(|c_id| {
                                        records
                                            .iter()
                                            .rfind(|r| match r {
                                                GraphRecord::Node { id: node_id, .. } => {
                                                    node_id == c_id
                                                }
                                                _ => false,
                                            })
                                            .and_then(|r| match r {
                                                GraphRecord::Node {
                                                    user_context: node_uc,
                                                    ..
                                                } => node_uc.proposed_rule_kind.as_deref(),
                                                _ => None,
                                            })
                                            == Some("revocation")
                                    });

                                if is_revocation {
                                    let edge_id = crate::ir::user_context_stable_id(&[
                                        "edge",
                                        "REVOKED_BY",
                                        mat_id,
                                        id,
                                    ]);
                                    copied_synthesized_edges.push(GraphRecord::Edge {
                                        id: edge_id,
                                        schema_version: crate::ir::USER_CONTEXT_SCHEMA_VERSION,
                                        label: crate::ir::EdgeLabel::RevokedBy,
                                        source: mat_id.clone(),
                                        target: id.clone(),
                                        confidence: None,
                                        temporal: None,
                                        summary:
                                            "PromotionDecision revoked durable user-context record"
                                                .to_owned(),
                                        producer: None,
                                    });
                                } else {
                                    let edge_id = crate::ir::user_context_stable_id(&[
                                        "edge",
                                        "MATERIALIZED_AS",
                                        id,
                                        mat_id,
                                    ]);
                                    copied_synthesized_edges.push(GraphRecord::Edge {
                                        id: edge_id,
                                        schema_version: crate::ir::USER_CONTEXT_SCHEMA_VERSION,
                                        label: crate::ir::EdgeLabel::MaterializedAs,
                                        source: id.clone(),
                                        target: mat_id.clone(),
                                        confidence: None,
                                        temporal: None,
                                        summary: "PromotionDecision materialized durable user-context record".to_owned(),
                                        producer: None,
                                    });
                                }
                            }
                        }
                    } else if *kind == NodeKind::PromotionPrompt
                        && let Some(c_id) = &user_context.candidate_id
                    {
                        let edge_id =
                            crate::ir::user_context_stable_id(&["edge", "PROMPTED_FOR", id, c_id]);
                        copied_synthesized_edges.push(GraphRecord::Edge {
                            id: edge_id,
                            schema_version: crate::ir::USER_CONTEXT_SCHEMA_VERSION,
                            label: crate::ir::EdgeLabel::PromptedFor,
                            source: id.clone(),
                            target: c_id.clone(),
                            confidence: None,
                            temporal: None,
                            summary: "PromotionPrompt prompted for PromoteCandidate".to_owned(),
                            producer: None,
                        });
                    }
                }
            }

            let mut all_records = generated;
            all_records.extend(edges);
            all_records.extend(validation_edges);

            for edge in copied_synthesized_edges {
                if seen_ids.insert(edge.id().to_owned()) {
                    all_records.push(edge);
                }
            }

            let source_records_filtered: Vec<GraphRecord> = source_records_to_persist
                .into_iter()
                .filter(|r| !generated_ids.contains(r.id()))
                .collect();
            all_records.extend(source_records_filtered);
            let report = ingest_records(&all_records, &mut sink);
            if !report.is_success() {
                for failure in &report.failures {
                    eprintln!("{}: {}", failure.record_id, failure.message);
                }
                anyhow::bail!("failed to write decision records to store");
            }
            sink.persist_indexes()
                .with_context(|| format!("failed to persist embedded store {}", dir.display()))?;
        }
        #[cfg(not(feature = "embedded-aletheiadb"))]
        {
            let _ = dir;
            anyhow::bail!("--data-dir requires the embedded-aletheiadb feature");
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------------------------------------------
// AC7: Semantic query JSON output contract conformance
//
// This test module locks the stable field names for `eg query semantic --format
// json`. If any field is removed or renamed without updating this test (and the
// docs in docs/cli/query.md), the test suite will fail during CI.
// ---------------------------------------------------------------------------
#[cfg(all(test, feature = "embeddings"))]
mod semantic_contract {
    use super::*;

    const fn full_span() -> SourceSpan {
        SourceSpan {
            start_byte: 4096,
            end_byte: 5200,
            start_line: 142,
            end_line: 168,
        }
    }

    /// All stable fields present — verifies required and optional contract fields.
    #[test]
    fn semantic_result_json_contract_all_stable_fields_present() {
        let result = SemanticResult {
            record_id: "codegraph:v1:abc123",
            name: Some("EmbeddedAletheiaSink::write_record"),
            repo_relative_path: Some("src/sink/embedded.rs"),
            score: 0.9231_f32,
            span: Some(full_span()),
        };
        let json =
            serde_json::to_value(&result).expect("SemanticResult must serialize to JSON value");

        // Required stable fields — test fails if either is removed or renamed.
        assert!(
            json.get("record_id").is_some(),
            "stable contract field 'record_id' must be present in JSON output"
        );
        assert!(
            json.get("score").is_some(),
            "stable contract field 'score' must be present in JSON output"
        );

        // Optional stable fields — must appear in the JSON when the field is populated.
        assert!(
            json.get("name").is_some(),
            "optional contract field 'name' must appear in JSON when populated"
        );
        assert!(
            json.get("repo_relative_path").is_some(),
            "optional contract field 'repo_relative_path' must appear in JSON when populated"
        );
        assert!(
            json.get("span").is_some(),
            "optional contract field 'span' must appear in JSON when populated"
        );

        // span sub-fields are part of the stable contract.
        let span = &json["span"];
        for sub in ["start_byte", "end_byte", "start_line", "end_line"] {
            assert!(
                span.get(sub).is_some(),
                "span.{sub} is a stable contract sub-field and must be present"
            );
        }
    }

    /// Optional fields absent when None — verifies `skip_serializing_if` contract.
    #[test]
    fn semantic_result_json_contract_optional_fields_omitted_when_none() {
        let result = SemanticResult {
            record_id: "codegraph:v1:abc123",
            name: None,
            repo_relative_path: None,
            score: 0.42_f32,
            span: None,
        };
        let json = serde_json::to_value(&result).expect("serialize");

        assert!(json.get("record_id").is_some(), "record_id always present");
        assert!(json.get("score").is_some(), "score always present");
        assert!(
            json.get("name").is_none(),
            "contract: 'name' must be absent from JSON when None"
        );
        assert!(
            json.get("repo_relative_path").is_none(),
            "contract: 'repo_relative_path' must be absent from JSON when None"
        );
        assert!(
            json.get("span").is_none(),
            "contract: 'span' must be absent from JSON when None"
        );
    }
}
