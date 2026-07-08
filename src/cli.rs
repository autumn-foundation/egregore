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
    freshness::{self, Freshness},
    identity,
    ir::{EdgeLabel, EvidenceLink, Graph, GraphRecord, NodeKind, SnapshotHead, SourceSpan},
    link_evidence::{self, LinkOptions},
    local_project, query, scan_repository_history_with_override, scan_repository_with_exclusions,
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
use crate::incremental::scan_repository_incremental_excluding;
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
    ///
    /// When run in a Git repository root, scopes the scan strictly to Git-tracked files
    /// (committed and staged), honoring .gitignore rules and skipping ignored/untracked files.
    /// A count of skipped source files by language (.rs, .py, .ts/.tsx, .go) is printed to stderr.
    /// If the target is not a Git repository root, falls back to a filesystem walk that
    /// skips .git and target directories.
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
    /// Incrementally refresh an ingested store from working-tree edits.
    ///
    /// Reuses the incremental file-cache (BLAKE3 per-file hashes) and the
    /// tombstone-aware embedded re-ingest to update only changed/added/removed
    /// files, leaving all other graph records (agent-memory, project, artifact,
    /// verification) completely untouched.
    ///
    /// Shortest workflow (`docs/cli/refresh.md`):
    ///   1. `eg scan <repo> --out g.jsonl && eg ingest g.jsonl --adapter embedded --data-dir .egregore`
    ///   2. (edit source files …)
    ///   3. `eg refresh <repo> --data-dir .egregore`
    ///   4. `eg query symbol <name> --data-dir .egregore`
    ///
    /// Precondition failures (exit 2, machine-readable JSON to stderr):
    ///   `{"code":"no_prior_scan"}` — `--data-dir` does not exist; run step 1 first.
    ///   `{"code":"repository_identity_mismatch"}` — cache was built for a different
    ///     repository; delete `<data-dir>/codegraph-cache.json` and re-run from step 1.
    #[cfg(feature = "embedded-aletheiadb")]
    Refresh {
        /// Repository path to scan.
        repo_path: PathBuf,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
        /// Incremental scan cache path.
        /// Defaults to `<data-dir>/codegraph-cache.json`.
        #[arg(long)]
        cache: Option<PathBuf>,
        /// Output format for the refresh report (rebuilt / reused / tombstoned counts).
        #[arg(long, default_value = "text")]
        format: OutputFormat,
        /// Refresh semantic embeddings for changed file and symbol nodes.
        ///
        /// Without this flag, structural records are updated but the semantic index
        /// retains embeddings for any nodes whose source has since changed.
        /// See `docs/cli/refresh.md` for the documented embedding-refresh workflow.
        #[cfg(feature = "embeddings")]
        #[arg(long)]
        embed: bool,
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
    /// Report whether a store still matches the current working tree (issue #82).
    ///
    /// Reads the store-level source-snapshot identity stamped by `scan`/`ingest`
    /// (HEAD commit + dirty flag) and compares it against the live working tree,
    /// classifying the store as `fresh`, `stale_head`, `stale_dirty`, or
    /// `unknown`. An agent uses this to avoid citing file/span handles the live
    /// code has already invalidated.
    ///
    /// Strictly read-only and fully offline: it never creates, modifies, or
    /// deletes any graph record, runtime file, index, or idempotency receipt, and
    /// never accesses the network.
    ///
    /// Both human-readable text (`--format text`, default) and machine-readable
    /// JSON (`--format json`) are supported; the JSON `freshness` code is stable.
    ///
    /// Exits 0 regardless of the freshness verdict (the verdict is the payload,
    /// not an error); exits non-zero only on I/O or store-read failure.
    ///
    /// See `docs/cli/freshness.md`.
    Freshness {
        /// Working-tree path to compare the store against (defaults to the current directory).
        #[arg(default_value = ".")]
        repo_path: PathBuf,
        /// Graph JSONL store to check (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory to check (mutually exclusive with --graph).
        #[arg(long, conflicts_with = "graph")]
        data_dir: Option<PathBuf>,
        /// Override the auto-detected repository identity used to locate the stored snapshot.
        #[arg(long)]
        repo_id_override: Option<String>,
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
    /// Import an Antigravity transcript JSONL into agent-memory JSONL.
    ImportAntigravity {
        /// Path to the Antigravity session transcript JSONL file.
        antigravity_path: PathBuf,
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
        source: Box<ImportSource>,
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
        subcommand: Box<QuerySubcommand>,
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
        kind: Box<WriteKind>,
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
    /// Run the agent-memory recall evaluation against a corpus file (issue #91).
    ///
    /// Requires a pre-built embedded store seeded with imported memory and
    /// ingested with `--embed`. Exits 0 if top-3 recall meets the threshold,
    /// 1 with a diagnostic if missed.
    #[cfg(feature = "embeddings")]
    EvalMemoryRecall {
        /// Path to the agent-memory recall corpus JSON file.
        #[arg(long, default_value = "corpus/agent_memory_recall_corpus.json")]
        corpus: PathBuf,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long)]
        data_dir: PathBuf,
        /// Number of top memory results to retrieve per question.
        #[arg(long, default_value = "3")]
        top_k: usize,
        /// Minimum top-3 recall fraction required to pass (0.0–1.0, default 0.80).
        #[arg(long, default_value = "0.8", value_parser = parse_threshold)]
        threshold: f64,
        /// Exclude unverified observations from recall, as the trust filter does.
        #[arg(long)]
        verified_only: bool,
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
    /// Capture and retrieve protected raw artifact payloads (issue #60).
    ///
    /// By default Egregore stores only content hashes and handles for raw payloads
    /// such as transcripts, command output, patches, task narratives, and reports.
    /// This command gives operators an opt-in workflow to also retain the original
    /// raw bytes in a local, content-addressed protected store that the graph, query,
    /// and semantic surfaces never read.
    ///
    /// See `docs/cli/protected-artifacts.md` for the full operator workflow.
    Protected {
        /// Protected-artifact subcommand.
        #[command(subcommand)]
        subcommand: ProtectedSubcommand,
    },
    /// Manage, export, verify, and inspect redaction-safe evidence bundles (issue #68).
    Bundle {
        /// Bundle subcommand.
        #[command(subcommand)]
        subcommand: BundleSubcommand,
    },
    /// Audit citation completeness across the public query workflows (issue #65).
    ///
    /// Drives every public query workflow over a seeded local record set and
    /// measures, per workflow and overall, whether returned rows carry the
    /// citation handles their trust class requires. Local-first; no network,
    /// hosted indexing, remote crawling, or mandatory remote embeddings.
    ///
    /// See `docs/cli/citation-audit.md` for the full workflow.
    Audit {
        /// Citation-audit subcommand.
        #[command(subcommand)]
        subcommand: AuditSubcommand,
    },
    /// Report local setup readiness for the scan → ingest → semantic-search workflow.
    ///
    /// Read-only by default: does not download models, create graph records, mutate
    /// `.egregore`, start or stop the daemon, or contact hosted services unless
    /// `--network` is explicitly passed.
    ///
    /// Exits 0 when all required structural checks pass; exits 1 when a required
    /// check fails. Optional and semantic failures (python, model cache) never
    /// change the exit code.
    ///
    /// Both human-readable text (`--format text`) and machine-readable JSON
    /// (`--format json`, default) are supported. JSON check IDs are stable.
    ///
    /// See docs/cli/doctor.md for the full check matrix, exit codes, and the
    /// distinction from the post-ingest semantic index readiness report (issue #71).
    ///
    /// Note: `--out` defaults to `graph.jsonl` here (unlike `scan`/`ingest` where
    /// it is required). The default is for diagnostic convenience only.
    Doctor {
        /// Repository path to inspect.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Output JSONL path whose parent writability is checked.
        #[arg(long, default_value = "graph.jsonl")]
        out: PathBuf,
        /// Embedded data directory whose writability is checked.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
        /// Promote git-history readability from optional to required.
        /// Use when you need `eg scan-history` to work.
        #[arg(long)]
        require_history: bool,
        /// Perform one optional Hugging Face TCP reachability check.
        /// Without this flag, no hosted services are contacted.
        #[arg(long)]
        network: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Watch directories for new/modified agent transcripts and auto-ingest them.
    #[cfg(feature = "embedded-aletheiadb")]
    Watch {
        /// Embedded `AletheiaDB` data directory (default: `.egregore`).
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
        /// Directory containing Antigravity transcripts.
        #[arg(long)]
        antigravity_dir: Option<PathBuf>,
        /// Directory containing Codex transcripts.
        #[arg(long)]
        codex_dir: Option<PathBuf>,
        /// Directory containing Claude Code transcripts.
        #[arg(long)]
        claude_dir: Option<PathBuf>,
        /// Polling interval in seconds.
        #[arg(long, default_value = "2")]
        poll_interval: u64,
        /// Generate embeddings for ingested records.
        #[cfg(feature = "embeddings")]
        #[arg(long)]
        embed: bool,
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
pub(crate) enum OutputFormat {
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
        /// Restrict results to one repository. Accepts the stable Repository
        /// record ID or a human-usable identity handle (display name such as
        /// `owner/name`, basename / operator override, remote URL, root commit
        /// SHA, or canonical path). Unknown or ambiguous selectors exit 1 with
        /// a machine-readable diagnostic.
        #[arg(long)]
        repo: Option<String>,
        /// Working-tree path to compute store freshness against (issue #82).
        ///
        /// When set, each result carries a non-fatal `freshness` code
        /// (`fresh` / `stale_head` / `stale_dirty` / `unknown`) so an agent can
        /// downgrade trust in the cited handle. Omitted → no freshness field.
        #[arg(long)]
        repo_path: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List symbol nodes whose name matches a partial-name pattern.
    ///
    /// A pattern containing `*` is an anchored glob over the whole name
    /// (`handle_*` for a prefix, `*_sink` for a suffix); otherwise the
    /// pattern matches as a literal substring anywhere in the name.
    /// Deterministic and structural-store only: no embedding model or
    /// `--embed` store is required, and only `Symbol` node names are
    /// searched — comments, string literals, and doc text never match
    /// (issue #102).
    Symbols {
        /// Name pattern: literal substring, or an anchored `*` glob when it
        /// contains `*`. Case-sensitive unless --case-insensitive is set.
        pattern: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Match case-insensitively (default is case-sensitive).
        #[arg(long)]
        case_insensitive: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Find who last changed a symbol.
    Who {
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
        /// Restrict results to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Working-tree path to compute store freshness against.
        #[arg(long)]
        repo_path: Option<PathBuf>,
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
        /// Restrict results to one repository (see `eg query symbol --help`).
        /// A colliding path in another repository is excluded and reported via
        /// a stderr diagnostic, never mixed into the result set.
        #[arg(long)]
        repo: Option<String>,
        /// Working-tree path to compute store freshness against (issue #82).
        ///
        /// When set, each result carries a non-fatal `freshness` code; omitted →
        /// no freshness field. See `eg query symbol --help`.
        #[arg(long)]
        repo_path: Option<PathBuf>,
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
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
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
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Maximum number of results (default 10).
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Answer a natural-language query with evidence-backed context for the
    /// top-N semantic matches in one call (issue #90).
    ///
    /// Bridges semantic discovery and the symbol-context lane: it embeds the
    /// query locally, ranks matches against the embedded store, then returns —
    /// per match — the stable record ID, repo-relative file/span handle, the
    /// relevance score, and the same five trust-separated context sections
    /// produced by `eg query context`. File-typed matches are first-class
    /// (their defined symbols are seeded); an ambiguous symbol name reports all
    /// candidate record IDs instead of silently picking one.
    ///
    /// Read-only and deterministic. On no semantic hit clearing `--min-score`:
    /// emits `{"ok":false,"error":{"code":"no_match",...}}` to stdout and exits
    /// with code 2. Documented in `docs/cli/semantic-search-guidance.md`.
    #[cfg(feature = "embeddings")]
    SemanticContext {
        /// Natural-language query text.
        query: String,
        /// Embedded `AletheiaDB` data directory (must be ingested with `--embed`).
        #[arg(long)]
        data_dir: PathBuf,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Maximum number of matches to expand (bounded; safe default 5).
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Relevance floor in `[0.0, 1.0]`; matches scoring below it are
        /// dropped, and an all-below result is a no-match (default 0.0).
        #[arg(long, default_value_t = 0.0)]
        min_score: f32,
        /// Supersession resolution mode for memory/observations.
        #[arg(long, value_enum, default_value_t = crate::temporal_status::SupersessionMode::Exclude)]
        supersession: crate::temporal_status::SupersessionMode,
    },
    /// Recall prior agent memory by meaning (issue #91).
    ///
    /// Returns agent-authored observations, decisions, and failures ranked by
    /// semantic similarity to the natural-language query — each carrying its
    /// provenance handle: record ID, kind, source transcript/session handle,
    /// authoring agent, confidence, observed time, and any linked code handle.
    ///
    /// Results are typed `agent_authored` and are NEVER blended with
    /// deterministic code hits (use `eg query semantic` for code). A memory hit
    /// that cannot cite where it came from is excluded, not returned. A semantic
    /// match is recall, not verification: a returned lesson is a prior agent's
    /// subjective claim, not source truth.
    ///
    /// Documented in `docs/cli/semantic-memory-recall.md`.
    #[cfg(feature = "embeddings")]
    SemanticMemory {
        /// Natural-language question to recall memory by meaning.
        query: String,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long)]
        data_dir: PathBuf,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Maximum number of results (default 10).
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Exclude unverified agent observations (no cited verification evidence).
        #[arg(long)]
        verified_only: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
        /// Supersession resolution mode for memory/observations.
        #[arg(long, value_enum, default_value_t = crate::temporal_status::SupersessionMode::Exclude)]
        supersession: crate::temporal_status::SupersessionMode,
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
        /// Working-tree path to compute store freshness against (issue #82).
        ///
        /// When set, the response carries a non-fatal top-level `freshness` code
        /// so an agent can downgrade trust in the returned handles; omitted → no
        /// freshness field. See `eg query symbol --help`.
        #[arg(long)]
        repo_path: Option<PathBuf>,
        /// Supersession resolution mode for memory/observations.
        #[arg(long, value_enum, default_value_t = crate::temporal_status::SupersessionMode::Exclude)]
        supersession: crate::temporal_status::SupersessionMode,
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
    /// Find changes and trust-separated evidence over a commit range.
    Changes {
        /// Base commit SHA or unique prefix.
        base: String,
        /// Head commit SHA or unique prefix.
        head: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict commit resolution and changed-fact selection to one repository.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Surface prior failed attempts linked to a code or task handle (issue #63).
    ///
    /// Starts from a symbol record ID / name, a repo-relative file path, or a
    /// task / source handle and returns prior FAILED attempts as citable local
    /// facts — separated into `runtime_failures` (verification-domain evidence,
    /// trust `verification_evidence`) and `agent_failures` (agent-authored
    /// `Failure` claims, trust `agent_authored`) so neither is presented as
    /// source truth. A later passing verification on the same target appears in
    /// `superseding_successes`, and each failure carries a read-time
    /// `resolution_status` (`still_failing` | `since_resolved`). Output never
    /// includes raw transcript text, command output, or patch hunks — only
    /// hashes, handles, bounded summaries, and redaction markers.
    ///
    /// "No prior failure found" is not evidence the code or task is correct.
    ///
    /// Documented in `docs/cli/failure-history.md`.
    Failures {
        /// Symbol record ID / name, repo-relative file path, or task/source handle.
        handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict symbol/file handle resolution to one repository (issue #67).
        #[arg(long)]
        repo: Option<String>,
    },
    /// Flag agent observations whose cited code has drifted since recording (issue #85).
    ///
    /// For every agent `Observation` / `Decision` citing a code handle, returns a
    /// per-evidence-link **freshness verdict**: `current`, `drifted`,
    /// `unresolved`, or `untemporal`. A `drifted` / `unresolved` verdict is a
    /// **freshness lead, never a truth claim** — it states only that the evidence
    /// basis moved, never that the observation is now false. Strictly read-only
    /// and deterministic; reuses existing drift records, content hashes, and
    /// temporal anchors. The verdict attaches to the evidence link and never
    /// rewrites, hides, or marks stale any deterministic code fact.
    ///
    /// Documented in `docs/cli/evidence-freshness.md`.
    EvidenceFreshness {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Return only stale observations (`drifted` + `unresolved`). An empty
        /// result is reported with a stable diagnostic, never silently (AC7).
        #[arg(long)]
        stale_only: bool,
    },
    /// Retrieve cross-domain context for a repo-relative directory or module prefix (issue #83).
    ///
    /// Returns a structured JSON object with six trust-separated sections:
    /// `source_facts` (code-graph files and symbols under the prefix),
    /// `observations` (agent-authored), `project_state` (tasks/ACs),
    /// `artifacts`, `verification_evidence`, and `semantic_drift`.
    /// Missing evidence links are surfaced as `unresolved` items.
    ///
    /// Both the bare form (`src/alpha`) and the trailing-slash form
    /// (`src/alpha/`) resolve to the same record set. Prefix matching is
    /// segment-aware: `src/alpha` never bleeds into `src/alphabet/`.
    ///
    /// On no-match: emits `{"ok":false,"error":{"code":"no_match",...}}` to
    /// stdout and exits 2. On malformed/empty prefix: exits 1 with
    /// `{"ok":false,"error":{"code":"malformed_prefix",...}}`.
    Subsystem {
        /// Repo-relative directory or module path prefix (e.g. `src/parser` or `src/parser/`).
        prefix: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
        /// Supersession resolution mode for memory/observations.
        #[arg(long, value_enum, default_value_t = crate::temporal_status::SupersessionMode::Exclude)]
        supersession: crate::temporal_status::SupersessionMode,
    },
    /// Surface graph-derived change-impact LEADS for a symbol or file handle (issue #76).
    ///
    /// Given a symbol record ID / exact name or a repo-relative file path,
    /// returns nearby code to inspect before editing: direct callers, direct
    /// callees, importing/referencing files, implementation-related symbols,
    /// and containing file/module context — grouped, deterministic,
    /// redaction-safe, bounded by --depth.
    ///
    /// Every row is an impact LEAD, not proof of breakage. Absence of a lead
    /// is not proof a change is safe. Results are code-graph source facts only;
    /// they are never blended with agent observations or verification verdicts.
    ///
    /// Exit codes:
    ///   0 — leads returned (or resolved target has no relationships).
    ///   1 — malformed / ambiguous / unsupported handle (machine-readable JSON on stderr).
    ///   2 — handle resolves to no live record (`no_match` or `stale_handle`).
    ///
    /// Documented in `docs/cli/change-impact.md`.
    ChangeImpact {
        /// Symbol record ID (`codegraph:vN:<hex>`), exact symbol name, or
        /// repo-relative file path (e.g. `src/lib.rs`).
        handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict symbol/file resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Neighborhood hop limit from the resolved anchor(s).
        /// Exceeding it yields a truncation diagnostic with counts rather than
        /// silently dropping relationship classes.
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Return a repository orientation map for cold-starting in an unfamiliar repository.
    Orient {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict symbol/file resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Limit the number of top most-referenced symbols returned (default: 20).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Trace a single symbol's lifecycle across Git history.
    Lifeline {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Symbol stable ID or exact symbol name.
        #[arg(index = 1)]
        symbol: String,
        /// Restrict symbol/file resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
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

/// Subcommands for `audit`.
#[derive(Debug, Subcommand)]
enum AuditSubcommand {
    /// Audit citation completeness across the public query workflows.
    ///
    /// Reads a seeded record set from a JSONL graph (`--graph`) or an embedded
    /// store (`--data-dir`), drives every public query workflow over it, and
    /// prints a deterministic per-workflow + overall citation report with a
    /// default pass/fail gate.
    ///
    /// Exit codes:
    ///   0 — gate passed (`ok: true`).
    ///   1 — gate failed (`ok: false`); the full JSON report is still printed.
    ///   2 — usage/load error (bad path, unparseable graph).
    Citations {
        /// Graph JSONL path (mutually exclusive with `--data-dir`).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` store directory (mutually exclusive with `--graph`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Minimum fraction of code-answer rows that must be cited (AC4 gate).
        #[arg(long, default_value_t = crate::citation_audit::DEFAULT_MIN_CODE_CITATION)]
        min_code_citation: f64,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Audit agent-memory composition health to flag reviewability risk (issue #94).
    ///
    /// Reads a seeded record set from a JSONL graph (`--graph`) or an embedded
    /// store (`--data-dir`), aggregates observation records, and prints a
    /// report with a default pass/fail gate.
    ///
    /// Exit codes:
    ///   0 — gate passed (`ok: true`).
    ///   1 — gate failed (`ok: false`); the report is still printed.
    ///   2 — usage/load error (bad path, unparseable graph).
    MemoryHealth {
        /// Graph JSONL path (mutually exclusive with `--data-dir`).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` store directory (mutually exclusive with `--graph`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Minimum fraction of observation records that must have provenance coverage.
        #[arg(long, default_value_t = 1.0)]
        min_provenance_coverage: f64,
        /// Maximum fraction of observation records that can have dangling evidence.
        #[arg(long, default_value_t = 0.0)]
        max_dangling_evidence: f64,
        /// Optional maximum fraction of observation records that can be unverified.
        #[arg(long)]
        max_unverified: Option<f64>,
        /// Optional maximum fraction of active observation records that can be contaminated.
        #[arg(long)]
        max_current_guidance_contamination: Option<f64>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Measure query-answer token cost against the ripgrep baseline (issue #84).
    ///
    /// Reads a pinned corpus manifest, scans the corpus into an in-memory graph,
    /// and reports, per question class and in aggregate, the baseline-to-Egregore
    /// token-savings ratio with raw counts. Local-first and offline; the grep
    /// baseline is computed in-process (no ripgrep dependency, no network).
    ///
    /// Exit codes:
    ///   0 — gate passed (`ok: true`).
    ///   1 — gate failed (`ok: false`); the full JSON report is still printed.
    ///   2 — usage/load error (bad manifest path, unparseable corpus, scan error).
    TokenCost {
        /// Path to the token-cost corpus manifest JSON.
        #[arg(long, default_value = "corpus/token_cost_corpus.json")]
        corpus: PathBuf,
        /// Minimum baseline-to-Egregore savings ratio each class must meet.
        ///
        /// Defaults to the manifest's `min_ratio`. When supplied, overrides it.
        #[arg(long)]
        min_ratio: Option<f64>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Measure code-graph extraction accuracy against a ground-truth labeled corpus (issue #93).
    Accuracy {
        /// Path to the Rust test corpus directory.
        #[arg(long, default_value = "corpus/accuracy")]
        corpus_dir: PathBuf,
        /// Path to the expected labels JSON file.
        #[arg(long, default_value = "corpus/accuracy_labels.json")]
        labels: PathBuf,
        /// Line tolerance for span matching.
        #[arg(long, default_value_t = 0)]
        span_line_tolerance: usize,
        /// Target precision threshold (default: overrides from labels JSON).
        #[arg(long)]
        min_precision: Option<f64>,
        /// Target recall threshold (default: overrides from labels JSON).
        #[arg(long)]
        min_recall: Option<f64>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
}

/// Subcommands for `bundle`.
#[derive(Debug, Subcommand)]
enum BundleSubcommand {
    /// Export an evidence bundle for a selected selector.
    Export {
        /// The starting selector (e.g. `symbol:my_func`, `file:src/lib.rs`, `id:<record_id>`).
        #[arg(long)]
        root_selector: String,
        /// Path to the graph JSONL file (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long, conflicts_with = "graph")]
        data_dir: Option<PathBuf>,
        /// Output path for the generated bundle JSON file.
        #[arg(long)]
        out: PathBuf,
    },
    /// Verify an exported evidence bundle's integrity, coverage, and safety.
    Verify {
        /// Path to the bundle JSON file to verify.
        path: PathBuf,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Inspect and print the manifest of an evidence bundle.
    Inspect {
        /// Path to the bundle JSON file to inspect.
        path: PathBuf,
    },
}

/// Subcommands for `protected`.
#[derive(Debug, Subcommand)]
enum ProtectedSubcommand {
    /// Capture raw payloads listed in a manifest JSONL into the protected store.
    ///
    /// Without `--protected-raw-artifacts`: preview mode — computes and prints
    /// content hashes and handles but writes **nothing** to disk.
    ///
    /// With `--protected-raw-artifacts`: enabled mode — stores blobs, updates the
    /// manifest, and records the producer as an authorised operator.
    ///
    /// Exit codes:
    ///   0 — capture complete (or preview complete in disabled mode).
    ///   1 — manifest file I/O or parse error.
    Capture {
        /// Path to the capture manifest JSONL (one `{class,source_path}` per line).
        #[arg(long)]
        manifest: PathBuf,
        /// Protected store directory.
        #[arg(long)]
        store: PathBuf,
        /// Enable protected-raw-artifact mode; without this flag no bytes are stored.
        #[arg(long)]
        protected_raw_artifacts: bool,
        /// Stable producer identity (required when `--protected-raw-artifacts` is set).
        #[arg(long)]
        producer: Option<String>,
        /// Override capture timestamp (RFC 3339) for deterministic tests.
        #[arg(long)]
        captured_at: Option<String>,
    },
    /// Retrieve raw bytes for a protected handle, verifying the content hash.
    ///
    /// Exit codes:
    ///   0 — bytes written (to `--out` or stdout).
    ///   1 — store absent / operator unauthorized / malformed handle / blob missing
    ///       / hash mismatch.
    ///   2 — handle not found in manifest.
    Get {
        /// Protected handle string (`protected:v1:<hex>`).
        handle: String,
        /// Protected store directory.
        #[arg(long)]
        store: PathBuf,
        /// Operator identity (must be in the store's authorised set).
        #[arg(long)]
        operator: String,
        /// Write raw bytes to this path instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// List all protected handles (metadata only — no raw bytes).
    ///
    /// Exit codes:
    ///   0 — list emitted (may be empty when store is not yet initialised).
    ///   1 — manifest I/O or parse error.
    List {
        /// Protected store directory.
        #[arg(long)]
        store: PathBuf,
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
        Commands::Freshness {
            repo_path,
            graph,
            data_dir,
            repo_id_override,
            format,
        } => freshness_cmd(
            &repo_path,
            graph.as_deref(),
            data_dir.as_deref(),
            repo_id_override.as_deref(),
            format,
        ),
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
        Commands::ImportAntigravity {
            antigravity_path,
            out,
        } => import_antigravity_cmd(&antigravity_path, &out),
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
        Commands::Import { source } => match *source {
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
        Commands::Query { subcommand } => query_cmd(*subcommand),
        Commands::Write { kind } => write_evidence(*kind),
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
        #[cfg(feature = "embeddings")]
        Commands::EvalMemoryRecall {
            corpus,
            data_dir,
            top_k,
            threshold,
            verified_only,
        } => eval_memory_recall_cmd(&corpus, &data_dir, top_k, threshold, verified_only),
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
        Commands::Protected { subcommand } => protected_cmd(subcommand),
        Commands::Bundle { subcommand } => bundle_cmd(subcommand),
        Commands::Audit { subcommand } => audit_cmd(subcommand),
        Commands::Doctor {
            path,
            out,
            data_dir,
            require_history,
            network,
            format,
        } => doctor_cmd(path, out, data_dir, require_history, network, format),
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::Refresh {
            repo_path,
            data_dir,
            cache,
            format,
            #[cfg(feature = "embeddings")]
            embed,
        } => scan_refresh_cmd(
            &repo_path,
            &data_dir,
            cache.as_deref(),
            format,
            #[cfg(feature = "embeddings")]
            embed,
        ),
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::Watch {
            data_dir,
            antigravity_dir,
            codex_dir,
            claude_dir,
            poll_interval,
            #[cfg(feature = "embeddings")]
            embed,
        } => watch_cmd(
            &data_dir,
            antigravity_dir.as_deref(),
            codex_dir.as_deref(),
            claude_dir.as_deref(),
            poll_interval,
            #[cfg(feature = "embeddings")]
            embed,
            #[cfg(not(feature = "embeddings"))]
            false,
        ),
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

fn import_antigravity_cmd(antigravity_path: &Path, out: &Path) -> Result<()> {
    let opts = crate::antigravity::ImportOptions::default();
    let graph =
        crate::antigravity::import_antigravity(antigravity_path, &opts).with_context(|| {
            format!(
                "failed to import Antigravity transcript from {}",
                antigravity_path.display()
            )
        })?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize agent-memory JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    println!(
        "imported {} records from {}",
        graph.records().len(),
        antigravity_path.display()
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
    // Exclude the graph output and any in-tree egregore store from the dirty probe
    // (PR #186 E/FF1): a pre-existing graph.jsonl or .egregore data-dir from a
    // previous workflow must not stamp `dirty = true` on the new scan output.
    let exclusions = store_exclusions_including_egregore(repo_path, &[Some(out)]);
    let graph = scan_repository_with_exclusions(repo_path, repo_id_override, &exclusions)
        .with_context(|| format!("failed to scan repository {}", repo_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write graph JSONL to {}", out.display()))?;
    Ok(())
}

fn scan_history(repo_path: &Path, out: &Path, repo_id_override: Option<&str>) -> Result<()> {
    // AC5: Verify git is available in PATH.
    let git_available = std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !git_available {
        let diag = serde_json::json!({
            "code": "git_unavailable",
            "message": "git command not found in PATH"
        });
        eprintln!("{}", serde_json::to_string(&diag).unwrap_or_default());
        std::process::exit(2);
    }

    // AC5: Verify the path is a git repository.
    let is_git_repo = std::process::Command::new("git")
        .args(["-c", "core.excludesFile="])
        .arg("-C")
        .arg(repo_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["rev-parse", "--git-dir"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !is_git_repo {
        let diag = serde_json::json!({
            "code": "not_a_git_repository",
            "message": format!("path is not a git repository: {}", repo_path.display())
        });
        eprintln!("{}", serde_json::to_string(&diag).unwrap_or_default());
        std::process::exit(2);
    }

    // AC5: Verify the git history is readable (has at least one commit).
    let git_history_readable = std::process::Command::new("git")
        .args(["-c", "core.excludesFile="])
        .arg("-C")
        .arg(repo_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["log", "-1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !git_history_readable {
        let diag = serde_json::json!({
            "code": "git_history_unreadable",
            "message": "git history is not readable (e.g. repository has no commits)"
        });
        eprintln!("{}", serde_json::to_string(&diag).unwrap_or_default());
        std::process::exit(2);
    }

    // History replay reads only committed Git objects, so the stamped snapshot is
    // always `dirty=false` (committed HEAD state); a pre-existing in-tree output or
    // companion store cannot affect it, and no dirty-probe exclusions are needed
    // (TT1 supersedes the earlier CC1/GG1 exclusion machinery).
    let graph = scan_repository_history_with_override(repo_path, repo_id_override)
        .with_context(|| format!("failed to scan Git history for {}", repo_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize history graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write history graph JSONL to {}", out.display()))?;
    Ok(())
}

/// Machine-readable report emitted by `eg freshness`.
///
/// `freshness` carries the stable code (`fresh` / `stale_head` / `stale_dirty` /
/// `unknown`); `current_head` and `stored_snapshot` reuse the on-disk snapshot
/// serialization so the report is self-describing.
#[derive(Debug, Serialize)]
struct FreshnessReport {
    /// Stable freshness code.
    freshness: String,
    /// Convenience boolean: `true` only when `freshness == "fresh"`.
    fresh: bool,
    /// Stable `Repository` record ID the freshness was computed for.
    repository_id: String,
    /// Where the store was read from: `"graph"` or `"data_dir"`.
    store_kind: String,
    /// Current working-tree HEAD state.
    current_head: SnapshotHead,
    /// Current working-tree dirty flag.
    current_dirty: bool,
    /// The snapshot the store was built from; absent for pre-stamping stores.
    #[serde(skip_serializing_if = "Option::is_none")]
    stored_snapshot: Option<crate::ir::SourceSnapshotPayload>,
    /// Human-oriented one-line explanation of the verdict.
    message: String,
}

/// Renders a [`SnapshotHead`] for human-readable output.
fn head_display(head: &SnapshotHead) -> String {
    match head {
        SnapshotHead::Commit { sha } => format!("commit {sha}"),
        SnapshotHead::NoGit => "no_git".to_owned(),
        SnapshotHead::UnbornHead => "unborn_head".to_owned(),
    }
}

/// Builds the human-oriented explanation for a freshness verdict.
fn freshness_message(verdict: Freshness) -> String {
    match verdict {
        Freshness::Fresh => {
            "store matches the current working tree (HEAD unchanged, tree clean)".to_owned()
        }
        Freshness::StaleHead => {
            "current HEAD differs from the stored snapshot; queried file/span handles may be \
             invalid — re-scan before citing them"
                .to_owned()
        }
        Freshness::StaleDirty => {
            "working tree has uncommitted changes relative to the stored snapshot; queried \
             file/span handles may be invalid — re-scan before citing them"
                .to_owned()
        }
        Freshness::Unknown => {
            "store predates snapshot stamping or no Git context exists; freshness cannot be \
             determined"
                .to_owned()
        }
    }
}

/// Handles `eg freshness [repo_path] (--graph <p> | --data-dir <d>) [--format ...]`.
///
/// Strictly read-only (issue #82 AC4): loads the store through the same
/// read-only path queries use, probes the working tree with `git rev-parse` /
/// `git status`, and never writes anything. Always returns `Ok(())` once a
/// verdict is produced; the verdict (including `unknown`) is the payload, not an
/// error.
fn freshness_cmd(
    repo_path: &Path,
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    repo_id_override: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let store_kind = if graph.is_some() { "graph" } else { "data_dir" };
    let identity = identity::compute_repository_identity(repo_path, repo_id_override);
    // Exclude both known store artifacts plus any in-tree `.egregore` store from
    // the dirty probe (PR #186 E/F/FF1): when checking `--graph`, the companion
    // `.egregore` data-dir created by the documented ingest workflow sits untracked
    // (and vice versa for `--data-dir` + `graph.jsonl`). Mirroring `scan`'s
    // store-artifact exclusions keeps a just-written store from reading as
    // `stale_dirty` before the user gitignores or deletes the intermediate output.
    let exclusions = store_exclusions_including_egregore(repo_path, &[graph, data_dir]);
    let (current_head, current_dirty) =
        identity::working_tree_snapshot_excluding(repo_path, &exclusions);

    // AC4: strictly read-only. A `--graph` JSONL is read directly (a plain file
    // read). A `--data-dir` embedded store is read through a throwaway copy,
    // because the embedded engine re-persists its on-disk index files on open;
    // operating on a copy guarantees the live store's records, indexes, runtime
    // files, and receipts are never created, modified, or deleted.
    let records = match data_dir {
        Some(dir) => load_records_from_data_dir_readonly(dir)?,
        None => load_query_records(graph, None)?,
    };
    // An explicit `--repo-id-override` pins the identity used to locate the stored
    // snapshot, so it must match exactly: a wrong/typo'd override must not borrow an
    // unrelated sole repository's snapshot via the single-repository fallback (which
    // could even report `fresh` under the caller's unmatched ID). Without an
    // override, the auto-detected identity keeps that fallback so legacy single-repo
    // stores still classify (PR #186 follow-up YY1).
    //
    // Report the repository that actually OWNS the matched snapshot, not the
    // recomputed checkout identity: a store scanned with `--repo-id-override` and
    // checked without it classifies the sole repository via the fallback, and the
    // JSON `repository_id` must be that stored Repository's ID so consumers keying
    // the verdict by repository are not misled (PR #186 follow-up ZZ1).
    let (report_repository_id, stored) = if repo_id_override.is_some() {
        // Exact match required; when found, the owner is the requested identity.
        (
            identity.id.clone(),
            freshness::stored_snapshot_exact(&records, &identity.id),
        )
    } else {
        // Mirror the per-row query freshness path (Z1): when the identity probe
        // misses, fall back to the sole STAMPED repository so a combined store with
        // exactly one stamped Repository (e.g. an override-scanned repo alongside a
        // legacy unstamped node) classifies unambiguously instead of reporting
        // `unknown` — and `eg freshness` agrees with `eg query ... --repo-path` on
        // the same store (PR #186 follow-up DDD1).
        match freshness::stored_snapshot_with_owner(&records, &identity.id)
            .or_else(|| freshness::stored_snapshot_sole_stamped(&records))
        {
            Some((owner, snapshot)) => (owner.to_owned(), Some(snapshot)),
            None => (identity.id.clone(), None),
        }
    };
    let mut verdict = freshness::classify(stored, &current_head, current_dirty);
    // A `fresh` verdict still misses a previously scanned source that a
    // sparse-checkout cone change removed: such a file is `skip-worktree` + absent,
    // so `git status` stays blind to it. Downgrade to `stale_dirty` when the store
    // cites such a removed path (FFF1). Only `fresh` is overridden: a `stale_head`
    // store already requires a re-scan.
    if verdict.is_fresh() {
        let removed = identity::index_hidden_absent_source_inputs(repo_path);
        let index = query::RepositoryIndex::build(&records);
        if cited_source_stale_on_disk(&records, &index, &report_repository_id, repo_path, &removed)
        {
            verdict = Freshness::StaleDirty;
        }
    }

    let report = FreshnessReport {
        freshness: verdict.code().to_owned(),
        fresh: verdict.is_fresh(),
        repository_id: report_repository_id,
        store_kind: store_kind.to_owned(),
        current_head,
        current_dirty,
        stored_snapshot: stored.cloned(),
        message: freshness_message(verdict),
    };

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&report).context("failed to serialize freshness report")?
            );
        }
        OutputFormat::Text => {
            println!("freshness: {}", report.freshness);
            println!("repository_id: {}", report.repository_id);
            println!("store: {store_kind}");
            println!(
                "current_head: {} (dirty: {})",
                head_display(&report.current_head),
                report.current_dirty
            );
            match &report.stored_snapshot {
                Some(snapshot) => println!(
                    "stored_head: {} (dirty: {})",
                    head_display(&snapshot.head),
                    snapshot.dirty
                ),
                None => println!("stored_head: (none — store predates snapshot stamping)"),
            }
            println!("message: {}", report.message);
        }
    }
    Ok(())
}

/// Computes the store-freshness code for a query against `repo_path` (issue #82).
///
/// Returns `None` when `repo_path` is absent, so freshness-unaware queries emit
/// byte-identical output to before this feature. When present, returns the stable
/// freshness code (including `"fresh"`) so an agent always sees the signal it asked
/// for and the result is never silently suppressed.
///
/// `repo_id_hint` is an optional known repository ID from the already-resolved
/// `--repo` scope or context owner; when provided and distinct from the
/// auto-detected identity it is tried first so that operator-override IDs win in
/// a multi-repo store, and used as the owner ID when no snapshot is found so that
/// `stamp_freshness` can match selected rows (PR #186 follow-up).
fn query_freshness_code_with_hint(
    records: &[GraphRecord],
    repo_path: Option<&Path>,
    artifacts: &[Option<&Path>],
    repo_id_hint: Option<&str>,
) -> Option<(String, &'static str)> {
    query_freshness_code_inner(records, repo_path, artifacts, repo_id_hint)
}

fn query_freshness_code_inner(
    records: &[GraphRecord],
    repo_path: Option<&Path>,
    artifacts: &[Option<&Path>],
    repo_id_hint: Option<&str>,
) -> Option<(String, &'static str)> {
    let repo_path = repo_path?;
    let identity = identity::compute_repository_identity(repo_path, None);
    // Mirror `freshness_cmd`/`scan` and also exclude any in-tree `.egregore*`
    // companion store (PR #186 follow-up II1): the documented workflow leaves an
    // untracked `.egregore` data-dir beside the graph, which must not stamp query
    // rows `stale_dirty` when the graph itself was scanned from a clean tree.
    let exclusions = store_exclusions_including_egregore(repo_path, artifacts);
    let (head, dirty) = identity::working_tree_snapshot_excluding(repo_path, &exclusions);
    // Any explicit hint (a resolved `--repo` scope or context owner) is
    // authoritative: the verdict must be owned by the selected repository, even
    // when the hint equals the auto-detected identity. Look up ONLY its snapshot —
    // never fall back to the auto-detected identity's snapshot or the sole-stamped
    // repo, which may belong to a different repository and would mislabel the
    // verdict's owner. When the selected repo has no snapshot (legacy/pre-stamping
    // rows in a combined store), `matched` stays `None` and the owner below is
    // still the hint, so `stamp_freshness` stamps `unknown` on the selected rows
    // rather than omitting the field (PR #186 follow-up SS1/UU1).
    //
    // Only when NO hint is given does the identity probe run with a sole-stamped
    // fallback: if exactly one Repository node in the store carries a snapshot,
    // that snapshot is unambiguous and should be used. This handles combined
    // stores where --repo-id-override was used on the scanned checkout but no
    // --repo flag was passed to the query command (PR #186 follow-up Z1).
    let matched = match repo_id_hint {
        Some(h) => freshness::stored_snapshot_with_owner(records, h),
        None => freshness::stored_snapshot_with_owner(records, &identity.id)
            .or_else(|| freshness::stored_snapshot_sole_stamped(records)),
    };
    // When no snapshot is found but the caller supplied an explicit hint (from
    // `--repo`), use the hint as the owner ID so `stamp_freshness` can match
    // the selected rows.  Falling back to `identity.id` would emit the unknown
    // verdict under the wrong owner, making freshness invisible on those rows.
    let (owner_id, stored) = match matched {
        Some((owner, snapshot)) => (owner.to_owned(), Some(snapshot)),
        None => (
            repo_id_hint.map(ToOwned::to_owned).unwrap_or(identity.id),
            None,
        ),
    };
    let code = freshness::classify(stored, &head, dirty).code();
    // Downgrade `fresh` to `stale_dirty` when a previously scanned source owned by
    // this repository was removed by a sparse-checkout cone change (skip-worktree +
    // absent, invisible to `git status`) but the store still cites it (FFF1),
    // matching `freshness_cmd`.
    let code = if code == "fresh" {
        let removed = identity::index_hidden_absent_source_inputs(repo_path);
        let index = query::RepositoryIndex::build(records);
        if cited_source_stale_on_disk(records, &index, &owner_id, repo_path, &removed) {
            "stale_dirty"
        } else {
            code
        }
    } else {
        code
    };
    Some((owner_id, code))
}

/// Computes repo-relative dirty-probe exclusions for the store artifact being
/// read (issue #82 / PR #186).
///
/// When the `--graph` file or `--data-dir` directory lives under `repo_path`, it
/// is returned as a repo-relative pathspec so the freshness dirty probe ignores
/// it — an in-tree store the workflow just wrote must not by itself make the tree
/// look `stale_dirty`. Returns empty when there is no artifact or it lives
/// outside the working tree.
fn store_artifact_exclusions(repo_path: &Path, artifacts: &[Option<&Path>]) -> Vec<String> {
    let repo_abs = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    artifacts
        .iter()
        .filter_map(|a| *a)
        .filter_map(|artifact| {
            let abs = fs::canonicalize(artifact).unwrap_or_else(|_| artifact.to_path_buf());
            let rel = abs.strip_prefix(&repo_abs).ok()?;
            let s = rel.to_string_lossy().replace('\\', "/");
            (!s.is_empty()).then_some(s)
        })
        .collect()
}

/// Discovers untracked in-tree `.egregore*` embedded-store directories that must
/// be excluded from the dirty probe (PR #186 follow-up FF1/GG1).
///
/// A directory qualifies only when it is fully untracked (`git ls-files` reports
/// no content under it) and contains no `.rs` sources — the hallmark of a store
/// output (`eg ingest ... --data-dir .egregore`) rather than a source directory
/// that merely shares the prefix. The scanner never indexes such a store, so the
/// freshness dirty probe must not count it as source dirtiness.
fn egregore_store_dirs(repo_path: &Path) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    let Ok(entries) = std::fs::read_dir(repo_path) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let name_matches = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(".egregore"));
        // Only directories, not regular files such as `.egregore.rs`.
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !(name_matches && is_dir) {
            continue;
        }
        let path = entry.path();
        // `git ls-files` returns tracked paths under the directory; an empty
        // result means the entire subtree is untracked / gitignored, which is the
        // hallmark of a store output rather than a source directory. Use the entry
        // name directly so `git ls-files` receives a repo-relative path regardless
        // of whether `repo_path` is absolute or relative.
        let name = entry.file_name();
        let has_tracked = std::process::Command::new("git")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .current_dir(repo_path)
            .args(["ls-files", "--", name.to_str().unwrap_or("")])
            .output()
            .is_ok_and(|out| !out.stdout.is_empty());
        // Even when no content is tracked, an untracked directory containing `.rs`
        // files is a source directory, not a store output: its files appear in the
        // graph but are outside `git status`, so excluding it would mask deletions.
        if !has_tracked && !dir_has_sources(&path) {
            dirs.push(path);
        }
    }
    dirs
}

/// Builds dirty-probe exclusions for the explicit store `artifacts` plus any
/// in-tree `.egregore*` embedded-store directories discovered under `repo_path`
/// (PR #186 follow-up FF1/GG1).
///
/// All three store-producing/checking entry points (`scan`, `scan-history`,
/// `freshness`) share this so a companion store written by one workflow never
/// makes another's output read `stale_dirty`.
fn store_exclusions_including_egregore(
    repo_path: &Path,
    artifacts: &[Option<&Path>],
) -> Vec<String> {
    let egregore_dirs = egregore_store_dirs(repo_path);
    let mut all: Vec<Option<&Path>> = artifacts.to_vec();
    for dir in &egregore_dirs {
        all.push(Some(dir.as_path()));
    }
    store_artifact_exclusions(repo_path, &all)
}

/// Returns `true` if `dir` or any subdirectory contains a supported source file.
///
/// Used in the `.egregore*` auto-exclusion check: an untracked directory whose
/// subtree contains source files is a source directory, not a store output, and
/// must not be excluded from the snapshot dirty probe.
fn dir_has_sources(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if dir_has_sources(&path) {
                return true;
            }
        } else if crate::languages::is_supported_source(&path) {
            return true;
        }
    }
    false
}

/// Returns `true` when an ancestor directory of `repo_path/rel` (below the repo
/// root) contains a nested `.git` sentinel, so the scanner would no longer reach
/// `rel`.
///
/// Walks the parents of the cited file up to — but not including — `repo_path`, so
/// the repository's own `.git` never counts. A submodule/worktree (`.git` file) or
/// nested clone (`.git` directory) appearing over a previously scanned tree makes
/// `fs::should_descend` skip it, yet `git status` cannot see that conversion
/// (GGG3 / PR #186 follow-up).
fn path_behind_nested_git(repo_path: &Path, rel: &str) -> bool {
    let full = repo_path.join(rel);
    let mut dir = full.parent();
    while let Some(d) = dir {
        if d == repo_path || !d.starts_with(repo_path) {
            break;
        }
        if d.join(".git").exists() {
            return true;
        }
        dir = d.parent();
    }
    false
}

/// Returns `true` when the store cites a `File` (owned by `owner_id`) that the
/// working tree no longer makes available to the scanner — a source the graph
/// indexed but that `git status` cannot flag.
///
/// Two cases, both keyed off the store's actual contents (so a path matters only
/// when cited — distinguishing a change to a *previously scanned* file from one
/// that was never indexed, which the pure working-tree probe cannot tell apart):
/// - `removed`: index-hidden (`skip-worktree`/`assume-unchanged`) yet absent
///   paths from [`identity::index_hidden_absent_source_inputs`] — a sparse-checkout
///   cone change removed a scanned file (FFF1) vs. a baseline omission (AAA1);
/// - a cited file now sitting behind a nested `.git` sentinel (GGG3).
fn cited_source_stale_on_disk(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    owner_id: &str,
    repo_path: &Path,
    removed: &[String],
) -> bool {
    let removed: std::collections::HashSet<&str> = removed.iter().map(String::as_str).collect();
    records.iter().any(|record| {
        matches!(
            record,
            GraphRecord::Node {
                kind: NodeKind::File,
                id,
                repo_relative_path: Some(path),
                ..
            } if index.owner_of(id) == Some(owner_id)
                && (removed.contains(path.as_str()) || path_behind_nested_git(repo_path, path))
        )
    })
}

/// Stamps the freshness `code` on each result whose repository matches the
/// checkout the code was computed for (issue #82). Rows owned by a different
/// repository (multi-repo stores) are left unstamped rather than mislabeled.
///
/// History (`scan-history`) source rows are attributed to their repository the
/// same way `scan` rows are: replay emits `Repository CONTAINS File` and
/// `File DEFINES Symbol` edges per commit, so `RepositoryIndex::owner_of`
/// resolves them and the verdict attaches via the normal ownership match
/// (verified by `query_symbol_repo_path_stamps_freshness_on_history_graph`).
fn stamp_freshness(results: &mut [SymbolResult<'_>], freshness: Option<&(String, &'static str)>) {
    let Some((repo_id, code)) = freshness else {
        return;
    };
    for result in results.iter_mut() {
        if result.repository_id == Some(repo_id.as_str()) {
            result.freshness = Some(code);
        }
    }
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

/// Machine-readable report emitted by `eg refresh`.
///
/// All counts are integers; file lists are sorted repository-relative paths.
/// `freshness_after_refresh` reports the verdict a follow-up `eg freshness`
/// would give (the write-side counterpart to the read-only signal in issue #82).
#[cfg(feature = "embedded-aletheiadb")]
#[derive(Debug, Serialize)]
struct RefreshReport {
    /// Repository-relative paths of files that were re-extracted in this refresh.
    rebuilt_files: Vec<String>,
    /// Number of rebuilt files.
    rebuilt_count: usize,
    /// Repository-relative paths of files whose cached records were reused unchanged.
    reused_files: Vec<String>,
    /// Number of reused files.
    reused_count: usize,
    /// Repository-relative paths of files that were tombstoned because they no longer exist.
    tombstoned_files: Vec<String>,
    /// Number of tombstoned files.
    tombstoned_count: usize,
    /// Total records submitted to the ingest adapter.
    ingest_attempted: usize,
    /// Records successfully written.
    ingest_succeeded: usize,
    /// Records that failed to write.
    ingest_failed: usize,
    /// Semantic embedding state after this refresh.
    ///
    /// `"not_requested"` — `--embed` was not passed; structural records are current
    /// but any prior semantic embeddings for rebuilt/tombstoned nodes may be stale.
    /// Re-run `eg ingest --adapter embedded --embed` to rebuild the full semantic index.
    ///
    /// `"refreshed"` — `--embed` was passed; embeddings for all changed nodes were
    /// regenerated as part of this refresh.
    embed_status: String,
    /// Freshness of the store with respect to the working tree after this refresh.
    ///
    /// The verdict `eg freshness --data-dir` would report for the rebuilt store:
    /// `"fresh"` for a clean tree at the stamped HEAD, or `"stale_dirty"` when the
    /// refresh captured uncommitted `.rs` edits (the store reflects an uncommitted
    /// state). This is the write counterpart to the read-only staleness signal
    /// (issue #82) and stays consistent with a follow-up freshness check (OO1).
    freshness_after_refresh: String,
}

/// Handles `eg refresh <repo_path> --data-dir <dir> [--cache <path>] [--format json|text]`.
///
/// Performs an incremental scan (BLAKE3 file-hash cache) and ingests only the
/// changed/added/removed records into the embedded store.  Non-codegraph records
/// (agent-memory, project, artifact, verification) are never touched.
#[cfg(feature = "embedded-aletheiadb")]
#[allow(clippy::too_many_lines)]
fn scan_refresh_cmd(
    repo_path: &Path,
    data_dir: &Path,
    cache: Option<&Path>,
    format: OutputFormat,
    #[cfg(feature = "embeddings")] embed: bool,
) -> Result<()> {
    // AC9: The embedded store must already exist before we can refresh it.
    if !data_dir.exists() {
        eprintln!(
            r#"{{"code":"no_prior_scan","message":"embedded store not found at {}; run `eg scan <repo> --out g.jsonl && eg ingest g.jsonl --adapter embedded --data-dir {}` first"}}"#,
            data_dir.display(),
            data_dir.display()
        );
        process::exit(2);
    }

    // Derive effective cache path: defaults to <data_dir>/codegraph-cache.json.
    let default_cache = data_dir.join("codegraph-cache.json");
    let cache_path = cache.unwrap_or(&default_cache);

    // AC9: If the cache already has a repository_id, it must match the current
    // repository — otherwise the cache was built for a different repo and a full
    // rebuild is required.
    if cache_path.exists() {
        let cache_raw = fs::read_to_string(cache_path)
            .with_context(|| format!("failed to read cache {}", cache_path.display()))?;
        if let Ok(cache_json) = serde_json::from_str::<serde_json::Value>(&cache_raw)
            && let Some(cached_repo_id) = cache_json
                .get("repository_id")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
        {
            let current_identity = crate::identity::compute_repository_identity(repo_path, None);
            if current_identity.id != cached_repo_id {
                eprintln!(
                    r#"{{"code":"repository_identity_mismatch","cached_id":"{}","current_id":"{}","message":"cache at {} was built for a different repository; delete it and re-run from `eg scan`"}}"#,
                    cached_repo_id,
                    current_identity.id,
                    cache_path.display()
                );
                process::exit(2);
            }
        }
    }

    // Perform the incremental scan (reads cache, hashes files, rebuilds changed ones).
    // Exclude the data-dir, the cache file, and any in-tree `.egregore*` companion
    // store from the dirty probe (PR #186 A/MM1): all are refresh/store artifacts;
    // counting any as dirty would stamp `dirty = true` on the snapshot and make a
    // follow-up `eg freshness --data-dir` report `stale_dirty` with no source change.
    let snapshot_exclusions =
        store_exclusions_including_egregore(repo_path, &[Some(data_dir), Some(cache_path)]);
    let scan = scan_repository_incremental_excluding(repo_path, cache_path, &snapshot_exclusions)
        .with_context(|| format!("failed to scan repository {}", repo_path.display()))?;

    let records = scan.graph.records().to_vec();

    // Open the embedded store and ingest the incremental graph.
    #[cfg(feature = "embeddings")]
    let mut sink = if embed {
        let (vectors, dimensions) = generate_embeddings(&records)?;
        EmbeddedAletheiaSink::open_with_embeddings(data_dir, vectors, dimensions)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?
    } else {
        EmbeddedAletheiaSink::open(data_dir)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?
    };
    #[cfg(not(feature = "embeddings"))]
    let mut sink = EmbeddedAletheiaSink::open(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let ingest_report = ingest_records(&records, &mut sink);
    if !ingest_report.is_success() {
        // Delete the cache so the next run does a clean rebuild; avoids a cache-ahead-of-store skew
        // where the cache reflects file hashes the store never ingested.
        let _ = fs::remove_file(cache_path);
        for failure in &ingest_report.failures {
            eprintln!("{}: {}", failure.record_id, failure.message);
        }
        anyhow::bail!("refresh failed for {} records", ingest_report.failed);
    }
    sink.persist_indexes()
        .with_context(|| format!("failed to persist embedded store {}", data_dir.display()))?;

    // Determine semantic embedding state for the report (AC8).
    #[cfg(feature = "embeddings")]
    let embed_status = if embed {
        "refreshed".to_owned()
    } else {
        "not_requested".to_owned()
    };
    #[cfg(not(feature = "embeddings"))]
    let embed_status = "not_requested".to_owned();

    let rebuilt_files = scan.rebuilt_files;
    let reused_files = scan.reused_files;
    let tombstoned_files = scan.tombstoned_files;
    let rebuilt_count = rebuilt_files.len();
    let reused_count = reused_files.len();
    let tombstoned_count = tombstoned_files.len();

    // Report the verdict `eg freshness --data-dir` would compute, not an
    // unconditional "fresh" (OO1 / PR #186 follow-up). When the working tree had
    // uncommitted `.rs` edits the refreshed snapshot is stamped `dirty`, so the
    // store is `stale_dirty` even immediately after rebuild — exactly as a full
    // scan of a dirty tree behaves. The snapshot was just computed from the
    // current tree, so classifying it against itself yields the same verdict a
    // follow-up freshness check would, without re-probing Git.
    let refresh_identity = identity::compute_repository_identity(repo_path, None);
    let freshness_after_refresh = freshness::stored_snapshot(&records, &refresh_identity.id)
        .map_or(Freshness::Unknown, |snapshot| {
            freshness::classify(Some(snapshot), &snapshot.head, snapshot.dirty)
        })
        .code()
        .to_owned();

    let refresh_report = RefreshReport {
        rebuilt_files,
        rebuilt_count,
        reused_files,
        reused_count,
        tombstoned_files,
        tombstoned_count,
        ingest_attempted: ingest_report.attempted,
        ingest_succeeded: ingest_report.succeeded,
        ingest_failed: ingest_report.failed,
        embed_status,
        freshness_after_refresh,
    };

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&refresh_report)
                    .context("failed to serialize refresh report")?
            );
        }
        OutputFormat::Text => {
            println!("rebuilt: {}", refresh_report.rebuilt_count);
            println!("reused: {}", refresh_report.reused_count);
            println!("tombstoned: {}", refresh_report.tombstoned_count);
            println!("attempted: {}", refresh_report.ingest_attempted);
            println!("succeeded: {}", refresh_report.ingest_succeeded);
            println!("failed: {}", refresh_report.ingest_failed);
            println!("embed_status: {}", refresh_report.embed_status);
            println!(
                "freshness_after_refresh: {}",
                refresh_report.freshness_after_refresh
            );
            for f in &refresh_report.rebuilt_files {
                println!("rebuilt_file: {f}");
            }
            for f in &refresh_report.reused_files {
                println!("reused_file: {f}");
            }
            for f in &refresh_report.tombstoned_files {
                println!("tombstoned_file: {f}");
            }
        }
    }

    Ok(())
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

/// Handles `eg doctor`: gather environment observations, build the preflight
/// report, print it, and exit with the appropriate code.
///
/// Exits 0 when structural checks all pass; exits 1 otherwise.
/// Optional/semantic failures never change the exit code.
fn doctor_cmd(
    path: PathBuf,
    out: PathBuf,
    data_dir: PathBuf,
    require_history: bool,
    network: bool,
    format: OutputFormat,
) -> Result<()> {
    use crate::preflight::{DoctorConfig, build_report, gather_observations, render_doctor_text};

    let config = DoctorConfig {
        repo_path: path,
        out,
        data_dir,
        require_history,
        network,
    };
    let obs = gather_observations(&config);
    let report = build_report(&config, &obs);

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        OutputFormat::Text => {
            print!("{}", render_doctor_text(&report));
        }
    }

    if !report.structural_ready {
        process::exit(1);
    }
    Ok(())
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

#[derive(Serialize, Clone, Eq, PartialEq, Debug)]
struct DiagnosticRef<'a> {
    record_id: &'a str,
    repo_relative_path: &'a str,
    span: SourceSpan,
}

#[derive(Serialize)]
struct SymbolResult<'a> {
    record_id: &'a str,
    schema_version: u32,
    name: &'a str,
    kind: &'static str,
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    /// Declaration visibility class (`public` / `crate` / `restricted` /
    /// `private`) — present on symbols extracted with issue #124 metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    visibility: Option<&'a str>,
    /// Normalized declaration header (body excluded) — present on symbols
    /// extracted with issue #124 metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<&'a str>,
    /// Redacted doc-comment text; omitted when the item has no doc comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    doc: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    /// Stable `Repository` record ID owning this row; absent when the store
    /// carries no repository topology for the record (legacy graphs).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    /// Human-usable repository identity handle (e.g. `owner/name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    /// Non-fatal store-freshness code relative to a working tree (issue #82).
    ///
    /// Present only when `--repo-path` was supplied so an agent can downgrade
    /// trust in the cited `repo_relative_path` + `span` handle. Absent (and the
    /// result never suppressed) when freshness was not requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<&'static str>,
    extraction_completeness: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<Vec<DiagnosticRef<'a>>>,
}

fn get_file_diagnostics<'a>(
    records: &'a [GraphRecord],
    file_path: &str,
    deleted: &std::collections::BTreeSet<&str>,
) -> (&'static str, Option<Vec<DiagnosticRef<'a>>>) {
    let mut diagnostics = Vec::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Diagnostic,
            repo_relative_path: Some(path),
            span: Some(span),
            ..
        } = r
            && path == file_path
            && !deleted.contains(id.as_str())
        {
            diagnostics.push(DiagnosticRef {
                record_id: id.as_str(),
                repo_relative_path: path.as_str(),
                span: *span,
            });
        }
    }
    if diagnostics.is_empty() {
        ("complete", None)
    } else {
        diagnostics.sort_by_key(|d| (d.span.start_line, d.record_id));
        ("partial", Some(diagnostics))
    }
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
    /// Stable `Repository` record ID owning the drift target; absent when the
    /// store carries no repository topology for the record (legacy graphs).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    /// Human-usable repository identity handle (e.g. `owner/name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
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
    /// Stable `Repository` record ID owning this row; absent when the store
    /// carries no repository topology for the record (legacy graphs).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    /// Human-usable repository identity handle (e.g. `owner/name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
}

#[cfg(feature = "embeddings")]
impl<'a> SemanticResult<'a> {
    fn from_match(m: &'a SemanticMatch, index: &'a query::RepositoryIndex) -> Self {
        let repository_id = index.owner_of(&m.record_id);
        Self {
            record_id: &m.record_id,
            name: m.name.as_deref(),
            repo_relative_path: m.repo_relative_path.as_deref(),
            score: m.score,
            span: m.span,
            repository_id,
            repository: repository_id.and_then(|id| index.display_of(id)),
        }
    }
}

/// The query outcome containing info on who last changed a given symbol.
#[derive(Serialize)]
struct WhoResult<'a> {
    /// Name of the queried symbol.
    symbol_name: &'a str,
    /// Git SHA of the commit that last changed the symbol's file.
    commit_sha: &'a str,
    /// Optional Git author display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    author_name: Option<&'a str>,
    /// Optional Git author email address.
    #[serde(skip_serializing_if = "Option::is_none")]
    author_email: Option<&'a str>,
    /// Valid time when the commit was recorded.
    valid_time: &'a str,
    /// Repository-relative path to the file containing the symbol.
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    /// Optional store-freshness code.
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<&'a str>,
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
use crate::query::{
    ContextLinkedItem, ContextObservation, context_linked_item, context_observation,
};

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

/// One record excluded by a filter or temporal constraint.
#[derive(Serialize)]
struct ExcludedDiagnostic<'a> {
    record_id: &'a str,
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<Vec<crate::temporal_status::TemporalReference>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contradicted_by: Option<Vec<crate::temporal_status::TemporalReference>>,
}

/// Full context query response envelope.
#[derive(Serialize)]
struct ContextResponse<'a> {
    ok: bool,
    symbol_name: &'a str,
    /// Non-fatal store-freshness code relative to a working tree (issue #82).
    ///
    /// Present only when `--repo-path` was supplied; the context is never
    /// suppressed on a non-`fresh` verdict so an agent can downgrade trust in the
    /// returned handles instead of losing them.
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<&'static str>,
    source_facts: Vec<ContextSourceFact<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topology_edges: Vec<ContextTopologyEdge<'a>>,
    observations: Vec<ContextObservation<'a>>,
    project_state: Vec<ContextLinkedItem<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<ExcludedDiagnostic<'a>>,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<ExcludedDiagnostic<'a>>,
}

/// One semantic drift item in the `semantic_drift` section of a subsystem response.
#[derive(Serialize)]
struct SubsystemDrift<'a> {
    record_id: &'a str,
    score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_span: Option<crate::ir::SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after_git_commit: Option<&'a str>,
}

/// Full subsystem context query response envelope (issue #83).
#[derive(Serialize)]
struct SubsystemResponse<'a> {
    ok: bool,
    prefix: &'a str,
    source_facts: Vec<ContextSourceFact<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topology_edges: Vec<ContextTopologyEdge<'a>>,
    observations: Vec<ContextObservation<'a>>,
    project_state: Vec<ContextLinkedItem<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    semantic_drift: Vec<SubsystemDrift<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<ExcludedDiagnostic<'a>>,
}

// ---------------------------------------------------------------------------
// semantic → context bridge (issue #90)
// ---------------------------------------------------------------------------

/// One semantic match expanded into evidence-backed context.
///
/// Carries the retrieval-lead handle (record ID, repo-relative path, span,
/// score, repository identity) and the same five trust-separated context
/// sections produced by `eg query context`. `match_kind` documents whether the
/// match anchored on a `symbol`, a `file` (its defined symbols are seeded into
/// `source_facts`), or some `other` node. When `ambiguous` is true the match
/// name resolved to more than one live symbol and `candidate_record_ids` lists
/// every candidate instead of silently picking one.
#[cfg(feature = "embeddings")]
#[derive(Serialize)]
struct SemanticContextMatch<'a> {
    record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    score: f32,
    match_kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    ambiguous: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    candidate_record_ids: Vec<&'a str>,
    source_facts: Vec<ContextSourceFact<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topology_edges: Vec<ContextTopologyEdge<'a>>,
    observations: Vec<ContextObservation<'a>>,
    project_state: Vec<ContextLinkedItem<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<ExcludedDiagnostic<'a>>,
}

/// Full `eg query semantic-context` response envelope.
#[cfg(feature = "embeddings")]
#[derive(Serialize)]
struct SemanticContextResponse<'a> {
    ok: bool,
    query: &'a str,
    min_score: f32,
    matches: Vec<SemanticContextMatch<'a>>,
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

/// One prior failed attempt: a redaction-safe [`AuditItem`] plus the
/// failure-specific read-time fields (issue #63).
#[derive(Serialize)]
struct FailureAttemptJson<'a> {
    #[serde(flatten)]
    item: AuditItem<'a>,
    /// Read-time `still_failing` / `since_resolved` status (AC5).
    resolution_status: &'static str,
    /// Record ID of the later passing verification that resolved it, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_by: Option<&'a str>,
    /// The target handle (anchor record ID) this attempt linked to.
    #[serde(skip_serializing_if = "str::is_empty")]
    matched_target: &'a str,
    /// `command_failure` / `patch_invalid` for an agent `Failure` claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_kind: Option<&'a str>,
    /// RFC-3339 execution time for a runtime verification failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    executed_at: Option<&'a str>,
}

/// Full prior-failed-attempt response envelope (issue #63).
#[derive(Serialize)]
struct FailureHistoryResponse<'a> {
    ok: bool,
    target_handle: &'a str,
    target_type: &'a str,
    target_ids: Vec<&'a str>,
    runtime_failures: Vec<FailureAttemptJson<'a>>,
    agent_failures: Vec<FailureAttemptJson<'a>>,
    superseding_successes: Vec<AuditItem<'a>>,
    patch_artifacts: Vec<AuditItem<'a>>,
    /// `AgentSession` record IDs reached via `AUTHORED_BY` from a failure — the
    /// citable provenance when a `Failure` carries no `agent_id`/`session_id`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    agent_sessions: Vec<&'a str>,
    /// `Agent` record IDs reached via `SESSION_OF`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    agents: Vec<&'a str>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
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
            repo,
            repo_path,
            format,
        } => {
            if let Some(tx) = tx_as_of.as_deref() {
                // --repo-path is used to stamp freshness onto results.  TxSymbolRow
                // has no freshness field and the tx-as-of path never computes one,
                // so accepting --repo-path here would silently drop the signal.
                // Reject the combination early so users see a clear error rather
                // than a result that looks correct but carries no freshness stamp.
                if repo_path.is_some() {
                    print_tx_error(
                        "unsupported_combination",
                        "--repo-path cannot be used with --tx-as-of; \
                         freshness stamping is not available for transaction-time queries",
                    )?;
                    std::process::exit(1);
                }
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
                    return query_symbol_tx_via_daemon(
                        &name,
                        dir,
                        tx,
                        as_of.as_deref(),
                        repo.as_deref(),
                        format,
                    );
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
                let index = query::RepositoryIndex::build(&records);
                let selected = resolve_repo_scope(&index, repo.as_deref());
                return query_symbol_tx_as_of(
                    &records,
                    &name,
                    tx,
                    as_of.as_deref(),
                    format,
                    &index,
                    selected.as_deref(),
                );
            }
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                if repo_path.is_some() {
                    // Daemon-routed results bypass the local freshness probe entirely;
                    // accepting --repo-path here would silently emit results without
                    // the promised `freshness` field (PR #186 follow-up).
                    eprintln!(
                        "error: --repo-path cannot be used with --daemon; \
                         run without --daemon to get freshness stamping"
                    );
                    std::process::exit(1);
                }
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_symbol_via_daemon(
                    &name,
                    dir,
                    at.as_deref(),
                    as_of.as_deref(),
                    repo.as_deref(),
                    format,
                );
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            let selected = selected.as_deref();
            let freshness_code = query_freshness_code_with_hint(
                &records,
                repo_path.as_deref(),
                &[graph.as_deref(), data_dir.as_deref()],
                selected,
            );
            as_of.map_or_else(
                || {
                    at.map_or_else(
                        || {
                            query_symbol_all(
                                &records,
                                &name,
                                format,
                                &index,
                                selected,
                                freshness_code.as_ref(),
                            )
                        },
                        |prefix| {
                            query_symbol_at(
                                &records,
                                &name,
                                &prefix,
                                format,
                                &index,
                                selected,
                                freshness_code.as_ref(),
                            )
                        },
                    )
                },
                |instant| {
                    query_symbol_as_of(
                        &records,
                        &name,
                        &instant,
                        format,
                        &index,
                        selected,
                        freshness_code.as_ref(),
                    )
                },
            )
        }
        QuerySubcommand::Symbols {
            pattern,
            graph,
            data_dir,
            repo,
            case_insensitive,
            format,
        } => {
            // An empty pattern would substring-match every symbol in the
            // store; reject it as malformed (exit 1) so the caller's typo is
            // never conflated with a real match set or a no-match signal.
            if pattern.is_empty() {
                eprintln!("error: empty pattern; provide a substring or `*` glob");
                std::process::exit(1);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_symbols_matching(
                &records,
                &pattern,
                case_insensitive,
                format,
                &index,
                selected.as_deref(),
            )
        }
        QuerySubcommand::Who {
            name,
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            at,
            as_of,
            tx_as_of,
            repo,
            repo_path,
            format,
        } => {
            if tx_as_of.is_some() {
                eprintln!("error: --tx-as-of is not supported for query who");
                std::process::exit(1);
            }
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                eprintln!("error: --daemon is not supported for query who");
                std::process::exit(1);
            }

            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            let selected = selected.as_deref();

            let freshness_code = query_freshness_code_with_hint(
                &records,
                repo_path.as_deref(),
                &[graph.as_deref(), data_dir.as_deref()],
                selected,
            );

            match query::who_last_changed(
                &records,
                &name,
                at.as_deref(),
                as_of.as_deref(),
                &index,
                selected,
            ) {
                Ok(Some((symbol_node, commit_node))) => {
                    let (author_name, author_email) = if let GraphRecord::Node {
                        author_name,
                        author_email,
                        ..
                    } = commit_node
                    {
                        (author_name.as_deref(), author_email.as_deref())
                    } else {
                        (None, None)
                    };

                    let commit_sha = if let GraphRecord::Node {
                        temporal: Some(t), ..
                    } = commit_node
                    {
                        t.git_commit.as_str()
                    } else {
                        ""
                    };

                    let valid_time = if let GraphRecord::Node {
                        temporal: Some(t), ..
                    } = commit_node
                    {
                        t.valid_time.as_str()
                    } else {
                        ""
                    };

                    let repo_relative_path = if let GraphRecord::Node {
                        repo_relative_path, ..
                    } = symbol_node
                    {
                        repo_relative_path.as_deref()
                    } else {
                        None
                    };

                    let repository_id = index.owner_of(symbol_node.id());
                    let freshness = freshness_code.as_ref().and_then(|(repo_id, code)| {
                        if repository_id == Some(repo_id.as_str()) {
                            Some(*code)
                        } else {
                            None
                        }
                    });

                    let result = WhoResult {
                        symbol_name: &name,
                        commit_sha,
                        author_name,
                        author_email,
                        valid_time,
                        repo_relative_path,
                        freshness,
                    };
                    print_result(&result, format)?;
                    Ok(())
                }
                Ok(None) => {
                    eprintln!("error: no match found for symbol `{name}`");
                    std::process::exit(2);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        QuerySubcommand::File {
            path,
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            repo,
            repo_path,
            format,
        } => {
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                if repo_path.is_some() {
                    eprintln!(
                        "error: --repo-path cannot be used with --daemon; \
                         run without --daemon to get freshness stamping"
                    );
                    std::process::exit(1);
                }
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_file_via_daemon(&path, dir, repo.as_deref(), format);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            let freshness_code = query_freshness_code_with_hint(
                &records,
                repo_path.as_deref(),
                &[graph.as_deref(), data_dir.as_deref()],
                selected.as_deref(),
            );
            query_file(
                &records,
                &path,
                format,
                &index,
                selected.as_deref(),
                freshness_code.as_ref(),
            )
        }
        QuerySubcommand::Drift {
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            repo,
            limit,
            format,
        } => {
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_drift_via_daemon(dir, limit, repo.as_deref(), format);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_drift(&records, limit, format, &index, selected.as_deref())
        }
        #[cfg(feature = "embeddings")]
        QuerySubcommand::Semantic {
            query,
            data_dir,
            daemon,
            repo,
            limit,
            format,
        } => {
            if daemon {
                query_semantic_via_daemon(&query, &data_dir, limit, repo.as_deref(), format)
            } else {
                query_semantic(&query, &data_dir, limit, repo.as_deref(), format)
            }
        }
        #[cfg(feature = "embeddings")]
        QuerySubcommand::SemanticContext {
            query,
            data_dir,
            repo,
            limit,
            min_score,
            supersession,
        } => query_semantic_context(
            &query,
            &data_dir,
            limit,
            min_score,
            repo.as_deref(),
            supersession,
        ),
        #[cfg(feature = "embeddings")]
        QuerySubcommand::SemanticMemory {
            query,
            data_dir,
            repo,
            limit,
            verified_only,
            format,
            supersession,
        } => query_semantic_memory(
            &query,
            &data_dir,
            limit,
            repo.as_deref(),
            verified_only,
            format,
            supersession,
        ),
        QuerySubcommand::Context {
            name,
            graph,
            data_dir,
            repo_path,
            supersession,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            // Pre-compute the context owner so the freshness hint matches the
            // repository that actually owns the returned source facts.  Without this,
            // `query_freshness_code` auto-detects the identity from `repo_path`, which
            // can differ from an operator-override ID: the snapshot lookup then fails
            // (no match for the auto-detected ID in a single-override-ID store), the
            // single-repo fallback is disabled, and the verdict is `unknown` even
            // though all facts are from one stamped repository (PR #186 follow-up).
            let owner_hint = if repo_path.is_some() {
                let index = query::RepositoryIndex::build(&records);
                let ctx = query::symbol_context(&records, &name);
                let owners: std::collections::BTreeSet<Option<&str>> = ctx
                    .source_facts
                    .iter()
                    .map(|r| index.owner_of(r.id()))
                    .collect();
                if owners.len() == 1 {
                    owners.into_iter().next().flatten().map(ToOwned::to_owned)
                } else {
                    None
                }
            } else {
                None
            };
            let freshness = query_freshness_code_with_hint(
                &records,
                repo_path.as_deref(),
                &[graph.as_deref(), data_dir.as_deref()],
                owner_hint.as_deref(),
            );
            query_context_cmd(&records, &name, freshness, supersession)
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
        QuerySubcommand::Changes {
            base,
            head,
            graph,
            data_dir,
            repo,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_changes_cmd(&records, &base, &head, repo.as_deref())
        }
        QuerySubcommand::Failures {
            handle,
            graph,
            data_dir,
            repo,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_failures_cmd(&records, &handle, &index, selected.as_deref())
        }
        QuerySubcommand::EvidenceFreshness {
            graph,
            data_dir,
            stale_only,
        } => {
            // Freshness compares an observation's anchored code version against
            // later ones, so it needs superseded (pre-change) versions — the
            // history-inclusive read. It is also strictly read-only, so the
            // embedded `--data-dir` store is read through a throwaway copy (opening
            // the live engine re-persists its index files); the `--graph` path is
            // already read-only.
            let records = load_evidence_freshness_records(graph.as_deref(), data_dir.as_deref())?;
            query_freshness_cmd(&records, stale_only)
        }
        QuerySubcommand::Subsystem {
            prefix,
            graph,
            data_dir,
            format,
            supersession,
        } => {
            // Validate the prefix before loading records so malformed input fails
            // fast with a machine-readable diagnostic, not a store I/O error.
            if prefix.trim_end_matches('/').is_empty() {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": "malformed_prefix",
                        "prefix": prefix,
                        "message": "prefix must be non-empty after stripping trailing slashes"
                    }
                });
                println!("{}", serde_json::to_string(&envelope)?);
                std::process::exit(1);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_subsystem_cmd(&records, &prefix, format, supersession)
        }
        QuerySubcommand::ChangeImpact {
            handle,
            graph,
            data_dir,
            repo,
            depth,
            format: _format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_change_impact_cmd(&records, &handle, &index, selected.as_deref(), depth)
        }
        QuerySubcommand::Orient {
            graph,
            data_dir,
            repo,
            limit,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_orient_cmd(&records, selected.as_deref(), limit, format)
        }
        QuerySubcommand::Lifeline {
            graph,
            data_dir,
            symbol,
            repo,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_lifeline_cmd(&records, &symbol, selected.as_deref(), format)
        }
    }
}

/// Re-raises a daemon query error, except for repository-selector rejections,
/// which are printed as the same stable machine-readable stderr JSON the
/// non-daemon paths emit (`unknown_repository_selector` /
/// `ambiguous_repository_selector`) before exiting 1.
#[cfg(feature = "embedded-aletheiadb")]
fn surface_daemon_selector_rejection(error: anyhow::Error, repo: Option<&str>) -> anyhow::Error {
    if let (Some(rejection), Some(selector)) = (
        error.downcast_ref::<crate::daemon::DaemonQueryRejection>(),
        repo,
    ) && matches!(
        rejection.code.as_str(),
        "unknown_repository_selector" | "ambiguous_repository_selector"
    ) {
        let mut diag = serde_json::json!({
            "code": rejection.code,
            "selector": selector,
            "message": rejection.message,
        });
        if let Some(candidates) = &rejection.candidates {
            diag["candidates"] = serde_json::json!(candidates);
        }
        eprintln!("{diag}");
        std::process::exit(1);
    }
    error
}

/// Fails closed when an unscoped daemon-backed single-answer time view
/// (`--as-of` / `--at`) returns rows from more than one repository: the
/// documented contract requires the `ambiguous_repository` diagnostic, not a
/// silently widened multi-repository answer (issue #67).
#[cfg(feature = "embedded-aletheiadb")]
fn fail_on_unscoped_daemon_repo_collision(records: &[serde_json::Value]) {
    // Rows without a `repository_id` (legacy/unattributed records) form their
    // own candidate group; they still count toward the collision.
    let groups: std::collections::BTreeSet<Option<&str>> = records
        .iter()
        .map(|r| r.get("repository_id").and_then(serde_json::Value::as_str))
        .collect();
    if groups.len() > 1 {
        exit_ambiguous_repository(&groups);
    }
}

#[cfg(feature = "embedded-aletheiadb")]
fn query_symbol_via_daemon(
    name: &str,
    data_dir: &Path,
    at: Option<&str>,
    as_of: Option<&str>,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let (verb, mut params) = at.map_or_else(
        || ("symbol_by_name", serde_json::json!({ "name": name })),
        |commit| {
            (
                "symbol_at_commit",
                serde_json::json!({ "name": name, "commit": commit }),
            )
        },
    );
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    let records = client
        .query_verb(verb, &params, as_of)
        .map_err(|e| surface_daemon_selector_rejection(e, repo))?;
    if repo.is_none() && (at.is_some() || as_of.is_some()) {
        fail_on_unscoped_daemon_repo_collision(&records);
    }
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
fn query_file_via_daemon(
    path: &str,
    data_dir: &Path,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let mut params = serde_json::json!({ "repo_relative_path": path });
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    let result = client
        .query_verb_raw("file_defines", &params, None)
        .map_err(|e| surface_daemon_selector_rejection(e, repo))?;
    // Forward the daemon's repository-scope diagnostics (e.g.
    // `excluded_other_repositories`) to stderr so the daemon-routed CLI keeps
    // the same machine-readable contract as the local path (issue #67).
    if let Some(diagnostics) = result.get("diagnostics").and_then(|v| v.as_array()) {
        for diagnostic in diagnostics {
            eprintln!("{}", serde_json::to_string(diagnostic)?);
        }
    }
    let records = result
        .get("records")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
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
fn query_drift_via_daemon(
    data_dir: &Path,
    limit: usize,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let mut params = serde_json::json!({ "limit": limit as u64 });
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    let records = client
        .query_verb("drift_top_n", &params, None)
        .map_err(|e| surface_daemon_selector_rejection(e, repo))?;
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

/// Resolves an optional `--repo` selector to a stable repository record ID.
///
/// On an unknown or ambiguous selector this prints a stable machine-readable
/// JSON diagnostic to stderr and exits 1 — no partial rows reach stdout, and
/// ambiguity is never resolved by picking a repository implicitly (issue #67).
fn resolve_repo_scope(index: &query::RepositoryIndex, repo: Option<&str>) -> Option<String> {
    let selector = repo?;
    match index.resolve_selector(selector) {
        Ok(id) => Some(id.to_owned()),
        Err(err) => {
            let diag = match &err {
                query::RepositorySelectorError::Unknown { selector } => serde_json::json!({
                    "code": err.code(),
                    "selector": selector,
                }),
                query::RepositorySelectorError::Ambiguous {
                    selector,
                    candidates,
                } => serde_json::json!({
                    "code": err.code(),
                    "selector": selector,
                    "candidates": candidates,
                }),
            };
            eprintln!("{diag}");
            std::process::exit(1);
        }
    }
}

/// Prints the stable `ambiguous_repository` diagnostic for an unscoped query
/// whose single-result answer would otherwise pick one repository implicitly,
/// then exits 1.
///
/// `groups` carries one entry per candidate group: `Some(repository_id)` for
/// attributed rows and `None` for rows the store topology cannot attribute
/// (legacy records). An unattributed group counts toward the collision — it is
/// still a distinct answer the caller did not choose between.
fn exit_ambiguous_repository(groups: &std::collections::BTreeSet<Option<&str>>) -> ! {
    let repositories: Vec<&str> = groups.iter().filter_map(|g| *g).collect();
    let mut diag = serde_json::json!({
        "code": "ambiguous_repository",
        "message": "multiple repositories match; rerun with --repo <SELECTOR>",
        "repositories": repositories,
    });
    if groups.contains(&None) {
        diag["includes_unattributed_rows"] = serde_json::Value::Bool(true);
    }
    eprintln!("{diag}");
    std::process::exit(1);
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

/// Routes `eg audit` subcommands.
fn audit_cmd(subcommand: AuditSubcommand) -> Result<()> {
    match subcommand {
        AuditSubcommand::Citations {
            graph,
            data_dir,
            min_code_citation,
            format,
        } => audit_citations_cmd(
            graph.as_deref(),
            data_dir.as_deref(),
            min_code_citation,
            format,
        ),
        AuditSubcommand::MemoryHealth {
            graph,
            data_dir,
            min_provenance_coverage,
            max_dangling_evidence,
            max_unverified,
            max_current_guidance_contamination,
            format,
        } => audit_memory_health_cmd(
            graph.as_deref(),
            data_dir.as_deref(),
            min_provenance_coverage,
            max_dangling_evidence,
            max_unverified,
            max_current_guidance_contamination,
            format,
        ),
        AuditSubcommand::TokenCost {
            corpus,
            min_ratio,
            format,
        } => audit_token_cost_cmd(&corpus, min_ratio, format),
        AuditSubcommand::Accuracy {
            corpus_dir,
            labels,
            span_line_tolerance,
            min_precision,
            min_recall,
            format,
        } => crate::accuracy::eval_accuracy_cmd(
            &corpus_dir,
            &labels,
            span_line_tolerance,
            min_precision,
            min_recall,
            format,
        ),
    }
}

/// Prints a redaction-safe JSON error and exits with the usage/load code (2).
fn token_cost_exit(code: &str, path: &str, message: &str) -> ! {
    eprintln!(
        "{}",
        serde_json::json!({ "code": code, "path": path, "message": message })
    );
    std::process::exit(2);
}

/// Loads the corpus manifest, applies the `--min-ratio` override, and validates
/// the pinned token-count method. Exits 2 on any load/usage error.
fn load_token_cost_corpus(
    corpus_path: &Path,
    min_ratio_override: Option<f64>,
) -> crate::token_cost::TokenCostCorpus {
    use crate::token_cost::{TOKEN_COUNT_METHOD, TokenCostCorpus};

    if let Some(min_ratio) = min_ratio_override
        && (!min_ratio.is_finite() || min_ratio <= 0.0)
    {
        // A non-positive or non-finite override would silently disable the gate
        // (ratio >= 0.0 is always true; zero is as useless as a negative value).
        token_cost_exit(
            "invalid_min_ratio",
            &corpus_path.display().to_string(),
            "--min-ratio must be a finite, positive value",
        );
    }
    let path = corpus_path.display().to_string();
    let text = std::fs::read_to_string(corpus_path)
        .unwrap_or_else(|error| token_cost_exit("corpus_read_error", &path, &error.to_string()));
    let mut corpus: TokenCostCorpus = serde_json::from_str(&text)
        .unwrap_or_else(|error| token_cost_exit("corpus_parse_error", &path, &error.to_string()));

    // The token-count method is pinned; reject a manifest that asks for another
    // so the reported ratio is always produced by the documented method (AC3).
    if corpus.token_count_method != TOKEN_COUNT_METHOD {
        token_cost_exit(
            "unsupported_token_count_method",
            &path,
            &format!("only '{TOKEN_COUNT_METHOD}' is supported"),
        );
    }
    if let Some(min_ratio) = min_ratio_override {
        corpus.min_ratio = min_ratio;
    }
    corpus
}

/// Scans the corpus into a deterministic graph and reads its source files for
/// the grep baseline. Exits 2 on any scan/read error.
fn load_token_cost_inputs(
    corpus: &crate::token_cost::TokenCostCorpus,
    source_dir: &Path,
) -> (Vec<GraphRecord>, BTreeMap<String, String>) {
    let dir = source_dir.display().to_string();
    let graph = crate::scan_repository_at_with_override(
        source_dir,
        &corpus.scan_time,
        Some(&corpus.repository_id_override),
    )
    .unwrap_or_else(|error| token_cost_exit("corpus_scan_error", &dir, &error.to_string()));
    let records = graph.records().to_vec();

    // Read the same source files for the grep-shaped baseline, keyed by their
    // repo-relative path so the baseline reads exactly what the scan indexed.
    let mut source_files: BTreeMap<String, String> = BTreeMap::new();
    let discovered = crate::fs::discover_source_files(source_dir)
        .unwrap_or_else(|error| token_cost_exit("corpus_discover_error", &dir, &error.to_string()));
    for source_file in discovered {
        let content = std::fs::read_to_string(&source_file.path).unwrap_or_else(|error| {
            token_cost_exit(
                "corpus_read_error",
                &source_file.path.display().to_string(),
                &error.to_string(),
            )
        });
        source_files.insert(source_file.repo_relative_path.clone(), content);
    }
    (records, source_files)
}

fn audit_token_cost_cmd(
    corpus_path: &Path,
    min_ratio_override: Option<f64>,
    format: OutputFormat,
) -> Result<()> {
    let corpus = load_token_cost_corpus(corpus_path, min_ratio_override);

    // Resolve the corpus source directory relative to the manifest's parent so
    // the gate is runnable regardless of the working directory.
    let manifest_dir = corpus_path.parent().unwrap_or_else(|| Path::new("."));
    let source_dir = manifest_dir.join(&corpus.source_dir);
    let corpus_display = source_dir.to_string_lossy().replace('\\', "/");

    let (records, source_files) = load_token_cost_inputs(&corpus, &source_dir);
    let report =
        crate::token_cost::run_token_cost_report(&corpus, &source_files, &records, &corpus_display);

    let output = match format {
        OutputFormat::Json | OutputFormat::Text => serde_json::to_string_pretty(&report)
            .unwrap_or_else(|error| token_cost_exit("serialize_error", "", &error.to_string())),
    };
    println!("{output}");
    std::process::exit(i32::from(!report.ok));
}

/// Handles `eg audit citations` (issue #65).
fn audit_citations_cmd(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    min_code_citation: f64,
    format: OutputFormat,
) -> Result<()> {
    // The gate threshold is a fraction; reject values that would silently disable
    // or invert the gate (e.g. a negative threshold makes 0% completeness pass).
    if !min_code_citation.is_finite() || !(0.0..=1.0).contains(&min_code_citation) {
        eprintln!(
            "{}",
            serde_json::json!({
                "code": "invalid_min_code_citation",
                "value": min_code_citation.to_string(),
                "message": "--min-code-citation must be a finite value in [0.0, 1.0]"
            })
        );
        std::process::exit(2);
    }

    // For an embedded store, read from a throwaway read-only copy: opening the
    // embedded engine re-persists index files, and a citation audit must never
    // mutate the store it is only measuring. The guard keeps the copy alive for
    // the duration of every read below.
    // `store_copy` owns the throwaway copy path plus its tempdir guard; keeping
    // it bound here holds the copy alive for every read below.
    let store_copy = data_dir.map(|dir| match readonly_audit_store(dir) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());

    let records = match load_query_records(graph, effective_data_dir) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };

    let semantic = collect_semantic_input(effective_data_dir, &records);
    // The evidence-freshness lane mirrors `eg query evidence-freshness`, which
    // reads the history-inclusive store view so superseded versions can produce
    // drift/unresolved verdicts. A JSONL graph already carries that history; an
    // embedded store needs the explicit history-inclusive load.
    // Surface a history-load failure rather than silently auditing current-only
    // rows (the public `eg query evidence-freshness --data-dir` uses `?`).
    let freshness_records = effective_data_dir.map(|dir| match load_records_from_db_history(dir) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let config = crate::citation_audit::AuditConfig {
        min_code_citation,
        semantic,
        freshness_records,
    };
    let report = crate::citation_audit::run_citation_audit(&records, &config);

    let output = match format {
        OutputFormat::Json | OutputFormat::Text => serde_json::to_string_pretty(&report)
            .context("failed to serialize citation audit report")?,
    };
    println!("{output}");
    // `process::exit` bypasses destructors, so the throwaway store copy's `TempDir`
    // guard would leak a full copied store under the temp dir on every `--data-dir`
    // run. Drop it explicitly before exiting (the borrow in `effective_data_dir` is
    // dead after the reads above).
    let exit_code = i32::from(!report.ok);
    drop(store_copy);
    std::process::exit(exit_code);
}

/// Handles `eg audit memory-health` (issue #94).
fn audit_memory_health_cmd(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    min_provenance_coverage: f64,
    max_dangling_evidence: f64,
    max_unverified: Option<f64>,
    max_current_guidance_contamination: Option<f64>,
    format: OutputFormat,
) -> Result<()> {
    // Validate inputs
    for (name, val) in [
        ("min-provenance-coverage", min_provenance_coverage),
        ("max-dangling-evidence", max_dangling_evidence),
    ] {
        if !val.is_finite() || !(0.0..=1.0).contains(&val) {
            eprintln!(
                "{}",
                serde_json::json!({
                    "code": format!("invalid_{}", name.replace('-', "_")),
                    "value": val.to_string(),
                    "message": format!("--{} must be a finite value in [0.0, 1.0]", name)
                })
            );
            std::process::exit(2);
        }
    }
    for (name, val_opt) in [
        ("max-unverified", max_unverified),
        (
            "max-current-guidance-contamination",
            max_current_guidance_contamination,
        ),
    ] {
        if val_opt.is_some_and(|val| !val.is_finite() || !(0.0..=1.0).contains(&val)) {
            let val = val_opt.unwrap();
            eprintln!(
                "{}",
                serde_json::json!({
                    "code": format!("invalid_{}", name.replace('-', "_")),
                    "value": val.to_string(),
                    "message": format!("--{} must be a finite value in [0.0, 1.0]", name)
                })
            );
            std::process::exit(2);
        }
    }

    let store_copy = data_dir.map(|dir| match readonly_audit_store(dir) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());

    let records = match load_query_records_history(graph, effective_data_dir) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };

    let config = crate::memory_health::MemoryHealthConfig {
        min_provenance_coverage,
        max_dangling_evidence,
        max_unverified,
        max_current_guidance_contamination,
    };
    let report = crate::memory_health::run_memory_health_audit(&records, &config);

    let output = match format {
        OutputFormat::Json | OutputFormat::Text => serde_json::to_string_pretty(&report)
            .context("failed to serialize memory health report")?,
    };
    println!("{output}");

    let exit_code = i32::from(!report.ok);
    drop(store_copy);
    std::process::exit(exit_code);
}

/// Collects embedded-store semantic retrieval leads for the audit, when the
/// `embeddings` feature is built and a `--data-dir` store is supplied.
#[cfg(feature = "embeddings")]
fn collect_semantic_input(
    data_dir: Option<&Path>,
    records: &[GraphRecord],
) -> crate::citation_audit::SemanticInput {
    use crate::citation_audit::{SemanticInput, SemanticRow};

    let Some(dir) = data_dir else {
        return SemanticInput::default();
    };
    let Ok(sink) = EmbeddedAletheiaSink::open_unleased(dir) else {
        return SemanticInput::Disabled {
            reason: "embedded_store_unavailable",
        };
    };
    let Ok(query_vector) = embed_query_text("foo") else {
        return SemanticInput::Disabled {
            reason: "embedding_unavailable",
        };
    };
    let fetch = records.len().max(10);
    let Ok(mut matches) = sink.semantic_search(&query_vector, fetch) else {
        return SemanticInput::Disabled {
            reason: "semantic_index_unavailable",
        };
    };
    matches.retain(|m| {
        m.kind
            .as_deref()
            .is_some_and(|k| k == "File" || k == "Symbol")
    });
    // Measure the DEFAULT `eg query semantic` output, which truncates the
    // code-filtered matches to the default `--limit` (mirrors `query_semantic`).
    matches.truncate(crate::citation_audit::DEFAULT_QUERY_LIMIT);
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let rows = matches
        .iter()
        .map(|m| {
            let (path, span) = by_id.get(m.record_id.as_str()).map_or((None, None), |r| {
                if let GraphRecord::Node {
                    repo_relative_path,
                    span,
                    ..
                } = r
                {
                    (repo_relative_path.clone(), *span)
                } else {
                    (None, None)
                }
            });
            SemanticRow {
                record_id: m.record_id.clone(),
                kind: m.kind.clone().unwrap_or_else(|| "Symbol".to_owned()),
                repo_relative_path: path,
                span,
            }
        })
        .collect();
    SemanticInput::Enabled { rows }
}

/// Without the `embeddings` feature there is no vector index; `semantic` is
/// reported disabled with a stable reason rather than silently dropped.
#[cfg(not(feature = "embeddings"))]
fn collect_semantic_input(
    data_dir: Option<&Path>,
    _records: &[GraphRecord],
) -> crate::citation_audit::SemanticInput {
    use crate::citation_audit::SemanticInput;
    if data_dir.is_some() {
        SemanticInput::Disabled {
            reason: "requires_embeddings_feature",
        }
    } else {
        SemanticInput::default()
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

/// Loads records from an embedded `--data-dir` store without mutating it (issue #82).
///
/// The embedded engine re-persists its index files on open, so a freshness check
/// that opened the live store directly would modify it — violating the read-only
/// guarantee. This copies the store to a throwaway temporary directory and reads
/// the copy, leaving the original byte-for-byte untouched.
/// Returns a read-only working location for embedded-store audit reads plus the
/// tempdir guard that must outlive those reads.
///
/// With the embedded feature this is a throwaway copy of the store, so the audit
/// never re-persists or otherwise mutates the original. Without the feature the
/// path is returned unchanged (the subsequent read bails on the missing feature).
fn readonly_audit_store(data_dir: &Path) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let temp =
            tempfile::tempdir().context("failed to create temporary read-only store copy")?;
        let copy_root = temp.path().join("store");
        copy_dir_recursive(data_dir, &copy_root).with_context(|| {
            format!(
                "failed to copy store {} for read-only audit",
                data_dir.display()
            )
        })?;
        Ok((copy_root, Some(temp)))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        Ok((data_dir.to_path_buf(), None))
    }
}

fn load_records_from_data_dir_readonly(data_dir: &Path) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let temp =
            tempfile::tempdir().context("failed to create temporary read-only store copy")?;
        let copy_root = temp.path().join("store");
        copy_dir_recursive(data_dir, &copy_root).with_context(|| {
            format!(
                "failed to copy store {} for read-only inspection",
                data_dir.display()
            )
        })?;
        load_records_from_db(&copy_root)
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// Recursively copies the regular files and directories under `src` into `dst`.
///
/// Symlinks and other non-regular entries are skipped; this is used only to make
/// a read-only working copy of an embedded store directory.
#[cfg(feature = "embedded-aletheiadb")]
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
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

/// Loads the history-inclusive view from a store without mutating it (issue #85).
///
/// `eg evidence_freshness` is strictly read-only, but opening the embedded engine
/// re-persists its on-disk index files. This copies the store to a throwaway
/// temporary directory and reads the history-inclusive view from the copy, leaving
/// the original byte-for-byte untouched (mirrors `load_records_from_data_dir_readonly`).
fn load_records_from_db_history_readonly(data_dir: &Path) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let temp =
            tempfile::tempdir().context("failed to create temporary read-only store copy")?;
        let copy_root = temp.path().join("store");
        copy_dir_recursive(data_dir, &copy_root).with_context(|| {
            format!(
                "failed to copy store {} for read-only inspection",
                data_dir.display()
            )
        })?;
        let sink = EmbeddedAletheiaSink::open_unleased(&copy_root)
            .with_context(|| format!("failed to open embedded store {}", copy_root.display()))?;
        sink.read_all_records_including_superseded()
            .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// History-inclusive record load for the strictly read-only evidence-freshness
/// command. `--graph` is already read-only; `--data-dir` reads a throwaway copy.
fn load_evidence_freshness_records(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<Vec<GraphRecord>> {
    match (graph, data_dir) {
        (Some(path), None) => load_records_from_jsonl(path),
        (None, Some(dir)) => load_records_from_db_history_readonly(dir),
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
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
pub(crate) fn generate_embeddings(
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
fn query_semantic(
    query: &str,
    data_dir: &Path,
    limit: usize,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    validate_existing_embedded_store(data_dir)?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    // Repository attribution requires the store topology, not just the vector
    // index: build the index from the full record set so each retrieval lead
    // carries its repository identity handle (issue #67). Resolve the selector
    // before loading the embedding model so a bad `--repo` fails fast.
    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
    let index = query::RepositoryIndex::build(&records);
    let selected = resolve_repo_scope(&index, repo);

    let query_vector = embed_query_text(query)?;

    // Over-fetch the whole index, not just `limit` raw hits: the shared vector
    // index now also embeds agent-memory nodes (issue #91), so a query whose top
    // `limit` raw matches are memory would otherwise drop them all and never see
    // the code hits ranked just behind them. Fetching the full pool lets the
    // code-kind filter below recover those code hits; the limit then bounds the
    // filtered result set. Scoping needs the full pool for the same reason.
    let fetch = records.len().max(limit);
    let mut matches = sink
        .semantic_search(&query_vector, fetch)
        .with_context(|| "semantic search failed — was the store ingested with --embed?")?;
    // Code search must never blend agent-authored memory hits into deterministic
    // code results (issue #91): the shared vector index now also embeds
    // observation-class memory nodes, recalled only via `eg query semantic-memory`.
    matches.retain(|m| {
        m.kind
            .as_deref()
            .is_some_and(|k| k == "File" || k == "Symbol")
    });
    if let Some(repo) = selected.as_deref() {
        matches.retain(|m| index.owner_of(&m.record_id) == Some(repo));
    }
    matches.truncate(limit);

    if matches.is_empty() {
        eprintln!("no results — store may not have embeddings (re-run ingest with --embed)");
        std::process::exit(2);
    }

    for m in &matches {
        print_result(&SemanticResult::from_match(m, &index), format)?;
    }
    Ok(())
}

/// One agent-authored memory record recalled by meaning (issue #91).
///
/// Typed `agent_authored` so a consuming agent can never mistake a recalled
/// lesson for deterministic source truth. Every emitted row carries a citable
/// `source_handle`; a hit lacking provenance is excluded upstream, never
/// returned with empty provenance.
#[cfg(feature = "embeddings")]
#[derive(Serialize)]
struct MemoryRecallResult<'a> {
    record_id: &'a str,
    kind: &'static str,
    trust_class: &'static str,
    retrieval_score: f32,
    /// Citable source transcript / session / turn handle proving where the
    /// memory came from.
    source_handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ingested_at: Option<&'a str>,
    /// `verified` when the claim cites present verification evidence, else
    /// `unverified` — a structural, non-inferential trust signal (issue #64).
    review_state: &'static str,
    redacted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<&'a str>,
    /// Resolved code handles this memory cites (`OBSERVES`/`MENTIONS_SYMBOL`/…).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    linked_code_handles: Vec<String>,
    /// The recalled memory body (post-redaction stored text).
    memory_text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temporal_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by_records: Option<Vec<crate::temporal_status::TemporalReference>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contradicted_by: Option<Vec<crate::temporal_status::TemporalReference>>,
}

#[cfg(feature = "embeddings")]
impl PrintText for MemoryRecallResult<'_> {
    fn as_text(&self) -> String {
        format!(
            "{} [{}] {} score={:.4} author={} source={} review={}\n  {}",
            self.record_id,
            self.kind,
            self.trust_class,
            self.retrieval_score,
            self.agent_id.unwrap_or("(unknown)"),
            self.source_handle,
            self.review_state,
            self.memory_text,
        )
    }
}

/// Returns a trimmed, non-empty string slice, or `None` for a missing or
/// blank-only value. Used so an imported memory record carrying
/// `source_handle: ""` is treated as having no provenance rather than passing
/// the recall gate and being emitted with an empty handle (issue #91).
#[cfg(feature = "embeddings")]
fn non_empty(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|s| !s.trim().is_empty())
}

/// Resolves the repositories a memory record belongs to (issue #91).
///
/// Agent-memory nodes are not part of the code-graph containment topology, so
/// [`query::RepositoryIndex::owner_of`] returns `None` for them directly. A
/// memory record is attributed to a repository through the code it cites: any
/// cited code target that resolves to a repository-owned node scopes the memory
/// to that repository. Both citation shapes are honored — inline
/// `evidence_links` and standalone outgoing `GraphRecord::Edge` records (e.g.
/// the `link-evidence` `MENTIONS_SYMBOL` / `FAILED_ON` / `TOUCHED_FILE` edges) —
/// so imported memory that stores normalized edges is not dropped under `--repo`.
/// Returned sorted and deduplicated for deterministic selection.
#[cfg(feature = "embeddings")]
fn memory_repo_owners<'a>(
    record_id: &str,
    links: Option<&Vec<EvidenceLink>>,
    edges_from: &query::OutgoingEdgeIndex<'_>,
    index: &'a query::RepositoryIndex,
) -> Vec<&'a str> {
    if let Some(owner) = index.owner_of(record_id) {
        return vec![owner];
    }
    let mut owners: Vec<&str> = Vec::new();
    if let Some(links) = links {
        owners.extend(
            links
                .iter()
                .filter_map(|l| l.target_record_id.as_deref())
                .filter_map(|target| index.owner_of(target)),
        );
    }
    if let Some(out) = edges_from.get(record_id) {
        owners.extend(out.iter().filter_map(|(_, target)| index.owner_of(target)));
    }
    owners.sort_unstable();
    owners.dedup();
    owners
}

/// Resolves one evidence link to a citable code handle string when it points at
/// the code-graph domain.
#[cfg(feature = "embeddings")]
fn code_handle_from_link(
    link: &EvidenceLink,
    by_id: &BTreeMap<&str, &GraphRecord>,
) -> Option<String> {
    let is_code = link.target_domain == "codegraph"
        || matches!(
            link.relation.as_str(),
            "OBSERVES" | "MENTIONS_SYMBOL" | "TOUCHED_FILE"
        );
    if !is_code {
        return None;
    }
    if let Some(target_id) = link.target_record_id.as_deref()
        && let Some(GraphRecord::Node {
            repo_relative_path,
            name,
            ..
        }) = by_id.get(target_id).copied()
    {
        if let Some(path) = repo_relative_path {
            return Some(
                name.as_ref()
                    .map_or_else(|| path.clone(), |n| format!("{path}::{n}")),
            );
        }
        return Some(target_id.to_owned());
    }
    link.target_repo_relative_path
        .clone()
        .or_else(|| link.target_record_id.clone())
}

/// Decides whether a semantic hit is a recallable agent-memory record (issue #91).
///
/// A hit qualifies only when it is an agent-memory observation-class kind, can
/// cite where it came from (a `source_handle`, source artifact path, or session
/// handle), and — under `verified_only` — cites present verification evidence.
/// A hit lacking provenance is rejected here so it is excluded, never returned.
#[cfg(feature = "embeddings")]
fn is_recallable_memory(
    m: &SemanticMatch,
    by_id: &BTreeMap<&str, &GraphRecord>,
    edges_from: &query::OutgoingEdgeIndex<'_>,
    tombstoned: &query::TombstonedSet<'_>,
    verified_only: bool,
) -> bool {
    if !m
        .kind
        .as_deref()
        .is_some_and(|k| matches!(k, "Observation" | "Decision" | "Failure"))
    {
        return false;
    }
    let Some(record) = by_id.get(m.record_id.as_str()).copied() else {
        return false;
    };
    let GraphRecord::Node {
        session_id,
        source_handle,
        source_artifact_path,
        ..
    } = record
    else {
        return false;
    };
    // Provenance must be a present, non-blank handle: a record carrying only
    // empty strings is excluded, never emitted with an empty `source_handle`.
    let has_provenance = non_empty(source_handle.as_ref()).is_some()
        || non_empty(source_artifact_path.as_ref()).is_some()
        || non_empty(session_id.as_ref()).is_some();
    if !has_provenance {
        return false;
    }
    // Verified-only reuses the memory-audit structural rule (issue #64): a
    // resolvable, non-tombstoned verification record cited via VALIDATED_BY /
    // HAS_EVIDENCE / PRODUCED_EVIDENCE, on either an inline evidence link or an
    // outgoing edge. A triple-only citation stub never counts as verified.
    if verified_only && !query::is_verified_claim(record, by_id, edges_from, tombstoned) {
        return false;
    }
    true
}

/// Recalls prior agent memory by meaning, trust-separated from code (issue #91).
///
/// Embeds the natural-language query with the local model, runs the same vector
/// search the code path uses, then keeps only agent-memory observation-class
/// hits — each enriched with its provenance handle. A hit that cannot cite
/// where it came from is excluded, not returned. With `--verified-only`,
/// observations lacking cited verification evidence are excluded too.
#[cfg(feature = "embeddings")]
#[allow(clippy::too_many_lines)]
#[derive(Serialize)]
struct ExcludedRecallDiagnostic<'a> {
    record_id: &'a str,
    reason: &'static str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<Vec<crate::temporal_status::TemporalReference>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contradicted_by: Option<Vec<crate::temporal_status::TemporalReference>>,
}

#[cfg(feature = "embeddings")]
impl PrintText for ExcludedRecallDiagnostic<'_> {
    fn as_text(&self) -> String {
        format!("Excluded record {} due to: {}", self.record_id, self.reason)
    }
}

#[cfg(feature = "embeddings")]
#[allow(clippy::too_many_lines)]
fn query_semantic_memory(
    query: &str,
    data_dir: &Path,
    limit: usize,
    repo: Option<&str>,
    verified_only: bool,
    format: OutputFormat,
    supersession: crate::temporal_status::SupersessionMode,
) -> Result<()> {
    validate_existing_embedded_store(data_dir)?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
    let index = query::RepositoryIndex::build(&records);
    let selected = resolve_repo_scope(&index, repo);

    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let (edges_from, tombstoned) = query::verification_support_indexes(&records);
    let resolver = crate::temporal_status::TemporalResolver::build(&records);
    let mut excluded_recall_diagnostics = Vec::new();

    let query_vector = embed_query_text(query)?;

    // The shared vector index holds both code and memory; fetch a generous pool
    // and filter to memory so the `limit` bounds recalled memory, not the blend.
    let fetch = records.len().max(limit);
    let matches = sink
        .semantic_search(&query_vector, fetch)
        .with_context(|| "semantic search failed — was the store ingested with --embed?")?;

    let mut rows: Vec<MemoryRecallResult> = Vec::new();
    for m in &matches {
        // Trust separation + provenance exclusion (AC3): keep only agent-memory
        // observation-class hits that can cite where they came from.
        if !is_recallable_memory(m, &by_id, &edges_from, &tombstoned, verified_only) {
            continue;
        }
        let Some(record) = by_id.get(m.record_id.as_str()).copied() else {
            continue;
        };
        let GraphRecord::Node {
            text,
            summary,
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            ingested_at,
            confidence,
            source_handle,
            source_artifact_path,
            redaction_policy_version,
            superseded_by,
            evidence_links,
            ..
        } = record
        else {
            continue;
        };

        // Scope through the code this memory cites: memory nodes are not in the
        // containment topology, so a `--repo` filter must resolve the repository
        // from the linked code handles (inline links and outgoing edges), not the
        // memory record ID directly.
        let owners = memory_repo_owners(&m.record_id, evidence_links.as_ref(), &edges_from, &index);
        if let Some(repo) = selected.as_deref()
            && !owners.contains(&repo)
        {
            continue;
        }

        // `is_recallable_memory` guarantees a present, non-blank handle; pick the
        // first non-empty among source handle, artifact path, and session ID.
        let source_handle_value = non_empty(source_handle.as_ref())
            .or_else(|| non_empty(source_artifact_path.as_ref()))
            .or_else(|| non_empty(session_id.as_ref()))
            .unwrap_or_default()
            .to_owned();

        let verified = query::is_verified_claim(record, &by_id, &edges_from, &tombstoned);

        let linked_code_handles: Vec<String> = evidence_links
            .as_ref()
            .map(|links| {
                let mut handles: Vec<String> = links
                    .iter()
                    .filter_map(|l| code_handle_from_link(l, &by_id))
                    .collect();
                handles.sort();
                handles.dedup();
                handles
            })
            .unwrap_or_default();

        // Label with the selected repository when scoped (the membership filter
        // above guarantees it is among `owners`), so a memory citing code in
        // several repositories is never misattributed to a different one than the
        // user selected; otherwise fall back to the first owner deterministically.
        let (status, superseded_by_refs, contradicted_by_refs) =
            resolver.resolve_status(record.id());
        let is_superseded = status == "superseded" || status == "cycle";
        let is_contradicted = status == "contradicted";

        if is_superseded || is_contradicted {
            let reason = if is_superseded {
                "superseded"
            } else {
                "contradicted"
            };
            match supersession {
                crate::temporal_status::SupersessionMode::Exclude => {
                    excluded_recall_diagnostics.push(ExcludedRecallDiagnostic {
                        record_id: record.id(),
                        reason,
                        status: "excluded",
                        superseded_by: if superseded_by_refs.is_empty() {
                            None
                        } else {
                            Some(superseded_by_refs)
                        },
                        contradicted_by: if contradicted_by_refs.is_empty() {
                            None
                        } else {
                            Some(contradicted_by_refs)
                        },
                    });
                }
                crate::temporal_status::SupersessionMode::IncludeButFlag => {
                    let repository_id = selected.as_deref().or_else(|| owners.first().copied());
                    rows.push(MemoryRecallResult {
                        record_id: record.id(),
                        kind: record.node_kind_name().unwrap_or("Observation"),
                        trust_class: "agent_authored",
                        retrieval_score: m.score,
                        source_handle: source_handle_value,
                        agent_id: agent_id.as_deref(),
                        agent_kind: agent_kind.as_deref(),
                        session_id: session_id.as_deref(),
                        confidence: confidence.as_deref(),
                        observed_at: observed_at.as_deref(),
                        ingested_at: ingested_at.as_deref(),
                        review_state: if verified { "verified" } else { "unverified" },
                        redacted: redaction_policy_version.is_some(),
                        superseded_by: superseded_by.as_deref(),
                        linked_code_handles,
                        memory_text: text.as_deref().unwrap_or(summary.as_str()),
                        repository_id,
                        repository: repository_id.and_then(|id| index.display_of(id)),
                        temporal_status: Some(status.to_string()),
                        superseded_by_records: if superseded_by_refs.is_empty() {
                            None
                        } else {
                            Some(superseded_by_refs)
                        },
                        contradicted_by: if contradicted_by_refs.is_empty() {
                            None
                        } else {
                            Some(contradicted_by_refs)
                        },
                    });
                }
            }
        } else {
            let repository_id = selected.as_deref().or_else(|| owners.first().copied());
            rows.push(MemoryRecallResult {
                record_id: record.id(),
                kind: record.node_kind_name().unwrap_or("Observation"),
                trust_class: "agent_authored",
                retrieval_score: m.score,
                source_handle: source_handle_value,
                agent_id: agent_id.as_deref(),
                agent_kind: agent_kind.as_deref(),
                session_id: session_id.as_deref(),
                confidence: confidence.as_deref(),
                observed_at: observed_at.as_deref(),
                ingested_at: ingested_at.as_deref(),
                review_state: if verified { "verified" } else { "unverified" },
                redacted: redaction_policy_version.is_some(),
                superseded_by: superseded_by.as_deref(),
                linked_code_handles,
                memory_text: text.as_deref().unwrap_or(summary.as_str()),
                repository_id,
                repository: repository_id.and_then(|id| index.display_of(id)),
                temporal_status: match supersession {
                    crate::temporal_status::SupersessionMode::IncludeButFlag => {
                        Some(status.to_string())
                    }
                    crate::temporal_status::SupersessionMode::Exclude => None,
                },
                superseded_by_records: None,
                contradicted_by: None,
            });
        }
    }

    // Canonical ordering before truncation (AC7): equal-score ANN results can be
    // returned in arbitrary order, so sort by score descending then record ID
    // ascending so repeated runs print byte-identical output and the row chosen
    // at the `limit` boundary is stable.
    rows.sort_by(|a, b| {
        b.retrieval_score
            .total_cmp(&a.retrieval_score)
            .then_with(|| a.record_id.cmp(b.record_id))
    });
    rows.truncate(limit);

    if rows.is_empty() {
        eprintln!(
            "no memory results — store may lack embedded memory (re-run ingest with --embed) or all hits were filtered"
        );
        std::process::exit(2);
    }

    for row in &rows {
        print_result(row, format)?;
    }

    for diag in &excluded_recall_diagnostics {
        print_result(diag, format)?;
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
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;

    let query_vector = embed_query_text(query)?;
    let mut params = serde_json::json!({
        "query_vector": query_vector,
        "limit": limit as u64,
    });
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    let records = client
        .query_verb("semantic_search", &params, None)
        .map_err(|e| surface_daemon_selector_rejection(e, repo))?;

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

/// Natural-language query → evidence-backed context for the top-N semantic
/// matches, in a single read-only call (issue #90).
///
/// Embeds the query locally, ranks matches against the embedded store, then —
/// for each match clearing `min_score` — resolves the same trust-separated
/// context sections as `eg query context`, anchored on the match's record ID so
/// File-typed matches are first-class. A no-match (no hit clears the floor)
/// emits a stable diagnostic to stdout and exits 2.
#[cfg(feature = "embeddings")]
fn query_semantic_context(
    query: &str,
    data_dir: &Path,
    limit: usize,
    min_score: f32,
    repo: Option<&str>,
    supersession: crate::temporal_status::SupersessionMode,
) -> Result<()> {
    validate_existing_embedded_store(data_dir)?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
    let index = query::RepositoryIndex::build(&records);
    let selected = resolve_repo_scope(&index, repo);

    let query_vector = embed_query_text(query)?;

    // Over-fetch the whole index, not just `limit` raw hits: the shared vector
    // index also embeds agent-memory nodes (issue #91), so a query whose top
    // `limit` raw matches are memory would otherwise drop the code hits ranked
    // just behind them. Fetch the full pool so the code-kind filter below
    // recovers those code hits; the limit then bounds the filtered set.
    let fetch = records.len().max(limit);
    let mut matches = sink
        .semantic_search(&query_vector, fetch)
        .with_context(|| "semantic search failed — was the store ingested with --embed?")?;
    // `semantic-context` is a code-context bridge: never expand agent-authored
    // memory hits (issue #91). Mirror `query semantic` and keep only
    // deterministic code kinds before building leads.
    matches.retain(|m| {
        m.kind
            .as_deref()
            .is_some_and(|k| k == "File" || k == "Symbol")
    });
    if let Some(repo) = selected.as_deref() {
        matches.retain(|m| index.owner_of(&m.record_id) == Some(repo));
    }
    // Canonical ordering before truncation: equal-score ANN results can be
    // returned in arbitrary order, so sort by score descending then record ID
    // ascending so repeated runs choose the same rows at the `limit` boundary
    // and emit byte-identical output.
    matches.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    matches.truncate(limit);

    let leads: Vec<query::SemanticLead> = matches
        .iter()
        .map(|m| query::SemanticLead {
            record_id: m.record_id.clone(),
            name: m.name.clone(),
            repo_relative_path: m.repo_relative_path.clone(),
            score: m.score,
            span: m.span,
        })
        .collect();

    // Scope the record slice for context expansion when a repo is selected so
    // that ambiguity detection (candidate_record_ids) and the path-based file
    // fallback in record_context don't return IDs from other repos. Cross-
    // domain records (observations, artifacts, verification) are unowned and
    // always kept so that context sections remain fully populated.
    let records: Vec<GraphRecord> = if let Some(repo) = selected.as_deref() {
        records
            .into_iter()
            .filter(|r| index.owner_of(r.id()).is_none_or(|o| o == repo))
            .collect()
    } else {
        records
    };

    let bundle = query::semantic_context_bundle(&records, &leads, min_score);
    let resolver = crate::temporal_status::TemporalResolver::build(&records);

    if bundle.is_no_match() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "query": query,
                "min_score": min_score,
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let match_rows: Vec<SemanticContextMatch<'_>> = bundle
        .matches
        .iter()
        .map(|m| {
            let sections = build_context_sections(&m.context);
            let (observations, excluded) =
                apply_supersession(sections.observations, &resolver, supersession);
            let repository_id = index.owner_of(&m.lead.record_id);
            SemanticContextMatch {
                record_id: &m.lead.record_id,
                name: m.lead.name.as_deref(),
                repo_relative_path: m.lead.repo_relative_path.as_deref(),
                span: m.lead.span,
                score: m.lead.score,
                match_kind: m.anchor_kind.as_str(),
                repository_id,
                repository: repository_id.and_then(|id| index.display_of(id)),
                ambiguous: !m.candidate_record_ids.is_empty(),
                candidate_record_ids: m.candidate_record_ids.iter().map(String::as_str).collect(),
                source_facts: sections.source_facts,
                topology_edges: sections.topology_edges,
                observations,
                project_state: sections.project_state,
                artifacts: sections.artifacts,
                verification_evidence: sections.verification_evidence,
                unresolved: sections.unresolved,
                excluded,
            }
        })
        .collect();

    let response = SemanticContextResponse {
        ok: true,
        query,
        min_score,
        matches: match_rows,
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize semantic context")?;
    println!("{output}");
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

    // The shared vector index may also hold agent-memory nodes (issue #91). The
    // code-relevance gate must score only deterministic code hits, exactly like
    // `eg query semantic`, so over-fetch the full pool and filter to File/Symbol
    // before scoring; otherwise embedded memory could occupy top-k slots or
    // count as ambiguous-query false positives and corrupt the gate.
    let total_records = sink
        .read_all_records()
        .map(|r| r.len())
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;

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
            .semantic_search(&query_vector, total_records.max(top_k.max(3)))
            .with_context(|| {
                format!(
                    "semantic search failed for query {} — was the store ingested with --embed?",
                    query.id
                )
            })?;

        let hits: Vec<SearchHit> = matches
            .iter()
            .filter(|m| {
                m.kind
                    .as_deref()
                    .is_some_and(|k| k == "File" || k == "Symbol")
            })
            .take(top_k.max(3))
            .map(SearchHit::from)
            .collect();
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

/// Runs the agent-memory recall corpus evaluation against an embedded store
/// seeded with imported memory records (issue #91).
///
/// Reads each natural-language question, embeds it with the local model, runs
/// semantic search, keeps only recallable agent-memory hits (trust-separated
/// from code, provenance-bearing), then evaluates top-1/top-3/MRR against the
/// reviewed expected memory record IDs. Exits 1 with a diagnostic if the top-3
/// recall threshold is missed.
#[cfg(feature = "embeddings")]
fn eval_memory_recall_cmd(
    corpus_path: &Path,
    data_dir: &Path,
    top_k: usize,
    threshold: f64,
    verified_only: bool,
) -> Result<()> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_NAME, aletheia_embeddings,
    };
    use crate::memory_recall_eval::{
        MemoryHit, MemoryRecallCorpus, build_report, evaluate_query, format_diagnostic,
        print_report,
    };

    validate_existing_embedded_store(data_dir)?;

    let corpus = MemoryRecallCorpus::from_json_file(corpus_path)?;

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let (edges_from, tombstoned) = query::verification_support_indexes(&records);

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

    let mut results = Vec::new();
    for question in &corpus.questions {
        let embed_data = rt
            .block_on(aletheia_embeddings::embed_query(
                &[question.text.as_str()],
                &embedder,
                None,
            ))
            .with_context(|| format!("failed to embed question {}", question.id))?;

        let query_vector = aletheia_embeddings::embed_data_to_dense_iter(embed_data, Some(1))
            .next()
            .with_context(|| format!("no embedding returned for question {}", question.id))?
            .with_context(|| format!("embedding result not dense for question {}", question.id))?
            .embedding;

        // Fetch a generous pool, then narrow to recallable memory so `top_k`
        // bounds memory hits rather than the code+memory blend.
        let matches = sink
            .semantic_search(&query_vector, records.len().max(top_k))
            .with_context(|| {
                format!(
                    "semantic search failed for question {} — was the store ingested with --embed?",
                    question.id
                )
            })?;

        // Collect every recallable hit, then apply the canonical score/record-id
        // ordering before truncating to top-k: truncating the raw ANN order first
        // could drop a record that belongs in the canonical top 3 when scores tie
        // (and vary between runs). `evaluate_query` re-applies canonical ordering.
        let mut hits: Vec<MemoryHit> = matches
            .iter()
            .filter(|m| is_recallable_memory(m, &by_id, &edges_from, &tombstoned, verified_only))
            .map(|m| MemoryHit {
                record_id: m.record_id.clone(),
                score: m.score,
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.record_id.cmp(&b.record_id))
        });
        hits.truncate(top_k.max(3));

        results.push(evaluate_query(question, &hits));
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

    // Validate scenario classes immediately
    for scenario in &corpus.scenarios {
        match scenario.class.as_str() {
            "meaning_changed" | "structure_changed_only" | "text_changed_only" | "unchanged" => {}
            other => {
                anyhow::bail!(
                    "Unrecognized or invalid scenario class '{}' in scenario '{}'. Allowed classes are: meaning_changed, structure_changed_only, text_changed_only, unchanged",
                    other,
                    scenario.id
                );
            }
        }
    }

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

        let path = std::path::Path::new(&scenario.file_path);
        if path.is_absolute() {
            anyhow::bail!(
                "Corpus scenario file_path must be relative: {}",
                scenario.file_path
            );
        }
        for component in path.components() {
            match component {
                std::path::Component::Prefix(_) => {
                    anyhow::bail!(
                        "Corpus scenario file_path cannot contain a drive/prefix component: {}",
                        scenario.file_path
                    );
                }
                std::path::Component::ParentDir => {
                    anyhow::bail!(
                        "Corpus scenario file_path cannot escape directory via '..': {}",
                        scenario.file_path
                    );
                }
                std::path::Component::RootDir => {
                    anyhow::bail!(
                        "Corpus scenario file_path must be relative: {}",
                        scenario.file_path
                    );
                }
                _ => {}
            }
        }

        let file_path = temp_path.join(path);
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

        let before_list: Vec<&str> = scenario.before.split_whitespace().collect();
        let after_list: Vec<&str> = scenario.after.split_whitespace().collect();

        let before_lookup: HashSet<&str> = before_list.iter().copied().collect();
        let after_lookup: HashSet<&str> = after_list.iter().copied().collect();

        let added_token = after_list
            .iter()
            .find(|w| !before_lookup.contains(*w))
            .copied();
        let removed_token = before_list
            .iter()
            .find(|w| !after_lookup.contains(*w))
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

                let (resolved_path, _resolved_name, resolved_span) = query::resolve_drift_target(
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

fn query_symbol_all(
    records: &[GraphRecord],
    name: &str,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
    freshness_code: Option<&(String, &'static str)>,
) -> Result<()> {
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
        .filter_map(|r| symbol_result(r, name, index, records, &deleted))
        .collect();
    if let Some(repo) = selected_repo {
        results.retain(|r| r.repository_id == Some(repo));
    }

    if results.is_empty() {
        eprintln!("error: no match found for symbol `{name}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| (r.span.map(|s| s.start_line), r.record_id));
    stamp_freshness(&mut results, freshness_code);
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

fn symbol_result<'a>(
    record: &'a GraphRecord,
    name: &str,
    index: &'a query::RepositoryIndex,
    all_records: &'a [GraphRecord],
    deleted: &std::collections::BTreeSet<&str>,
) -> Option<SymbolResult<'a>> {
    if let GraphRecord::Node {
        kind: NodeKind::Symbol,
        name: node_name,
        ..
    } = record
        && node_name.as_deref() == Some(name)
    {
        symbol_row(record, index, all_records, deleted)
    } else {
        None
    }
}

/// Builds a `SymbolResult` row for any `Symbol` node record, without a name
/// predicate. Shared by the exact-name (`query symbol`) and partial-name
/// (`query symbols`, issue #102) paths so both emit the same row shape.
fn symbol_row<'a>(
    record: &'a GraphRecord,
    index: &'a query::RepositoryIndex,
    all_records: &'a [GraphRecord],
    deleted: &std::collections::BTreeSet<&str>,
) -> Option<SymbolResult<'a>> {
    let GraphRecord::Node {
        id,
        kind: NodeKind::Symbol,
        schema_version,
        name: node_name,
        repo_relative_path,
        span,
        visibility,
        signature,
        doc,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    let (completeness, _) = repo_relative_path
        .as_deref()
        .map_or(("complete", None), |path| {
            get_file_diagnostics(all_records, path, deleted)
        });
    let repository_id = index.owner_of(id);
    Some(SymbolResult {
        record_id: id,
        schema_version: *schema_version,
        name: node_name.as_deref().unwrap_or(""),
        kind: "Symbol",
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        visibility: visibility.as_deref(),
        signature: signature.as_deref(),
        doc: doc.as_deref(),
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        repository_id,
        repository: repository_id.and_then(|repo| index.display_of(repo)),
        freshness: None,
        extraction_completeness: completeness,
        diagnostics: None,
    })
}

// ---------------------------------------------------------------------------
// query symbols (partial-name pattern, issue #102)
// ---------------------------------------------------------------------------

/// Lists `Symbol` nodes whose name matches a substring or anchored `*`-glob
/// pattern against the structural store — no embedding model required.
///
/// Only `Symbol` node names are searched, so comments, string literals, and
/// doc text can never produce a match. Tombstoned current-state symbols are
/// excluded (parity with `file_defines`). Output is deterministic and
/// byte-stable: rows are sorted by `(repo_relative_path, span.start_line,
/// record_id)`.
fn query_symbols_matching(
    records: &[GraphRecord],
    pattern: &str,
    case_insensitive: bool,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
) -> Result<()> {
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
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name: Some(node_name),
                    ..
                } if query::symbol_name_matches(pattern, node_name, case_insensitive)
            )
        })
        .filter_map(|r| symbol_row(r, index, records, &deleted))
        .collect();
    if let Some(repo) = selected_repo {
        results.retain(|r| r.repository_id == Some(repo));
    }

    if results.is_empty() {
        eprintln!("error: no match found for pattern `{pattern}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| {
        (
            r.repo_relative_path,
            r.span.map(|s| s.start_line),
            r.record_id,
        )
    });
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query symbol --as-of <instant>
// ---------------------------------------------------------------------------

fn query_symbol_as_of(
    records: &[GraphRecord],
    name: &str,
    as_of: &str,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
    freshness_code: Option<&(String, &'static str)>,
) -> Result<()> {
    match query::symbol_as_of_valid_time_by_repo(records, name, as_of, index, selected_repo) {
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
        Ok(results) if results.is_empty() => {
            eprintln!("error: no match found for symbol `{name}` at or before `{as_of}`");
            std::process::exit(2);
        }
        Ok(results) => {
            // One best record per repository (plus one for any unattributed
            // legacy group): a single-result time view must never pick one
            // group implicitly on a collision (issue #67).
            if selected_repo.is_none() {
                let groups: std::collections::BTreeSet<Option<&str>> =
                    results.iter().map(|r| index.owner_of(r.id())).collect();
                if groups.len() > 1 {
                    exit_ambiguous_repository(&groups);
                }
            }
            let deleted = current_deleted_ids(records);
            let mut symbol_results: Vec<SymbolResult<'_>> = results
                .iter()
                .filter_map(|r| symbol_result(r, name, index, records, &deleted))
                .collect();
            stamp_freshness(&mut symbol_results, freshness_code);
            for result in &symbol_results {
                print_result(result, format)?;
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
    /// Stable `Repository` record ID owning this row; absent when the store
    /// carries no repository topology for the record (legacy graphs).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    /// Human-usable repository identity handle (e.g. `owner/name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    extraction_completeness: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<Vec<DiagnosticRef<'a>>>,
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
fn tx_symbol_row<'a>(
    record: &'a GraphRecord,
    index: &'a query::RepositoryIndex,
    all_records: &'a [GraphRecord],
    deleted: &std::collections::BTreeSet<&str>,
) -> Option<TxSymbolRow<'a>> {
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
    let (completeness, _) = repo_relative_path
        .as_deref()
        .map_or(("complete", None), |path| {
            get_file_diagnostics(all_records, path, deleted)
        });
    // Prefer the explicit node `domain` field; otherwise fall back to the
    // kind-derived domain (a `'static str`).
    let domain_str = domain
        .as_deref()
        .unwrap_or_else(|| crate::schema_version::domain_for_node_kind("Symbol"));
    let repository_id = index.owner_of(id);
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
        repository_id,
        repository: repository_id.and_then(|repo| index.display_of(repo)),
        extraction_completeness: completeness,
        diagnostics: None,
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

#[allow(clippy::too_many_arguments)]
fn query_symbol_tx_as_of(
    records: &[GraphRecord],
    name: &str,
    tx_as_of: &str,
    as_of: Option<&str>,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
) -> Result<()> {
    // Repository scope applies to the record set BEFORE temporal resolution,
    // not to the row list afterwards: a forked repository's descendant commits
    // must not drive this repository's removal or supersession logic
    // (issue #67). Store-wide transaction bounds stay global so out-of-range
    // diagnostics keep reflecting the whole store.
    let scoped_records: Vec<GraphRecord> = selected_repo
        .map(|repo| {
            records
                .iter()
                .filter(|r| {
                    matches!(r, GraphRecord::Node { .. }) && index.owner_of(r.id()) == Some(repo)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    // The CLI loads the entire `--graph` file, so the unscoped store-wide
    // range is just the full record set: pass `None` to let the resolver
    // derive it.
    let (effective_records, store_bounds) = if selected_repo.is_some() {
        (
            scoped_records.as_slice(),
            query::store_transaction_bounds(records),
        )
    } else {
        (records, None)
    };
    match query::symbol_as_of_transaction_time(
        effective_records,
        name,
        tx_as_of,
        as_of,
        store_bounds,
    ) {
        Err(err) => {
            print_tx_error(&err.code, &err.message)?;
            std::process::exit(1);
        }
        Ok(result) => {
            let deleted = current_deleted_ids(records);
            let rows: Vec<TxSymbolRow<'_>> = result
                .records
                .iter()
                .filter_map(|r| tx_symbol_row(r, index, records, &deleted))
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
    repo: Option<&str>,
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
    let mut verb_params = serde_json::json!({ "name": name });
    if let Some(repo) = repo {
        verb_params["repo"] = serde_json::json!(repo);
    }
    let result = match client.query_verb_raw_with_as_of(
        "symbol_by_name",
        &verb_params,
        Some(&as_of_value),
    ) {
        Ok(r) => r,
        Err(e) => {
            let e = surface_daemon_selector_rejection(e, repo);
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
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
    freshness_code: Option<&(String, &'static str)>,
) -> Result<()> {
    // The ambiguity check is repository-scoped: a prefix that collides only
    // across the repository boundary is unambiguous within the selected repo.
    let matching_commits: std::collections::BTreeSet<&str> = records
        .iter()
        .filter(|r| {
            selected_repo.is_none_or(|repo| record_belongs_to_repo_for_commit_scan(r, index, repo))
        })
        .filter_map(|r| temporal_commit_if_prefix(r, prefix))
        .collect();

    if matching_commits.len() > 1 {
        eprintln!(
            "error: ambiguous commit prefix `{prefix}` matches {} commits",
            matching_commits.len()
        );
        std::process::exit(1);
    }

    let mut matches = query::symbols_at_commit(records, name, prefix);
    if let Some(repo) = selected_repo {
        matches.retain(|r| index.owner_of(r.id()) == Some(repo));
    } else {
        // Two clones of one history can share a commit SHA under distinct
        // repository identities: never pick one implicitly (issue #67).
        // Unattributed legacy rows form their own candidate group.
        let groups: std::collections::BTreeSet<Option<&str>> =
            matches.iter().map(|r| index.owner_of(r.id())).collect();
        if groups.len() > 1 {
            exit_ambiguous_repository(&groups);
        }
    }

    match matches.into_iter().next() {
        None => {
            eprintln!("error: no match found for symbol `{name}` at commit `{prefix}`");
            std::process::exit(2);
        }
        Some(record) => {
            let deleted = current_deleted_ids(records);
            if let Some(mut result) = symbol_result(record, name, index, records, &deleted) {
                stamp_freshness(std::slice::from_mut(&mut result), freshness_code);
                print_result(&result, format)?;
            }
        }
    }
    Ok(())
}

/// Returns `true` when a record participates in `repo` for the purposes of
/// the commit-prefix ambiguity scan: nodes by direct ownership, edges by the
/// ownership of either endpoint (edge records themselves carry no owner).
fn record_belongs_to_repo_for_commit_scan(
    record: &GraphRecord,
    index: &query::RepositoryIndex,
    repo: &str,
) -> bool {
    match record {
        GraphRecord::Node { id, .. } => index.owner_of(id) == Some(repo),
        GraphRecord::Edge { source, target, .. } => {
            index.owner_of(source) == Some(repo) || index.owner_of(target) == Some(repo)
        }
        GraphRecord::Tombstone { .. } => false,
    }
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

fn query_file(
    records: &[GraphRecord],
    path: &str,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
    freshness_code: Option<&(String, &'static str)>,
) -> Result<()> {
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
        repo_relative_path.as_deref() == Some(path)
            && !deleted.contains(id.as_str())
            && selected_repo.is_none_or(|repo| index.owner_of(id) == Some(repo))
    });

    let (completeness, diags) = get_file_diagnostics(records, path, &deleted);
    let mut results: Vec<SymbolResult<'_>> = Vec::new();
    // Same-path rows excluded by the repository scope: counted and surfaced
    // through a diagnostic only — never mixed into the result set (issue #67).
    let mut excluded_rows: usize = 0;
    let mut excluded_repos: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

    let mut is_first = true;
    for r in records {
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
            continue;
        };
        if repo_relative_path.as_deref() != Some(path) {
            continue;
        }
        if temporal.is_none() && deleted.contains(id.as_str()) {
            continue;
        }
        let repository_id = index.owner_of(id);
        if let Some(repo) = selected_repo
            && repository_id != Some(repo)
        {
            excluded_rows += 1;
            if let Some(other) = repository_id {
                excluded_repos.insert(other);
            }
            continue;
        }
        results.push(SymbolResult {
            record_id: id,
            schema_version: *schema_version,
            name: name.as_deref().unwrap_or(""),
            kind: "Symbol",
            repo_relative_path: repo_relative_path.as_deref(),
            span: *span,
            // Declaration-surface fields are a symbol-contract lane: they are
            // returned by `eg query symbol`, not repeated on every row of the
            // per-file listing (which would re-serialize much of the file and
            // regress the `eg audit token-cost` savings gate).
            visibility: None,
            signature: None,
            doc: None,
            git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
            repository_id,
            repository: repository_id.and_then(|repo| index.display_of(repo)),
            freshness: None,
            extraction_completeness: completeness,
            diagnostics: if is_first {
                is_first = false;
                diags.clone()
            } else {
                None
            },
        });
    }

    if excluded_rows > 0 {
        let diag = serde_json::json!({
            "code": "excluded_other_repositories",
            "repo_relative_path": path,
            "excluded_repository_count": excluded_repos.len(),
            "excluded_row_count": excluded_rows,
        });
        eprintln!("{diag}");
    }

    if !file_exists || results.is_empty() {
        eprintln!("error: no match found for file `{path}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| (r.span.map(|s| s.start_line), r.record_id));
    stamp_freshness(&mut results, freshness_code);
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query drift
// ---------------------------------------------------------------------------

fn query_drift(
    records: &[GraphRecord],
    limit: usize,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
) -> Result<()> {
    // Rank first, then apply the repository scope, then truncate: the limit
    // must bound the scoped result set, not pre-empt it.
    let mut drifts = query::largest_semantic_drifts(records, usize::MAX);
    if let Some(repo) = selected_repo {
        drifts.retain(|r| index.owner_of(r.id()) == Some(repo));
    }
    drifts.truncate(limit);

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

        let (resolved_path, resolved_name, resolved_span) = query::resolve_drift_target(
            records,
            id,
            drift,
            drift_path.as_deref(),
            drift_name.as_deref(),
        );

        let repository_id = index.owner_of(id);
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
            repository_id,
            repository: repository_id.and_then(|repo| index.display_of(repo)),
            status: "drift is a lead, not proof",
        };
        print_result(&result, format)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query context (issue #38)
// ---------------------------------------------------------------------------

/// The five trust-separated context sections (plus topology edges and
/// unresolved references) rendered from a [`query::SymbolContext`].
///
/// Shared by `eg query context` and `eg query semantic-context` so both emit
/// byte-identical section shapes from the same builders.
struct ContextSections<'a> {
    source_facts: Vec<ContextSourceFact<'a>>,
    topology_edges: Vec<ContextTopologyEdge<'a>>,
    observations: Vec<ContextObservation<'a>>,
    project_state: Vec<ContextLinkedItem<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
}

/// Renders a resolved [`query::SymbolContext`] into the serializable section
/// views, reusing the existing per-record builders (`context_source_fact`,
/// `context_observation`, `context_linked_item`). `.copied()` collapses the
/// `&&GraphRecord` from `iter()` so each view borrows the record slice directly.
fn build_context_sections<'a>(ctx: &'a query::SymbolContext<'a>) -> ContextSections<'a> {
    ContextSections {
        source_facts: ctx
            .source_facts
            .iter()
            .copied()
            .filter_map(context_source_fact)
            .collect(),
        topology_edges: ctx
            .topology_edges
            .iter()
            .copied()
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
            .collect(),
        observations: ctx
            .observations
            .iter()
            .copied()
            .filter_map(context_observation)
            .collect(),
        project_state: ctx
            .project_state
            .iter()
            .copied()
            .filter_map(context_linked_item)
            .collect(),
        artifacts: ctx
            .artifacts
            .iter()
            .copied()
            .filter_map(context_linked_item)
            .collect(),
        verification_evidence: ctx
            .verification_evidence
            .iter()
            .copied()
            .filter_map(context_linked_item)
            .collect(),
        unresolved: ctx
            .unresolved
            .iter()
            .map(|u| ContextUnresolved {
                source_record_id: &u.source_record_id,
                target_handle: &u.target_handle,
                relation: &u.relation,
                target_domain: &u.target_domain,
                verification_status: "unresolved",
            })
            .collect(),
    }
}

fn apply_supersession<'a>(
    observations: Vec<query::ContextObservation<'a>>,
    resolver: &crate::temporal_status::TemporalResolver<'a>,
    mode: crate::temporal_status::SupersessionMode,
) -> (
    Vec<query::ContextObservation<'a>>,
    Vec<ExcludedDiagnostic<'a>>,
) {
    let mut filtered = Vec::new();
    let mut excluded = Vec::new();

    for mut obs in observations {
        let (status, superseded_by, contradicted_by) = resolver.resolve_status(obs.record_id);

        let is_superseded = status == "superseded" || status == "cycle";
        let is_contradicted = status == "contradicted";

        if is_superseded || is_contradicted {
            let reason = if is_superseded {
                "superseded"
            } else {
                "contradicted"
            };
            match mode {
                crate::temporal_status::SupersessionMode::Exclude => {
                    excluded.push(ExcludedDiagnostic {
                        record_id: obs.record_id,
                        reason,
                        superseded_by: if superseded_by.is_empty() {
                            None
                        } else {
                            Some(superseded_by)
                        },
                        contradicted_by: if contradicted_by.is_empty() {
                            None
                        } else {
                            Some(contradicted_by)
                        },
                    });
                }
                crate::temporal_status::SupersessionMode::IncludeButFlag => {
                    obs.temporal_status = Some(status.to_string());
                    obs.superseded_by = if superseded_by.is_empty() {
                        None
                    } else {
                        Some(superseded_by)
                    };
                    obs.contradicted_by = if contradicted_by.is_empty() {
                        None
                    } else {
                        Some(contradicted_by)
                    };
                    filtered.push(obs);
                }
            }
        } else {
            match mode {
                crate::temporal_status::SupersessionMode::IncludeButFlag => {
                    obs.temporal_status = Some(status.to_string());
                    filtered.push(obs);
                }
                crate::temporal_status::SupersessionMode::Exclude => {
                    filtered.push(obs);
                }
            }
        }
    }

    (filtered, excluded)
}

fn query_context_cmd(
    records: &[GraphRecord],
    symbol_name: &str,
    freshness: Option<(String, &'static str)>,
    supersession: crate::temporal_status::SupersessionMode,
) -> Result<()> {
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

    let sections = build_context_sections(&ctx);

    // Attach the freshness verdict only when every source fact belongs to the
    // repository the verdict was computed for (PR #186): `query context` has no
    // repository selector, so in a multi-repo store the same symbol can collect
    // facts from several repositories — presenting one checkout's verdict across
    // all of them would be misleading. Omit it when the response spans repos.
    let freshness_code = freshness.and_then(|(owner_id, code)| {
        let index = query::RepositoryIndex::build(records);
        let owners: std::collections::BTreeSet<Option<&str>> = ctx
            .source_facts
            .iter()
            .map(|record| index.owner_of(record.id()))
            .collect();
        (owners.len() == 1 && owners.contains(&Some(owner_id.as_str()))).then_some(code)
    });

    let resolver = crate::temporal_status::TemporalResolver::build(records);
    let (observations, excluded) =
        apply_supersession(sections.observations, &resolver, supersession);

    let response = ContextResponse {
        ok: true,
        symbol_name,
        freshness: freshness_code,
        source_facts: sections.source_facts,
        topology_edges: sections.topology_edges,
        observations,
        project_state: sections.project_state,
        artifacts: sections.artifacts,
        verification_evidence: sections.verification_evidence,
        unresolved: sections.unresolved,
        excluded,
    };

    let output = serde_json::to_string_pretty(&response).context("failed to serialize context")?;
    println!("{output}");
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn query_subsystem_cmd(
    records: &[GraphRecord],
    prefix: &str,
    _format: OutputFormat,
    supersession: crate::temporal_status::SupersessionMode,
) -> Result<()> {
    let ctx = match query::subsystem_context(records, prefix) {
        Ok(ctx) => ctx,
        Err(query::SubsystemPrefixError::Malformed { prefix: p }) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "malformed_prefix",
                    "prefix": p,
                    "message": "prefix must be non-empty after stripping trailing slashes"
                }
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(1);
        }
    };

    if ctx.is_no_match() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "prefix": prefix,
                "message": "no records found under the given prefix"
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

    let raw_observations: Vec<ContextObservation<'_>> = ctx
        .observations
        .iter()
        .filter_map(|r| context_observation(r))
        .collect();

    let resolver = crate::temporal_status::TemporalResolver::build(records);
    let (observations, excluded) = apply_supersession(raw_observations, &resolver, supersession);

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

    let semantic_drift: Vec<SubsystemDrift<'_>> = ctx
        .semantic_drift
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                semantic_drift: Some(drift_meta),
                ..
            } = r
            else {
                return None;
            };
            let (path, _, span) = query::resolve_drift_target(records, id, drift_meta, None, None);
            Some(SubsystemDrift {
                record_id: id,
                score: drift_meta.score,
                target_repo_relative_path: path,
                target_span: span,
                after_git_commit: Some(drift_meta.after_git_commit.as_str()),
            })
        })
        .collect();

    let response = SubsystemResponse {
        ok: true,
        prefix: ctx.prefix.as_str(),
        source_facts,
        topology_edges,
        observations,
        project_state,
        artifacts,
        verification_evidence,
        semantic_drift,
        unresolved,
        excluded,
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize subsystem context")?;
    println!("{output}");
    Ok(())
}

// ---------------------------------------------------------------------------
// change-impact query (issue #76)
// ---------------------------------------------------------------------------

/// One impact lead row in the change-impact response.
#[derive(Serialize)]
struct ImpactLeadJson<'a> {
    record_id: &'a str,
    kind: &'static str,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    /// Wire relation label (e.g. "CALLS", "REFERENCES", "IMPLEMENTS").
    relation: &'static str,
    /// "inbound" or "outbound" relative to the queried anchor.
    direction: &'static str,
    /// Stable edge record ID.
    edge_record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    edge_git_commit: Option<&'a str>,
    /// Which anchor record ID reached this lead.
    anchor_id: &'a str,
    /// Hop distance from the anchor (1-based).
    hop: usize,
    /// Every row is an impact LEAD — not proof of breakage (AC5).
    trust: &'static str,
}

/// One truncation record emitted when the per-group cap is hit.
#[derive(Serialize)]
struct ImpactTruncationJson {
    group: &'static str,
    returned: usize,
    total: usize,
    depth: usize,
}

/// Top-level change-impact response envelope.
#[derive(Serialize)]
struct ChangeImpactResponse<'a> {
    ok: bool,
    handle: &'a str,
    target_type: &'a str,
    target_ids: Vec<&'a str>,
    depth: usize,
    /// Per-response disclaimer: rows are LEADS, not proof (AC5).
    disclaimer: &'static str,
    direct_callers: Vec<ImpactLeadJson<'a>>,
    direct_callees: Vec<ImpactLeadJson<'a>>,
    referencing_files: Vec<ImpactLeadJson<'a>>,
    implementation_symbols: Vec<ImpactLeadJson<'a>>,
    containing_context: Vec<ImpactLeadJson<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    truncations: Vec<ImpactTruncationJson>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
    page: AuditPage,
}

const IMPACT_DISCLAIMER: &str = "Rows are impact LEADS to inspect before editing, not proof of breakage. \
     Absence of a lead is not proof a change is safe.";

fn impact_lead_json<'a>(lead: &'a query::ImpactLead<'a>) -> Option<ImpactLeadJson<'a>> {
    let GraphRecord::Node {
        id,
        kind,
        schema_version,
        name,
        repo_relative_path,
        span,
        temporal,
        valid_time,
        symbol_kind,
        language,
        ..
    } = lead.record
    else {
        return None;
    };
    let edge_git_commit = if let GraphRecord::Edge {
        temporal: Some(t), ..
    } = lead.edge
    {
        Some(t.git_commit.as_str())
    } else {
        None
    };
    Some(ImpactLeadJson {
        record_id: id,
        kind: kind.as_str(),
        schema_version: *schema_version,
        name: name.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        valid_time: valid_time
            .as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
        symbol_kind: symbol_kind.as_deref(),
        language: language.as_deref(),
        relation: lead.relation,
        direction: lead.direction.as_str(),
        edge_record_id: lead.edge.id(),
        edge_git_commit,
        anchor_id: lead.anchor_id,
        hop: lead.hop,
        trust: "impact_lead",
    })
}

#[allow(clippy::too_many_lines)]
fn query_change_impact_cmd(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    depth: usize,
) -> Result<()> {
    let target = match query::resolve_failure_handle(records, handle, index, repo_scope) {
        Ok(t) => t,
        Err(
            err @ (query::FailureHandleError::Ambiguous { .. }
            | query::FailureHandleError::Unsupported { .. }),
        ) => {
            eprintln!("{}", serde_json::to_string(&err)?);
            std::process::exit(1);
        }
    };

    // change-impact only operates on code handles (symbol or file). A handle that
    // resolves to a task or source/provenance record is out of scope and must be
    // rejected rather than misclassified as an empty symbol result.
    if matches!(
        target.kind,
        query::FailureTargetKind::Task | query::FailureTargetKind::Source
    ) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {} target; change-impact accepts only code symbol or file handles",
                target.kind.as_str()
            ),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }

    // A canonical codegraph ID can resolve to a non-File/Symbol node kind
    // (Repository, Module, Import, Commit, Change, …) while still mapping to a
    // `Symbol` target kind. Such handles are out of scope for change-impact and
    // must be rejected rather than traversed as an empty symbol result.
    if let Some(kind) = query::change_impact_unsupported_anchor_kind(records, &target) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {kind:?} node; change-impact accepts only code symbol or file handles"
            ),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }

    if target.is_empty() {
        let code = if target.stale {
            "stale_handle"
        } else {
            "no_match"
        };
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "handle": handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let ctx = query::change_impact_context(records, &target, depth, index, repo_scope);

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

    // Run redaction gate over every reached code-graph record
    for lead in ctx
        .direct_callers
        .iter()
        .chain(&ctx.direct_callees)
        .chain(&ctx.referencing_files)
        .chain(&ctx.implementation_symbols)
        .chain(&ctx.containing_context)
    {
        protected_payload_diagnostics(lead.record, &mut diagnostics);
    }

    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.source_record_id.cmp(b.source_record_id))
            .then_with(|| a.target_handle.cmp(b.target_handle))
            .then_with(|| a.relation.cmp(b.relation))
    });
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
    });

    let total_returned = ctx.direct_callers.len()
        + ctx.direct_callees.len()
        + ctx.referencing_files.len()
        + ctx.implementation_symbols.len()
        + ctx.containing_context.len();

    let truncations: Vec<ImpactTruncationJson> = ctx
        .truncations
        .iter()
        .map(|t| ImpactTruncationJson {
            group: t.group,
            returned: t.returned,
            total: t.total,
            depth: t.depth,
        })
        .collect();

    let response = ChangeImpactResponse {
        ok: true,
        handle,
        target_type: ctx.target_kind,
        target_ids: ctx.target_ids.iter().map(String::as_str).collect(),
        depth: ctx.depth,
        disclaimer: IMPACT_DISCLAIMER,
        direct_callers: ctx
            .direct_callers
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        direct_callees: ctx
            .direct_callees
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        referencing_files: ctx
            .referencing_files
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        implementation_symbols: ctx
            .implementation_symbols
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        containing_context: ctx
            .containing_context
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        truncations,
        diagnostics,
        page: AuditPage {
            cursor: None,
            has_more: false,
            returned: total_returned,
        },
    };

    let output = serde_json::to_string_pretty(&response)
        .context("failed to serialize change-impact context")?;
    println!("{output}");
    Ok(())
}

fn query_orient_cmd(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    match query::orientation_map(records, repo_id, limit) {
        Ok(map) => {
            match format {
                OutputFormat::Json => {
                    let envelope = serde_json::json!({
                        "ok": true,
                        "result": map,
                    });
                    println!("{}", serde_json::to_string_pretty(&envelope)?);
                }
                OutputFormat::Text => {
                    println!("Entry Points:");
                    for ep in &map.entry_points {
                        println!("- {} ({})", ep.repo_relative_path, ep.record_id);
                    }
                    println!("\nModule/File Tree:");
                    for node in &map.module_tree {
                        print_tree_node_text(node, 0);
                    }
                    println!("\nTop Referenced Symbols:");
                    for (i, sym) in map.top_referenced_symbols.iter().enumerate() {
                        let citation = sym.span.map_or_else(
                            || " [no_span_module_level]".to_string(),
                            |span| {
                                let path = sym.repo_relative_path.as_deref().unwrap_or("");
                                format!(" @ {path}:{}", span.start_line)
                            },
                        );
                        println!(
                            "{}. {} degree={}{} ({})",
                            i + 1,
                            sym.name,
                            sym.inbound_degree,
                            citation,
                            sym.record_id
                        );
                    }
                }
            }
            Ok(())
        }
        Err(query::OrientationError::EmptyGraph) => {
            let code = "empty_graph";
            let msg = "graph has zero code-graph nodes";
            match format {
                OutputFormat::Json => {
                    let envelope = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": code,
                            "message": msg
                        }
                    });
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => {
                    eprintln!("Error: {msg}");
                }
            }
            std::process::exit(3);
        }
        Err(query::OrientationError::NoEntryPoints) => {
            let code = "no_entry_points";
            let msg = "no entry-point files found in the graph";
            match format {
                OutputFormat::Json => {
                    let envelope = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": code,
                            "message": msg
                        }
                    });
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => {
                    eprintln!("Error: {msg}");
                }
            }
            std::process::exit(4);
        }
    }
}

fn query_lifeline_cmd(
    records: &[GraphRecord],
    symbol: &str,
    repo_id: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    match query::symbol_lifeline(records, symbol, repo_id) {
        Ok(events) => {
            match format {
                OutputFormat::Json => {
                    let envelope = serde_json::json!({
                        "ok": true,
                        "result": events,
                    });
                    println!("{}", serde_json::to_string_pretty(&envelope)?);
                }
                OutputFormat::Text => {
                    println!("Advisory temporal facts: where and when this symbol changed");
                    for ev in &events {
                        let citation = match (&ev.repo_relative_path, &ev.span) {
                            (Some(path), Some(span)) => {
                                format!(" @ {path}:{}-{}", span.start_line, span.end_line)
                            }
                            (Some(path), None) => {
                                let reason = ev
                                    .absent_span_reason
                                    .as_ref()
                                    .map_or_else(String::new, |r| format!(" [{r}]"));
                                format!(" @ {path}{reason}")
                            }
                            (None, _) => ev
                                .absent_span_reason
                                .as_ref()
                                .map_or_else(String::new, |reason| format!(" [{reason}]")),
                        };
                        let drift = match (ev.drift_score, &ev.drift_record_id) {
                            (Some(score), Some(id)) => format!(" drift={score:.4} ({id})"),
                            _ => " drift=absent".to_string(),
                        };
                        println!(
                            "[{}] commit={} record_id={}{}{}",
                            ev.event_type, ev.commit, ev.record_id, citation, drift
                        );
                    }
                }
            }
            Ok(())
        }
        Err(query::LifelineError::UnknownSymbol { query }) => {
            let code = "unknown_symbol";
            let msg = format!("symbol not found in the graph: {query}");
            match format {
                OutputFormat::Json => {
                    let envelope = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": code,
                            "message": msg
                        }
                    });
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => {
                    eprintln!("Error: {msg}");
                }
            }
            std::process::exit(5);
        }
        Err(query::LifelineError::AmbiguousSymbol { query, candidates }) => {
            let code = "ambiguous_symbol";
            let msg = format!("ambiguous symbol name '{query}' matches multiple symbols");
            match format {
                OutputFormat::Json => {
                    let envelope = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": code,
                            "message": msg,
                            "candidates": candidates
                        }
                    });
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => {
                    eprintln!("Error: {msg}. Candidates: {}", candidates.join(", "));
                }
            }
            std::process::exit(6);
        }
    }
}

fn print_tree_node_text(node: &query::ModuleTreeNode, indent: usize) {
    let indent_str = "  ".repeat(indent);
    let is_dir = matches!(node.kind, query::ModuleNodeKind::Directory);
    let suffix = if is_dir { "/" } else { "" };
    let citation = node.absent_handle_reason.as_ref().map_or_else(
        || {
            node.record_id.as_ref().map_or_else(
                || format!("@ {}", node.path),
                |id| format!("({id}) @ {}", node.path),
            )
        },
        |reason| {
            let reason_str = match reason {
                crate::citation_audit::AbsentHandleRule::NoSpanModuleLevel => {
                    "no_span_module_level"
                }
                crate::citation_audit::AbsentHandleRule::NoSpanDriftTargetUnresolved => {
                    "no_span_drift_target_unresolved"
                }
            };
            format!("[{reason_str}]")
        },
    );
    println!(
        "{}- {}{} {} ({} symbols)",
        indent_str, node.name, suffix, citation, node.symbol_count
    );
    for child in &node.children {
        print_tree_node_text(child, indent + 1);
    }
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
        excluded: Vec::new(),
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
pub(crate) fn trust_class_for(record: &GraphRecord) -> &'static str {
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

/// Builds one redaction-safe failed-attempt view from a context attempt.
fn failure_attempt_json<'a>(attempt: &query::FailureAttempt<'a>) -> FailureAttemptJson<'a> {
    let item = audit_item(&attempt.item);
    let (failure_kind, executed_at) = match attempt.item.record {
        GraphRecord::Node {
            failure_kind,
            executed_at,
            ..
        } => (failure_kind.as_deref(), executed_at.as_deref()),
        _ => (None, None),
    };
    FailureAttemptJson {
        item,
        resolution_status: attempt.status.as_str(),
        resolved_by: attempt.resolved_by,
        matched_target: attempt.matched_target,
        failure_kind,
        executed_at,
    }
}

#[allow(clippy::too_many_lines)]
fn query_failures_cmd(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
) -> Result<()> {
    let target = match query::resolve_failure_handle(records, handle, index, repo_scope) {
        Ok(t) => t,
        Err(
            err @ (query::FailureHandleError::Ambiguous { .. }
            | query::FailureHandleError::Unsupported { .. }),
        ) => {
            eprintln!("{}", serde_json::to_string(&err)?);
            std::process::exit(1);
        }
    };

    // A handle that resolved to nothing live in the store is a no-match (or a
    // stale handle when it named a tombstoned record). This is distinct from a
    // resolved target that simply has no recorded failures, which is a real
    // exit-0 empty answer below (AC6).
    if target.is_empty() {
        let code = if target.stale {
            "stale_handle"
        } else {
            "no_match"
        };
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "handle": handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let ctx = query::failure_history_context(records, &target);

    let runtime_failures: Vec<FailureAttemptJson<'_>> = ctx
        .runtime_failures
        .iter()
        .map(failure_attempt_json)
        .collect();
    let agent_failures: Vec<FailureAttemptJson<'_>> = ctx
        .agent_failures
        .iter()
        .map(failure_attempt_json)
        .collect();
    let superseding_successes: Vec<AuditItem<'_>> =
        ctx.superseding_successes.iter().map(audit_item).collect();
    let patch_artifacts: Vec<AuditItem<'_>> = ctx.patch_artifacts.iter().map(audit_item).collect();

    // Diagnostics: context diagnostics + protected-payload + redaction markers
    // for every reached record, exactly as the memory audit (AC6, AC8).
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
    for attempt in ctx.agent_failures.iter().chain(&ctx.runtime_failures) {
        protected_payload_diagnostics(attempt.item.record, &mut diagnostics);
        if let GraphRecord::Node {
            id,
            redaction_policy_version: Some(ver),
            ..
        } = attempt.item.record
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
    for item in ctx.superseding_successes.iter().chain(&ctx.patch_artifacts) {
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
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
            && a.target_domain == b.target_domain
    });

    let returned = runtime_failures.len()
        + agent_failures.len()
        + superseding_successes.len()
        + patch_artifacts.len();

    let response = FailureHistoryResponse {
        ok: true,
        target_handle: handle,
        target_type: ctx.target_kind,
        target_ids: ctx.target_ids.iter().map(String::as_str).collect(),
        runtime_failures,
        agent_failures,
        superseding_successes,
        patch_artifacts,
        agent_sessions: ctx.agent_sessions.iter().map(|r| r.id()).collect(),
        agents: ctx.agents.iter().map(|r| r.id()).collect(),
        diagnostics,
        page: AuditPage {
            cursor: None,
            has_more: false,
            returned,
        },
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize failure history")?;
    println!("{output}");
    Ok(())
}

/// Machine-readable report emitted by `eg query evidence-freshness` (issue #85).
#[derive(serde::Serialize)]
struct EvidenceFreshnessReport {
    ok: bool,
    stale_only: bool,
    /// Verdict tally across `current` / `drifted` / `unresolved` / `untemporal`.
    counts: std::collections::BTreeMap<&'static str, usize>,
    /// Stable diagnostic so an empty stale-only result is never silent (AC7).
    diagnostic: &'static str,
    /// Per-evidence-link freshness verdicts, deterministically ordered.
    verdicts: Vec<crate::evidence_freshness::FreshnessVerdictEntry>,
}

/// Handles `eg query evidence-freshness --graph <path> | --data-dir <dir> [--stale-only]`.
///
/// Strictly read-only: computes verdicts from records already in the store and
/// never creates, modifies, or deletes anything. Output carries only record IDs,
/// hashes, handles, spans, confidence, and redaction markers — never raw
/// observation text or other protected payloads (AC9).
fn query_freshness_cmd(records: &[GraphRecord], stale_only: bool) -> Result<()> {
    let all = crate::evidence_freshness::evidence_link_freshness(records);
    let counts = crate::evidence_freshness::verdict_counts(&all);

    let verdicts = if stale_only {
        crate::evidence_freshness::stale_only(all)
    } else {
        all
    };

    // In stale-only mode an empty result is reported with a stable diagnostic,
    // never silently as success-with-nothing (AC7).
    let diagnostic = if stale_only {
        if verdicts.is_empty() {
            crate::evidence_freshness::NO_STALE_DIAGNOSTIC
        } else {
            crate::evidence_freshness::STALE_PRESENT_DIAGNOSTIC
        }
    } else {
        "freshness_verdicts"
    };

    let report = EvidenceFreshnessReport {
        ok: true,
        stale_only,
        counts,
        diagnostic,
        verdicts,
    };

    let output =
        serde_json::to_string_pretty(&report).context("failed to serialize freshness report")?;
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

fn query_changes_cmd(
    records: &[GraphRecord],
    base: &str,
    head: &str,
    repo: Option<&str>,
) -> Result<()> {
    let index = query::RepositoryIndex::build(records);
    let repo_scope = resolve_repo_scope(&index, repo);
    match query::changes_context(records, base, head, repo_scope.as_deref()) {
        Ok(ctx) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct ChangesResponse<'a> {
                ok: bool,
                #[serde(flatten)]
                context: query::ChangesContext<'a>,
            }
            let response = ChangesResponse {
                ok: true,
                context: ctx,
            };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize changes context")?;
            println!("{output}");
            Ok(())
        }
        Err(err) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct ChangesErrorResponse {
                ok: bool,
                error: query::ChangesError,
            }
            let response = ChangesErrorResponse {
                ok: false,
                error: err.clone(),
            };
            let output =
                serde_json::to_string(&response).context("failed to serialize changes error")?;
            println!("{output}");
            let exit_code = match err {
                query::ChangesError::MissingCommit { .. } | query::ChangesError::EmptyHistory => 2,
                _ => 1,
            };
            std::process::exit(exit_code);
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
        use std::fmt::Write as _;
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        let line = self.span.map_or(0, |s| s.start_line);
        let commit = self.git_commit.map_or(String::new(), |c| format!(" [{c}]"));
        let freshness = self
            .freshness
            .map_or(String::new(), |code| format!(" (freshness: {code})"));
        let completeness = format!(" (extraction: {})", self.extraction_completeness);
        let mut text = format!(
            "{} ({}) @ {path}:{line}{commit}{freshness}{completeness}",
            self.name, self.kind
        );
        if let Some(visibility) = self.visibility {
            let _ = write!(text, "\n  visibility: {visibility}");
        }
        if let Some(signature) = self.signature {
            let _ = write!(text, "\n  signature: {signature}");
        }
        if let Some(doc) = self.doc {
            let _ = write!(text, "\n  doc: {doc}");
        }
        text
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

impl PrintText for WhoResult<'_> {
    fn as_text(&self) -> String {
        let author = match (self.author_name, self.author_email) {
            (Some(name), Some(email)) => format!("{name} <{email}>"),
            (Some(name), None) => name.to_owned(),
            (None, Some(email)) => format!("<{email}>"),
            (None, None) => "unknown".to_owned(),
        };
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        let freshness = self
            .freshness
            .map_or(String::new(), |code| format!(" (freshness: {code})"));
        format!(
            "{} last changed by {} in commit {} @ {} ({}){}",
            self.symbol_name, author, self.commit_sha, self.valid_time, path, freshness
        )
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

// ── eg protected ──────────────────────────────────────────────────────────────

/// Implements `eg protected get`: retrieves the verified payload to `out` (a
/// file) or stdout without buffering the whole payload in memory.
///
/// The bytes are ALWAYS staged to a temp and only released to the destination
/// after a fully verified copy — for `--out` to a uniquely named, exclusively
/// created temp in the destination directory (never follows a symlink) that is
/// renamed into place, and for stdout to an ANONYMOUS (unlinked) temp that is
/// rewound and streamed out.  This preserves verify-before-release for both
/// destinations (a failed get never truncates a `--out` file and never emits
/// unverified bytes to stdout).  The `--out` temp is removed on every path,
/// including the `process::exit` error paths; the anonymous stdout temp leaves
/// no named entry to leak and is reclaimed by the OS on process exit.
/// Exits the process on any failure with the documented JSON envelope/exit code.
fn protected_get_cmd(handle: &str, store: &Path, operator: &str, out: Option<&Path>) {
    use crate::protected::{GetStreamError, ProtectedStore};
    let ps = ProtectedStore::new(store);

    let exit_get_error = |e: crate::protected::GetError| -> ! {
        let is_not_found = e.code() == "payload_not_found";
        eprintln!("{}", e.to_json()); // to_json() is not Display; format is deliberate
        process::exit(if is_not_found { 2 } else { 1 });
    };
    let exit_output_error = |code: &str, message: String| -> ! {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "detail": { "message": message } }
        });
        eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
        process::exit(1);
    };
    // Error code + human label for the destination.
    let (err_code, dest_label): (&str, String) = out.map_or_else(
        || ("stdout_write_error", "stdout".to_owned()),
        |p| ("output_write_error", p.display().to_string()),
    );

    // Surfaces the RETRIEVAL diagnostic first when temp creation fails — verify
    // into a discard sink so a store/auth/malformed handle is reported as such
    // rather than masked by a staging error.  Returns only when retrieval would
    // have succeeded; otherwise exits with the get error.
    let stage_failed = |create_err: std::io::Error| -> ! {
        let mut sink = std::io::sink();
        match ps.get_to_writer(handle, operator, &mut sink) {
            Err(GetStreamError::Get(e)) => exit_get_error(e),
            // Retrieval succeeded (sink writes never fail), so the failure is
            // genuinely the staging destination.
            _ => exit_output_error(
                err_code,
                format!("failed to stage bytes for {dest_label}: {create_err}"),
            ),
        }
    };

    if let Some(out_path) = out {
        // --out: stage a NAMED temp in the destination directory so the final
        // release is an in-directory atomic rename onto the destination.  Keep
        // the verified `NamedTempFile` (and its open descriptor) BOUND through
        // the release step: do not convert it to a bare path and reopen, which
        // would let another local process swap the staging entry between
        // verification and release.  `process::exit` skips the destructor, so
        // the temp is removed explicitly (via `close()`/`PersistError`).
        let stage_dir = out_path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf);
        let mut tmp = match tempfile::Builder::new()
            .prefix(".eg-")
            .suffix(".partial")
            .tempfile_in(&stage_dir)
        {
            Ok(t) => t,
            Err(create_err) => stage_failed(create_err),
        };
        match ps.get_to_writer(handle, operator, tmp.as_file_mut()) {
            Ok(_) => {
                // Atomically persist the verified temp onto the destination
                // (rename of the SAME file object, replacing an existing file).
                if let Err(e) = tmp.persist(out_path) {
                    let _ = e.file.close(); // remove the staged temp
                    exit_output_error(
                        err_code,
                        format!("failed to write bytes to {dest_label}: {}", e.error),
                    );
                }
            }
            Err(GetStreamError::Get(e)) => {
                let _ = tmp.close();
                exit_get_error(e);
            }
            Err(GetStreamError::Output(e)) => {
                let _ = tmp.close();
                exit_output_error(
                    err_code,
                    format!("failed to write bytes to {dest_label}: {e}"),
                );
            }
        }
    } else {
        // stdout: stage into an ANONYMOUS temp file (unlinked at creation) so a
        // crash or kill never leaves a `.eg-*.partial` entry behind in the
        // system temp dir.  Verify-before-release still holds — nothing reaches
        // stdout until the full verified copy lands in the temp, which is then
        // rewound and streamed out.  The anonymous inode is reclaimed by the OS
        // on process exit, so no explicit cleanup is needed on the exit paths.
        let mut tmp = match tempfile::tempfile() {
            Ok(t) => t,
            Err(create_err) => stage_failed(create_err),
        };
        match ps.get_to_writer(handle, operator, &mut tmp) {
            Ok(_) => {
                let result = (|| -> std::io::Result<()> {
                    use std::io::Seek as _;
                    tmp.seek(std::io::SeekFrom::Start(0))?;
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    std::io::copy(&mut tmp, &mut lock)?;
                    Ok(())
                })();
                if let Err(e) = result {
                    exit_output_error(
                        err_code,
                        format!("failed to write bytes to {dest_label}: {e}"),
                    );
                }
            }
            Err(GetStreamError::Get(e)) => exit_get_error(e),
            Err(GetStreamError::Output(e)) => exit_output_error(
                err_code,
                format!("failed to write bytes to {dest_label}: {e}"),
            ),
        }
    }
}

/// Dispatches `eg bundle <subcommand>` (issue #68).
#[allow(clippy::too_many_lines)]
fn bundle_cmd(subcommand: BundleSubcommand) -> Result<()> {
    match subcommand {
        BundleSubcommand::Export {
            root_selector,
            graph,
            data_dir,
            out,
        } => {
            // For an embedded store, read from a throwaway read-only copy
            let store_copy = data_dir
                .as_ref()
                .map(|dir| match readonly_audit_store(dir) {
                    Ok(pair) => pair,
                    Err(error) => {
                        eprintln!("{error}");
                        std::process::exit(2);
                    }
                });
            let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());

            let records = match load_query_records(graph.as_deref(), effective_data_dir) {
                Ok(records) => records,
                Err(error) => {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "ok": false,
                            "error": {
                                "code": "load_failed",
                                "message": error.to_string()
                            }
                        })
                    );
                    std::process::exit(2);
                }
            };

            let version = env!("CARGO_PKG_VERSION");
            let bundle = match crate::bundle::export_bundle(&records, &root_selector, version) {
                Ok(b) => b,
                Err(error) => {
                    let (exit_code, error_code) = match &error {
                        crate::error::CodegraphError::InvalidArgument { .. } => {
                            (2, "invalid_argument")
                        }
                        _ => (1, "export_failed"),
                    };
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "ok": false,
                            "error": {
                                "code": error_code,
                                "message": error.to_string()
                            }
                        })
                    );
                    std::process::exit(exit_code);
                }
            };

            let json = match serde_json::to_string_pretty(&bundle) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "ok": false,
                            "error": {
                                "code": "serialization_failed",
                                "message": format!("failed to serialize bundle: {e}")
                            }
                        })
                    );
                    std::process::exit(2);
                }
            };
            if let Err(e) = fs::write(&out, &json) {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": "file_write_failed",
                            "message": format!("failed to write bundle to {}: {e}", out.display())
                        }
                    })
                );
                std::process::exit(2);
            }
            Ok(())
        }
        BundleSubcommand::Verify { path, format } => {
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(error) => {
                    let msg = format!("failed to read bundle file: {error}");
                    match format {
                        OutputFormat::Json => {
                            eprintln!(
                                "{}",
                                serde_json::json!({
                                    "ok": false,
                                    "error": {
                                        "code": "file_read_failed",
                                        "message": msg
                                    }
                                })
                            );
                        }
                        OutputFormat::Text => {
                            eprintln!("{msg}");
                        }
                    }
                    std::process::exit(2);
                }
            };
            let bundle: crate::bundle::EvidenceBundle = match serde_json::from_str(&content) {
                Ok(b) => b,
                Err(error) => {
                    let msg = format!("failed to parse bundle JSON: {error}");
                    match format {
                        OutputFormat::Json => {
                            eprintln!(
                                "{}",
                                serde_json::json!({
                                    "ok": false,
                                    "error": {
                                        "code": "parse_failed",
                                        "message": msg
                                    }
                                })
                            );
                        }
                        OutputFormat::Text => {
                            eprintln!("{msg}");
                        }
                    }
                    std::process::exit(2);
                }
            };

            let report = crate::bundle::verify_bundle(&bundle);

            let output = match format {
                OutputFormat::Json => serde_json::to_string_pretty(&report)
                    .map_err(|e| anyhow::anyhow!("failed to serialize report: {e}"))?,
                OutputFormat::Text => {
                    format!(
                        "Verification Verdict: {}\n\n- Integrity: {} ({})\n- Coverage: {} ({})\n- Safety: {} ({})\n",
                        if report.ok { "PASS" } else { "FAIL" },
                        if report.integrity.passed {
                            "PASS"
                        } else {
                            "FAIL"
                        },
                        report.integrity.detail,
                        if report.coverage.passed {
                            "PASS"
                        } else {
                            "FAIL"
                        },
                        report.coverage.detail,
                        if report.safety.passed { "PASS" } else { "FAIL" },
                        report.safety.detail,
                    )
                }
            };
            println!("{output}");

            if !report.ok {
                std::process::exit(1);
            }
            Ok(())
        }
        BundleSubcommand::Inspect { path } => {
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(error) => {
                    eprintln!("failed to read bundle file: {error}");
                    std::process::exit(2);
                }
            };
            let bundle: crate::bundle::EvidenceBundle = match serde_json::from_str(&content) {
                Ok(b) => b,
                Err(error) => {
                    eprintln!("failed to parse bundle JSON: {error}");
                    std::process::exit(2);
                }
            };

            let m = &bundle.manifest;
            println!("Evidence Bundle Manifest:");
            println!("  Root Selector: {}", m.root_selector);
            println!("  Source Query: {}", m.source_query);
            println!("  Repository Identity: {}", m.repository_identity);
            println!("  Egregore Version: {}", m.egregore_version);
            if let Some(s) = &m.snapshot {
                match s {
                    SnapshotHead::Commit { sha } => println!("  Snapshot HEAD Commit: {sha}"),
                    SnapshotHead::NoGit => println!("  Snapshot HEAD: no git"),
                    SnapshotHead::UnbornHead => println!("  Snapshot HEAD: unborn"),
                }
            }
            println!("  Omitted Records: {}", m.omitted_record_counts);
            println!("  Included Records by Trust Class:");
            for (tc, count) in &m.included_record_counts {
                println!("    {tc}: {count}");
            }
            println!("  Root Record IDs Selected: {:?}", m.root_record_ids);
            if !bundle.unresolved_links.is_empty() {
                println!("  Diagnostics (Unresolved Links):");
                for link in &bundle.unresolved_links {
                    let src_id = &link.source_id;
                    let tgt_handle = &link.target_handle;
                    let rel = &link.relation;
                    println!("    - Link from {src_id} to missing {tgt_handle} via {rel}");
                }
            }
            Ok(())
        }
    }
}

/// Dispatches `eg protected <subcommand>` (issue #60).
fn protected_cmd(subcommand: ProtectedSubcommand) -> Result<()> {
    use crate::protected::ProtectedStore;
    match subcommand {
        ProtectedSubcommand::Capture {
            manifest,
            store,
            protected_raw_artifacts,
            producer,
            captured_at,
        } => protected_capture_cmd(
            &manifest,
            &store,
            protected_raw_artifacts,
            producer.as_deref(),
            captured_at.as_deref(),
        ),
        ProtectedSubcommand::Get {
            handle,
            store,
            operator,
            out,
        } => {
            protected_get_cmd(&handle, &store, &operator, out.as_deref());
            Ok(())
        }
        ProtectedSubcommand::List { store } => {
            let ps = ProtectedStore::new(&store);
            let handles = match ps.list() {
                Ok(h) => h,
                Err(e) => {
                    let envelope = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": "store_io_error",
                            "detail": {
                                "message": format!(
                                    "failed to read protected store at {}: {e}",
                                    store.display()
                                )
                            }
                        }
                    });
                    eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
                    process::exit(1);
                }
            };
            let envelope = serde_json::json!({
                "ok": true,
                "count": handles.len(),
                "handles": handles,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&envelope)
                    .context("failed to serialise list response")?
            );
            Ok(())
        }
    }
}

/// Implements `eg protected capture`.
#[allow(clippy::too_many_lines)]
fn protected_capture_cmd(
    manifest_path: &Path,
    store_path: &Path,
    enabled: bool,
    producer: Option<&str>,
    captured_at_override: Option<&str>,
) -> Result<()> {
    use crate::protected::{CaptureEntry, ProtectedStore};

    // Validate: enabled mode requires a non-empty --producer.
    if enabled && producer.is_none() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "missing_field",
                "detail": {
                    "field": "producer",
                    "message": "--producer is required when --protected-raw-artifacts is set"
                }
            }
        });
        eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
        process::exit(1);
    }
    if enabled && producer.is_some_and(|p| p.trim().is_empty()) {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "invalid_field",
                "detail": {
                    "field": "producer",
                    "message": "--producer must not be empty when --protected-raw-artifacts is set"
                }
            }
        });
        eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
        process::exit(1);
    }

    // Read manifest — emit JSON envelope on failure so automation can distinguish
    // manifest errors from other stderr output.
    //
    // Emits the `manifest_read_error` envelope and exits 1.  Defined as a
    // closure so the regular-file/size guard and the read error path share one
    // emission site.
    let emit_manifest_error = |message: String| -> ! {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "manifest_read_error",
                "detail": { "message": message }
            }
        });
        eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
        process::exit(1);
    };

    // Read the manifest bound to a single no-follow, regular-file, size-capped
    // descriptor.  Opening once and reading that descriptor (rather than
    // stat-then-reopen) closes the TOCTOU window: a `--manifest` in a writable
    // location cannot be swapped for a FIFO, device, symlink, or much larger
    // file between a check and the read, so capture cannot be made to block or
    // allocate unbounded memory before emitting the JSON diagnostic.
    let manifest_content = match crate::protected::read_capped_regular_file(
        manifest_path,
        crate::protected::MAX_STORE_FILE_BYTES,
    ) {
        Ok(c) => c,
        Err(e) => emit_manifest_error(format!(
            "failed to read capture manifest at {}: {e}",
            manifest_path.display()
        )),
    };
    let mut entries: Vec<CaptureEntry> = Vec::new();
    for (i, line) in manifest_content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: CaptureEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(e) => {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": "invalid_manifest",
                        "detail": {
                            "line": i + 1,
                            "message": format!(
                                "manifest line {}: failed to parse JSON: {e}",
                                i + 1
                            )
                        }
                    }
                });
                eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
                process::exit(1);
            }
        };
        entries.push(entry);
    }

    let producer_id = producer.unwrap_or("preview");
    let producer_version = env!("CARGO_PKG_VERSION");
    let ts: String;
    let captured_at = if let Some(ov) = captured_at_override {
        ov
    } else {
        ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        &ts
    };

    let ps = ProtectedStore::new(store_path);
    let report = match ps.capture(
        &entries,
        producer_id,
        producer_version,
        captured_at,
        enabled,
    ) {
        Ok(r) => r,
        Err(e) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "store_io_error",
                    "detail": {
                        "message": format!(
                            "protected store I/O failed at {}: {e}",
                            store_path.display()
                        )
                    }
                }
            });
            eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
            process::exit(1);
        }
    };

    let envelope = serde_json::json!({
        "ok": true,
        "enabled": report.enabled,
        "stored_count": report.stored_count,
        "skipped_count": report.skipped_count,
        "entries": report.entries,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&envelope).context("failed to serialise capture response")?
    );
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
fn watch_cmd(
    data_dir: &Path,
    antigravity_dir: Option<&Path>,
    codex_dir: Option<&Path>,
    claude_dir: Option<&Path>,
    poll_interval: u64,
    embed: bool,
) -> Result<()> {
    let home = crate::watch::get_home_dir();

    let default_antigravity = home.as_ref().map(|h| h.join(".gemini/antigravity/brain"));
    let default_codex = home.as_ref().map(|h| h.join(".codex/sessions"));
    let default_claude = home.as_ref().map(|h| h.join(".claude/projects"));

    // Warn only if paths were explicitly requested but do not exist
    if let Some(p) = antigravity_dir.filter(|p| !p.exists()) {
        eprintln!(
            "[Watcher Warning] Specified Antigravity directory does not exist: {}",
            p.display()
        );
    }
    if let Some(p) = codex_dir.filter(|p| !p.exists()) {
        eprintln!(
            "[Watcher Warning] Specified Codex directory does not exist: {}",
            p.display()
        );
    }
    if let Some(p) = claude_dir.filter(|p| !p.exists()) {
        eprintln!(
            "[Watcher Warning] Specified Claude Code directory does not exist: {}",
            p.display()
        );
    }

    // Filter resolved paths to only watch them if they actually exist
    let resolved_antigravity = antigravity_dir
        .or(default_antigravity.as_deref())
        .filter(|p| p.exists());
    let resolved_codex = codex_dir
        .or(default_codex.as_deref())
        .filter(|p| p.exists());
    let resolved_claude = claude_dir
        .or(default_claude.as_deref())
        .filter(|p| p.exists());

    // Zero-watch validation: bail out if no valid directories remain
    if resolved_antigravity.is_none() && resolved_codex.is_none() && resolved_claude.is_none() {
        anyhow::bail!(
            "No valid agent directories to watch. Ensure at least one directory exists or was explicitly specified."
        );
    }

    crate::watch::watch(
        data_dir,
        resolved_antigravity,
        resolved_codex,
        resolved_claude,
        std::time::Duration::from_secs(poll_interval),
        embed,
        None,
    )?;

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
            repository_id: Some("codegraph:v1:repo"),
            repository: Some("acme/widget"),
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
            repository_id: None,
            repository: None,
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
