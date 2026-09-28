//! Command-line interface for Egregore.

#![allow(clippy::redundant_pub_crate, clippy::wildcard_imports)]

mod as_of;
mod at;
mod audit;
mod belief_timeline;
mod brief;
mod bundle;
mod candidates;
mod capture_bench;
mod capture_coverage;
mod capture_lanes;
mod capture_tests;
mod change_impact;
mod changes;
mod churn;
mod clones;
mod complexity;
mod config;
mod conflicts;
mod context;
mod coupling;
mod cycles;
mod daemon;
mod dead_code;
mod debt_markers;
mod decide;
mod deltas;
mod dep_usage;
mod deprecated_symbols;
mod deps;
mod diagnostics;
mod doctor;
mod drift;
mod error_context;
mod eval;
mod evidence;
mod evidence_freshness;
mod evidence_path;
mod export;
mod failure_history;
mod failure_hotspots;
mod file_at_point;
mod forget;
mod freshness_cmd;
mod implementors;
mod import;
mod index;
mod ingest;
#[cfg(feature = "embedded-aletheiadb")]
mod init;
mod inspect;
mod lanes;
mod lifeline;
mod link_logs;
pub(crate) mod locate;
mod log_deltas;
mod manifest_deps;
mod memory;
mod memory_audit;
mod orientation;
mod origin;
mod output;
mod ownership;
mod policy;
mod producer_drift;
mod protected;
mod public_api;
mod public_api_deltas;
mod query_bench;
mod recency;
mod record_budget;
mod records;
mod redaction_audit;
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
mod reembed_cmd;
mod repair_cmd;
mod repos;
mod resolve;
mod resolve_frames;
mod retire;
mod risk_markers;
mod scan;
mod scan_logs;
mod schema;
mod schema_constraints;
pub(crate) mod semantic;
mod subsystem;
mod symbols;
mod task;
mod task_ready;
mod transaction_time;
mod transitive_callees;
mod transitive_callers;
mod tx_delta;
mod undocumented;
mod unreferenced;
mod unsafe_sites;
mod unwrap_expect;
mod validate;
mod verification_coverage;
mod verification_freshness;
mod watch;
mod who;
// Appended (issue #225); kept at the end to minimize cross-lane merge conflicts.
mod path;
// Appended (issue #444); kept at the end to minimize cross-lane merge conflicts.
mod who_imports;
// Appended (issue #471); kept at the end to minimize cross-lane merge conflicts.
mod who_constructs;
// Appended (issue #248); kept at the end to minimize cross-lane merge conflicts.
mod forget_repo;
// Appended (issue #112); kept at the end to minimize cross-lane merge conflicts.
mod sessions;
// Appended (issue #265); kept at the end to minimize cross-lane merge conflicts.
mod blind_spots;
// Appended (issue #262); kept at the end to minimize cross-lane merge conflicts.
mod track_record;
// Appended (issue #259); kept at the end to minimize cross-lane merge conflicts.
mod session_retrospective;
// Appended (issue #239); kept at the end to minimize cross-lane merge conflicts.
mod verify_scan;
// Appended (issue #236); kept at the end to minimize cross-lane merge conflicts.
mod trust_audit;
// Appended (issue #228); kept at the end to minimize cross-lane merge conflicts.
mod diagram;
// Appended (issue #227); kept at the end to minimize cross-lane merge conflicts.
mod suppressions;
// Appended (issue #154); kept at the end to minimize cross-lane merge conflicts.
mod similar;

pub(crate) use as_of::*;
pub(crate) use at::*;
pub(crate) use audit::*;
pub(crate) use belief_timeline::*;
pub(crate) use brief::*;
pub(crate) use bundle::*;
pub(crate) use candidates::*;
pub(crate) use capture_bench::*;
pub(crate) use change_impact::*;
pub(crate) use changes::*;
pub(crate) use churn::*;
pub(crate) use clones::*;
pub(crate) use complexity::*;
pub(crate) use config::*;
pub(crate) use conflicts::*;
pub(crate) use context::*;
pub(crate) use coupling::*;
pub(crate) use cycles::*;
pub(crate) use daemon::*;
pub(crate) use dead_code::*;
pub(crate) use debt_markers::*;
pub(crate) use decide::*;
pub(crate) use deltas::*;
pub(crate) use deps::*;
pub(crate) use diagnostics::*;
pub(crate) use doctor::*;
pub(crate) use drift::*;
pub(crate) use error_context::*;
pub(crate) use eval::*;
pub(crate) use evidence::*;
pub(crate) use evidence_freshness::*;
pub(crate) use evidence_path::*;
pub(crate) use export::*;
pub(crate) use failure_history::*;
pub(crate) use failure_hotspots::*;
pub(crate) use file_at_point::*;
pub(crate) use forget::*;
pub(crate) use freshness_cmd::*;
pub(crate) use implementors::*;
pub(crate) use import::*;
pub(crate) use index::*;
pub(crate) use ingest::*;
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) use init::*;
pub(crate) use inspect::*;
pub(crate) use lanes::*;
pub(crate) use lifeline::*;
pub(crate) use link_logs::*;
pub(crate) use locate::*;
pub(crate) use log_deltas::*;
pub(crate) use manifest_deps::*;
pub(crate) use memory::*;
pub(crate) use memory_audit::*;
pub(crate) use orientation::*;
pub(crate) use origin::*;
pub(crate) use output::*;
pub(crate) use ownership::*;
pub(crate) use policy::*;
pub(crate) use producer_drift::*;
pub(crate) use protected::*;
pub(crate) use public_api::*;
pub(crate) use public_api_deltas::*;
pub(crate) use query_bench::*;
pub(crate) use recency::*;
pub(crate) use records::*;
pub(crate) use redaction_audit::*;
pub(crate) use repair_cmd::*;
pub(crate) use repos::*;
pub(crate) use resolve::*;
pub(crate) use resolve_frames::*;
pub(crate) use retire::*;
pub(crate) use risk_markers::*;
pub(crate) use scan::*;
pub(crate) use scan_logs::*;
pub(crate) use schema::*;
pub(crate) use schema_constraints::*;
pub(crate) use semantic::*;
pub(crate) use subsystem::*;
pub(crate) use symbols::*;
pub(crate) use task::*;
pub(crate) use task_ready::*;
pub(crate) use transaction_time::*;
pub(crate) use transitive_callees::*;
pub(crate) use transitive_callers::*;
pub(crate) use tx_delta::*;
pub(crate) use undocumented::*;
pub(crate) use unreferenced::*;
pub(crate) use unsafe_sites::*;
pub(crate) use unwrap_expect::*;
pub(crate) use validate::*;
pub(crate) use verification_coverage::*;
pub(crate) use verification_freshness::*;
pub(crate) use watch::*;
// Appended (issue #225); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use path::*;
// Appended (issue #444); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use who_imports::*;
// Appended (issue #471); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use who_constructs::*;
// Appended (issue #248); kept at the end to minimize cross-lane merge conflicts.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) use forget_repo::*;
// Appended (issue #112); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use sessions::*;
// Appended (issue #265); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use blind_spots::*;
// Appended (issue #262); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use track_record::*;
// Appended (issue #259); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use session_retrospective::*;
// Appended (issue #258); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use dep_usage::*;
// Appended (issue #249); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use deprecated_symbols::*;
// Appended (issue #239); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use verify_scan::*;
// Appended (issue #236); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use trust_audit::*;
// Appended (issue #228); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use diagram::*;
// Appended (issue #227); kept at the end to minimize cross-lane merge conflicts.
pub(crate) use suppressions::*;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use self::record_budget::BudgetedSection;
use crate::{
    adapters::{
        DanglingCitationPolicy, DryRunSink, ingest_records, ingest_records_with_policy,
        records_from_jsonl,
    },
    evidence::{
        ArtifactRequest, CommandEvidenceRequest, EvidenceProvenance, FailureRequest,
        ObservationRequest, VerificationRequest, build_artifact_records,
        build_command_evidence_records, build_failure_records, build_observation_records,
        build_verification_records,
    },
    freshness::{self, Freshness},
    history::HistoryWindow,
    identity,
    ir::{
        CallResolution, EdgeLabel, EmbeddingModel, EvidenceLink, Graph, GraphRecord, NodeKind,
        SnapshotHead, SourceSpan,
    },
    link_evidence::{self, LinkOptions},
    local_project, query, scan_repository_history_with_override,
    scan_repository_history_with_window, scan_repository_with_exclusions,
    schema_version::{RecordVersion, record_version},
    traj::{self, ImportOptions},
};

#[cfg(feature = "embedded-aletheiadb")]
use crate::adapters::EmbeddedAletheiaSink;
#[cfg(feature = "embeddings")]
use crate::adapters::SemanticMatch;
#[cfg(feature = "embedded-aletheiadb")]
use crate::adapters::preflight::{MAX_INTERNED_STRINGS, PreflightRefusal, check_ingest_capacity};
#[cfg(feature = "embedded-aletheiadb")]
use crate::adapters::{AdapterError, STORE_CONTENDED_CODE};
#[cfg(feature = "embedded-aletheiadb")]
use crate::daemon::{DaemonClient, DaemonConfig};
#[cfg(feature = "embedded-aletheiadb")]
use crate::incremental::scan_repository_incremental_excluding;
#[cfg(feature = "embedded-aletheiadb")]
use crate::repair;

#[derive(Debug, Parser)]
#[command(
    name = "egregore",
    version,
    about = "Manage agentic SWE knowledge graphs on AletheiaDB"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
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
        /// Keep source-embedded secrets in raw form instead of redacting them.
        #[arg(long)]
        raw_literals: bool,
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
        /// Keep source-embedded secrets in raw form instead of redacting them.
        #[arg(long)]
        raw_literals: bool,
        /// Replay only the N most-recent commits reachable from HEAD.
        ///
        /// Kept as a raw string so a non-integer value becomes the
        /// single-line JSON `invalid_window` diagnostic (issue #256), not
        /// Clap usage prose.
        #[arg(long)]
        max_commits: Option<String>,
        /// Replay commits committed at or after this RFC 3339 instant.
        #[arg(long)]
        since: Option<String>,
        /// Start of a `<from>..<to>` revision range (`from` excluded).
        /// Defaults the end to HEAD when `--to` is absent.
        #[arg(long)]
        from: Option<String>,
        /// End of a `<from>..<to>` revision range (`to` included). Without
        /// `--from`, replays everything reachable from this revision.
        #[arg(long)]
        to: Option<String>,
        /// Resume from a prior full `scan-history` frontier instead of
        /// replaying every commit (issue #224).
        ///
        /// Reads the history-replay tip recorded in `<path>`, replays only
        /// the commits that landed after it, and merges them with the
        /// frontier's records. The output is byte-identical to a fresh full
        /// replay. Cannot be combined with any window flag (`--max-commits`,
        /// `--since`, `--from`, `--to`): a windowed store is not a valid
        /// resume frontier.
        #[arg(long)]
        resume_from: Option<PathBuf>,
    },
    /// Extract runtime log signatures from a captured log file (issues #319 / #320).
    ///
    /// Parses one `plain-v1` or `jsonl-v1` log into deterministic,
    /// redaction-safe graph records — a `LogSource`, one `ErrorSignature` per
    /// `template-v1` fingerprint, capped `LogEvent` exemplars, and hourly
    /// `LogOccurrenceBucket` nodes — and writes them as JSONL. Raw log text
    /// never enters the graph; signatures are the producing program's own
    /// claims, deterministically parsed but never verified.
    ///
    /// An unrecognized (binary / non-UTF-8) input prints a machine-readable
    /// `{"ok":false,"error":{"code":"unrecognized_format",...}}` diagnostic to
    /// stdout and exits 1 with no partial output. See `docs/cli/scan-logs.md`.
    ScanLogs {
        /// Path to the log file to scan.
        log_path: PathBuf,
        /// Repository root for repository attribution.
        #[arg(long)]
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
        /// Capture the scanned log's POST-REDACTION raw bytes into the protected
        /// artifact store (issue #321). Disabled by default: without this flag no
        /// blob and no manifest entry are written. Requires `--protected-store`
        /// and `--producer`.
        #[arg(long)]
        protected_raw_artifacts: bool,
        /// Protected store directory (required when `--protected-raw-artifacts`
        /// is set). The graph JSONL never stores the protected handle.
        #[arg(long)]
        protected_store: Option<PathBuf>,
        /// Stable producer identity recorded as the authorised operator for the
        /// captured log blob (required when `--protected-raw-artifacts` is set).
        #[arg(long)]
        producer: Option<String>,
        /// Override the capture timestamp (RFC 3339) for the protected blob, for
        /// deterministic manifests in tests. Defaults to the scan's transaction
        /// time. Not part of the handle identity.
        #[arg(long)]
        captured_at: Option<String>,
    },
    /// Re-scan a repository twice and verify byte-for-byte stability (issue #239).
    ///
    /// Scans the target twice within one invocation, pinning the transaction
    /// time and repository identity for both scans so wall-clock and
    /// checkout-directory differences never cause a false mismatch, then
    /// reports whether the two canonical JSONL outputs are byte-for-byte
    /// identical. Exits 0 with `stable: true` on a deterministic repository;
    /// exits 1 with `stable: false` plus the differing record handle(s),
    /// first differing field, and first differing JSONL line when the scans
    /// disagree.
    ///
    /// Determinism is guaranteed only relative to a fixed producer (this
    /// binary) plus the pinned time and identity; cross-version extraction
    /// drift is the complementary `eg query producer-drift` check (#234).
    /// See `docs/cli/verify-scan.md`.
    VerifyScan {
        /// Repository path to scan.
        repo_path: PathBuf,
        /// Override the auto-detected repository identity.
        ///
        /// Forces `identity_source = operator_override`. Recommended when the
        /// two scans being compared run in different checkout directories
        /// (e.g. CRLF vs LF checkouts of the same commit), where
        /// path-derived identity would otherwise differ.
        #[arg(long)]
        repo_id_override: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Inspect the checked-in project configuration (issue #261).
    ///
    /// `eg config show` prints the effective resolved configuration as JSON:
    /// the discovered `egregore.toml` (found by walking up from the working
    /// directory, or `null`), every value, and where each value came from
    /// (`"config"` or `"default"`). An agent or CI step runs this to verify —
    /// and cite — exactly what governed a run. See `docs/cli/config.md`.
    Config {
        /// Config action.
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Capture a criterion benchmark run as citable verification-domain
    /// `BenchmarkRun` records (issue #237).
    ///
    /// CAPTURE-ONLY: never executes a benchmark runner. The caller runs the
    /// benchmarks; criterion's own `*/new/estimates.json` files (plus optional
    /// `*/base/estimates.json` saved baselines) are handed here via `--input`
    /// and parsed into one redaction-safe `BenchmarkRun` node per benchmark
    /// (verification domain, `benchmark_run` kind). Each record carries the
    /// benchmark id, the central estimate (`mean`, nanoseconds) and its
    /// confidence interval, the baseline verdict (`regression | improvement |
    /// unchanged | no_baseline`) plus the percentage delta, the captured
    /// commit, and a `source_artifact_path`/`source_artifact_hash` pinning the
    /// raw criterion artifact for later re-parsing. With `--graph` (a code
    /// graph from `eg scan`) each benchmark whose final `/`-segment resolves
    /// to exactly one `Symbol` mints `MENTIONS_SYMBOL` / `TOUCHED_FILE` edges;
    /// zero or two-plus matches mint a `Diagnostic` and join the envelope's
    /// `unresolved` section instead of a wrong edge. Re-capturing identical
    /// input is a byte-identical no-op (issue #130).
    ///
    /// Exit codes: 0 success; 1 usage/provenance error; 4 empty input
    /// (no `*/new/estimates.json` found); 5 unparseable estimates. See
    /// `docs/cli/capture-bench.md`.
    CaptureBench {
        /// Path to the criterion output directory (`target/criterion`
        /// shape: `<benchmark-id>/new/estimates.json`). Stored, never run.
        #[arg(long)]
        input: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
        /// Stable session identity (part of the record ID).
        #[arg(long)]
        session_id: String,
        /// Commit handle / external identifier (part of the record ID).
        #[arg(long)]
        commit: String,
        /// Suite name (part of the record ID and the node name).
        #[arg(long)]
        suite: String,
        /// The exact command that produced the criterion output. Stored, never
        /// executed.
        #[arg(long)]
        command: Option<String>,
        /// Caller-supplied RFC 3339 timestamp. Validated.
        #[arg(long)]
        executed_at: String,
        /// Optional repository identity (reserved for scoping).
        #[arg(long)]
        repo: Option<String>,
        /// Optional code graph (from `eg scan`) to resolve benchmark ids to
        /// Symbol/File.
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Input format. Only `criterion-estimates` (the default) is accepted.
        #[arg(long, default_value = "criterion-estimates")]
        format: String,
    },
    /// Capture a `cargo test` / libtest JSON run as a citable verification-domain
    /// `TestRun` record (issue #165).
    ///
    /// CAPTURE-ONLY: never executes a test runner. The caller runs the tests,
    /// captures the machine-readable libtest JSON event stream to a file, and
    /// hands that file plus the run metadata here. Parses it into a
    /// redaction-safe `TestRun` node (verification domain, `test_run` kind);
    /// raw failing-test output never enters the graph. With `--graph` (a code
    /// graph from `eg scan`) each test whose final `::`-segment resolves to
    /// exactly one `Symbol` mints `MENTIONS_SYMBOL` / `TOUCHED_FILE` and, for a
    /// failure, `FAILED_ON` edges; zero or two-plus matches mint a `Diagnostic`
    /// instead of a wrong edge.
    ///
    /// Exit codes: 0 success (partial stream still 0); 1 usage/provenance error;
    /// 3 protected-store I/O failure; 4 empty input; 5 unparseable input. A
    /// captured pass is a recorded observation of one run — "no captured
    /// failure" is not proof of correctness. See `docs/cli/capture-tests.md`.
    CaptureTests {
        /// Path to the file holding the libtest JSON event stream (stored, never run).
        #[arg(long)]
        input: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
        /// Stable session identity (part of the record ID).
        #[arg(long)]
        session_id: String,
        /// Commit handle / external identifier (part of the record ID).
        #[arg(long)]
        commit: String,
        /// Suite name (part of the record ID and the node name).
        #[arg(long)]
        suite: String,
        /// The exact command that produced the stream. Stored, never executed.
        #[arg(long)]
        command: String,
        /// The runner's exit status.
        #[arg(long)]
        exit_code: i64,
        /// Caller-supplied RFC 3339 timestamp. Validated.
        #[arg(long)]
        executed_at: String,
        /// Optional runner name (e.g. `libtest`, `cargo-nextest`).
        #[arg(long)]
        runner: Option<String>,
        /// Optional runner version.
        #[arg(long)]
        runner_version: Option<String>,
        /// Optional repository identity (reserved for scoping).
        #[arg(long)]
        repo: Option<String>,
        /// Optional code graph (from `eg scan`) to resolve test names to Symbol/File.
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Input format. Only `libtest-json` (the default) is accepted.
        #[arg(long, default_value = "libtest-json")]
        format: String,
        /// Capture the raw input bytes into the protected store (issue #60).
        /// Requires `--protected-store` and `--producer`.
        #[arg(long)]
        protected_raw_artifacts: bool,
        /// Protected store directory (required with `--protected-raw-artifacts`).
        #[arg(long)]
        protected_store: Option<PathBuf>,
        /// Producer identity for the captured blob (required with `--protected-raw-artifacts`).
        #[arg(long)]
        producer: Option<String>,
    },
    /// Capture a `cargo llvm-cov` run as a citable verification-domain
    /// `CoverageReport` record (issue #230).
    ///
    /// CAPTURE-ONLY: never executes a coverage tool. The caller runs the
    /// coverage tool, captures its machine-readable artefact (`cargo llvm-cov
    /// --json` or `--lcov`) to a file, and hands that file plus the run
    /// metadata here. Parses it into a redaction-safe `CoverageReport` node
    /// (verification domain, `coverage_report` kind) with overall and per-file
    /// line coverage (branch/region/function figures when the artefact has
    /// them); raw report text never enters the graph. With `--graph` (a code
    /// graph from `eg scan`) each measured file that resolves to a `File`
    /// mints a `TOUCHED_FILE` edge, and each named function (LCOV) whose final
    /// `::`-segment resolves to exactly one `Symbol` mints a
    /// `MENTIONS_SYMBOL` edge; files that do not resolve join a distinct
    /// `unresolved` set with a `Diagnostic` instead of a wrong edge.
    ///
    /// Exit codes: 0 success; 1 usage/provenance error; 3 protected-store I/O
    /// failure; 4 empty input; 5 unparseable input or missing coverage tool (a
    /// `coverage_tool_missing` diagnostic when the artefact carries the tool's
    /// own not-installed error text). A captured coverage number is a recorded
    /// observation of one run — lines executed by some test, never proof the
    /// behavior is correct. See `docs/cli/capture-coverage.md`.
    CaptureCoverage {
        /// Path to the file holding the coverage artefact (stored, never run).
        #[arg(long)]
        input: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
        /// Stable session identity (part of the record ID).
        #[arg(long)]
        session_id: String,
        /// Commit handle / external identifier (part of the record ID).
        #[arg(long)]
        commit: String,
        /// Optional suite/target name (node `name`; not part of the record ID).
        #[arg(long)]
        suite: Option<String>,
        /// The exact command that produced the artefact. Stored, never executed.
        #[arg(long)]
        command: String,
        /// The coverage command's exit status.
        #[arg(long)]
        exit_code: i64,
        /// Caller-supplied RFC 3339 timestamp. Validated.
        #[arg(long)]
        executed_at: String,
        /// Coverage tool identity (part of the record ID).
        #[arg(long, default_value = "cargo-llvm-cov")]
        tool: String,
        /// Optional coverage tool version.
        #[arg(long)]
        tool_version: Option<String>,
        /// Optional repository identity (reserved for scoping).
        #[arg(long)]
        repo: Option<String>,
        /// Optional repository root used to relativize absolute artefact paths.
        #[arg(long)]
        repo_root: Option<PathBuf>,
        /// Optional code graph (from `eg scan`) to resolve files/functions to File/Symbol.
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Input format. `llvm-cov-json` (the default) or `llvm-cov-lcov`.
        #[arg(long, default_value = "llvm-cov-json")]
        format: String,
        /// Capture the raw input bytes into the protected store (issue #60).
        /// Requires `--protected-store` and `--producer`.
        #[arg(long)]
        protected_raw_artifacts: bool,
        /// Protected store directory (required with `--protected-raw-artifacts`).
        #[arg(long)]
        protected_store: Option<PathBuf>,
        /// Producer identity for the captured blob (required with `--protected-raw-artifacts`).
        #[arg(long)]
        producer: Option<String>,
    },
    /// Link error signatures to the agent runs and tasks that preceded them
    /// (issue #323).
    ///
    /// Reads a union graph (`--graph` may be repeated to union multiple JSONL
    /// files, or `--data-dir`) of log records (`scan-logs`), agent-memory &
    /// verification records (`AgentRun` / `AgentTurn` / `CommandRun`), and
    /// project `Task` records, and emits `EMITTED_DURING` edges from each
    /// `ErrorSignature` to the runs/commands that produced it. Each edge carries
    /// a closed-set correlation basis: `content_hash_join` (a `CommandRun`'s
    /// captured stdout/stderr hash equals the signature's `LogSource` artifact
    /// hash — confidence 1.0) or `temporal_correlation` (the signature's valid
    /// time falls inside a same-repository run window — confidence 0.5, a
    /// correlation lead never causation). Task/issue links reuse
    /// `REFERENCES_TASK` (no new project label). Every edge is mirrored by an
    /// evidence link on the signature. Output is deterministic and byte-identical
    /// across runs; raw log / transcript / command text never enters the graph.
    /// See `docs/cli/link-logs.md`.
    LinkLogs {
        /// Path(s) to graph JSONL to union (log + code + agent records). Repeat
        /// the flag to union multiple files. Mutually exclusive with
        /// `--data-dir`.
        #[arg(long)]
        graph: Vec<PathBuf>,
        /// Embedded store holding the union of records (mutually exclusive with
        /// `--graph`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output JSONL path for the enriched records.
        #[arg(long)]
        out: PathBuf,
        /// Symmetric window tolerance in seconds for `temporal_correlation`
        /// (default 0 = strict). A run window `[start, end]` matches a signature
        /// time `t` when `start - tolerance <= t <= end + tolerance`.
        #[arg(long, default_value_t = 0)]
        tolerance: i64,
        /// Record this commit view (SHA or unique prefix) on emitted evidence
        /// links (requires a history graph). Mutually exclusive with --as-of.
        #[arg(long)]
        at: Option<String>,
        /// Record the commit view at or before this RFC 3339 instant on emitted
        /// evidence links. Mutually exclusive with --at.
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Resolve log backtrace frames to code-graph symbols (issue #322).
    ///
    /// Reads a log graph (from `scan-logs`) carrying `ErrorSignature` records
    /// with structured backtrace frames plus a code graph of `File`/`Symbol`
    /// records, and emits `FRAME_RESOLVES_TO` edges — each labeled with a
    /// closed-set `FrameResolution` (`resolved` / `ambiguous` / `path_only` /
    /// `unresolved`) and a `frame_index` — mirrored by evidence links on each
    /// `ErrorSignature`. Frames into the standard library or a dependency are
    /// classified `external` in a per-signature tally and mint no edge. A
    /// binding proves the frame NAMES the symbol, never that the symbol is at
    /// fault. Output is deterministic and byte-identical across runs; raw log
    /// text never enters the graph. See `docs/cli/resolve-frames.md`.
    ResolveFrames {
        /// Path to the log graph JSONL (from `scan-logs`). Omit when using
        /// `--data-dir`, which holds both the log and code graphs.
        log_graph: Option<PathBuf>,
        /// Path to the code graph JSONL (from `scan`). Required with
        /// `log_graph`; mutually exclusive with `--data-dir`.
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded store holding both the log and code graphs (mutually
        /// exclusive with the positional log graph and `--graph`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output JSONL path for the enriched records.
        #[arg(long)]
        out: PathBuf,
        /// Resolve against the code-graph state at this commit SHA or unique
        /// prefix (requires a history graph). Mutually exclusive with --as-of.
        #[arg(long)]
        at: Option<String>,
        /// Resolve against the code-graph state at the most recent commit at or
        /// before this RFC 3339 instant. Mutually exclusive with --at.
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Incrementally refresh an ingested store from working-tree edits.
    ///
    /// Reuses the incremental file-cache (BLAKE3 per-file hashes) and the
    /// tombstone-aware embedded re-ingest to update only changed/added/removed
    /// files, leaving all other graph records (agent-memory, project, artifact,
    /// verification) completely untouched.
    ///
    /// Shortest workflow (`docs/cli/refresh.md`):
    ///   1. `eg init <repo> --data-dir .egregore`
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
        ///
        /// Defaults to the `data_dir` pinned in `egregore.toml`, else `.egregore`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
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
        /// Embedding model identifier for `--embed`.
        ///
        /// Precedence: `--embed-model` \> `[embeddings].model` in `egregore.toml`
        /// \> built-in default. The resolved model is what the embedder loads
        /// and what the store's vector-index identity records; writing into an
        /// index built by a different model is refused rather than blending
        /// two vector spaces (issue #104).
        #[cfg(feature = "embeddings")]
        #[arg(long)]
        embed_model: Option<String>,
        /// Keep source-embedded secrets in raw form instead of redacting them.
        #[arg(long)]
        raw_literals: bool,
    },
    /// Re-embed an `--embed` store under a different local embedding model
    /// without re-scanning sources (issue #167).
    ///
    /// Every node that already carries a persisted `embedding` vector is
    /// re-embedded with the target model; the `#104` vector-index identity
    /// record is superseded in the same transaction, and the vector index is
    /// rebuilt when the dimension changes. No source scan, no graph
    /// re-extraction, no remote embedding service: the target model must
    /// already be available locally (a local directory, or a Hugging Face id
    /// present in the local HF cache) — it is never downloaded.
    ///
    /// Machine-readable report on `--format json` (`candidates`,
    /// `reembedded`, `skipped`, `failed` counts); exit `12` when the model is
    /// not available locally, `13` for `--dry-run` with work remaining, `14`
    /// when the store has nothing to migrate. See `docs/cli/re-embed.md`.
    #[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
    #[command(name = "re-embed")]
    Reembed {
        /// Embedded `AletheiaDB` data directory holding the `--embed` store.
        #[arg(long)]
        data_dir: PathBuf,
        /// Re-embed target model: a local model directory, or a Hugging Face
        /// model id resolved from the local HF cache only (never downloaded).
        #[arg(long)]
        model: String,
        /// Output format for the re-embed report.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
        /// Plan without mutating: prints the candidate count and target model,
        /// exits `13` while work remains, leaves the store byte-identical.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect a graph JSONL file, an embedded store, or a running daemon.
    ///
    /// `--data-dir` alone reads an embedded `AletheiaDB` store directly — no
    /// daemon, no network, no embeddings — and reports totals plus
    /// per-domain/per-kind/per-schema-version counts grouped by trust class
    /// (issue #125). The read is strictly read-only and the output is
    /// byte-identical across runs on an unchanged store. Unknown
    /// `(domain, kind, schema_version)` tuples are counted and labeled
    /// distinctly, never folded into known versions. See `docs/cli/inspect.md`
    /// for the documented JSON contract.
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
        ///
        /// Defaults to `text` for graph JSONL and daemon inspection, and to
        /// newline-delimited `json` for embedded `--data-dir` inspection.
        #[arg(long)]
        format: Option<OutputFormat>,
    },
    /// Catalog a store's repositories with the selectors that scope repository-scoped queries (issue #193).
    ///
    /// Reads an embedded `--data-dir` store directly — no daemon, no network,
    /// no embeddings — and lists every live repository with its stable record
    /// ID, its `identity_source`, the human-usable scope selector the `--repo`
    /// query lanes accept verbatim, a node count, and a recency hint. The
    /// read is strictly read-only and the output is byte-identical across
    /// runs on an unchanged store. When two repositories share a human
    /// selector, the collision is surfaced with each repository's distinct
    /// stable ID rather than silently de-duplicated. An empty store reports
    /// an explicit empty roster with the stable `empty_repository_roster`
    /// diagnostic. See `docs/cli/repos.md` for the documented JSON contract.
    Repos {
        /// Graph JSONL path to catalog.
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, conflicts_with = "graph")]
        data_dir: Option<PathBuf>,
        /// Output format: newline-delimited `json` (default) or `text`.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
    },
    /// Brief an agent on graph evidence scoped to the working-tree diff (issue #214).
    ///
    /// Resolves the uncommitted working-tree diff at `repo_path` (staged +
    /// unstaged, or staged-only with `--staged-only`) into `File` and `Symbol`
    /// handles and returns one JSON object with trust-separated sections:
    /// `source_facts`, `prior_failures` (runtime vs agent-authored),
    /// `observations`, `decisions`, `in_flight_tasks`,
    /// `verification_evidence`, `drift_warnings`, and a `store_coverage` map.
    /// Also carries the store-freshness staleness marker shared with
    /// `eg freshness`: a briefing over a dirty tree reads `stale_dirty`
    /// instead of failing or silently returning partial data.
    ///
    /// Strictly read-only: the diff is computed with read-only Git plumbing
    /// (`GIT_OPTIONAL_LOCKS=0`; no checkout, stash, or index refresh) and the
    /// store is only read, never re-scanned or mutated. Clean working trees
    /// return `ok: true` with empty but well-formed sections. See
    /// `docs/cli/brief.md`.
    Brief {
        /// Repository working-tree path to diff (defaults to the current directory).
        #[arg(default_value = ".")]
        repo_path: PathBuf,
        /// Graph JSONL store to read (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory to read (mutually exclusive with --graph).
        #[arg(long, conflicts_with = "graph")]
        data_dir: Option<PathBuf>,
        /// Restrict graph resolution to one stored repository by its identity ID.
        #[arg(long)]
        repo: Option<String>,
        /// Only consider staged (index vs HEAD) changes; unstaged working-tree
        /// changes and untracked files are excluded.
        #[arg(long = "staged-only", visible_alias = "staged")]
        staged_only: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
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
    /// Validate a graph JSONL for referential integrity before ingest (issue #103).
    ///
    /// One read-only pass that asserts the graph is referentially closed: every
    /// edge endpoint resolves to a present node, every DEFINES / CONTAINS /
    /// CALLS / IMPORTS / MENTIONS edge targets a node of an allowed kind, no
    /// tombstoned record is still referenced by a live edge, and no topology
    /// node is orphaned. Structural reference closure only — never parse
    /// correctness, semantic accuracy, schema-version compatibility, or
    /// extraction completeness. Local and offline; no network access.
    ///
    /// Exit 0 with zero diagnostics on a clean graph; exit 1 with one
    /// machine-readable JSONL diagnostic per defect (deterministic canonical
    /// order); exit 2 on a load error. Output carries only record IDs, defect
    /// categories, relation labels, paths, spans, and counts — never record
    /// payloads. See `docs/cli/validate.md`.
    Validate {
        /// Graph JSONL path to validate.
        graph: PathBuf,
        /// Output format (JSONL diagnostics by default).
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Build a persistent sidecar index for a graph JSONL (issue #447).
    ///
    /// Writes `<graph>.idx` next to the graph so targeted `eg query … --graph`
    /// lanes seek to the records they need instead of deserializing the whole
    /// file. The index is content-addressed on the graph's BLAKE3 + length: a
    /// query lane transparently cold-scans when the index is absent, stale, or
    /// corrupt, so results are byte-identical with or without it. This command
    /// is the only writer of the index and refuses (exit 2) any graph a cold
    /// load would also reject. See `docs/cli/index.md`.
    Index {
        /// Graph JSONL path to index.
        graph: PathBuf,
    },
    /// Bootstrap a repository into an embedded store in one command (issue #229).
    ///
    /// Runs the current-tree scan, embedded ingest with semantic embeddings
    /// (on by default; pass `--no-embed` to skip loudly), Git history replay,
    /// and history ingest end-to-end into a single embedded `AletheiaDB` data
    /// directory, then prints a machine-readable bootstrap report. This is
    /// the one-command equivalent of the manual onboarding sequence:
    /// `eg scan` → `eg ingest --embed` → `eg scan-history` → `eg ingest`.
    /// Fully offline except for the one-time embedding-model download.
    ///
    /// History replay reads Git objects read-only and never mutates the
    /// working tree or Git state. Intermediate graph JSONL lives in a system
    /// temporary directory, never in the repository being bootstrapped.
    ///
    /// Idempotent: a second `init` on an unchanged repository converges to a
    /// no-op, exits with code 3, and reports `status: "already_current"`.
    /// Optional stages (embeddings, history) may be skipped or partially
    /// fail — the report names every stage and the store stays non-corrupt —
    /// while required-stage failures print the report before exiting 1.
    /// A refusal before any write (capacity preflight, embedding-index
    /// identity conflict) exits 2 and leaves the store untouched.
    ///
    /// See `docs/cli/init.md`.
    Init {
        /// Working-tree path of the repository to bootstrap.
        #[arg(default_value = ".")]
        repo_path: PathBuf,
        /// Embedded `AletheiaDB` data directory to bootstrap into.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output format for the bootstrap report.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
        /// Skip semantic embedding generation (loudly reported in the report).
        #[cfg(feature = "embeddings")]
        #[arg(long)]
        no_embed: bool,
        /// Embedding model identifier.
        ///
        /// Precedence: `--embed-model` \> `[embeddings].model` in
        /// `egregore.toml` \> built-in default. The resolved model is recorded
        /// in the store's vector-index identity; a store whose index was
        /// built by a different model refuses the bootstrap (issue #104).
        #[cfg(feature = "embeddings")]
        #[arg(long)]
        embed_model: Option<String>,
        /// Override the auto-detected repository identity.
        #[arg(long)]
        repo_id_override: Option<String>,
        /// Include raw literal values in scanned records.
        #[arg(long)]
        raw_literals: bool,
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
        /// Embedding model identifier for `--embed`.
        ///
        /// Precedence: `--embed-model` \> `[embeddings].model` in `egregore.toml`
        /// \> built-in default. The resolved model is recorded in the store's
        /// vector-index identity, so `eg query semantic` can prove the query
        /// embedder shares the index's vector space (issue #261).
        #[cfg(feature = "embeddings")]
        #[arg(long)]
        embed_model: Option<String>,
        /// Bypass the ingest capacity preflight (issue #439). The preflight
        /// refuses fast when a graph is estimated to overflow the configured
        /// `AletheiaDB` string-interner cap (10M since the 0.2.0 upgrade);
        /// `--force` skips that estimate. A real capacity overflow during the
        /// write/persist remains fatal even with `--force`.
        #[cfg(feature = "embedded-aletheiadb")]
        #[arg(long)]
        force: bool,
        /// Dangling cross-domain evidence citation policy (issue #241):
        /// `quarantine` (default) skips records whose evidence citations cite
        /// no live node and ingests the rest of the batch; `reject-batch`
        /// writes nothing when any citation dangles. For `--adapter daemon`
        /// the policy is forwarded to the daemon in the ingest request.
        #[arg(long, value_enum, default_value_t = DanglingCitationPolicy::Quarantine)]
        dangling_citation_policy: DanglingCitationPolicy,
    },
    /// Export every persisted record from an embedded store as canonical JSONL.
    ///
    /// The inverse of `eg ingest` (distinct from the issue #68 evidence
    /// bundle): reads an embedded `AletheiaDB` store directly — no daemon, no
    /// network, no embeddings — and writes every persisted graph record (nodes,
    /// edges, valid-time tombstones, diagnostics, superseded versions, and
    /// unknown-version records) across all domains, in the exact record shapes
    /// `eg scan` / `eg ingest` emit (issue #155).
    ///
    /// Uses the same physical read surface as `eg inspect --data-dir`, so
    /// re-ingesting the output reproduces the inspect totals and
    /// per-domain/kind/schema-version counts. The one deliberate exception:
    /// a record hidden by `eg forget` (issue #231) is never re-emitted, so its
    /// retracted body cannot resurface. Output carries no header, manifest, or
    /// timestamp and is byte-identical across runs on an unchanged store; the
    /// read is strictly read-only. See `docs/cli/export.md`.
    ///
    /// The bare form (`eg export --data-dir <dir> --out <file.jsonl>`) is the
    /// issue #155 JSONL dump. The `scip` subcommand
    /// (`eg export scip --graph <f>|--data-dir <d> --out <index.scip>`, issue
    /// #233) emits a definitions-only SCIP code-intelligence index instead;
    /// see `docs/cli/export-scip.md`.
    Export {
        /// Optional export format subcommand (`scip`). Omitted → JSONL dump.
        #[command(subcommand)]
        format: Option<ExportSubcommand>,
        /// Embedded `AletheiaDB` data directory to export (bare JSONL form).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output JSONL path, created or overwritten (bare JSONL form).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Import a rust-swe-agent .traj trajectory file into agent-memory JSONL.
    ImportTraj {
        /// Path to the `.traj` trajectory file.
        traj_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
        /// Emit a secret-free JSON redaction summary to this path (`-` for
        /// stdout). Emitted even when zero redactions occur (issue #266).
        #[arg(long)]
        redaction_report: Option<PathBuf>,
    },
    /// Import a Codex session or rollout JSONL into agent-memory JSONL.
    ImportCodex {
        /// Path to the Codex session or rollout JSONL file.
        codex_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
        /// Emit a secret-free JSON redaction summary to this path (`-` for
        /// stdout). Emitted even when zero redactions occur (issue #266).
        #[arg(long)]
        redaction_report: Option<PathBuf>,
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
    /// Retract one persisted record from every current read surface (issue #231).
    ///
    /// Logical, auditable retraction for agent-authored or sensitive records:
    /// writes a citable retraction event (who retracted, when on the
    /// transaction-time axis, why, and the prior record handle) plus a
    /// tombstone, so structural, semantic, context, task, memory, audit,
    /// failures, changes, and MCP reads all stop returning the record's
    /// content. The bytes are not destroyed: a transaction-time view predating
    /// the retraction still reflects that the record existed then.
    ///
    /// Deterministic code-graph facts (File / Symbol / Import / CALLS edges /
    /// Commit / Change) are refused with a machine-readable error; they are
    /// reproducible from source and are corrected with `eg refresh` or a
    /// re-scan. Re-running on an already-retracted handle is a no-op success
    /// returning the original retraction event.
    ///
    /// Success prints a JSON envelope on stdout and exits 0. Failures print a
    /// machine-readable JSON envelope on stderr and exit 1 (refused or
    /// malformed) or 2 (handle not found). See `docs/cli/forget.md`.
    #[cfg(feature = "embedded-aletheiadb")]
    Forget {
        /// Stable record ID of the record to retract.
        handle: String,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
        /// Retraction reason recorded on the auditable retraction event.
        #[arg(long)]
        reason: String,
        /// Operator handle recorded as the retraction actor.
        #[arg(long, default_value = "operator")]
        retracted_by: String,
        /// Fixed RFC 3339 transaction time for deterministic output (useful for
        /// tests). Defaults to the current wall-clock instant.
        #[arg(long)]
        transaction_time: Option<String>,
    },
    /// Logically evict EVERY record of ONE repository from a shared multi-repo
    /// embedded store, across every domain (issue #248).
    ///
    /// Dry-run is the DEFAULT: without `--confirm` the command is strictly
    /// read-only, takes no write lease, and prints the eviction plan. `--confirm`
    /// writes one eviction event plus one tombstone per attributed record.
    /// Selector-resolution failures exit 2 (unknown / ambiguous); malformed
    /// fields exit 1. See `docs/cli/forget-repo.md`.
    #[cfg(feature = "embedded-aletheiadb")]
    ForgetRepo {
        /// Repository selector (record ID, `owner/name`, basename, remote URL,
        /// root commit SHA, or canonical path).
        selector: String,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
        /// Operator eviction reason recorded on the auditable eviction event.
        #[arg(long)]
        reason: String,
        /// Operator handle recorded as the eviction actor.
        #[arg(long, default_value = "operator")]
        evicted_by: String,
        /// Fixed RFC 3339 transaction time for deterministic output (useful for
        /// tests). Defaults to the current wall-clock instant.
        #[arg(long)]
        transaction_time: Option<String>,
        /// Perform the eviction. Without this flag the command is a read-only
        /// dry-run that mutates nothing.
        #[arg(long)]
        confirm: bool,
    },
    /// Retire an agent-memory record from recall without erasing its history
    /// (issue #156).
    ///
    /// Appends a `RetirementReceipt` node recording who retired the record,
    /// when, and why; the target record and its provenance are never mutated.
    /// The retired record is excluded from default `eg query semantic-memory`
    /// recall until reinstated. Retiring an already-retired record is an
    /// idempotent success returning the existing receipt.
    ///
    /// Only observation-class agent-memory records (Observation, Decision,
    /// Failure) are retireable. Deterministic code-graph facts are refused
    /// with a machine-readable error and zero writes.
    ///
    /// Reasons: `superseded` (requires `--superseded-by` naming a live,
    /// active, observation-class record; also writes a normal `SUPERSEDES`
    /// edge), `drifted` (requires `--evidence-handle` naming an evidence
    /// handle the target cited that no longer resolves), `contradicted`
    /// (optional `--evidence-handle` naming a live record), and
    /// `operator-decision` (no evidence required).
    ///
    /// Success prints a JSON envelope on stdout and exits 0. Failures print a
    /// machine-readable JSON envelope on stderr and exit 1 (refused or
    /// malformed) or 2 (handle not found). See `docs/cli/retire.md`.
    #[cfg(feature = "embedded-aletheiadb")]
    Retire {
        /// Stable record ID of the record to retire.
        handle: String,
        /// Graph JSONL file to append the receipt to (offline mode; mutually
        /// exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with
        /// --graph; defaults to `.egregore`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Typed retirement reason: `superseded`, `drifted`, `contradicted`,
        /// or `operator-decision` (`operator_decision` is also accepted).
        #[arg(long)]
        reason: String,
        /// Superseding record (required for `superseded`).
        #[arg(long)]
        superseded_by: Option<String>,
        /// Evidence handle (required for `drifted`; optional for
        /// `contradicted` and `operator-decision`).
        #[arg(long)]
        evidence_handle: Option<String>,
        /// Operator handle recorded as the retiring actor.
        #[arg(long, default_value = "operator")]
        retired_by: String,
        /// Fixed RFC 3339 transaction time for deterministic output (useful for
        /// tests). Defaults to the current wall-clock instant.
        #[arg(long)]
        transaction_time: Option<String>,
    },
    /// Reinstate a retired agent-memory record to active recall (issue #156).
    ///
    /// Appends a `ReinstatementReceipt` node; the retirement receipt stays in
    /// history and the record returns to default recall because the
    /// reinstatement is the newest receipt. Reinstating a record that is not
    /// retired is a refusal, not a silent no-op.
    ///
    /// Success prints a JSON envelope on stdout and exits 0. Failures print a
    /// machine-readable JSON envelope on stderr and exit 1 (refused or
    /// malformed) or 2 (handle not found). See `docs/cli/retire.md`.
    #[cfg(feature = "embedded-aletheiadb")]
    Reinstate {
        /// Stable record ID of the record to reinstate.
        handle: String,
        /// Graph JSONL file to append the receipt to (offline mode; mutually
        /// exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with
        /// --graph; defaults to `.egregore`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Free-text reason for the reinstatement, recorded on the receipt.
        #[arg(long, default_value = "")]
        reason: String,
        /// Operator handle recorded as the reinstating actor.
        #[arg(long, default_value = "operator")]
        reinstated_by: String,
        /// Fixed RFC 3339 transaction time for deterministic output (useful for
        /// tests). Defaults to the current wall-clock instant.
        #[arg(long)]
        transaction_time: Option<String>,
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
    /// Emit JSON Schemas for the persisted graph record contracts (issue #226).
    ///
    /// `eg schema export` prints draft 2020-12 documents derived from the Rust
    /// types in `src/ir.rs`, one per addressable `(domain, kind,
    /// schema_version)` tuple. `eg schema list` lists the addressable tuples.
    /// See `docs/cli/schema.md`.
    ///
    /// Exit codes:
    ///   0 — schemas emitted.
    ///   1 — I/O error writing `--out` files.
    ///   2 — unknown domain/kind/version tuple (machine-readable JSON error on
    ///       stderr).
    Schema {
        /// Schema action.
        #[command(subcommand)]
        action: SchemaAction,
    },
}

/// Subcommands for `import`.
#[derive(Debug, Subcommand)]
pub(crate) enum ImportSource {
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

/// Collapse clustering mode for `eg query semantic-memory --collapse`
/// (issue #163).
///
/// Only used by the `embeddings`-gated `SemanticMemory` subcommand, so the
/// enum is gated the same way: without the feature it would be dead code.
#[cfg(feature = "embeddings")]
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CollapseModeArg {
    /// Use embedding-cosine similarity when the store has a loaded vector
    /// index; degrade to normalized-text equality clustering when the store
    /// was never embedded. An unreadable index is refused, not degraded over.
    #[default]
    Auto,
    /// Cosine similarity over stored vectors; refused without a loaded
    /// vector index (no silent fallback).
    #[value(name = "embedding-cosine")]
    EmbeddingCosine,
    /// Normalized-text equality clustering over the stored (redacted) body
    /// text; works without embeddings and never loads a model.
    #[value(name = "normalized-text")]
    NormalizedText,
}

/// Test-vs-production role selector for the scopable code lanes
/// (`query symbol`, `query file`; issue #238).
///
/// Filters rows by the deterministic `role` stamped on each `Symbol` record
/// at scan time. `All` (the default) is a no-op: rows whose record predates
/// issue #238 (role unknown) are returned only under `All` — they match
/// neither `Production` nor `Test`, because an unknown role is not a
/// `Production` fact.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum RoleFilter {
    /// Return test and production rows (default); the only selector that
    /// returns rows whose role is unknown (pre-#238 records).
    #[default]
    All,
    /// Return only rows stamped `production`.
    Production,
    /// Return only rows stamped `test`.
    Test,
}

impl RoleFilter {
    /// Whether a row carrying `role` survives this selector. A `None` role
    /// (record predates issue #238) survives only [`RoleFilter::All`].
    pub(crate) fn matches(self, role: Option<crate::ir::SymbolRole>) -> bool {
        match self {
            Self::All => true,
            Self::Production => role == Some(crate::ir::SymbolRole::Production),
            Self::Test => role == Some(crate::ir::SymbolRole::Test),
        }
    }

    /// The stable CLI spelling: `"all"`, `"production"`, or `"test"`.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Production => "production",
            Self::Test => "test",
        }
    }
}

/// Which input sources `eg audit query-latency` measures (issue #255).
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum LatencySource {
    /// Measure only `--graph <JSONL>`.
    Graph,
    /// Measure only `--data-dir <embedded store>`.
    DataDir,
    /// Measure both sources (default).
    #[default]
    Both,
}

/// Subcommands for `config` (issue #261).
#[derive(Debug, Subcommand)]
pub(crate) enum ConfigAction {
    /// Print the effective resolved configuration as JSON: the discovered
    /// `egregore.toml` (or `null` when absent), every value, and where each
    /// value came from (`"config"` or `"default"`). See `docs/cli/config.md`.
    Show,
}

/// Subcommands for `schema` (issue #226).
#[derive(Debug, Subcommand)]
pub(crate) enum SchemaAction {
    /// Emit JSON Schema (draft 2020-12) documents for persisted record contracts.
    ///
    /// With `--domain`/`--kind`/`--schema-version`, emits only the matching
    /// contracts; a single matching document prints as one JSON object,
    /// several print as a JSON array. Without filters, emits every
    /// addressable `(domain, kind, schema_version)` tuple as a JSON array.
    /// With `--out <dir>`, writes one
    /// `<domain>.<record_type>.<kind>.v<version>.schema.json` file per tuple
    /// and prints a JSON summary instead of the documents.
    Export {
        /// Restrict to one domain (e.g. `codegraph`).
        #[arg(long)]
        domain: Option<String>,
        /// Restrict to one node kind or edge label (e.g. `Symbol`, `CALLS`).
        /// `"Tombstone"` selects tombstone contracts.
        #[arg(long)]
        kind: Option<String>,
        /// Restrict to one schema version.
        #[arg(long)]
        schema_version: Option<u32>,
        /// Write one file per tuple into this directory instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// List every addressable `(domain, kind, schema_version)` contract as JSON.
    List {
        /// Restrict to one domain.
        #[arg(long)]
        domain: Option<String>,
    },
}

/// Subcommands for `query`.
#[derive(Debug, Subcommand)]
pub(crate) enum QuerySubcommand {
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
        /// Scope results to one owning Cargo package by NAME (issue #117).
        ///
        /// The name is matched EXACTLY against the package attribution stamped
        /// on each record at scan time (resolved from the nearest enclosing
        /// `Cargo.toml`) — no case folding and no `-`/`_` normalization. A
        /// selector no package in the corpus carries exits 1 with a
        /// machine-readable `unknown_package_selector` naming the known
        /// packages, so a typo is never a silent empty answer. A name owned by
        /// more than one repository in a shared store exits 1
        /// (`ambiguous_package_selector`); add `--repo` to disambiguate.
        ///
        /// Spelled `--package`, not `--crate`: `eg query who-imports --crate`
        /// already means something else (rewriting a leading `crate::` in the
        /// query and in import paths).
        #[arg(long)]
        package: Option<String>,
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, so a symbol removed at HEAD does not
        /// appear. This is the DEFAULT when a source snapshot exists; the flag
        /// makes it explicit. Mutually exclusive with --all-history/--at/--as-of
        /// (enforced at runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): return the UNION of all commit
        /// snapshots so a symbol present only at an earlier commit still
        /// appears. Mutually exclusive with --at-head/--at/--as-of (enforced at
        /// runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Scope results by test-vs-production role (issue #238): `all`
        /// (default), `production`, or `test`. Rows whose record predates
        /// issue #238 carry no role and appear only under `all`.
        #[arg(long, default_value = "all")]
        role: RoleFilter,
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
        /// Scope results to one owning Cargo package by NAME (issue #117).
        ///
        /// The name is matched EXACTLY against the package attribution stamped
        /// on each record at scan time (resolved from the nearest enclosing
        /// `Cargo.toml`) — no case folding and no `-`/`_` normalization. A
        /// selector no package in the corpus carries exits 1 with a
        /// machine-readable `unknown_package_selector` naming the known
        /// packages, so a typo is never a silent empty answer. A name owned by
        /// more than one repository in a shared store exits 1
        /// (`ambiguous_package_selector`); add `--repo` to disambiguate.
        ///
        /// Spelled `--package`, not `--crate`: `eg query who-imports --crate`
        /// already means something else (rewriting a leading `crate::` in the
        /// query and in import paths).
        #[arg(long)]
        package: Option<String>,
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
        /// Pin the listing to the file's recorded state at this commit SHA or
        /// unique prefix (issue #158). Requires a `scan-history` store.
        /// Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Pin the listing to the file's recorded state at the most recent
        /// commit at or before this RFC 3339 instant (valid-time axis).
        /// Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Transaction-time selector (reserved for query file, not implemented).
        /// Returns a `not_implemented` error envelope rather than silently ignoring the flag.
        #[arg(long)]
        tx_as_of: Option<String>,
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
        /// Scope results by test-vs-production role (issue #238): `all`
        /// (default), `production`, or `test`. Rows whose record predates
        /// issue #238 carry no role and appear only under `all`.
        #[arg(long, default_value = "all")]
        role: RoleFilter,
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
    /// Group exact-duplicate Rust symbol bodies into citable clone classes (issue #216).
    ///
    /// A clone class is the set of live `Symbol` records sharing one
    /// normalized-body content hash (`blake3:` over the comment-stripped,
    /// whitespace-collapsed body), reported with citable handles only
    /// (record ID, repo-relative path, span, qualified name) — never raw
    /// source. Classes order by size descending, then hash; the answer is
    /// byte-stable across repeated runs on an unchanged store.
    ///
    /// Exit codes:
    ///   0 — report returned (an empty class list is a well-formed
    ///       `no_clone_classes` answer, not an error).
    ///   1 — load error, invalid --limit/--min-size, unknown/ambiguous --repo selector.
    ///
    /// Documented in `docs/cli/clones.md`.
    Clones {
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
        /// Minimum clone-class size: only classes with at least N members are
        /// reported (default 2).
        #[arg(long, default_value_t = query::CLONES_DEFAULT_MIN_SIZE)]
        min_size: usize,
        /// Maximum clone classes returned (default 50, max 500); excess
        /// classes are truncated with an explicit `truncated` signal.
        #[arg(long, default_value_t = query::CLONES_DEFAULT_LIMIT)]
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
        /// Scope results to a repo-relative directory prefix (issue #198).
        ///
        /// Segment-aware: `--under src/alpha` matches `src/alpha/foo.rs` but
        /// never `src/alphabet/x.rs`; the trailing-slash and bare forms resolve
        /// identically. The scope is applied BEFORE `--limit`, so `--limit N`
        /// returns the N best in-subsystem hits. Not supported with `--daemon`
        /// (scoped retrieval is a local-CLI surface for this slice).
        #[arg(long, conflicts_with = "daemon")]
        under: Option<String>,
        /// Maximum number of results (default 10).
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
        /// Embedding model used to vectorize the query text: a local model
        /// directory, or a Hugging Face model id resolved from the local HF
        /// cache only (never downloaded; issue #167).
        ///
        /// Defaults to the built-in model. The store's `#104` vector-index
        /// identity must match the resolved model; otherwise the query is
        /// refused — e.g. after `eg re-embed --model <m>`, query with
        /// `--embed-model <m>`.
        #[cfg(feature = "embeddings")]
        #[arg(long, conflicts_with = "daemon")]
        embed_model: Option<String>,
    },
    /// Find symbols semantically similar to a given symbol (issue #154).
    ///
    /// Ranks the symbol/file nodes most similar to an existing symbol by
    /// cosine similarity of stored embeddings — the "who else does this?"
    /// lookup for surfacing reusable code. The anchor's already-stored
    /// embedding is the query: no model is loaded, nothing is re-scanned,
    /// and the anchor itself is never returned.
    ///
    /// Rows are `SemanticResult` records (`record_id`, `name`,
    /// `repo_relative_path`, `span`, `score`), ordered by score descending
    /// with ties broken by `record_id` ascending; the answer is byte-stable
    /// across repeated runs on an unchanged store.
    ///
    /// Exit codes:
    ///   0 — at least one similar node returned.
    ///   1 — malformed/unresolvable/ambiguous handle, anchor without a stored
    ///       embedding, or a store ingested without `--embed`.
    ///   2 — the anchor resolved and is embedded, but no other node scored
    ///       above the similarity floor.
    ///
    /// Documented in `docs/cli/query.md`.
    #[cfg(feature = "embeddings")]
    Similar {
        /// Anchor handle: a qualified symbol name (e.g. `nested::Widget::new`)
        /// or a `record_id` (`codegraph:v1:…`).
        handle: String,
        /// Embedded `AletheiaDB` data directory (must be ingested with `--embed`).
        #[arg(long)]
        data_dir: PathBuf,
        /// Maximum number of similar nodes returned (default 10).
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
        /// Restrict recall to observations authored by this agent identity
        /// (issue #195). Composes with `--repo`, `--verified-only`,
        /// `--not-agent`, and `--supersession`; the default (no flag) leaves
        /// recall unscoped. Deterministic code-graph facts carry no `agent_id`
        /// and are never returned by an author-scoped recall.
        #[arg(long)]
        agent: Option<String>,
        /// Exclude observations authored by this agent identity (issue #195).
        /// Composes with `--agent`: a recalled observation must satisfy both
        /// selectors (when both name the same agent the exclusion wins).
        #[arg(long)]
        not_agent: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
        /// Supersession resolution mode for memory/observations.
        #[arg(long, value_enum, default_value_t = crate::temporal_status::SupersessionMode::Exclude)]
        supersession: crate::temporal_status::SupersessionMode,
        /// Collapse near-duplicate observations into one representative row
        /// per cluster (issue #163).
        ///
        /// The answer starts with an envelope naming the requested and actual
        /// modes, the similarity threshold, and source/representative counts,
        /// followed by one compact row per representative carrying
        /// `cluster_size`, `member_ids`, `cluster_observed_at_min`,
        /// `cluster_observed_at_max`, and `trust_spread`. Without this flag the
        /// recall answer is unchanged.
        #[arg(long)]
        collapse: bool,
        /// Cosine-similarity threshold for collapse clustering, in `[0.0, 1.0]`
        /// (issue #163). Pinned default 0.85, echoed in the answer envelope;
        /// ignored by normalized-text equality clustering. Raising it
        /// monotonically refines the partition (clusters only split, never
        /// merge); lowering it monotonically coarsens it.
        #[arg(long, default_value_t = crate::query::DEFAULT_COLLAPSE_SIMILARITY_THRESHOLD)]
        similarity_threshold: f32,
        /// Collapse clustering mode (issue #163). `auto` (default) uses
        /// embedding-cosine similarity when the store has a loaded vector
        /// index and degrades to normalized-text equality when the store was
        /// never embedded; an unreadable index is refused rather than
        /// degraded over.
        #[arg(long, value_enum, default_value_t = CollapseModeArg::Auto)]
        collapse_mode: CollapseModeArg,
        /// Include records retired from recall (issue #156), labeled with
        /// their `retirement_state`. By default retired records are excluded
        /// from recall with an exclusion diagnostic.
        #[arg(long)]
        include_retired: bool,
    },
    /// Retrieve evidence-backed context for a named symbol.
    ///
    /// Returns a structured JSON object with five trust-separated sections:
    /// `source_facts` (code-graph), `observations` (agent-authored),
    /// `project_state` (tasks/ACs), `artifacts`, and `verification_evidence`.
    /// Missing evidence links are surfaced as `unresolved` items.
    ///
    /// On no-match: emits `{"ok":false,"error":{"code":"no_match",...}}` to
    /// stdout and exits with code 2. On ambiguity (issue #192): emits
    /// `{"ok":false,"error":{"code":"ambiguous_symbol","candidates":[...],...}}`
    /// to stdout and exits with code 1. No synthesized prose; no hallucinated
    /// fallback records.
    Context {
        /// Symbol name to look up.
        name: String,
        /// Disambiguation selector (issue #192): a candidate `record_id` or
        /// `file:span` handle from an `ambiguous_symbol` response, to get
        /// context scoped to exactly that symbol.
        #[arg(long)]
        candidate: Option<String>,
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
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history (enforced at runtime
        /// with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a record removed at a later commit still appears. Mutually
        /// exclusive with --at-head (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Supersession resolution mode for memory/observations.
        #[arg(long, value_enum, default_value_t = crate::temporal_status::SupersessionMode::Exclude)]
        supersession: crate::temporal_status::SupersessionMode,
        /// Cap the total evidence records returned across all sections
        /// (issue #211).
        ///
        /// Sections fill in envelope order; each keeps its top-ranked prefix
        /// and a section that loses records is rendered as
        /// `{"returned","total","records"}` so every omission is counted. A
        /// budget at or above the natural section sizes leaves the answer
        /// byte-identical to the un-budgeted one. Omit for the full answer.
        #[arg(long)]
        max_records: Option<usize>,
    },
    /// Return the latest captured benchmark run for a benchmark id, symbol, or file.
    Bench {
        /// Benchmark id, symbol name, or file path to look up.
        target: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict to the run captured at this commit SHA or unique prefix.
        /// Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Return the latest run at or before this RFC 3339 instant
        /// (valid-time axis). Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
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
    /// Surface ready-to-dispatch tasks by resolving task dependencies (issue #161).
    TaskReady {
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
    /// Query the retirement state and receipt trail of one agent-memory
    /// record (issue #156).
    ///
    /// Reports whether the record is currently retired from recall (or was at
    /// the `--as-of` pin) plus the ordered receipt trail — retirements and
    /// reinstatements with actor, transaction time, reason, and metadata —
    /// without mutating anything. The target's original provenance is
    /// reported from the untouched record.
    ///
    /// Documented in `docs/cli/retire.md`.
    Retirement {
        /// Agent-memory record ID (`agent_memory:v1:...`) whose retirement
        /// state and receipt trail to report.
        handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Pin the recall state to this RFC 3339 transaction-time instant:
        /// receipts after the pin are invisible, so the reported state is
        /// what recall would have shown then.
        #[arg(long)]
        as_of: Option<String>,
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
        /// Cap the total evidence records returned across all sections
        /// (issue #211).
        ///
        /// Sections fill in envelope order; each keeps its top-ranked prefix
        /// and a section that loses records is rendered as
        /// `{"returned","total","records"}` so every omission is counted. A
        /// budget at or above the natural section sizes leaves the answer
        /// byte-identical to the un-budgeted one. Omit for the full answer.
        #[arg(long)]
        max_records: Option<usize>,
    },
    /// Trace the agent-belief timeline for one code target (issue #235).
    ///
    /// Resolves `<TARGET>` (a symbol record ID, a repo-relative `path`,
    /// a `path:line`, or an exact symbol name) to one `Symbol`/`File` record,
    /// then returns every live agent-authored `Observation`, `Decision`, and
    /// `Failure` citing it — oldest to newest by `observed_at` (falling back
    /// to `ingested_at`, record ID tiebreaker) — each carrying a
    /// machine-readable status (`current` | `superseded` | `contradicted` —
    /// closed vocabulary), the forward record that overrode or disputed it,
    /// and citation provenance. Output is metadata only: record IDs, handles,
    /// timestamps, confidence, and statuses — never record bodies.
    ///
    /// Statuses derive solely from author-written `superseded_by` /
    /// `SUPERSEDES` / `CONTRADICTS` data already in the store, resolved
    /// through the shared temporal resolver. Contradiction is not truth
    /// arbitration: both sides of a dispute stay visible.
    ///
    /// Exit codes:
    ///   0 — timeline returned (possibly an explicit empty `entries: []`).
    ///   1 — malformed handle or flag combination (JSON diagnostic, stderr).
    ///   2 — target handle unknown, stale, or ambiguous, or a `path:line`
    ///       with no enclosing symbol (JSON diagnostic on stderr; the
    ///       `error.code` distinguishes `no_match`, `stale_handle`,
    ///       `ambiguous`, and `no_symbol_at_line`).
    ///
    /// Documented in `docs/cli/belief-timeline.md` and `docs/cli/query.md`.
    BeliefTimeline {
        /// Symbol record ID, repo-relative file path, `path:line`, or exact
        /// symbol name.
        target: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict target handle resolution to one repository (issue #67).
        #[arg(long)]
        repo: Option<String>,
        /// Output format: `json` (default, machine-readable) or `text`.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
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
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history (enforced at runtime
        /// with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a record removed at a later commit still appears. Mutually
        /// exclusive with --at-head (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Cap the total evidence records returned across all sections
        /// (issue #211).
        ///
        /// Sections fill in envelope order; each keeps its top-ranked prefix
        /// and a section that loses records is rendered as
        /// `{"returned","total","records"}` so every omission is counted. A
        /// budget at or above the natural section sizes leaves the answer
        /// byte-identical to the un-budgeted one. Omit for the full answer.
        #[arg(long)]
        max_records: Option<usize>,
    },
    /// Rank code targets by repeated agent-failure density (issue #254).
    ///
    /// The store-wide complement to `eg query failures <HANDLE>`: instead of
    /// requiring a caller-supplied target, this lane aggregates every live
    /// agent-authored `Failure` record and ranks the code targets
    /// (`Symbol`/`File`) they resolve to by **distinct failing-run count**
    /// (total failure count is the documented tie-break) — the "here's where
    /// agents keep drowning" map, for the operator deciding where to send
    /// the next agent (or where a context fix is needed).
    ///
    /// Each row cites its evidence: the target record ID + repo-relative
    /// path/span, and every contributing `Failure` record ID with its
    /// run/session handle. Failures that resolve to no code target are
    /// reported in an explicit `unresolved` section, never silently dropped.
    /// Ordering is deterministic and byte-stable across repeated runs on an
    /// unchanged store.
    ///
    /// Exit codes:
    ///   0 — at least one live `Failure` record in scope (ranked rows may be
    ///       empty when every failure is `unresolved`).
    ///   1 — invalid `--limit`, or a malformed `--as-of`/`--since` instant
    ///       (machine-readable JSON on stderr).
    ///   2 — zero live `Failure` records in scope (`no_match`).
    ///
    /// Documented in `docs/cli/failure-hotspots.md` and `docs/cli/query.md`.
    FailureHotspots {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Cap the number of ranked hotspots returned. The answer signals
        /// whether the list was truncated.
        #[arg(long, default_value_t = query::FAILURE_HOTSPOTS_DEFAULT_LIMIT)]
        limit: usize,
        /// Aggregate only failures observed at or before this RFC 3339
        /// instant. May be combined with --since for a window.
        #[arg(long)]
        as_of: Option<String>,
        /// Aggregate only failures observed at or after this RFC 3339
        /// instant. May be combined with --as-of for a window.
        #[arg(long)]
        since: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
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
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history (enforced at runtime
        /// with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a record removed at a later commit still appears. Mutually
        /// exclusive with --at-head (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
        /// Supersession resolution mode for memory/observations.
        #[arg(long, value_enum, default_value_t = crate::temporal_status::SupersessionMode::Exclude)]
        supersession: crate::temporal_status::SupersessionMode,
        /// Restrict the `observations` section to memories authored by this
        /// agent identity (issue #195). Composes with `--not-agent`; the
        /// default (no flag) leaves recall unscoped. Deterministic code-graph
        /// facts (the `source_facts` section) carry no `agent_id` and are
        /// never presented as author-scoped memory — the selector applies to
        /// agent-authored observations only.
        #[arg(long)]
        agent: Option<String>,
        /// Exclude observations authored by this agent identity (issue #195).
        /// Composes with `--agent`: an observation must satisfy both selectors
        /// (when both name the same agent the exclusion wins).
        #[arg(long)]
        not_agent: Option<String>,
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
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history (enforced at runtime
        /// with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a record removed at a later commit still appears. Mutually
        /// exclusive with --at-head (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
        /// Cap the total evidence records returned across all sections
        /// (issue #211).
        ///
        /// Sections fill in envelope order; each keeps its top-ranked prefix
        /// and a section that loses records is rendered as
        /// `{"returned","total","records"}` so every omission is counted. A
        /// budget at or above the natural section sizes leaves the answer
        /// byte-identical to the un-budgeted one. Omit for the full answer.
        #[arg(long)]
        max_records: Option<usize>,
    },
    /// Walk the transitive inbound callers/referencers of a symbol with call paths (issue #139).
    ///
    /// Given a symbol record ID or an exact symbol name, walks the inbound
    /// `CALLS`/`REFERENCES` closure up to --max-depth hops and returns every
    /// reachable symbol with its hop distance and one concrete shortest
    /// connecting call path (record-ID/edge-label handles). Cycles terminate
    /// deterministically: each symbol is reported once with its shortest
    /// discovered path. Call-resolution labels (issues #152/#134) propagate
    /// along paths: each row carries the weakest resolution on its chain.
    ///
    /// Every row is a reachability LEAD — a call path exists in the graph —
    /// never proof that a change breaks a caller or that a test will fail.
    ///
    /// Output is newline-delimited JSON: a summary envelope line (target,
    /// counts, truncation, diagnostics) followed by one line per reachable
    /// row, byte-identical across runs. Reaching the depth bound emits a
    /// truncation diagnostic counting dropped frontier nodes per depth.
    ///
    /// Exit codes:
    ///   0 — walk completed (including an explicit empty reachable set).
    ///   1 — malformed / ambiguous / unsupported handle or selector
    ///       (machine-readable JSON on stderr; ambiguous names list all
    ///       candidate record IDs).
    ///   2 — handle resolves to no live record, or --at/--as-of names no
    ///       resolvable commit.
    ///
    /// Documented in `docs/cli/transitive-callers.md` and `docs/cli/query.md`.
    TransitiveCallers {
        /// Symbol record ID (`codegraph:vN:<hex>`) or exact symbol name.
        handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict symbol resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Inbound walk depth bound (hops from the queried symbol). Reaching
        /// the bound yields a truncation diagnostic with dropped frontier
        /// counts per depth rather than silently omitting reachable nodes.
        #[arg(long, default_value_t = 5)]
        max_depth: usize,
        /// Restrict the walk to the graph state at this commit SHA or unique
        /// prefix (requires a history graph). Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Restrict the walk to the graph state at the most recent commit at
        /// or before this RFC 3339 instant. Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Corpus selector (issue #427): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding callers removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history/--at/--as-of
        /// (enforced at runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #427): read the UNION of all commit snapshots
        /// so a caller removed at a later commit still appears. Mutually
        /// exclusive with --at-head/--at/--as-of (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Render one symbol's local call neighborhood as a diagram (issue #228).
    ///
    /// A local-orientation lane for the transitive-family queries: given a
    /// symbol record ID or an exact symbol name, draws the bounded
    /// neighborhood around it — direct callers (`CALLS` in-edges), direct
    /// callees (`CALLS` out-edges), and the containing file/module
    /// (`DEFINES`/`CONTAINS`) — in a standard text diagram format
    /// (Mermaid by default, or Graphviz DOT). `--depth` extends the walk
    /// beyond depth 1 (containers then contribute their members and imports);
    /// `--max-nodes` caps the rendered node count.
    ///
    /// The diagram is a deterministic projection of already-stored,
    /// citable edges: every node carries its stable record ID plus a
    /// repo-relative file/span citation in the legend, every edge is labeled
    /// with its stored kind, and no edge appears that is not in the graph.
    /// Node keys (`n0`, `n1`, …) are assigned in ascending record-ID order,
    /// so output is byte-identical across runs. It is NOT a control-flow or
    /// data-flow diagram: the lines are graph edges, not proof of runtime
    /// behavior.
    ///
    /// Exit codes:
    ///   0 — diagram rendered.
    ///   1 — malformed handle / invalid --depth / invalid --max-nodes /
    ///       ambiguous name / unsupported (non-symbol) handle; machine-
    ///       readable JSON on stderr.
    ///   2 — handle resolves to no live record (stale or unknown), or
    ///       --at/--as-of names no resolvable commit.
    ///   3 — symbol resolved but has no callers, callees, or container in
    ///       this view (`empty_neighborhood`).
    ///
    /// Documented in `docs/cli/diagram.md` and `docs/cli/query.md`.
    Diagram {
        /// Symbol record ID (`codegraph:vN:<hex>`) or exact symbol name.
        handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict symbol resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Neighborhood depth bound (hops from the queried symbol; default 1).
        /// Reachable nodes beyond the bound yield a truncation diagnostic with
        /// dropped frontier counts per depth rather than silently omitted.
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Maximum rendered nodes, anchor included (default 100). Dropped
        /// nodes and their incident edges are counted in the truncation
        /// diagnostic, never silently omitted.
        #[arg(long, default_value_t = 100)]
        max_nodes: usize,
        /// Restrict the walk to the graph state at this commit SHA or unique
        /// prefix (requires a history graph). Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Restrict the walk to the graph state at the most recent commit at
        /// or before this RFC 3339 instant. Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Output format (mermaid, dot, json, text; default mermaid).
        #[arg(long, value_enum, default_value_t = DiagramFormat::Mermaid)]
        format: DiagramFormat,
    },
    /// Walk the transitive outbound callees/dependencies of a symbol with dependency paths (issue #253).
    ///
    /// The outbound mirror of `eg query transitive-callers` (#139): given a
    /// symbol record ID or an exact symbol name, walks the outbound
    /// `CALLS`/`IMPLEMENTS`/`IMPORTS`/`REFERENCES` closure up to --max-depth
    /// hops and returns every reachable symbol with its hop distance and one
    /// concrete shortest connecting dependency path (record-ID/edge-label
    /// handles). The `--max-depth=1` result is exactly the direct outbound
    /// dependency set of `eg query deps` (#123). Cycles terminate
    /// deterministically: each symbol is reported once with its shortest
    /// discovered path. Call-resolution labels (issues #152/#134) propagate
    /// along paths: each row carries the weakest resolution on its chain. An
    /// outbound edge whose target is not in-graph (an unresolved call's
    /// Diagnostic marker or a missing record) is reported in an explicit
    /// `unresolved` category rather than silently dropped, and is never counted
    /// as reachable.
    ///
    /// Every row is a reachability LEAD — a dependency path exists in the graph
    /// — never proof that a change breaks a callee or that a test will fail.
    ///
    /// Output is newline-delimited JSON: a summary envelope line (target,
    /// counts, truncation, diagnostics) followed by one line per reachable
    /// row, then one line per unresolved target, byte-identical across runs.
    /// Reaching the depth bound emits a truncation diagnostic counting dropped
    /// frontier nodes per depth.
    ///
    /// Exit codes:
    ///   0 — walk completed (including an explicit empty reachable set).
    ///   1 — malformed / ambiguous / unsupported handle or selector
    ///       (machine-readable JSON on stderr; ambiguous names list all
    ///       candidate record IDs).
    ///   2 — handle resolves to no live record, or --at/--as-of names no
    ///       resolvable commit.
    ///
    /// Documented in `docs/cli/transitive-callees.md` and `docs/cli/query.md`.
    TransitiveCallees {
        /// Symbol record ID (`codegraph:vN:<hex>`) or exact symbol name.
        handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict symbol resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Outbound walk depth bound (hops from the queried symbol). Reaching
        /// the bound yields a truncation diagnostic with dropped frontier
        /// counts per depth rather than silently omitting reachable nodes.
        #[arg(long, default_value_t = 5)]
        max_depth: usize,
        /// Restrict the walk to the graph state at this commit SHA or unique
        /// prefix (requires a history graph). Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Restrict the walk to the graph state at the most recent commit at
        /// or before this RFC 3339 instant. Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Corpus selector (issue #427): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding callees removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history/--at/--as-of
        /// (enforced at runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #427): read the UNION of all commit snapshots
        /// so a callee removed at a later commit still appears. Mutually
        /// exclusive with --at-head/--at/--as-of (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List the direct outbound dependencies of a symbol (issue #123).
    ///
    /// Given a symbol record ID or an exact symbol name, returns the symbols
    /// it directly depends on — outbound `CALLS`, `IMPLEMENTS`, `IMPORTS`,
    /// and `REFERENCES` neighbors — as a citable, evidence-handled reading
    /// list. Each row is labeled with the edge type that produced it, and an
    /// edge whose target is not in-graph (an unresolved call's Diagnostic
    /// marker or a missing record) is reported in an explicit `unresolved`
    /// category rather than silently dropped. `CALLS` resolution labels
    /// (issues #152/#134) are carried through.
    ///
    /// Every row is a dependency LEAD from parse-derived edges — never proof
    /// that a dependency is exercised at runtime, and absence of an edge is
    /// not proof of independence.
    ///
    /// Output is newline-delimited JSON: a summary envelope line (target,
    /// counts, diagnostics) followed by one line per dependency, then one
    /// line per unresolved target, byte-identical across runs.
    ///
    /// Exit codes:
    ///   0 — dependencies returned (including an explicit empty set).
    ///   1 — malformed / ambiguous / unsupported handle or selector
    ///       (machine-readable JSON on stderr; ambiguous names list all
    ///       candidate record IDs).
    ///   2 — handle resolves to no live record, or --at/--as-of names no
    ///       resolvable commit.
    ///
    /// Documented in `docs/cli/deps.md` and `docs/cli/query.md`.
    Deps {
        /// Symbol record ID (`codegraph:vN:<hex>`) or exact symbol name.
        handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict symbol resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Return the dependency set as of this commit SHA or unique prefix
        /// (requires a history graph). Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Return the dependency set at the most recent commit at or before
        /// this RFC 3339 instant. Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Corpus selector (issue #427): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding dependencies removed at
        /// HEAD. This is the DEFAULT when a source snapshot exists; the flag
        /// makes it explicit. Mutually exclusive with --all-history/--at/--as-of
        /// (enforced at runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #427): read the UNION of all commit snapshots
        /// so a dependency removed at a later commit still appears. Mutually
        /// exclusive with --at-head/--at/--as-of (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Enumerate the crate's externally-reachable public API surface (issue #213).
    ///
    /// Returns the set of externally-reachable public items — functions,
    /// structs, enums, traits, type aliases, consts, statics, and modules —
    /// computed from recorded per-symbol visibility (issue #124) and module
    /// containment, never from a `pub` token grep. A `pub` item inside a
    /// non-`pub` module is excluded; a `pub use` re-export that widens
    /// visibility is included and attributed to the re-export site.
    /// `pub(crate)` / `pub(super)` / `pub(in path)` items are crate-internal
    /// and excluded (tallied in `counts`).
    ///
    /// Scope: the Rust library crate rooted at `src/` (excluding `src/bin/`)
    /// at the current graph state. Output is deterministic and byte-stable.
    /// Parse-derived: never a build-verified or semver claim.
    ///
    /// An empty surface is an explicit machine-readable success (`ok:true`,
    /// empty `items`, an `empty_surface` diagnostic), exit 0 — not an error.
    /// Exit 1 on malformed input (unknown/ambiguous `--repo`, unreadable
    /// graph).
    ///
    /// Documented in `docs/cli/public-api.md`.
    PublicApi {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the surface to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List symbols with no recorded inbound reference edges — prune-triage LEADS (issue #113).
    ///
    /// Returns the code symbols that nothing in this graph references: zero
    /// inbound edges of the recorded reference classes (CALLS / IMPORTS /
    /// MENTIONS, plus the extractor's REFERENCES and IMPLEMENTS usage edges).
    /// The structural DEFINES/CONTAINS edge from a symbol's own file or
    /// module never counts — every symbol has one.
    ///
    /// Every row is a candidate to INSPECT before removal, never proof the
    /// symbol is dead: public API consumed outside this repository,
    /// trait-dispatched methods, macro-generated call sites, FFI /
    /// `#[no_mangle]` exports, derive-generated use, and crate entry points
    /// (`main`, `#[test]`) can all be used without a recorded in-graph edge.
    /// Candidates in a file scope containing extraction `Diagnostic` markers
    /// carry an advisory extraction-completeness caveat (issue #87).
    ///
    /// An empty candidate set (every symbol referenced) is an explicit
    /// machine-readable success: exit 0, `ok:true`, a `no_candidates`
    /// diagnostic — distinct from the `no_symbols` diagnostic of a
    /// symbol-free store and from a store-absent error (exit 1).
    ///
    /// Documented in `docs/cli/unreferenced.md`.
    Unreferenced {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the candidate set to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List declared Cargo dependencies as citable graph facts (issue #180).
    ///
    /// Reports every directly-declared Cargo dependency captured at scan
    /// time from `[dependencies]`, `[dev-dependencies]`, and
    /// `[build-dependencies]`: crate name, dependency kind, the declared
    /// version requirement as written, the resolved version from the nearest
    /// `Cargo.lock` (or a documented unresolved marker — never a guess), the
    /// declaring package, and the repo-relative manifest handle. `--name`
    /// answers the direct "do we depend on X?" lookup, returning only
    /// matching declarations.
    ///
    /// Rows are parse-derived declaration facts — never proof the dependency
    /// is used in code, builds, or resolves. Output is deterministic and
    /// byte-identical across runs; an empty surface or a name miss is a
    /// machine-readable success (exit 0 with a stable diagnostic), not an
    /// error.
    ///
    /// Documented in `docs/cli/manifest-deps.md`.
    ManifestDeps {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Return only declarations of this exact crate name.
        #[arg(long)]
        name: Option<String>,
        /// Restrict the surface to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Classify public-API surface changes across a commit range (issue #157).
    ///
    /// Composes the issue #118 range-delta mechanics with the issue #124
    /// per-symbol visibility/signature capture to classify changes to the
    /// Rust library crate's externally-reachable public API surface between
    /// two commit handles (full SHA or unique prefix): `added`, `removed`,
    /// `signature_changed`, `visibility_narrowed`, `visibility_widened`.
    /// Non-exported symbol deltas never appear as public-API changes; they
    /// are tallied and, with `--include-internal`, listed in a separate
    /// clearly-labeled `internal` group. `--callers` attaches base-endpoint
    /// internal caller leads to `removed`/`signature_changed` rows.
    ///
    /// Rows are observed structural surface changes with citable handles —
    /// never proof of semver breakage, downstream build failure, or behavior
    /// change, and no version bump is asserted. Reads only the supplied
    /// store; never touches Git state or the working tree.
    ///
    /// Documented in `docs/cli/public-api-deltas.md`.
    PublicApiDeltas {
        /// Base commit SHA or unique prefix (older endpoint).
        base: String,
        /// Head commit SHA or unique prefix (newer endpoint).
        head: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict commit resolution and classification to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Also list non-exported symbol deltas in a separate `internal` group.
        #[arg(long)]
        include_internal: bool,
        /// Attach base-endpoint caller leads to removed/signature-changed rows.
        #[arg(long)]
        callers: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List externally-reachable public symbols with no doc comment (issue #257).
    ///
    /// Joins the issue #213 public-surface set with the issue #124 recorded
    /// doc-comment facts: a symbol is reported when it is externally
    /// reachable AND its captured doc-comment fact is absent. Every row
    /// carries the concrete evidence asserted plus a citable repo-relative
    /// file/span handle. A symbol carrying any doc comment (`///`, `/** */`,
    /// or `#[doc = "..."]`) is excluded; a plain `//` comment is not
    /// documentation. A re-export counts as documented when either the
    /// `pub use` site or the resolved target carries a doc fact.
    /// `--include-private` widens the audit to all symbols (adding methods)
    /// for whole-crate doc audits.
    ///
    /// Soundness boundary: asserts the presence/absence of a recorded doc
    /// comment — never doc quality, accuracy, or completeness. A store that
    /// predates issue #124 doc capture yields an explicit
    /// `doc_facts_unavailable` capability verdict (exit 0), never a claim
    /// that every symbol is undocumented.
    ///
    /// An empty result is an explicit machine-readable success (`ok:true`,
    /// empty `items`, a `no_undocumented_items` diagnostic — or
    /// `empty_result_with_blind_spots` when unresolved re-exports or missing
    /// doc capture kept the audit from being certified clean), exit 0 — not
    /// an error. Exit 1 on malformed input (unknown/ambiguous `--repo`,
    /// unreadable graph). Output is deterministic and byte-stable.
    ///
    /// Documented in `docs/cli/undocumented.md`.
    Undocumented {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the audit to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Maximum rows returned; excess rows are truncated (deterministic
        /// sort order preserved) with a `results_truncated` diagnostic.
        #[arg(long)]
        limit: Option<usize>,
        /// Widen the audit to all symbols regardless of visibility or
        /// reachability, for whole-crate doc audits.
        #[arg(long)]
        include_private: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List the recorded implementors of a locally-defined trait (issue #133).
    ///
    /// Walks the inbound `IMPLEMENTS` edges of the resolved trait symbol and
    /// returns one newline-delimited JSON row per recorded impl, each carrying
    /// the implementing type's qualified name (resolved from the graph, not
    /// just the raw `impl X for Y` display string), the impl symbol record ID,
    /// and the repo-relative file/span handle of the impl block.
    ///
    /// Completeness contract: `IMPLEMENTS` edges exist only for traits whose
    /// definition was resolvable at extraction time (locally-defined traits);
    /// impls of external/std traits are not edge-backed. Every answer carries
    /// `completeness: "local_traits_only"`, and a resolved trait with zero
    /// recorded implementors emits an explicit `zero_implementors_recorded`
    /// signal — never a bare empty answer. An ambiguous trait name returns
    /// every candidate labeled by `trait_record_id`; one is never picked
    /// implicitly.
    ///
    /// Exit codes:
    ///   0 — implementor rows returned, or the resolved trait has zero
    ///       recorded implementors (explicit signal row).
    ///   1 — invalid selector (bad timestamp, ambiguous commit prefix,
    ///       unknown/ambiguous repository).
    ///   2 — trait could not be resolved (`no_match`) or resolves only to a
    ///       tombstoned record (`stale_handle`); JSON envelope on stdout.
    ///
    /// Documented in `docs/cli/query.md`.
    Implementors {
        /// Trait symbol name (exact, qualified when module-nested) or
        /// canonical Symbol record ID.
        name: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict to the implementor set at this commit SHA or unique
        /// prefix. Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Return the implementor set at the most recent record at or before
        /// this RFC 3339 instant (valid-time axis). Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Audit stored producer identity against the current binary (issue #234).
    ///
    /// Flags every code-graph record whose producer envelope — the recorded
    /// `egregore_version` and grammar/component versions
    /// (`producer_components`) — differs from the running binary, separated
    /// from records that match it. Results group by the distinct producer
    /// signature `(producer_kind, egregore_version, component set)`; drifted
    /// groups carry per-field mismatches and per-record file/span handles.
    ///
    /// Only code-graph-extraction producers (`code_graph_extractor`,
    /// `history_replay`, `incremental_cache`) are compared against
    /// grammar/binary identity. Agent-memory and importer producers land in a
    /// separate never-flagged `non_code_producer` bucket; records without a
    /// producer envelope land in `legacy_pre_v1`, never merged into any other
    /// bucket (re-extraction backfill is forever out of scope).
    ///
    /// Read-only: reports which records a re-extraction with this binary
    /// could change, and never re-extracts, re-embeds, or mutates the store.
    /// Drift is a reproducibility lead, never proof the recorded facts are
    /// wrong, and no trust policy is enforced. A store written entirely by
    /// one binary version yields an explicit empty drift result (`no_drift`
    /// diagnostic, exit 0) — zero false positives.
    ///
    /// Documented in `docs/cli/producer-drift.md`.
    ProducerDrift {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the audit to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Enumerate dependency cycles among files over IMPORTS/CALLS edges (issue #138).
    ///
    /// Reports every dependency cycle in the file-level dependency graph
    /// derived from already-extracted records: resolved cross-file `CALLS`
    /// edges (issues #152/#134) and import declarations that name-resolve to
    /// exactly one in-repo defining file. `ambiguous` / `unresolved` CALLS
    /// edges, cross-file CALLS edges carrying no resolution label (stores
    /// predating the resolution field), and ambiguous imports are excluded
    /// from cycle detection and tallied — ambiguity never fabricates a
    /// cycle, and an unlabeled edge is never treated as resolved.
    ///
    /// Each cycle lists the ordered member files that close the loop, with
    /// stable record IDs and repo-relative handles, plus the citable records
    /// each closing edge was derived from. Cycles are canonical: rotated to
    /// start at the lexicographically smallest member, reported once (never
    /// once per starting point), sorted by a stable key, byte-identical
    /// across runs.
    ///
    /// The optional scope handle (symbol record ID / exact name, or
    /// repo-relative file path) filters to cycles containing that node — the
    /// pre-refactor check. An acyclic graph (or a scope in no cycle) is an
    /// explicit success: exit 0, empty `cycles`, an `acyclic` diagnostic.
    ///
    /// Exit codes:
    ///   0 — cycles returned, or explicitly none found.
    ///   1 — malformed / ambiguous / unsupported scope handle or --repo selector.
    ///   2 — scope handle resolves to no live record (`no_match` / `stale_handle`).
    ///
    /// Documented in `docs/cli/cycles.md`.
    Cycles {
        /// Optional scope: symbol record ID (`codegraph:vN:<hex>`), exact
        /// symbol name, or repo-relative file path (e.g. `src/lib.rs`).
        scope: Option<String>,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the dependency graph to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history (enforced at runtime
        /// with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a record removed at a later commit still appears. Mutually
        /// exclusive with --at-head (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Dead-code triage candidates: symbols with no live in-graph callers (issue #240).
    ///
    /// Lists indexed symbols that have **zero recorded direct incoming
    /// `CALLS` edges**, plus symbols whose every recorded direct caller is
    /// itself such a candidate (the one-hop dead-cluster rule: if `a` is
    /// called only by `b` and `b` is unreferenced, both are reported).
    ///
    /// Excluded from candidacy (counted, never silently dropped):
    /// recognized non-call entry points (`#[test]` / `#[bench]` harness
    /// entries, `#[no_mangle]` / `#[export_name]` FFI exports, binary-crate
    /// `fn main`) and externally-reachable symbols under the issue #213
    /// public-surface rule (top-level `pub`, visibility-widening `pub use`
    /// re-exports) — an unused-internally `pub fn` may be a real external
    /// entry point.
    ///
    /// Every row is a **candidate** (suspected dead code), never proof: the
    /// response metadata states the soundness boundary — the graph cannot
    /// observe dynamic dispatch via trait objects, macro-generated callers,
    /// reflection-like usage, FFI consumers, or cross-crate consumers.
    /// Deletion stays a human/agent decision; transitive whole-program
    /// reachability pruning is out of scope.
    ///
    /// Documented in `docs/cli/dead-code.md`.
    DeadCode {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the candidate set to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Maximum candidate rows returned (default 100, max 500). Values
        /// outside 1..=500 are rejected with an `invalid_limit` diagnostic.
        #[arg(long, default_value_t = query::DEAD_CODE_DEFAULT_LIMIT)]
        limit: usize,
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
    /// Symbol- and file-level deltas between two commits, grouped by change class (issue #118).
    ///
    /// Resolves two commit handles (full SHA or unique prefix) against the
    /// store's history and reports the observed structural deltas between
    /// them: `added_symbols`, `removed_symbols`, `modified_symbols`,
    /// `added_files`, `removed_files`, `modified_files`, plus an `unresolved`
    /// diagnostic group. Semantic drift falling inside the range is surfaced
    /// where drift records exist, labeled as semantic movement rather than
    /// structural change; without them the section is marked unavailable, not
    /// empty. Rows are observed deltas with citable handles, never proof of
    /// behavior change — and absence of a delta is not proof a behavior was
    /// preserved. Reads only the supplied store; never touches Git state or
    /// the working tree.
    ///
    /// Documented in `docs/cli/deltas.md`.
    Deltas {
        /// Base commit SHA or unique prefix (older endpoint).
        base: String,
        /// Head commit SHA or unique prefix (newer endpoint).
        head: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict commit resolution and delta selection to one repository.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Classify runtime error-signatures across a commit range (issue #326).
    ///
    /// Answers "did this commit range introduce new runtime error
    /// signatures?" by composing the issue #118 range mechanics with the
    /// issue #319/#320 `ErrorSignature` valid-time model and the issue #322
    /// `FRAME_RESOLVES_TO` frame-resolution edges. Derives the valid-time
    /// window from the committer dates of the range commits and classifies
    /// every in-scope signature into `new_signatures` (first observed inside
    /// the window — the regression signal), `ceased_signatures` (existed
    /// before the range and went silent by its end), or
    /// `continuing_signatures` (existed before and still occurring through
    /// the end). Signatures first observed after the window are excluded as a
    /// future range. Each `new_signatures` row joins its resolved backtrace
    /// frames to overlapping symbol deltas from the same range.
    ///
    /// Rows are regression LEADS, never proof this range caused the failure;
    /// a ceased signature is not proof of a fix; occurrence data only reflects
    /// the scanned log sources. Reads only the supplied store; never touches
    /// Git state or the working tree. Raw log payload text never enters the
    /// response.
    ///
    /// Documented in `docs/cli/log-deltas.md`.
    LogDeltas {
        /// Base commit SHA or unique prefix (older endpoint).
        base: String,
        /// Head commit SHA or unique prefix (newer endpoint).
        head: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Scope the code side only (commit/window resolution and the
        /// symbol-delta join) to one repository. Does NOT filter log
        /// signatures: log records carry no retrievable repository
        /// attribution, so every in-window signature is always classified
        /// regardless of `--repo`. Per-repository log separation requires
        /// per-repository stores.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Assemble one trust-separated cross-domain error-context bundle (issue #324).
    ///
    /// Resolves an `ErrorSignature` handle — a `log:v1:<hex>` record ID, a
    /// unique fingerprint prefix, or an exact `Symbol` name whose backtrace
    /// frames resolved to it — and emits a single deterministic envelope: the
    /// signature identity plus its occurrence buckets and resolved frames
    /// (`runtime_observation`), the code source facts its frames name
    /// (`source_fact`), the agent runs/commands it was `EMITTED_DURING`
    /// (`agent_observation` / `verification`), the tasks it references
    /// (`project_state`), a history `first_seen_range`, and — behind an opt-in
    /// `--protected-store` — protected raw-payload handles matched by content
    /// hash. One cited envelope replaces four separate tool round-trips.
    ///
    /// Rows are CORRELATION LEADS, never proof of cause. Read-only; raw
    /// log/transcript/command text never enters the response beyond the
    /// signature's bounded template excerpt. Documented in
    /// `docs/cli/error-context.md`.
    ErrorContext {
        /// `log:v1:<hex>` `ErrorSignature` ID, unique fingerprint prefix, or
        /// exact symbol name.
        handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Scope the code side of the history join to one repository. Log
        /// records carry no retrievable repository attribution.
        #[arg(long)]
        repo: Option<String>,
        /// Bound the occurrence/bucket view at an RFC 3339 instant (mutually
        /// exclusive with --at — enforced at runtime with a machine-readable
        /// `unsupported_combination` envelope, mirroring `eg resolve-frames`).
        #[arg(long)]
        as_of: Option<String>,
        /// Re-resolve backtrace frames against a commit view (mutually
        /// exclusive with --as-of).
        #[arg(long)]
        at: Option<String>,
        /// Supersession policy: exclude superseded rows (default) or keep and
        /// flag them.
        #[arg(long, value_enum, default_value_t = crate::temporal_status::SupersessionMode::Exclude)]
        supersession: crate::temporal_status::SupersessionMode,
        /// Read-time protected-store directory; matches a signature's
        /// `source_artifact_hash` to a `protected:v1:` handle. Raw bytes are
        /// never read.
        #[arg(long)]
        protected_store: Option<PathBuf>,
    },
    /// Trace a cross-domain evidence witness path between two records (issue #247).
    ///
    /// Answers "is record A grounded in record B, and by what chain?" by tracing
    /// the deterministic shortest connecting path between two record handles over
    /// the graph's cross-domain evidence/provenance edge subgraph, returning one
    /// citable witness path (or an explicit `no_path` verdict — never a silent
    /// empty list).
    ///
    /// Only evidence/provenance edges are traversed (`OBSERVES`, `HAS_EVIDENCE`,
    /// `VALIDATED_BY`, `PRODUCED_EVIDENCE`, `FRAME_RESOLVES_TO`, `EMITTED_DURING`,
    /// `REFERENCES_TASK`, …); code-graph topology (`CALLS`, `CONTAINS`, `DEFINES`,
    /// …) and intra-memory scaffolding (`SESSION_OF`, `AUTHORED_BY`) are EXCLUDED
    /// by design — the classification is an exhaustive compile-time partition of
    /// every edge label. Reachability is undirected (a grounding chain mixes edge
    /// directions), so each hop reports the edge's native `from`/`to` plus a
    /// `traversal_direction`; the tie-break is fewest hops, then the smallest
    /// `(neighbor_record_id, edge_record_id)` at each step. Deleted (tombstoned,
    /// non-temporal) records are excluded — a current-state view.
    ///
    /// A witness path proves a live evidence-edge chain connects two records; it
    /// is not proof the cited code still matches current source, and
    /// `EMITTED_DURING` hops are correlation leads, never causation. Read-only;
    /// no raw source/transcript/command/patch text ever enters the response.
    ///
    /// Exit codes:
    ///   0 — a witness path was found (>= 1 hop).
    ///   1 — identical endpoints, or two live endpoints with no evidence chain
    ///       (`no_path`).
    ///   2 — an endpoint is absent (`endpoint_not_found`) or tombstoned
    ///       (`endpoint_tombstoned`).
    ///
    /// Documented in `docs/cli/evidence-path.md`.
    EvidencePath {
        /// Source record handle (stable record ID).
        source: String,
        /// Target record handle (stable record ID).
        target: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history (enforced at runtime
        /// with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a record removed at a later commit still appears. Mutually
        /// exclusive with --at-head (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Rank the files that historically changed in the same commits as a target file (issue #153).
    ///
    /// Over a temporal store produced by `scan-history`, counts the distinct
    /// commits in which each other file changed together with the target
    /// file and ranks partners by a documented normalized coupling strength
    /// (`jaccard_v1`: shared commits over the union of both files' change
    /// sets), with a directional confidence (shared commits over the
    /// target's changes) on every row. A minimum-support threshold
    /// (`--min-support`, default 2, max 100) suppresses noise pairs; the
    /// threshold used is echoed in the answer. `--limit` (default 20, max
    /// 500) caps output and the answer states whether it was truncated.
    ///
    /// Temporal scope: full history by default; `--base`+`--head` bound it
    /// to the `(base, head]` commit range (issue #118 semantics), `--at` to
    /// the ancestor closure of one commit, `--as-of` to commits recorded at
    /// or before an RFC 3339 instant.
    ///
    /// Rows are historical co-change LEADS — files observed changing in the
    /// same commits — never proof of dependency, breakage, or behavior
    /// change, and absence of coupling is not proof of independence. Because
    /// partners must resolve to `File` nodes, untracked, ignored, and
    /// non-source paths never appear. Reads only the supplied store; never
    /// touches Git state or the working tree.
    ///
    /// Exit codes:
    ///   0 — ranked partners returned (including an explicit empty set).
    ///   1 — malformed path/selector/threshold, ambiguous handle, identical
    ///       endpoints, or reversed range (machine-readable JSON).
    ///   2 — unknown file (no `File` node), missing commit, empty history,
    ///       or no commit at/before the --as-of instant.
    ///
    /// Documented in `docs/cli/coupling.md`.
    Coupling {
        /// Repo-relative path of the target file (e.g. `src/lib.rs`).
        path: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict commit and file resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Range base commit SHA or unique prefix (older, exclusive
        /// endpoint). Requires --head.
        #[arg(long, requires = "head", conflicts_with_all = ["at", "as_of"])]
        base: Option<String>,
        /// Range head commit SHA or unique prefix (newer, inclusive
        /// endpoint). Requires --base.
        #[arg(long, requires = "base", conflicts_with_all = ["at", "as_of"])]
        head: Option<String>,
        /// Bound the in-scope commits to the ancestor closure of this commit
        /// SHA or unique prefix. Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Bound the in-scope commits to those recorded at or before this
        /// RFC 3339 instant. Mutually exclusive with --at.
        #[arg(long)]
        as_of: Option<String>,
        /// Minimum shared-commit count for a partner row (1..=100).
        #[arg(long, default_value_t = query::CO_CHANGE_DEFAULT_MIN_SUPPORT)]
        min_support: usize,
        /// Maximum partner rows returned (1..=500); truncation is reported.
        #[arg(long, default_value_t = query::CO_CHANGE_DEFAULT_LIMIT)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Inventory `.unwrap()` / `.expect()` panic-risk call sites (issue #223).
    ///
    /// Returns every Tree-sitter-detected `.unwrap()` / `.expect()` method-call
    /// expression as an advisory triage lead: a stable record ID, the closed
    /// category (`unwrap` / `expect`), a `production` vs `test` context class
    /// (`#[cfg(test)]` modules, `#[test]` fns, and files under `tests/` are
    /// test context), the repo-relative file/span handle, and the enclosing
    /// symbol handle (explicit `null` when top-level). Text inside comments,
    /// string literals, and doc comments is never returned. The known-risk
    /// method set is closed for this slice: `unwrap`, `expect`.
    ///
    /// Rows derive solely from deterministic extractor facts and assert only
    /// that a call exists at a span in a context — never a verdict on whether
    /// it is justified. Strictly read-only; byte-identical across runs on an
    /// unchanged store.
    ///
    /// Exit codes:
    ///   0 — sites returned (or the scoped slice contains zero sites, with
    ///       `empty_reason: "no_sites_in_scope"`).
    ///   1 — malformed prefix, ambiguous commit prefix, or unknown/ambiguous
    ///       repository selector.
    ///   2 — scope not found (`scope_not_found`) or unknown commit
    ///       (`unknown_commit`).
    ///
    /// Documented in `docs/cli/unwrap-expect.md`.
    UnwrapExpect {
        /// Optional repo-relative directory or module path prefix scoping the
        /// inventory (segment-aware; same contract as `eg query subsystem`).
        #[arg(long)]
        path: Option<String>,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Pin the inventory to a commit SHA or unique prefix on the
        /// valid-time axis (same selector contract as `eg query symbol --at`).
        #[arg(long)]
        at: Option<String>,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history and --at (enforced at
        /// runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a record removed at a later commit still appears. Mutually
        /// exclusive with --at-head and --at (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Inventory TODO/FIXME/HACK/XXX debt-comment markers (issue #218).
    ///
    /// Returns every human-authored debt-comment marker detected inside a
    /// Tree-sitter comment node (line, block, and doc comments) as an
    /// advisory triage lead: a stable record ID, the closed category
    /// (`todo` / `fixme` / `hack` / `xxx`), the trimmed single-line note
    /// text, the repo-relative file/span handle, and the enclosing symbol
    /// handle (explicit `null` at module top level). A marker token inside a
    /// string or character literal is never returned, and identifier
    /// substrings (`TODOIST`, `fixmeup`) never match. The recognized marker
    /// set is closed for this slice; matching is case-insensitive on the
    /// marker token only.
    ///
    /// Rows derive solely from deterministic extractor facts and assert only
    /// that a comment of category C with note text T exists at a span —
    /// never that the surrounding code is correct or incorrect. Strictly
    /// read-only; byte-identical across runs on an unchanged store.
    ///
    /// Exit codes:
    ///   0 — markers returned (or the scoped slice contains zero markers,
    ///       with `empty_reason: "no_markers_in_scope"`).
    ///   1 — malformed prefix, ambiguous commit prefix, or unknown/ambiguous
    ///       repository selector.
    ///   2 — scope not found (`scope_not_found`) or unknown commit
    ///       (`unknown_commit`).
    ///
    /// Documented in `docs/cli/debt-markers.md`.
    DebtMarkers {
        /// Optional repo-relative directory or module path prefix scoping the
        /// inventory (segment-aware; same contract as `eg query subsystem`).
        #[arg(long)]
        path: Option<String>,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Pin the inventory to a commit SHA or unique prefix on the
        /// valid-time axis (same selector contract as `eg query symbol --at`).
        #[arg(long)]
        at: Option<String>,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history and --at (enforced at
        /// runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a record removed at a later commit still appears. Mutually
        /// exclusive with --at-head and --at (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List extractor-coverage-gap diagnostics (issue #246).
    ///
    /// Returns every persisted `Diagnostic` graph node carrying both a
    /// repo-relative path and a span — the extractor's self-declared blind
    /// spots (unsupported macro invocations, unresolved call/dispatch stubs)
    /// — as citable rows, one NDJSON object per gap, with no scan or
    /// recompute. Each row carries the record ID, the complete byte/line
    /// span (which slices the cited source), the diagnostic name, and a
    /// human summary.
    ///
    /// The lane never reclassifies source-authored markers: TODO/FIXME
    /// `DebtMarker`s and `.unwrap()`/`.expect()` `PanicRiskSite`s belong to
    /// other lanes and are excluded by construction.
    ///
    /// A scope with zero gap diagnostics is an explicit success (exit 0)
    /// with an empty `diagnostics` array and `empty_reason`
    /// `no_gaps_in_scope` — distinct from unknown/no-match/error. "No
    /// extraction-gap diagnostics in scope" is not proof the code is fully
    /// understood for any other purpose, only that the extractor flagged
    /// nothing it could not parse.
    Diagnostics {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Restrict results to one repo-relative file path.
        #[arg(long)]
        file: Option<String>,
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history (enforced at
        /// runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a gap removed at a later commit still appears (once per commit,
        /// distinguished by `git_commit`). Mutually exclusive with --at-head
        /// (enforced at runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Surface stub and panic-risk macro markers as a code-triage lane (issue #210).
    ///
    /// Returns every persisted `Diagnostic` macro-invocation node whose macro
    /// name is in the closed known-risk set — `todo!`, `unimplemented!`,
    /// `unreachable!` (category `stub`) and `panic!` (category `panic`) — as
    /// citable rows, one per marker, with no scan or recompute. Each row
    /// carries the stable category, the macro name, the `Diagnostic` record
    /// ID, the repo-relative file/span handle, and the enclosing symbol's
    /// handle when a `DEFINES` owner exists (explicit `null` at module top
    /// level).
    ///
    /// Classification is exact and conservative over the closed set:
    /// path-qualified names reduce to their final `::` segment
    /// (`std::panic!` is still a `panic!`), matching is exact (no substring
    /// matches: `mytodo!` and `panic_info!` never classify), and a benign
    /// macro (`println!`, `vec!`, `format!`, …) is never returned as a risk
    /// marker. `assert!`/`assert_eq!` and `.unwrap()`/`.expect()` are
    /// deliberately outside the set (see `eg query unwrap-expect`).
    ///
    /// A scope with zero risk markers is an explicit success (exit 0) with an
    /// empty `markers` array and `empty_reason` `no_markers_in_scope` —
    /// distinct from unknown/no-match/error. Rows are advisory: each asserts
    /// only that a marker of the given category exists at the span, never
    /// that the surrounding code is correct or incorrect.
    ///
    /// Documented in `docs/cli/risk-markers.md`.
    RiskMarkers {
        /// Optional repo-relative directory or module path prefix scoping the
        /// inventory (segment-aware; same contract as `eg query subsystem`).
        #[arg(long)]
        path: Option<String>,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Pin the inventory to a commit SHA or unique prefix on the
        /// valid-time axis (same selector contract as `eg query symbol --at`).
        #[arg(long)]
        at: Option<String>,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history and --at (enforced at
        /// runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a marker removed at a later commit still appears. Mutually
        /// exclusive with --at-head and --at (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Inventory `#[deprecated]` symbols with their still-resolvable call sites (issue #249).
    ///
    /// Returns every code symbol carrying a `#[deprecated]` attribute mark —
    /// bare, `= "..."` note shorthand, or `(since = "...", note = "...")`
    /// meta form — as an inventory row: a stable record ID, the repo-relative
    /// file/span handle, and the verbatim bounded `deprecated_since` /
    /// `deprecated_note` payloads
    /// (`null` when the attribute did not carry them — never fabricated).
    /// For each deprecated symbol, the lane reports its still-resolvable call
    /// sites over the existing `CALLS` edge vocabulary as a migration
    /// worklist, each call site carrying a citable edge record ID and the
    /// caller's repo-relative file/span. Call edges the graph cannot resolve
    /// are cited under `coverage.unresolved_call_edges` — never silently
    /// dropped and never counted as zero.
    ///
    /// Rows derive solely from deterministic extractor facts and assert only
    /// that the mark exists and the listed calls resolve — never what should
    /// replace the symbol. No LLM prose, no replacement suggestions, and no
    /// raw source text beyond the bounded attribute payloads is synthesized.
    /// Strictly read-only; byte-identical across runs on an unchanged store.
    /// Ordering is deterministic: symbols by (path, span start line, record
    /// ID), call sites by (caller path, caller span start line, edge ID).
    ///
    /// `--file <PATH>` scopes the inventory (and the `symbols_considered`
    /// tally) to declarations in that repo-relative file; the call-site
    /// worklists of the selected symbols stay complete, citing callers
    /// wherever they are.
    ///
    /// Exit codes:
    ///   0 — inventory returned (at least one deprecated symbol).
    ///   3 — `no_deprecated_symbols`: live code symbols exist, none marked.
    ///   4 — `no_code_symbols`: the input resolves to zero code-graph nodes.
    ///   1 — malformed input (unknown/ambiguous `--repo`, unreadable graph,
    ///       both/neither `--graph`/`--data-dir`).
    ///
    /// Documented in `docs/cli/deprecated-symbols.md`.
    DeprecatedSymbols {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the inventory to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Restrict the inventory to symbols declared in this repo-relative
        /// file.
        #[arg(long)]
        file: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Audit the resting store for secret-shaped values that bypassed redaction (issue #244).
    ///
    /// Sweeps every persisted queryable string field across all domains —
    /// agent-memory, artifact, verification, project, user-context, log-graph,
    /// and code-graph records (code-graph symbol bodies/spans and import
    /// paths are exempt from the write gate, so they are the prime
    /// unredacted-at-rest risk) — and reports secret-shaped values.
    ///
    /// Detection is two-layered: (a) known token patterns aligned to the
    /// `secret_class` taxonomy in `docs/schema/redaction.md` (`api_token`,
    /// `ssh_private_key`, `database_url`, `cloud_credential`,
    /// `webhook_secret`, `session_cookie`, `env_secret`, `email`), and (b)
    /// generic tokens above the documented Shannon-entropy + length threshold
    /// (≥20 chars, ≥4.2 bits/char). Each finding carries `record_id`,
    /// `domain`, `field_path`, `classification` (`secret_class` or
    /// `high_entropy`), and a BLAKE3 `hash_prefix` — never the raw value.
    ///
    /// Records stamped `redaction_policy_version: "v1"` and values that are
    /// already `<REDACTED:secret_class:hash_prefix>` markers are recognized as
    /// already-handled and are never flagged; the documented allowlist
    /// suppresses public sample keys and test-fixture tokens. Strictly
    /// read-only; byte-identical across runs on an unchanged store. Findings
    /// are advisory heuristic matches — never confirmed secrets — and a clean
    /// sweep is not proof the store holds no secrets.
    ///
    /// Exit codes:
    ///   0 — clean store: zero findings.
    ///   3 — ≥1 finding reported.
    ///   1 — malformed input (unknown/ambiguous `--repo`, unreadable graph,
    ///       both/neither `--graph`/`--data-dir`).
    ///
    /// Documented in `docs/cli/redaction-audit.md`.
    RedactionAudit {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the sweep to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Audit producer trust-class consistency across every persisted record (issue #236).
    ///
    /// Read-only: scans every persisted node, edge, and tombstone and verifies
    /// that each record's `producer_kind` is consistent with the trust class
    /// its node kind implies. Deterministic code-fact kinds may only be written
    /// by deterministic producers; agent-authored kinds may only be written by
    /// agent producers. Each violation is a citable JSON record (`record_id`,
    /// `kind`, `producer_kind`, the repo-relative handle/span where applicable,
    /// and the broken rule) — never synthesized prose. A clean store returns an
    /// explicit `ok:true` with counted totals.
    ///
    /// Edges are audited against their source node's kind and tombstones against
    /// their target record's kind (resolved in-batch, last-write-wins — the same
    /// semantics as the daemon's `lookup_node_kind`); unresolvable endpoints are
    /// skipped, not flagged. `producer_kind: other` fails closed. Legacy records
    /// without a producer envelope are exempt.
    ///
    /// Exit codes:
    ///   0 — audit ran (violations reported, or clean with
    ///       `empty_reason: "no_trust_violations"`).
    ///   1 — malformed input (unreadable graph, both/neither
    ///       `--graph`/`--data-dir`).
    ///
    /// Documented in `docs/cli/trust-audit.md`; the rule set in
    /// `docs/schema/producer-version.md` §10.
    TrustAudit {
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
    /// Inventory the scanned repo's own `unsafe`-code surface (issue #222).
    ///
    /// Returns every Tree-sitter-detected `unsafe { .. }` block, `unsafe fn`
    /// declaration, and `unsafe impl` block as an advisory inventory row: a
    /// stable record ID, the closed site kind (`block` / `fn` / `impl`), the
    /// repo-relative file/span handle, and the enclosing symbol handle
    /// (explicit `null` when top-level), plus an aggregate count equal to the
    /// number of returned sites. The word `unsafe` inside comments, string
    /// literals, doc comments, or identifiers is never returned. The site
    /// kind set is closed for this slice: `block`, `fn`, `impl`.
    ///
    /// Rows derive solely from deterministic extractor facts and assert only
    /// that an unsafe site of a kind exists at a span — never that the code
    /// is sound or unsound. A zero count is not a safety guarantee:
    /// macro-expanded, build-script, and dependency `unsafe` are out of this
    /// slice. Strictly read-only; byte-identical across runs on an unchanged
    /// store.
    ///
    /// Exit codes:
    ///   0 — sites returned (or the scoped slice contains zero sites, with
    ///       `empty_reason: "no_sites_in_scope"`).
    ///   1 — malformed prefix, ambiguous commit prefix, or unknown/ambiguous
    ///       repository selector.
    ///   2 — scope not found (`scope_not_found`) or unknown commit
    ///       (`unknown_commit`).
    ///
    /// Documented in `docs/cli/unsafe-sites.md`.
    UnsafeSites {
        /// Optional repo-relative directory or module path prefix scoping the
        /// inventory (segment-aware; same contract as `eg query subsystem`).
        #[arg(long)]
        path: Option<String>,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Pin the inventory to a commit SHA or unique prefix on the
        /// valid-time axis (same selector contract as `eg query symbol --at`).
        #[arg(long)]
        at: Option<String>,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history and --at (enforced at
        /// runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a record removed at a later commit still appears. Mutually
        /// exclusive with --at-head and --at (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Trace a single symbol's lifecycle across Git history.
    ///
    /// Resolves one symbol (stable record ID or exact name) against a
    /// `scan-history` graph or embedded store and returns its chronologically
    /// ordered lifecycle events: `introduced`, each `modified` commit with
    /// its semantic-drift record where one exists, `removed` if the symbol
    /// was tombstoned, and `reintroduced` if it came back. Every event
    /// carries the commit SHA, its valid time, a stable record ID, and a
    /// repo-relative file/span handle (or a documented absent-span reason).
    /// Output is newline-delimited JSON by default — one event object per
    /// line — and byte-identical across runs; `--format text` prints a
    /// human-readable timeline. Events are advisory temporal facts, never a
    /// risk or behavior claim. An unknown symbol or a symbol with no
    /// commit-linked history exits 2; an ambiguous name reports all
    /// candidate record IDs and exits 6.
    ///
    /// Documented in `docs/cli/lifeline.md`.
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
    /// Aggregate Git authorship into per-file ownership shares, a primary
    /// owner, and a bus-factor signal (issue #245).
    ///
    /// Over a history store produced by `scan-history`, returns one row per
    /// indexed source file present at the resolved anchor commit: the ranked
    /// author list (distinct in-scope commits + ownership share per author),
    /// the max-share primary owner (ties break to the lexicographically
    /// smallest `(author_email, author_name)` identity), and the bus factor —
    /// the minimum number of top authors whose cumulative share reaches
    /// `--threshold` percent (default 50). Rows are empirical
    /// history-derived leads, never declared ownership, review authority, or
    /// proven expertise. Reads Git-object-derived records only; the working
    /// tree is never touched. Documented in `docs/cli/ownership.md`.
    Ownership {
        /// Optional repo-relative file path to report one file only.
        path: Option<String>,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Report ownership as-of this commit SHA or unique prefix
        /// (valid-time axis). Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Report ownership at the most recent commit at or before this
        /// RFC 3339 instant (valid-time axis). Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Restrict aggregation to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Cumulative ownership-share threshold percent for the bus factor
        /// (1..=100).
        #[arg(long, default_value_t = query::OWNERSHIP_DEFAULT_THRESHOLD_PERCENT)]
        threshold: u32,
        /// Maximum number of file rows (1..=1000; the answer states whether
        /// it was truncated).
        #[arg(long, default_value_t = query::OWNERSHIP_DEFAULT_LIMIT)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Trace a code symbol to the commit that introduced it, plus the
    /// project-graph PR / issue / review records whose `merge_commit_sha`
    /// equals that commit (issue #159).
    ///
    /// The introducing commit is the first commit whose snapshot contains the
    /// symbol (reintroductions never move the origin). The project link is
    /// deterministic commit-SHA byte equality ONLY — no fuzzy title or body
    /// matching, and no live GitHub calls: the lane resolves entirely offline
    /// against the already-ingested store.
    ///
    /// Code facts (the introducing commit) and project facts (PR/issue/review
    /// rows) render in trust-separated `code` / `project` sections, mirroring
    /// `eg query context`'s contract. When the store carries code history but
    /// no GitHub import, the answer degrades to a commit-only result with an
    /// explicit `github_import_absent` note — never an error, and never
    /// implying no PR exists.
    ///
    /// Exit codes:
    ///   0 — origin traced (possibly with empty project sections).
    ///   1 — ambiguous symbol name, ambiguous commit prefix, or malformed
    ///       `--as-of` timestamp (JSON diagnostic on stdout).
    ///   2 — unknown symbol (`no_match`), symbol without commit-linked
    ///       history (`no_history`), or `--at` commit absent from the store
    ///       (`missing_commit`).
    ///
    /// Documented in `docs/cli/origin.md` and `docs/cli/query.md`.
    Origin {
        /// Symbol stable ID or exact symbol name.
        #[arg(index = 1)]
        symbol: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict symbol resolution to one repository (issue #67).
        #[arg(long)]
        repo: Option<String>,
        /// Resolve the origin against the code state as of this commit SHA or
        /// unique prefix (valid-time axis). Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Resolve the origin against the code state as of the most recent
        /// commit at or before this RFC 3339 instant (valid-time axis).
        /// Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Resolve a `file:line` location to its smallest enclosing code symbol (issue #151).
    ///
    /// Turns a raw location — a compiler diagnostic, panic backtrace frame,
    /// diff hunk, or `git blame -L` line — into the innermost `Symbol` node
    /// whose recorded span contains that line, plus the enclosing chain
    /// (outermost → innermost) of containing modules and symbols. Purely a
    /// span-containment lookup over already-stored data: never a
    /// nearest-neighbor guess when the line sits outside every symbol span.
    ///
    /// Exit codes:
    ///   0 — an enclosing symbol was found (JSON envelope on stdout).
    ///   1 — malformed location, ambiguous commit prefix, or unknown/ambiguous
    ///       repository selection (machine-readable JSON).
    ///   2 — unknown path (`no_match`), commit absent from the store
    ///       (`missing_commit`), or no enclosing symbol
    ///       (`no_enclosing_symbol`).
    ///
    /// Documented in `docs/cli/query.md`.
    At {
        /// Location as `<repo-relative-path>:<line>`, e.g. `src/lib.rs:42`.
        location: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Resolve against symbol spans as they existed at this commit SHA or
        /// unique prefix (requires a history-bearing store).
        #[arg(long)]
        at: Option<String>,
        /// Restrict resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Resolve a `file:line` position to its innermost symbol *and* that symbol's
    /// trust-separated cross-domain context (issue #212).
    ///
    /// The positional sibling of `query at`: instead of returning only the
    /// symbol handle, `locate` is positional entry into the `eg query context`
    /// contract. It resolves the innermost `Symbol` whose recorded span contains
    /// the line (reusing the issue #151 span-containment resolver) and returns
    /// the same trust-separated bundle as `query context` — source facts, agent
    /// observations, project state, artifacts, and verification evidence — so an
    /// agent holding a stack-trace frame, blame line, or diff hunk gets the
    /// evidence graph without first guessing a symbol name.
    ///
    /// Absence is always typed, never a nearest-neighbor guess: a line outside
    /// every symbol span is `no_enclosing_symbol`, a line beyond the file's last
    /// recorded structural span is `line_out_of_range`, and an unknown path is
    /// `no_match`.
    ///
    /// Exit codes:
    ///   0 — an enclosing symbol was found (JSON envelope on stdout).
    ///   1 — malformed location, malformed `--as-of` timestamp, ambiguous commit
    ///       prefix, ambiguous unscoped repository collision, or unknown/ambiguous
    ///       repository selector.
    ///   2 — unknown path (`no_match`), commit absent (`missing_commit`), no
    ///       enclosing symbol (`no_enclosing_symbol`), or line beyond the file's
    ///       recorded extent (`line_out_of_range`).
    ///
    /// Documented in `docs/cli/query.md`.
    Locate {
        /// Location as `<repo-relative-path>:<line>`, e.g. `src/lib.rs:42`.
        location: String,
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
        /// Resolve against symbol spans as they existed at this commit SHA or
        /// unique prefix (requires a history-bearing store).
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Resolve against symbol spans as of the most recent commit at or
        /// before this RFC 3339 instant (requires a history-bearing store).
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Restrict resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Supersession resolution mode for memory/observations (matches
        /// `eg query context`).
        #[arg(long, value_enum, default_value_t = crate::temporal_status::SupersessionMode::Exclude)]
        supersession: crate::temporal_status::SupersessionMode,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Rank Git-tracked files by change frequency across commit history (issue #128).
    ///
    /// Over a temporal store produced by `eg scan-history`, returns files
    /// ranked by descending count of distinct commits that modified them —
    /// the software-archaeology hotspot signal. Each row carries the stable
    /// `File` record ID, the repo-relative path handle, the integer commit
    /// count, and the inclusive commit range the frequency was measured over.
    /// Only committed, Git-tracked, indexed source files can rank; untracked
    /// or ignored paths never appear.
    ///
    /// Ordering is deterministic and byte-stable: commit count descending,
    /// then repo-relative path ascending (documented tie-break). The answer
    /// states explicitly whether `--limit` truncated it.
    ///
    /// Exit codes:
    ///   0 — ranking returned.
    ///   1 — load error, invalid --limit, unknown/ambiguous --repo selector.
    ///   2 — no commit history in scope (`no_history`) or no file changes (`no_match`).
    ///
    /// Documented in `docs/cli/churn.md`.
    Churn {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the ranking to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Maximum ranked files returned (default 50, max 500). Values outside
        /// 1..=500 are rejected with an `invalid_limit` diagnostic.
        #[arg(long, default_value_t = query::CHURN_DEFAULT_LIMIT)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Rank Rust callable symbols by deterministic structural complexity,
    /// highest first (issue #162).
    ///
    /// Every Rust callable (`function` / `method` / `test` symbol kind) carries
    /// a source-derived integer `complexity`: 1 plus one per decision point in
    /// the item's own body (`if` / `else if`, `for`, `while`, `loop`, each
    /// `match` arm, each `?`, each `&&`, each `||`). Closure bodies count
    /// toward the enclosing callable; nested `fn` items get their own symbol.
    /// The score is a code fact, not agent confidence.
    ///
    /// Ordering is deterministic and byte-stable: complexity descending, then
    /// qualified name ascending (documented tie-break), then symbol record ID
    /// ascending. The answer states explicitly whether `--limit` truncated it.
    ///
    /// Exit codes:
    ///   0 — ranking returned.
    ///   1 — load error, invalid --limit, unknown/ambiguous --repo selector.
    ///   2 — no scored callable symbols in scope (`no_match`).
    ///
    /// Documented in `docs/cli/complexity.md`.
    Complexity {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the ranking to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Maximum ranked symbols returned (default 50, max 500). Values outside
        /// 1..=500 are rejected with an `invalid_limit` diagnostic.
        #[arg(long, default_value_t = query::COMPLEXITY_DEFAULT_LIMIT)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Surface recorded contradicting observations on a shared code target (issue #232).
    ///
    /// Returns the pairs of records joined by a recorded `CONTRADICTS` edge —
    /// agent-authored claims (`Observation` / `Decision` / `Failure`), recorded
    /// verification executions (`TestRun` / `CommandRun` / …), and user-context
    /// records (`Preference` / `WorkflowRule` / …) — that share a cited code
    /// target within the resolved scope. Deterministic code facts are never a
    /// party; a `CONTRADICTS` edge touching one is skipped whole.
    ///
    /// The scope resolves in precedence order: exact record ID, exact symbol
    /// name, repo-relative file path (covering the symbols the file defines),
    /// or a segment-aware subsystem path prefix. A conflict where any party
    /// was superseded by a later record is marked `resolved` and excluded
    /// unless `--include-resolved` is passed. This lane only *surfaces*
    /// recorded disagreements — it never infers them and never adjudicates;
    /// a passing verification shown as a `verification` party against a
    /// recorded `Failure` belief is the operator's adjudication lead.
    ///
    /// Output is deterministic and byte-stable across repeated runs on an
    /// unchanged store. Exit 0 with an empty `conflicts` list when the scope
    /// is valid but has no recorded conflicts; exit 2 (`no_match`) when the
    /// scope resolves to nothing; exit 1 on malformed input.
    ///
    /// Documented in `docs/cli/conflicts.md` and `docs/cli/query.md`.
    Conflicts {
        /// Symbol name / record ID, repo-relative file path, or subsystem
        /// path prefix the conflicts are scoped to.
        scope: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict scope resolution to one repository (issue #67).
        #[arg(long)]
        repo: Option<String>,
        /// Include conflicts where a party was superseded by a later record.
        #[arg(long)]
        include_resolved: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Partition the public API surface into verification-covered / uncovered (issue #109).
    ///
    /// Joins the issue #213 externally-reachable public surface with the
    /// recorded verification-domain nodes (`Verification` / `CommandRun` /
    /// `TestRun` / `ProofResult` / `CIStatus` / `CommandEvidence` /
    /// `BenchmarkRun` / `CoverageReport`) over the evidence-link edge/citation
    /// registry — never a `#[test]`/coverage-tool grep and never a build or
    /// coverage run. A public
    /// symbol is COVERED when a verification node links to it directly (any
    /// evidence-link label) or to its containing file via `TOUCHED_FILE` /
    /// `FAILED_ON`; an agent-memory node linking to it never confers coverage.
    ///
    /// Capability-degradation contract (mirrors `undocumented`'s
    /// `doc_facts_unavailable`): when the store records no verification nodes,
    /// or records them but none link to code, the report is an explicit
    /// `verification_facts_unavailable` verdict (exit 0) with EMPTY buckets —
    /// never every symbol flooded into "uncovered". Absence of recorded
    /// evidence is a prioritization signal, NEVER proof that code is untested,
    /// unverified in reality, unsafe, or broken; presence is a recorded link,
    /// never proof of correctness or that a test/proof passed.
    ///
    /// The optional `scope` handle filters to one code item, resolved in
    /// precedence order: exact record ID, exact symbol name, or a segment-aware
    /// repo-relative path prefix. Exit 2 when a supplied scope matches no code
    /// item (`scope_not_found` for a path, `no_match` for a name/id). Exit 1 on
    /// malformed input (unknown/ambiguous `--repo`, out-of-range `--limit`).
    /// Output is deterministic and byte-stable.
    ///
    /// Documented in `docs/cli/verification-coverage.md`.
    VerificationCoverage {
        /// Optional scope handle: record ID, exact symbol name, or a
        /// repo-relative path prefix (segment-aware).
        scope: Option<String>,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the surface and links to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Resolve the surface against records as they existed at this commit
        /// SHA or unique prefix (requires a history-bearing store).
        #[arg(long)]
        at: Option<String>,
        /// Maximum rows per bucket; excess rows are truncated (deterministic
        /// sort order preserved) with a `results_truncated` diagnostic.
        #[arg(long)]
        limit: Option<usize>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Flag verification records whose cited code has drifted since the run (issue #111).
    ///
    /// For every `TestRun` / `CIStatus` / `BenchmarkRun` / `CoverageReport` /
    /// `ProofResult` record citing code via `FAILED_ON` / `MENTIONS_SYMBOL` /
    /// `TOUCHED_FILE`, returns a per-citation **freshness verdict**: `current`,
    /// `stale`, `unresolved`, or `unanchored`. A `stale` / `unresolved` verdict
    /// is a **freshness lead, never a re-judgment of pass/fail** — it states
    /// only that the verified basis moved, never that the recorded `status` is
    /// now wrong. Strictly read-only and deterministic; reuses existing drift
    /// records, content hashes, and temporal anchors. Never rewrites,
    /// hides, or re-stamps a code fact or a verification record's `status`.
    ///
    /// Distinct from `eg query evidence-freshness` (issue #85), which ages
    /// agent-memory observations, not verification evidence.
    ///
    /// Exit codes:
    ///   0 — verdicts returned (including the explicit "nothing stale" /
    ///       "no verification records" diagnostics -- never a silent empty
    ///       success).
    ///   1 — invalid --limit, ambiguous --repo or scope selector, or
    ///       --graph/--data-dir usage errors.
    ///   2 — scope handle matches no in-store code item.
    ///
    /// Documented in `docs/cli/verification-freshness.md`.
    VerificationFreshness {
        /// Optional scope handle: record ID, exact symbol name, or a
        /// repo-relative path prefix (segment-aware).
        scope: Option<String>,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict verdicts to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Working-tree root to re-hash a record's `source_artifact_path`
        /// against its recorded `source_artifact_hash` (AC2). Read-only;
        /// omitted entirely when not supplied -- never assumed to match.
        #[arg(long)]
        repo_path: Option<PathBuf>,
        /// Return only stale/unresolved records. An empty result is reported
        /// with a stable diagnostic, never silently (AC7).
        #[arg(long)]
        stale_only: bool,
        /// Maximum verdict rows; excess rows are truncated (deterministic
        /// sort order preserved) with a `results_truncated` diagnostic.
        #[arg(long)]
        limit: Option<usize>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Rank indexed symbols by least-recent last change across commit history (issue #219).
    ///
    /// Over a temporal store produced by `eg scan-history`, returns symbols
    /// ranked by least-recent last change — most dormant first — the
    /// dormancy-triage signal. Each row carries the stable `Symbol` record ID
    /// and name, the repo-relative path and span, the last-change commit SHA
    /// and its valid time, and a dormancy span (seconds and whole days)
    /// measured against the **newest indexed commit per repository**, never
    /// wall-clock "now". Only live (non-tombstoned) symbols with an
    /// attributable last change can rank.
    ///
    /// Ordering is deterministic and byte-stable: dormancy descending (most
    /// dormant first), then last-change commit topological rank ascending, then
    /// repo-relative path ascending, then record ID ascending. The answer
    /// states explicitly whether `--limit` truncated it.
    ///
    /// Exit codes:
    ///   0 — ranking returned.
    ///   1 — load error, invalid --limit, unknown/ambiguous --repo selector.
    ///   2 — no commit history in scope (`no_history`, the honesty case for a
    ///       current-tree-only scan) or no attributable symbols (`no_match`).
    ///
    /// Documented in `docs/cli/recency.md`.
    Recency {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the ranking to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Maximum ranked symbols returned (default 50, max 500). Values
        /// outside 1..=500 are rejected with an `invalid_limit` diagnostic.
        #[arg(long, default_value_t = query::RECENCY_DEFAULT_LIMIT)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Trace a directed shortest call path between two symbols (issue #225).
    ///
    /// Answers the reachability question between two *named* endpoints: does
    /// `FROM` reach `TO` through the call graph, and if so by what concrete
    /// path? The walk is a directed BFS over **resolved `CALLS` edges only**
    /// (the outbound call direction), so ambiguous/unresolved `CALLS` edges
    /// (issues #152/#134) and every other label (`REFERENCES`, `MENTIONS`,
    /// `IMPORTS`, `IMPLEMENTS`, containment) are excluded. Direction is
    /// honored: `path A B` and `path B A` are distinct queries.
    ///
    /// Both endpoints resolve by the same symbol-name / record-ID semantics as
    /// the other code-handle verbs. When a directed path exists the answer is a
    /// single deterministic witness path: a summary envelope line followed by
    /// one line per hop, each hop citing the from/to `record_id`, `name`,
    /// `kind`, `repo_relative_path`, and `span`, plus the edge label and
    /// confidence. `A == B` is a trivial zero-hop path.
    ///
    /// Selection is deterministic (documented tie-break: minimum hop count,
    /// then the lexicographically smallest `(source_record_id,
    /// edge_record_id)` discovery pointer per node), byte-identical across
    /// runs. The witness is a reachability LEAD, never proof of runtime control
    /// flow, and a `no_path` verdict is not proof of non-reachability.
    ///
    /// Exit codes:
    ///   0 — a directed path was found (including the trivial `A == B` path).
    ///   1 — malformed / ambiguous / unsupported handle or selector for either
    ///       endpoint (machine-readable JSON on stderr; ambiguous names list
    ///       all candidate record IDs).
    ///   2 — an endpoint resolves to no live record (`no_match` / `stale_handle`),
    ///       both endpoints resolve but no directed path exists (`no_path`), or
    ///       --at/--as-of names no resolvable commit.
    ///
    /// Documented in `docs/cli/path.md` and `docs/cli/query.md`.
    Path {
        /// Source symbol record ID (`codegraph:vN:<hex>`) or exact symbol name.
        from: String,
        /// Target symbol record ID (`codegraph:vN:<hex>`) or exact symbol name.
        to: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict symbol resolution to one repository.
        #[arg(long)]
        repo: Option<String>,
        /// Trace the path against the graph state at this commit SHA or unique
        /// prefix (requires a history graph). Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Trace the path against the graph state at the most recent commit at
        /// or before this RFC 3339 instant. Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Corpus selector (issue #427): head-anchor the current-state view to
        /// each repository's stamped HEAD, so a path over an edge removed at
        /// HEAD yields `no_path`. This is the DEFAULT when a source snapshot
        /// exists; the flag makes it explicit. Mutually exclusive with
        /// --all-history/--at/--as-of (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #427): trace over the UNION of all commit
        /// snapshots so a path over an edge removed at a later commit still
        /// resolves. Mutually exclusive with --at-head/--at/--as-of (enforced
        /// at runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List the files that import a module path (issue #444).
    ///
    /// A read-only lookup over the `Import` nodes the language extractors mint:
    /// given a `::`-separated module path (`serde`, `foo::bar`,
    /// `crate::query::liveness`), returns every importing file whose recorded
    /// `use` declaration names a module path with the query as a SEGMENT-AWARE
    /// prefix (`foo::bar` matches `foo::bar::Baz` and `foo::bar`, never
    /// `foo::barbell`). Alias (`as`), group (`{A, B}`), and glob (`*`) imports
    /// are reduced to their module path before matching. Because only
    /// extractor-minted Import nodes are considered, a doc-comment or string
    /// mention of the path is invisible — the precision win over `grep`.
    ///
    /// `crate::`-relative and absolute `<crate>::` imports are DISTINCT by
    /// default. Pass `--crate <name>` to rewrite a leading `crate::` (in the
    /// query and in imports) to that crate name so the two forms unify. This is
    /// caller-supplied ground truth, never guessed — and it is NOT the same
    /// flag as `eg query symbol --package` (issue #117), which scopes by the
    /// Cargo package that OWNS a file rather than rewriting a module path.
    ///
    /// Every row carries a stable record ID plus the repo-relative importing
    /// file/span handle. Output is deterministic and byte-identical across runs
    /// and across `--graph` / `--data-dir`. Rows are import-site LEADS, never
    /// proof the imported item is used.
    ///
    /// Exit codes:
    ///   0 — at least one importer found.
    ///   1 — malformed module path (machine-readable JSON on stderr).
    ///   2 — well-formed query with zero importers (`no_match`).
    ///
    /// Documented in `docs/cli/who-imports.md`.
    WhoImports {
        /// The `::`-separated module path to look up (e.g. `serde::Serialize`).
        module_path: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Override the owning-crate auto-resolution (issue #450): unify a leading
        /// `crate::` with this crate name for the query and every import.
        /// Without it, each import's `crate::` resolves from the owning-crate
        /// facts (target name, package attribution, workspace directory name).
        #[arg(long = "crate")]
        crate_name: Option<String>,
        /// Restrict the importer set to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Corpus selector (issue #427): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding imports removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. who-imports has no --at/--as-of selector; mutually
        /// exclusive with --all-history (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #427): read the UNION of all commit snapshots
        /// so an import present only in an earlier commit still appears.
        /// Mutually exclusive with --at-head (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List the symbols that construct a type via a `Type { … }` literal
    /// (issue #471) — the inbound `CONSTRUCTS` construction sites (PR #467).
    ///
    /// The type-anchored, first-class form of the `construction_sites` group
    /// `eg query change-impact <type>` already surfaces: both read the same
    /// `CONSTRUCTS` edges and expose the same `e0063_risk` signal. Resolve a
    /// type NAME or canonical record ID; each row cites the constructing
    /// symbol's stable record ID + repo-relative file/span handle and the
    /// `e0063_risk` flag (`true` when a new required field would break the
    /// site — the exhaustive, non-`..base` form; `false` for the FRU `..base`
    /// form). Output is deterministic and byte-identical across runs and across
    /// `--graph` / `--data-dir`. Rows are construction-site LEADS.
    ///
    /// Exit codes:
    ///   0 — at least one constructor found.
    ///   1 — malformed handle, ambiguous name, or unsupported handle
    ///       (machine-readable JSON on stderr).
    ///   2 — well-formed type with zero live constructors (`no_match`).
    ///
    /// Documented in `docs/cli/who-constructs.md`.
    WhoConstructs {
        /// The type to look up (exact name or canonical record ID).
        handle: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the constructor set to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Resolve construction sites at this commit (full SHA or unique
        /// prefix). Mutually exclusive with --as-of and the corpus flags.
        #[arg(long)]
        at: Option<String>,
        /// Resolve construction sites as of this RFC 3339 instant. Mutually
        /// exclusive with --at and the corpus flags.
        #[arg(long)]
        as_of: Option<String>,
        /// Corpus selector (issue #427): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding sites removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history / --at / --as-of.
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #427): read the UNION of all commit snapshots
        /// so a site present only in an earlier commit still appears. Mutually
        /// exclusive with --at-head / --at / --as-of.
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Digest the recent agent sessions recorded for one repository (issue #112).
    ///
    /// Answers "what did agents recently do in this repository?" from records
    /// that already exist: one row per live `AgentSession` whose members cite
    /// code in the selected repository, ordered by last activity descending,
    /// carrying the agent/session handles, time bounds, run outcomes,
    /// referenced tasks, and per-kind record counts.
    ///
    /// Trust separation is explicit and never collapsed: every session row is
    /// `agent_authored` — an agent CLAIM, never verification, never proof a
    /// task completed or that code works — while each referenced task carries
    /// `project_state`, the recorded project fact, passed through verbatim and
    /// never re-judged. An absent citation is not evidence that no work
    /// happened. Session summaries never leave the store verbatim: rows carry a
    /// structured label plus a BLAKE3 handle.
    ///
    /// Membership is EDGE-DERIVED only (`AUTHORED_BY` / `SESSION_OF`, at most
    /// three hops); a record merely stamped with a matching `session_id` string
    /// is NOT a member and is reported once under
    /// `unlinked_session_stamped_records`. Repository scope comes from member
    /// citations (`MENTIONS_SYMBOL` / `TOUCHED_FILE` / `OBSERVES` /
    /// `FAILED_ON`, as graph edges or on-node evidence links) and from one hop
    /// through a referenced task; a session with no resolvable repository is
    /// excluded from every digest and listed under
    /// `unresolved_repository_scope`, never silently dropped.
    ///
    /// This slice has no temporal selectors (`--at` / `--as-of`) and no corpus
    /// flags: the answer is the current recorded state. Read-only over
    /// `--graph` / `--data-dir` (the embedded store is read through a throwaway
    /// copy) and byte-identical across runs.
    ///
    /// Exit codes:
    ///   0 — rows returned, or an explicit empty digest (`no_sessions`).
    ///   1 — unknown / ambiguous repository selector, or a `--limit` outside
    ///       1..=200 (machine-readable JSON on stderr).
    ///
    /// Documented in `docs/cli/agent-sessions.md`.
    Sessions {
        /// Repository selector (record ID, basename, remote URL, root commit
        /// SHA, canonical path, or final-path-segment shorthand).
        repo: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Maximum session rows returned (default 20, max 200). Values outside
        /// 1..=200 — negative, non-numeric, or wider than any fixed-width
        /// integer — are rejected with an `invalid_limit` diagnostic.
        ///
        /// Raw `String`, not a fixed-width integer: ANY bounded integer field
        /// (`usize`, `i64`, even `i128`) still gives clap a ceiling of its
        /// own, so a decimal literal one past that type's max reaches clap's
        /// built-in parser error before this lane's dispatch arm ever runs
        /// (issue #112 review round 22, after rounds 6/19 each just moved the
        /// same ceiling to a wider type). Accepting the raw token instead and
        /// parsing it ourselves below removes the ceiling entirely — no
        /// magnitude of decimal integer can bypass the documented
        /// machine-readable envelope.
        #[arg(long, allow_hyphen_values = true, default_value_t = query::SESSIONS_DEFAULT_LIMIT.to_string())]
        limit: String,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    // Appended (issue #265); kept at the end to minimize cross-lane merge conflicts.
    /// List code targets (symbols and/or files) with zero cross-domain
    /// evidence edges into the agent-memory, verification, or project
    /// domains, ranked riskiest-first by inbound structural reference count.
    ///
    /// A target is a blind spot when no live, current-state edge with one of
    /// the documented evidence classes (`OBSERVES`, `MENTIONS_SYMBOL`,
    /// `TOUCHED_FILE`, `FAILED_ON`, `TOUCHES_FILE`) targets it from a node in
    /// the agent-memory, verification, or project domains — i.e. no linked
    /// observation, verification record, or task. Evidence does not inherit
    /// between a file and its symbols.
    ///
    /// Every row is a triage lead, never a verdict: absence of recorded
    /// evidence is not evidence the code is unimportant, untested, or
    /// unsafe. An empty blind-spot set is an explicit machine-readable
    /// success: exit 0, `ok:true`, an empty `blind_spots` array, and a
    /// `no_blind_spots` diagnostic — distinct from the `no_targets`
    /// diagnostic of a target-free store and from a store-absent error
    /// (exit 1).
    ///
    /// Documented in `docs/cli/blind-spots.md`.
    BlindSpots {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the evaluated targets to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Which code-target populations to evaluate.
        #[arg(long, default_value = "both")]
        kind: BlindSpotKindArg,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    // Appended (issue #262); kept at the end to minimize cross-lane merge conflicts.
    /// Rank agents by the downstream fate of their observations.
    ///
    /// One row per `agent_id` (with `agent_kind`), carrying deterministic
    /// counts over: observations written; promotion outcomes split by
    /// terminal verdict (`approved`, `edited_then_approved`, `rejected`,
    /// `deferred`, `expired`) for candidates whose supporting evidence is
    /// this agent's observations; observations later superseded
    /// (`superseded_by`); and linked verification outcomes
    /// (passed/failed/inconclusive) attributable to the agent's sessions.
    ///
    /// Trust is separated structurally: `observations_written` and
    /// `superseded_observations` aggregate agent-authored claims,
    /// `promotion_outcomes` aggregates recorded operator decisions, and
    /// `verification_outcomes` aggregates recorded verification evidence —
    /// the output never presents an agent's claim as source truth. Every
    /// nonzero bucket cites resolvable `record_id` handles; an empty store
    /// (or no qualifying agents) returns an explicit `no_agents` diagnostic
    /// rather than silence.
    ///
    /// Rows sort canonically by `agent_id`; the answer is byte-stable across
    /// repeated runs and platforms. JSON is the default; `--format text`
    /// gives a skimmable table. Respects `--repo` scoping like the other
    /// lanes; requires no network and no `--embed` store.
    ///
    /// This slice reports — it never gates, weights, throttles, or disables
    /// agents, and it computes no learned reputation score: deterministic
    /// counts over existing edges only.
    ///
    /// Documented in `docs/cli/agent-track-record.md`.
    #[command(visible_alias = "agents")]
    TrackRecord {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Retrace one agent session's footprint, claims, and verification.
    ///
    /// No footprint found is not evidence the run did nothing: a run that
    /// never wrote a cited edge, a dropped transcript import, or a partially
    /// ingested store all look the same as an idle session.
    ///
    /// Documented in `docs/cli/session-retrospective.md`.
    Session {
        /// Session record ID or imported session handle.
        id_or_handle: String,
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
    // Appended (issue #258); kept at the end to minimize cross-lane merge conflicts.
    /// Find every usage site of an external dependency symbol before upgrade.
    ///
    /// Answers "where is `tokio::spawn` actually called?" from the
    /// AST-derived unresolved `CALLS` edges, resolving each written callee
    /// against the file's `use` declarations — so
    /// `use a::b::c as d; d(...)` reports under `a::b::c`. The query path may
    /// be fully or partially qualified (`tokio::spawn`, `aletheiadb`);
    /// matching is segment-aware, so `tokio::spaw` never matches. Comments and
    /// string literals can never produce rows: every row is backed by a parsed
    /// call site. Paths inside the repository's own crate namespace are
    /// reported with `external: false` and a hint toward
    /// `eg query transitive-callers`.
    ///
    /// Exit codes:
    ///   0 — at least one usage site found.
    ///   1 — malformed path, or an unsupported corpus/temporal combination
    ///       (machine-readable JSON on stderr).
    ///   2 — well-formed path with zero usage sites (`no_match`).
    ///
    /// Documented in `docs/cli/query.md`.
    Uses {
        /// The dependency symbol path to find (fully or partially qualified,
        /// `::`-separated, e.g. `tokio::spawn` or `aletheiadb`).
        path: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict the usage sites to one repository in a multi-repo store.
        #[arg(long)]
        repo: Option<String>,
        /// Resolve usage sites at this commit (full SHA or unique
        /// prefix). Mutually exclusive with --as-of and the corpus flags.
        #[arg(long)]
        at: Option<String>,
        /// Resolve usage sites as of this RFC 3339 instant. Mutually
        /// exclusive with --at and the corpus flags.
        #[arg(long)]
        as_of: Option<String>,
        /// Corpus selector (issue #427): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding sites removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history / --at / --as-of.
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #427): read the UNION of all commit snapshots
        /// so a site present only in an earlier commit still appears. Mutually
        /// exclusive with --at-head / --at / --as-of.
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Emit the machine-readable query-lane capability manifest (issue #251).
    ///
    /// Local and pure: derived from the command surface alone — no network,
    /// no ingested store, no project config. Works on a fresh clone with an
    /// empty data dir.
    Lanes {
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Inventory `#[allow(...)]` / `#![allow(...)]` lint suppressions (issue #227).
    ///
    /// Returns every Tree-sitter-detected lint-suppression attribute as a
    /// citable debt row: a stable record ID, the sorted lint-name list
    /// (`dead_code`, `clippy::too_many_arguments`, …), the closed scope
    /// (`item` / `module` / `crate`), the adjacent justification-comment
    /// signal, the repo-relative file/span handle, and the enclosing symbol
    /// handle (explicit `null` when no symbol encloses the attribute —
    /// always at crate scope, and at module scope where the module is a
    /// `Module` record rather than a `Symbol`). `#[allow(` text inside line
    /// comments, doc comments, block comments, and string literals is never
    /// returned; detection runs over the attribute AST, not the raw text.
    ///
    /// Rows derive solely from deterministic extractor facts and assert only
    /// that a suppression of a lint set exists at a span — never that the
    /// suppression is warranted. Strictly read-only; byte-identical across
    /// runs on an unchanged store.
    ///
    /// Exit codes:
    ///   0 — suppressions returned (or the scoped slice contains zero
    ///       suppressions, with `empty_reason: "no_suppressions_in_scope"`).
    ///   1 — malformed prefix, ambiguous commit prefix, or unknown/ambiguous
    ///       repository selector.
    ///   2 — scope not found (`scope_not_found`) or unknown commit
    ///       (`unknown_commit`).
    ///
    /// Documented in `docs/cli/suppressions.md`.
    Suppressions {
        /// Optional repo-relative directory or module path prefix scoping the
        /// inventory (segment-aware; same contract as `eg query subsystem`).
        #[arg(long)]
        path: Option<String>,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Pin the inventory to a commit SHA or unique prefix on the
        /// valid-time axis (same selector contract as `eg query symbol --at`).
        #[arg(long)]
        at: Option<String>,
        /// Restrict results to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Corpus selector (issue #456): head-anchor the current-state view to
        /// each repository's stamped HEAD, excluding records removed at HEAD.
        /// This is the DEFAULT when a source snapshot exists; the flag makes it
        /// explicit. Mutually exclusive with --all-history and --at (enforced at
        /// runtime with an `unsupported_combination` envelope).
        #[arg(long)]
        at_head: bool,
        /// Corpus selector (issue #456): read the UNION of all commit snapshots
        /// so a record removed at a later commit still appears. Mutually
        /// exclusive with --at-head and --at (enforced at runtime with an
        /// `unsupported_combination` envelope).
        #[arg(long)]
        all_history: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Records added to the store since a transaction-time cursor (issue #197).
    ///
    /// The delta feed for resuming and swarm agents: returns every record
    /// whose transaction-time handle is strictly after `--tx-after`, spanning
    /// all domains, ordered by `(transaction_time, record_id)`. The answer
    /// carries a `next_cursor`; feeding it back returns only records added
    /// after the prior call, with no gaps and no overlap. Retraction and
    /// supersession records appear as records (classified by `event`), so a
    /// consumer learns a prior record was retired.
    ///
    /// Exit codes:
    ///   0 — delta returned (an empty delta is a well-formed `up_to_date`
    ///       answer, not an error).
    ///   1 — load error, invalid cursor, unknown domain, invalid --limit, or
    ///       unknown/ambiguous --repo selector.
    ///
    /// Documented in `docs/cli/since.md`.
    Since {
        /// Transaction-time cursor: only records with `transaction_time`
        /// strictly after this RFC 3339 instant are returned.
        #[arg(long)]
        tx_after: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Restrict records to one repository (see `eg query symbol --help`).
        #[arg(long)]
        repo: Option<String>,
        /// Restrict the feed to one or more domains (repeatable). One of
        /// `codegraph`, `agent_memory`, `verification`, `artifact`,
        /// `project`, `semantic`, `user_context`, `log`. Unknown names exit 1
        /// with an `unknown_domain` diagnostic.
        #[arg(long)]
        domain: Vec<String>,
        /// Maximum records returned (default 500, max 5000); excess records
        /// are truncated with an explicit `truncated` signal. Truncation only
        /// ever cuts between transaction-time groups, so `next_cursor` stays
        /// gap-free.
        #[arg(long, default_value_t = query::SINCE_DEFAULT_LIMIT)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Dereference a cited record-id handle back to its live source record
    /// with a typed drift verdict (issue #160).
    ///
    /// The read-back half of the citation contract: a `record_id` emitted by
    /// `query symbol|file|semantic` resolves to its record and reports
    /// `valid` (live, coordinates unchanged), `drifted` (the pinned
    /// `--at`/`--as-of` snapshot's record differs from the current view), or
    /// `dangling` (no record matches — structured envelope, exit 2, never a
    /// fuzzy guess). Codegraph ids only; other domains are refused as
    /// `unsupported_handle_domain`.
    Resolve {
        /// The cited record id (e.g. `codegraph:v1:<suffix>`).
        record_id: String,
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
        /// Resolve the record at this commit SHA or unique prefix, then
        /// compare against the current view for the drift verdict. Mutually
        /// exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Resolve the record at the most recent commit at or before this
        /// RFC 3339 instant (valid-time axis), then compare against the
        /// current view for the drift verdict. Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum IngestAdapter {
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
pub(crate) enum DaemonAction {
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
pub(crate) enum RepairCliAction {
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
        /// Output format: `json` (default, machine-readable) or `text`.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
    },
    /// Run an offline repair session (requires `--confirm` or `--dry-run`).
    ///
    /// Dry-run proves zero mutations while returning the same allow/refuse verdict
    /// and a `planned` manifest preview. Confirmed repair removes (default) or
    /// quarantines stale metadata, writes a redaction-safe manifest, and writes a
    /// recovery report. A healthy stopped store reports `no_repair_needed` and is
    /// never touched.
    ///
    /// Supported actions: `stale_metadata_cleanup`, `stale_metadata_quarantine`,
    /// `recovery_report_generation`. Unsupported: graph-record deletion, store
    /// rewrite, compaction, migration.
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
        /// Quarantine (move aside, recoverable) stale metadata instead of
        /// deleting it.
        #[arg(long)]
        quarantine: bool,
        /// Repair write-receipt (idempotency) anomalies instead of runtime
        /// metadata (issue #460). Enumerates anomalies by default (read-only);
        /// applies the provably-safe subset under `--confirm`.
        #[arg(long)]
        receipts: bool,
        /// Output format: `json` (default, machine-readable) or `text`.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        /// Fixed RFC 3339 action time for deterministic output (useful for
        /// tests). Defaults to the current wall-clock instant.
        #[arg(long)]
        transaction_time: Option<String>,
    },
}

/// Subcommands for `write`.
#[derive(Debug, Subcommand)]
pub(crate) enum WriteKind {
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
        /// Supersede a prior observation-class record (issue #184): authors a
        /// `SUPERSEDES` edge from the new observation to this record ID. The
        /// target must be a live observation-class record (`Observation`,
        /// `Decision`, `Failure`) in the store pointed at by `--graph` or
        /// `--data-dir`; deterministic code-graph facts are rejected. Mutually
        /// exclusive with `--contradicts`.
        #[arg(long, conflicts_with = "contradicts")]
        supersedes: Option<String>,
        /// Contradict a prior observation-class record (issue #184): authors
        /// a `CONTRADICTS` edge from the new observation to this record ID.
        /// A contradiction is a mutual dispute, not a winner declaration —
        /// both records stay live and citable. Same target rules as
        /// `--supersedes`. Mutually exclusive with `--supersedes`.
        #[arg(long, conflicts_with = "supersedes")]
        contradicts: Option<String>,
        /// Graph JSONL file used to validate the `--supersedes` /
        /// `--contradicts` target at write time. Mutually exclusive with
        /// `--data-dir`. The file is read, never modified.
        #[arg(long, conflicts_with = "data_dir")]
        graph: Option<PathBuf>,
        /// Embedded store directory used to validate the `--supersedes` /
        /// `--contradicts` target at write time. Mutually exclusive with
        /// `--graph`. The store is opened read-only for validation.
        #[arg(long, conflicts_with = "graph")]
        data_dir: Option<PathBuf>,
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
    /// Write a typed live-authored `Failure` record in the agent-memory domain.
    ///
    /// Records a failed attempt as a citable node: provenance (`agent_id`,
    /// `agent_kind`, `session_id`, `observed_at`, `source_handle`) plus a
    /// reserved failure kind and at least one resolvable target
    /// (`--failed-on` for codegraph/artifact record IDs, `--references-task`
    /// for project task record IDs). Missing required fields fail with a
    /// machine-readable JSON envelope on stderr
    /// (`{"code":"missing_field","field":"<name>"}`) that never echoes
    /// the failure text.
    Failure {
        /// Stable agent identity; required.
        #[arg(long, default_value = "")]
        agent_id: String,
        /// Agent kind (`codex`, `claude-code`, `vantage`, `rust-swe-agent`,
        /// `human`, `other`); required.
        #[arg(long, default_value = "")]
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
        /// Failure kind: `command_failure`, `patch_invalid`,
        /// `assumption_rejected`, or `workflow_blocked`; required.
        #[arg(long, default_value = "")]
        failure_kind: String,
        /// Failure description; required. Redacted and bounded to a 500-byte
        /// excerpt before storage; never echoed in error output.
        #[arg(long, default_value = "")]
        text: String,
        /// Shell exit code; only valid for `command_failure`/`patch_invalid`.
        #[arg(long)]
        exit_code: Option<i64>,
        /// Canonical codegraph/artifact record IDs the attempt failed on
        /// (`FAILED_ON` links; repeatable). At least one of `--failed-on` /
        /// `--references-task` is required.
        #[arg(long, num_args = 0..)]
        failed_on: Vec<String>,
        /// Canonical project task record IDs this failure relates to
        /// (`REFERENCES_TASK` links; repeatable).
        #[arg(long, num_args = 0..)]
        references_task: Vec<String>,
        /// Output JSONL path for the produced records.
        #[arg(long, required = true)]
        out: PathBuf,
    },
}

/// Subcommands for `audit`.
#[derive(Debug, Subcommand)]
pub(crate) enum AuditSubcommand {
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
        /// Minimum fraction of runtime-observation (log-domain) rows that must be
        /// cited (issue #328); defaults to `1.0`, the strictest gate.
        #[arg(long, default_value_t = crate::citation_audit::DEFAULT_MIN_LOG_CITATION)]
        min_log_citation: f64,
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
    /// Measure cold query latency and gate p50 against the latency budget (issue #255).
    ///
    /// Builds the pinned reference corpus, then times cold wall-clock from
    /// process start to the first emitted result line of
    /// `eg query symbol <NAME>` — fresh processes, `--graph <JSONL>` and
    /// `--data-dir <embedded store>` measured separately — reporting p50/p95
    /// per source. Local-first and offline.
    ///
    /// Exit codes:
    ///   0 — gate passed (`ok: true`).
    ///   1 — gate failed (`ok: false`); the full JSON report is still printed.
    ///   2 — usage/load error (bad manifest path, corpus build error, no result).
    QueryLatency {
        /// Path to the query-latency corpus manifest JSON.
        #[arg(long, default_value = "corpus/query_latency_corpus.json")]
        corpus: PathBuf,
        /// Cold samples measured per input source.
        ///
        /// Defaults to the manifest's `samples`. When supplied, overrides it.
        #[arg(long)]
        samples: Option<usize>,
        /// p50 budget in milliseconds each source must meet.
        ///
        /// Defaults to the manifest's `budget_p50_ms`. When supplied,
        /// overrides it.
        #[arg(long)]
        budget_p50_ms: Option<f64>,
        /// Which input sources to measure.
        #[arg(long, default_value = "both")]
        source: LatencySource,
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
    /// Load, validate, and hash-pin a SOC2 control->evidence-class catalog (issue #337).
    ///
    /// Parses a versioned control catalog (the embedded `soc2-v1` by default, or
    /// a `--catalog <path>` override), validates its schema-version tuple and
    /// evidence-class vocabulary, and prints a deterministic report carrying the
    /// catalog identity, its BLAKE3 hash-pin handle, and the per-control
    /// evidence-class map. Pure, offline, read-only.
    ///
    /// Exit codes:
    ///   0 — the catalog is valid; the report is printed to stdout.
    ///   2 — read/parse error, unknown evidence class, unknown schema version,
    ///       invalid requirement, duplicate control ID, or duplicate evidence
    ///       class; a redaction-safe JSON error is printed to stderr.
    ControlCatalog {
        /// Catalog document path. Defaults to the embedded `soc2-v1` catalog.
        #[arg(long)]
        catalog: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Assemble or re-verify a control-scoped, time-windowed evidence pack (issue #338).
    ///
    /// `assemble` composes the #337 catalog, #68 bundle scrub/hash/citation
    /// mechanics, and #65 citation classification into a deterministic,
    /// redaction-safe evidence pack scoped to one control and one half-open
    /// valid-time window. `verify` re-checks an assembled pack offline.
    ///
    /// Exit codes:
    ///   0 — every verdict passed (`ok: true`); empty windows are vacuous success.
    ///   1 — a verdict failed (`ok: false`); the full report is still printed.
    ///   2 — usage/load error (unknown control, reversed/invalid window, both or
    ///       neither input flag, unreadable store/graph/catalog).
    EvidencePack {
        /// The assemble/verify action.
        #[command(subcommand)]
        action: EvidencePackAction,
    },
    /// Gate review coverage over pull requests merged in a valid-time window (issue #339).
    ///
    /// For every PR merged in the half-open window `[from, to)` (keyed on
    /// `merged_at`), classifies it into the closed verdict set `{covered,
    /// approval_stale_head, self_approved_only, uncovered}` and gates on the
    /// `covered / merged_prs` ratio. Measures RECORDED review execution in the
    /// graph — never GitHub branch-protection configuration, review quality, or
    /// unrecorded reviews elsewhere. Shares its per-PR derivation with the #338
    /// evidence-pack `review_coverage` section (single implementation).
    ///
    /// Exit codes:
    ///   0 — coverage met the threshold (`ok: true`); empty windows are vacuous
    ///       success.
    ///   1 — coverage below threshold (`ok: false`); the full report is still
    ///       printed with a `below_review_coverage_threshold` diagnostic.
    ///   2 — usage/load error (reversed/invalid window, both or neither input
    ///       flag, out-of-range --min-coverage, unreadable store/graph).
    ReviewCoverage {
        /// Inclusive lower bound of the valid-time window (RFC 3339).
        #[arg(long)]
        from: String,
        /// Exclusive upper bound of the valid-time window (RFC 3339).
        #[arg(long)]
        to: String,
        /// Graph JSONL path (mutually exclusive with `--data-dir`).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` store directory (mutually exclusive with `--graph`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Minimum `covered / merged_prs` coverage fraction the window must meet.
        #[arg(long, default_value_t = 1.0)]
        min_coverage: f64,
        /// Require an approving review from a non-author identity (default on).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        require_non_author: bool,
        /// Require the approval to be anchored at the PR's final head (default on).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        require_final_head: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Report store-wide acceptance-criterion verification coverage (issue #115).
    ///
    /// Classifies every live `AcceptanceCriterion` in the store into a CLOSED,
    /// mutually exclusive bucket set — `proven`, `failed_evidence`,
    /// `dangling_evidence`, `non_verification_evidence`, `inconclusive_evidence`,
    /// `unverified` — names the CLAIMED-DONE-BUT-UNPROVEN set (criteria owned by
    /// a closed/done `Task` that carry no passing evidence), and gates on the
    /// proven ratio and that set's size.
    ///
    /// `proven` derives ONLY from a live `CLOSES_ACCEPTANCE_CRITERION` link to a
    /// verification-DOMAIN record with a passing outcome — never from
    /// `Task.status`, never from the criterion's own recorded `status`, never
    /// from an agent-authored claim. A poor number is a reason to REVIEW/VERIFY,
    /// never proof that any one task is wrong.
    ///
    /// Distinct from `eg query task` (#48, one task's evidence),
    /// `eg query verification-coverage` (#109, code symbols lacking
    /// verification), and `eg audit memory-health` (#94, agent-memory health).
    ///
    /// Exit codes:
    ///   0 — every threshold met (`ok: true`); a store with zero acceptance
    ///       criteria is a vacuous pass carrying `no_acceptance_criteria`.
    ///   1 — a threshold was breached (`ok: false`); the full report is still
    ///       printed with a `breaches` array naming each metric and its observed
    ///       value.
    ///   2 — usage/load error (both or neither input flag, out-of-range
    ///       threshold or `--limit`, unreadable/empty store or graph).
    CriteriaCoverage {
        /// Graph JSONL path (mutually exclusive with `--data-dir`).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` store directory (mutually exclusive with `--graph`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Minimum acceptable `proven / total_criteria` ratio.
        #[arg(long, default_value_t = 1.0)]
        min_proven_ratio: f64,
        /// Maximum acceptable claimed-done-but-unproven criterion count.
        #[arg(long, default_value_t = 0)]
        max_claimed_done_unproven: usize,
        /// Per-list row cap.
        #[arg(long)]
        limit: Option<usize>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Gate semantic-search relevance against a labeled query corpus (issue #106).
    ///
    /// Runs every corpus query against an `--embed` embedded store, computes
    /// hit-rate@k (k = 1, 5, 10), MRR, and recall, and gates on hit-rate@5 and
    /// MRR floors. The store-backed path requires the `embeddings` feature; with
    /// it off the command emits an honest capability-unavailable report and never
    /// silently passes. Rows carry record IDs, handles, metrics, and counts only.
    ///
    /// Exit codes:
    ///   0 — both floors met (`ok: true`).
    ///   1 — a floor was missed (`ok: false`); the full JSON report is still
    ///       printed with a machine-readable `breaches` list naming each metric
    ///       and its observed value.
    ///   2 — usage/capability error (bad `--min-*`, unreadable corpus/store, or
    ///       the `embeddings` feature is unavailable).
    SemanticRelevance {
        /// Path to the labeled relevance corpus JSON.
        #[arg(long, default_value = "corpus/semantic_relevance_corpus.json")]
        corpus: PathBuf,
        /// Embedded `AletheiaDB` store directory built with `--embed`.
        #[arg(long)]
        data_dir: PathBuf,
        /// Minimum hit-rate@5 the corpus must clear. Defaults to the issue floor.
        #[arg(long)]
        min_hit_rate_5: Option<f64>,
        /// Minimum MRR the corpus must clear. Defaults to the issue floor.
        #[arg(long)]
        min_mrr: Option<f64>,
        /// Ambiguous-query false-positive score threshold.
        #[arg(long, default_value_t = 0.5)]
        fp_threshold: f64,
        /// Retrieval depth kept per query (must be >= 10 to measure hit@10).
        #[arg(long, default_value_t = 10)]
        top_k: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Audit store-wide evidence-link integrity across all domains (issue #217).
    ///
    /// Sweeps every cross-domain evidence edge (standalone edge records and
    /// inline `EvidenceLink`s) in a JSONL graph (`--graph`) or embedded store
    /// (`--data-dir`) and reports each whose target record is absent or
    /// tombstoned. Strictly read-only; no network, indexing, or remote calls.
    ///
    /// Exit codes:
    ///   0 — no broken evidence links (`ok: true`).
    ///   1 — broken links found (`ok: false`); the full report is still printed.
    ///   2 — usage/load error (both/neither input flag, unreadable/empty store).
    EvidenceLinks {
        /// Graph JSONL path (mutually exclusive with `--data-dir`).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` store directory (mutually exclusive with `--graph`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Evaluate `AletheiaDB` schema constraints against an embedded store (issue #486).
    ///
    /// `eg validate` (#103) gates a graph JSONL file BETWEEN scan and ingest, so
    /// it cannot see a bad write that reaches a store by another path. 0.2.0's
    /// opt-in per-label schema constraints put a backstop at the write boundary
    /// itself; this command is how you evaluate and (optionally) declare them.
    ///
    /// The DEFAULT action is a strictly read-only Phase 1 report: the inventory
    /// of node labels and edge types the embedded adapter can write, reconciled
    /// against what the store actually holds, plus an upstream `.dry_run()`
    /// conformance scan of the chosen profile. It declares nothing.
    ///
    /// `--declare` is the opt-in Phase 2 action; `--drop` retracts it. Both take
    /// the exclusive write lease. A declaration covers only the universal
    /// identity/routing spine — never a per-kind payload field, because a
    /// constraint that fires on a legitimate future write is worse than no
    /// constraint.
    ///
    /// Exit codes:
    ///   0 — report produced with no violation, or declare/drop succeeded.
    ///   1 — non-conforming entities found, or `--declare` was refused (either
    ///       up front by non-conforming current state, or part-way by the
    ///       store). The FULL report is printed in every case, so a partial
    ///       declaration is always visible.
    ///   2 — usage/load error (missing or unreadable store, unreadable schema,
    ///       both mode flags, unknown profile, build without the embedded
    ///       adapter).
    ///
    /// See `docs/cli/schema-constraints.md`.
    SchemaConstraints {
        /// Embedded `AletheiaDB` store directory.
        #[arg(long)]
        data_dir: PathBuf,
        /// Candidate constraint profile: `spine` (default) or `full-base`.
        #[arg(long, default_value = "spine")]
        profile: String,
        /// Declare the profile on every writable label (opt-in, takes the write lease).
        #[arg(long)]
        declare: bool,
        /// Retract the schema constraints declared on the store.
        ///
        /// Scoped by default to labels Egregore itself can write - the exact
        /// inverse of `--declare`. A declaration on any other label was made by
        /// something else and is retained and reported, not silently destroyed.
        #[arg(long)]
        drop: bool,
        /// Widen `--drop` to retract declarations on labels outside Egregore's
        /// writable inventory too.
        #[arg(long)]
        include_foreign: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    // Appended (issue #185); kept at the end to minimize cross-lane merge conflicts.
    /// Sweep agent-memory evidence health: classify every evidence link of every
    /// live observation-class node (`Observation`, `Decision`, `Failure`) into
    /// exactly one bucket — `resolves_live`, `drifted`, or `dangling` — and flag
    /// denormalized-array vs stored-edge integrity violations (issue #185).
    ///
    /// Reads a seeded record set from a JSONL graph (`--graph`) or an embedded
    /// store (`--data-dir`); strictly read-only, no daemon required. Default
    /// output is newline-delimited JSON (one object per line: link rows, then
    /// integrity-violation rows, then a summary line); `--format text` renders
    /// the human view. Documented in `docs/cli/memory-evidence-health.md`.
    ///
    /// Exit codes:
    ///   0 — clean (`ok: true`): no dangling links, no integrity violations.
    ///       `drifted` links are freshness leads, not rot: they do not fail the gate.
    ///   1 — findings (`ok: false`); the full report is still printed.
    ///   2 — usage/load error (both or neither input flag, unreadable/empty store or graph).
    MemoryEvidenceHealth {
        /// Graph JSONL path (mutually exclusive with `--data-dir`).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` store directory (mutually exclusive with `--graph`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
}

/// Actions for `eg audit evidence-pack` (issue #338).
#[derive(Debug, Subcommand)]
pub(crate) enum EvidencePackAction {
    /// Assemble a control-scoped, time-windowed evidence pack.
    Assemble {
        /// Anchoring control ID (e.g. `CC8.1`).
        #[arg(long)]
        control: String,
        /// Inclusive lower bound of the valid-time window (RFC 3339).
        #[arg(long)]
        from: String,
        /// Exclusive upper bound of the valid-time window (RFC 3339).
        #[arg(long)]
        to: String,
        /// Graph JSONL path (mutually exclusive with `--data-dir`).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` store directory (mutually exclusive with `--graph`).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Control catalog document path. Defaults to the embedded `soc2-v1` catalog.
        #[arg(long)]
        catalog: Option<PathBuf>,
        /// Minimum review-coverage fraction the pack must meet.
        #[arg(long, default_value_t = 1.0)]
        min_review_coverage: f64,
        /// Pinned capture time (RFC 3339); no wall clock is read otherwise.
        #[arg(long)]
        captured_at: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Re-verify an assembled evidence pack offline (integrity, coverage, safety, window).
    Verify {
        /// Path to the evidence-pack JSON file to verify.
        path: PathBuf,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
}

/// Subcommands for `bundle`.
#[derive(Debug, Subcommand)]
pub(crate) enum BundleSubcommand {
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

/// Subcommands for `export` (issue #233).
#[derive(Debug, Subcommand)]
pub(crate) enum ExportSubcommand {
    /// Export a definitions-only SCIP code-intelligence index (issue #233).
    ///
    /// Reads a `scan`/`scan-history` graph JSONL (`--graph`) or an embedded
    /// store (`--data-dir`), strictly read-only, and writes an encoded SCIP
    /// protobuf to `--out`. Every span-bearing symbol becomes a
    /// `SymbolInformation` + definition `Occurrence`; edges, diagnostics,
    /// span-less nodes, and anonymous `impl` blocks are dropped. Byte-identical
    /// across runs. See `docs/cli/export-scip.md`.
    Scip {
        /// Path to the graph JSONL file (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long, conflicts_with = "graph")]
        data_dir: Option<PathBuf>,
        /// Output path for the encoded SCIP index (created or overwritten).
        #[arg(long)]
        out: PathBuf,
    },
}

/// Subcommands for `protected`.
#[derive(Debug, Subcommand)]
pub(crate) enum ProtectedSubcommand {
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
pub(crate) fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Scan {
            repo_path,
            out,
            repo_id_override,
            raw_literals,
        } => {
            let args = resolve_scan_args(repo_id_override, raw_literals);
            scan(&repo_path, &out, &args)
        }
        Commands::ScanHistory {
            repo_path,
            out,
            repo_id_override,
            raw_literals,
            max_commits,
            since,
            from,
            to,
            resume_from,
        } => {
            let mut args = resolve_scan_args(repo_id_override, raw_literals);
            args.max_commits = max_commits;
            args.since = since;
            args.from_rev = from;
            args.to_rev = to;
            scan_history(&repo_path, &out, &args, resume_from.as_deref())
        }
        Commands::Config { action } => match action {
            ConfigAction::Show => config_show(),
        },
        Commands::Schema { action } => match action {
            SchemaAction::Export {
                domain,
                kind,
                schema_version,
                out,
            } => schema_export_cmd(
                domain.as_deref(),
                kind.as_deref(),
                schema_version,
                out.as_deref(),
            ),
            SchemaAction::List { domain } => schema_list_cmd(domain.as_deref()),
        },
        Commands::ScanLogs {
            log_path,
            repo_path,
            out,
            repo_id_override,
            protected_raw_artifacts,
            protected_store,
            producer,
            captured_at,
        } => {
            // scan-logs takes no raw-literals flag; the config still governs
            // identity, fails fast when malformed, and warns on scope pins.
            let args = resolve_scan_args(repo_id_override, false);
            scan_logs(
                &log_path,
                &repo_path,
                &out,
                args.repo_id_override.as_deref(),
                protected_raw_artifacts,
                protected_store.as_deref(),
                producer.as_deref(),
                captured_at.as_deref(),
            )
        }
        Commands::VerifyScan {
            repo_path,
            repo_id_override,
            format,
        } => verify_scan_cmd(&repo_path, repo_id_override.as_deref(), format),
        Commands::CaptureBench {
            input,
            out,
            session_id,
            commit,
            suite,
            command,
            executed_at,
            repo,
            graph,
            format,
        } => capture_bench(&CaptureBenchArgs {
            input: &input,
            out: &out,
            session_id: &session_id,
            commit: &commit,
            suite: &suite,
            command: command.as_deref(),
            executed_at: &executed_at,
            graph: graph.as_deref(),
            format: &format,
            repo: repo.as_deref(),
        }),
        Commands::CaptureTests {
            input,
            out,
            session_id,
            commit,
            suite,
            command,
            exit_code,
            executed_at,
            runner,
            runner_version,
            repo,
            graph,
            format,
            protected_raw_artifacts,
            protected_store,
            producer,
        } => capture_tests::capture_tests(&capture_tests::CaptureTestsArgs {
            input: &input,
            out: &out,
            session_id: &session_id,
            commit: &commit,
            suite: &suite,
            command: &command,
            exit_code,
            executed_at: &executed_at,
            runner: runner.as_deref(),
            runner_version: runner_version.as_deref(),
            repo: repo.as_deref(),
            graph: graph.as_deref(),
            format: &format,
            protected_raw_artifacts,
            protected_store: protected_store.as_deref(),
            producer: producer.as_deref(),
        }),
        Commands::CaptureCoverage {
            input,
            out,
            session_id,
            commit,
            suite,
            command,
            exit_code,
            executed_at,
            tool,
            tool_version,
            repo,
            repo_root,
            graph,
            format,
            protected_raw_artifacts,
            protected_store,
            producer,
        } => capture_coverage::capture_coverage(&capture_coverage::CaptureCoverageArgs {
            input: &input,
            out: &out,
            session_id: &session_id,
            commit: &commit,
            suite: suite.as_deref(),
            command: &command,
            exit_code,
            executed_at: &executed_at,
            tool: &tool,
            tool_version: tool_version.as_deref(),
            repo: repo.as_deref(),
            repo_root: repo_root.as_deref(),
            graph: graph.as_deref(),
            format: &format,
            protected_raw_artifacts,
            protected_store: protected_store.as_deref(),
            producer: producer.as_deref(),
        }),
        Commands::LinkLogs {
            graph,
            data_dir,
            out,
            tolerance,
            at,
            as_of,
        } => link_logs_cmd(
            &graph,
            data_dir.as_deref(),
            &out,
            tolerance,
            at.as_deref(),
            as_of.as_deref(),
        ),
        Commands::ResolveFrames {
            log_graph,
            graph,
            data_dir,
            out,
            at,
            as_of,
        } => resolve_frames_cmd(
            log_graph.as_deref(),
            graph.as_deref(),
            data_dir.as_deref(),
            &out,
            at.as_deref(),
            as_of.as_deref(),
        ),
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
        Commands::Repos {
            graph,
            data_dir,
            format,
        } => repos_cmd(graph.as_deref(), data_dir.as_deref(), format),
        Commands::Brief {
            repo_path,
            graph,
            data_dir,
            repo,
            staged_only,
            format,
        } => brief_cmd(
            &repo_path,
            graph.as_deref(),
            data_dir.as_deref(),
            repo.as_deref(),
            staged_only,
            format,
        ),
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
        Commands::Validate { graph, format } => validate_cmd(&graph, format),
        Commands::Index { graph } => index_cmd(&graph),
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::Init {
            repo_path,
            data_dir,
            format,
            #[cfg(feature = "embeddings")]
            no_embed,
            #[cfg(feature = "embeddings")]
            embed_model,
            repo_id_override,
            raw_literals,
        } => init_cmd(&InitArgs {
            repo_path,
            data_dir,
            format,
            #[cfg(feature = "embeddings")]
            no_embed,
            #[cfg(feature = "embeddings")]
            embed_model,
            repo_id_override,
            raw_literals,
        }),
        #[cfg(not(feature = "embedded-aletheiadb"))]
        Commands::Init { .. } => anyhow::bail!(
            "eg init requires the embedded-aletheiadb feature (re-run without --no-default-features)"
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
            #[cfg(feature = "embeddings")]
            embed_model,
            #[cfg(feature = "embedded-aletheiadb")]
            force,
            dangling_citation_policy,
        } => ingest(
            &graph,
            adapter,
            data_dir.as_deref(),
            &agent_id,
            &session_id,
            idempotency_key.as_deref(),
            #[cfg(feature = "embeddings")]
            embed,
            #[cfg(feature = "embeddings")]
            embed_model,
            #[cfg(feature = "embedded-aletheiadb")]
            force,
            dangling_citation_policy,
        ),
        Commands::Export {
            format,
            data_dir,
            out,
        } => match format {
            Some(ExportSubcommand::Scip {
                graph,
                data_dir: scip_data_dir,
                out,
            }) => export_scip(graph.as_deref(), scip_data_dir.as_deref(), &out),
            None => {
                let data_dir = data_dir.ok_or_else(|| {
                    anyhow::anyhow!("eg export requires --data-dir <dir> for the JSONL dump")
                })?;
                let out = out.ok_or_else(|| {
                    anyhow::anyhow!("eg export requires --out <file> for the JSONL dump")
                })?;
                export(&data_dir, &out)
            }
        },
        Commands::ImportTraj {
            traj_path,
            out,
            redaction_report,
        } => import_traj_cmd(&traj_path, &out, redaction_report.as_deref()),
        Commands::ImportCodex {
            codex_path,
            out,
            redaction_report,
        } => import_codex_cmd(&codex_path, &out, redaction_report.as_deref()),
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
        Commands::Forget {
            handle,
            data_dir,
            reason,
            retracted_by,
            transaction_time,
        } => forget_cmd(&handle, &data_dir, reason, retracted_by, transaction_time),
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::ForgetRepo {
            selector,
            data_dir,
            reason,
            evicted_by,
            transaction_time,
            confirm,
        } => forget_repo_cmd(
            &selector,
            &data_dir,
            reason,
            evicted_by,
            transaction_time,
            confirm,
        ),
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::Retire {
            handle,
            graph,
            data_dir,
            reason,
            superseded_by,
            evidence_handle,
            retired_by,
            transaction_time,
        } => retire_cmd(RetireCmdArgs {
            handle: &handle,
            graph: graph.as_deref(),
            data_dir: data_dir.as_deref(),
            reason: &reason,
            superseded_by,
            evidence_handle,
            retired_by,
            transaction_time,
        }),
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::Reinstate {
            handle,
            graph,
            data_dir,
            reason,
            reinstated_by,
            transaction_time,
        } => reinstate_cmd(ReinstateCmdArgs {
            handle: &handle,
            graph: graph.as_deref(),
            data_dir: data_dir.as_deref(),
            reason,
            reinstated_by,
            transaction_time,
        }),
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
            #[cfg(feature = "embeddings")]
            embed_model,
            raw_literals,
        } => scan_refresh_cmd(
            &repo_path,
            &resolve_data_dir(data_dir.as_deref()),
            cache.as_deref(),
            format,
            #[cfg(feature = "embeddings")]
            embed,
            #[cfg(feature = "embeddings")]
            embed_model,
            raw_literals,
        ),
        #[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
        Commands::Reembed {
            data_dir,
            model,
            format,
            dry_run,
        } => reembed_cmd::reembed_cmd(&data_dir, &model, format, dry_run),
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

// ---------------------------------------------------------------------------
// Query output types
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Eq, PartialEq, Debug)]
pub(crate) struct DiagnosticRef<'a> {
    record_id: &'a str,
    repo_relative_path: &'a str,
    span: SourceSpan,
}

#[derive(Serialize)]
pub(crate) struct SymbolResult<'a> {
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
    /// The instant this row's version is valid as of (RFC 3339), from the
    /// temporal metadata (issue #181) — the same axis the `symbol_at` MCP
    /// tool cites. Absent for records that predate temporal metadata: never
    /// fabricated.
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
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
    /// Owning Cargo package (issue #117): the package NAME plus the
    /// repo-relative path of the owning `Cargo.toml`, or the closed-set reason
    /// no package owns this record.
    ///
    /// OMITTED entirely for a record produced before issue #117 — attribution
    /// UNKNOWN, which is a different fact from a present value carrying
    /// `status: unattributed` (computed, and provably ownerless).
    #[serde(skip_serializing_if = "Option::is_none")]
    crate_attribution: Option<&'a crate::ir::CrateAttribution>,
    /// The containment caveat, stamped only when `--package` scoped the answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    crate_attribution_disclaimer: Option<&'static str>,
    /// Corpus this row was read from (issue #427). Present only on the
    /// `query symbol` lane (stamped by [`stamp_symbol_corpus`]); absent on the
    /// shared `query symbols` partial-name lane so its row shape is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    corpus_mode: Option<&'static str>,
    /// How the corpus mode was chosen (`default`/`selector`). Present only with
    /// `corpus_mode`.
    #[serde(skip_serializing_if = "Option::is_none")]
    corpus_mode_source: Option<&'static str>,
    /// One-line human description of the corpus that was read. Present only with
    /// `corpus_mode`.
    #[serde(skip_serializing_if = "Option::is_none")]
    corpus_disclaimer: Option<String>,
    /// Test-vs-production role of the row's record (issue #238): `test` or
    /// `production`, drawn from the AST and the file path (a
    /// `TrustClass::SourceDerived` fact, never agent-authored).
    ///
    /// OMITTED entirely for a record produced before issue #238 — role
    /// UNKNOWN, which is a different fact from a present `production` value
    /// (computed, and provably not test-gated). The text render likewise
    /// prints nothing for an absent role rather than fabricating one.
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'a crate::ir::SymbolRole>,
    /// Conditional-compilation gate chain (issue #190): the normalized
    /// `#[cfg(...)]` / `#[cfg_attr(...)]` predicates lexically gating the
    /// row's record, outermost gate first (file `#![cfg]` inner attributes,
    /// then enclosing gated items/modules, then the item's own attributes).
    /// Each entry is the predicate exactly as written — never evaluated.
    ///
    /// OMITTED entirely for a record produced before issue #190, and for an
    /// ungated record — gate UNKNOWN-or-absent, which is a different fact
    /// from a present chain. The text render likewise prints nothing for an
    /// absent chain rather than fabricating a gate.
    #[serde(skip_serializing_if = "Option::is_none")]
    cfg: Option<&'a Vec<String>>,
    /// Deterministic structural complexity score (issue #162): 1 plus one
    /// per decision point in the item's own body. Present on Rust callable
    /// `Symbol` nodes (`function` / `method` / `test` symbol kinds); a
    /// `TrustClass::SourceDerived` code fact, never agent confidence.
    ///
    /// OMITTED entirely for a record produced before issue #162 and for
    /// non-callable symbols — score UNKNOWN-or-inapplicable, which is a
    /// different fact from a present minimum. The text render likewise
    /// prints nothing for an absent score rather than fabricating one.
    #[serde(skip_serializing_if = "Option::is_none")]
    complexity: Option<u32>,
}

/// Resolves the effective corpus mode for a current-state code lane and,
/// when the mode is [`query::CorpusMode::HeadAnchored`], returns the
/// head-anchored record slice to traverse (issue #456).
///
/// This is the shared CLI-layer wiring the flipped lanes call: it composes
/// [`query::resolve_corpus_mode`] (flag/selector precedence + mutual-exclusion)
/// with [`query::non_head_current_record_ids`] (the HEAD-anchor pre-filter).
/// The returned `Vec` — when `Some` — is the narrowed record set with every
/// record NOT current at its owning repository's stamped HEAD dropped BEFORE the
/// lane's pure query fn runs (whose own #468 liveness gate then applies over the
/// narrowed slice; the two filters are orthogonal). `None` means "traverse the
/// full slice unchanged" — [`query::CorpusMode::Union`],
/// [`query::CorpusMode::SingleSnapshot`], and [`query::CorpusMode::CommitPinned`]
/// (a temporal pin is handled by the lane's own `--at`/`--as-of` machinery).
///
/// On a conflicting flag combination, prints the machine-readable
/// `unsupported_combination` envelope on stdout and exits 1 — never returns.
pub(crate) fn resolve_current_state_corpus(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    has_temporal_pin: bool,
    at_head: bool,
    all_history: bool,
) -> Result<(
    query::CorpusMode,
    query::CorpusModeSource,
    Option<Vec<GraphRecord>>,
)> {
    let has_snapshot = query::store_has_source_snapshot(records);
    let (corpus_mode, corpus_mode_source) =
        match query::resolve_corpus_mode(has_temporal_pin, at_head, all_history, has_snapshot) {
            Ok(pair) => pair,
            Err(message) => {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": { "code": "unsupported_combination", "message": message },
                });
                println!("{}", serde_json::to_string(&envelope)?);
                std::process::exit(1);
            }
        };
    let filtered: Option<Vec<GraphRecord>> =
        if matches!(corpus_mode, query::CorpusMode::HeadAnchored) {
            let non_current = query::non_head_current_record_ids(records, index);
            Some(
                records
                    .iter()
                    .filter(|r| !non_current.contains(r.id()))
                    .cloned()
                    .collect(),
            )
        } else {
            None
        };
    Ok((corpus_mode, corpus_mode_source, filtered))
}

/// Resolves the corpus-disclosure triple for a lane that HEAD-anchors its
/// current-state view by default (issue #427): `commit_pinned` + `selector`
/// when a temporal selector (`--at`/`--as-of`) is active, otherwise the
/// [`query::disclose_corpus`] head-anchored default (`head_anchored` over a
/// scan-history store carrying a `source_snapshot`, `single_snapshot` over a
/// plain snapshot-less scan).
pub(crate) fn disclose_head_anchored_corpus(
    records: &[GraphRecord],
    selector_active: bool,
) -> (&'static str, &'static str, String) {
    if selector_active {
        let mode = query::CorpusMode::CommitPinned;
        (
            mode.as_str(),
            query::CorpusModeSource::Selector.as_str(),
            mode.disclaimer().to_owned(),
        )
    } else {
        let (mode, source, disclaimer) =
            query::disclose_corpus(records, query::CorpusMode::HeadAnchored);
        (mode.as_str(), source.as_str(), disclaimer)
    }
}

/// Stamps corpus-disclosure fields (issue #427) onto every `query symbol` row.
///
/// The `query symbol` lane emits bare NDJSON rows with no summary envelope, so
/// the disclosure rides each row. All rows in one query share the same corpus.
pub(crate) fn stamp_symbol_corpus(
    results: &mut [SymbolResult<'_>],
    mode: query::CorpusMode,
    source: query::CorpusModeSource,
) {
    for row in results.iter_mut() {
        row.corpus_mode = Some(mode.as_str());
        row.corpus_mode_source = Some(source.as_str());
        row.corpus_disclaimer = Some(mode.disclaimer().to_owned());
    }
}

pub(crate) fn get_file_diagnostics<'a>(
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
pub(crate) struct DriftResult<'a> {
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
pub(crate) struct SemanticResult<'a> {
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
    /// Confidence band for this row (issue #263): `strong` when
    /// `score >= SEMANTIC_CONFIDENT_THRESHOLD`, else `weak`. The row-level
    /// band complements the answer-level `confidence` verdict (issue #221):
    /// the verdict judges the answer by its best row, the band judges each
    /// row on its own.
    confidence_band: &'static str,
    /// The calibrated threshold this row was judged against (issue #263).
    selection_threshold: f32,
    /// How the threshold was set (issue #263); mirrors drift's `selection_basis`.
    selection_basis: &'static str,
}

#[cfg(feature = "embeddings")]
impl<'a> SemanticResult<'a> {
    pub(crate) fn from_match(m: &'a SemanticMatch, index: &'a query::RepositoryIndex) -> Self {
        let repository_id = index.owner_of(&m.record_id);
        Self {
            record_id: &m.record_id,
            name: m.name.as_deref(),
            repo_relative_path: m.repo_relative_path.as_deref(),
            score: m.score,
            span: m.span,
            repository_id,
            repository: repository_id.and_then(|id| index.display_of(id)),
            confidence_band: crate::semantic_confidence::ConfidenceBand::of_score(m.score).as_str(),
            selection_threshold: crate::semantic_confidence::SEMANTIC_CONFIDENT_THRESHOLD,
            selection_basis: crate::semantic_confidence::SEMANTIC_SELECTION_BASIS,
        }
    }
}

/// The query outcome containing info on who last changed a given symbol.
#[derive(Serialize)]
pub(crate) struct WhoResult<'a> {
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
    /// Corpus this answer was read from (issue #427): `commit_pinned` under
    /// `--at`/`--as-of`, otherwise `head_anchored` over a scan-history store
    /// carrying a `source_snapshot`, `single_snapshot` over a plain scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: `selector` under a temporal pin, else
    /// `default`.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
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
pub(crate) struct ContextSourceFact<'a> {
    record_id: &'a str,
    kind: &'static str,
    /// Derived trust class (issue #114): always `source_derived` for a
    /// code-graph or semantic record. See `crate::query::TrustClass`.
    trust: crate::query::TrustClass,
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
    ContextDecision, ContextLinkedItem, ContextObservation, context_decision, context_linked_item,
    context_observation,
};

/// One unresolved evidence link target, surfaced per AC5.
#[derive(Serialize)]
pub(crate) struct ContextUnresolved<'a> {
    source_record_id: &'a str,
    target_handle: &'a str,
    relation: &'a str,
    target_domain: &'a str,
    verification_status: &'static str,
}

/// One codegraph topology edge in the context response.
#[derive(Serialize)]
pub(crate) struct ContextTopologyEdge<'a> {
    record_id: &'a str,
    /// Derived trust class (issue #114): always `source_derived` — this section
    /// carries code-graph topology edges only.
    trust: crate::query::TrustClass,
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

/// One item in the `drift_history` section: a `SemanticDrift` record whose
/// resolved target is one of the queried symbol's own records (issue #108).
/// Deterministic source-derived evidence — never mixed into `observations`.
/// Carries the same resolved-target citation handle `eg query drift`'s
/// `DriftResult` does, so the citation audit's classification of this row
/// (via `resolve_drift_target`) matches what this command actually renders.
#[derive(Serialize)]
pub(crate) struct ContextDrift<'a> {
    record_id: &'a str,
    /// Derived trust class (issue #114): always `source_derived` — a
    /// `SemanticDrift` record is a deterministic measurement, never an agent
    /// claim.
    trust: crate::query::TrustClass,
    score: f64,
    before_commit: &'a str,
    after_commit: &'a str,
    before_valid_time: &'a str,
    after_valid_time: &'a str,
    embedding_model: &'a EmbeddingModel,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
}

/// One record excluded by a filter or temporal constraint.
#[derive(Serialize)]
pub(crate) struct ExcludedDiagnostic<'a> {
    record_id: &'a str,
    /// Derived trust class of the excluded record (issue #114), so a consumer
    /// reading the diagnostics section sees the same label the row would have
    /// carried in the answer.
    trust: crate::query::TrustClass,
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<Vec<crate::temporal_status::TemporalReference>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contradicted_by: Option<Vec<crate::temporal_status::TemporalReference>>,
}

/// Full context query response envelope.
#[derive(Serialize)]
pub(crate) struct ContextResponse<'a> {
    ok: bool,
    symbol_name: &'a str,
    /// Non-fatal store-freshness code relative to a working tree (issue #82).
    ///
    /// Present only when `--repo-path` was supplied; the context is never
    /// suppressed on a non-`fresh` verdict so an agent can downgrade trust in the
    /// returned handles instead of losing them.
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<&'static str>,
    source_facts: BudgetedSection<ContextSourceFact<'a>>,
    #[serde(skip_serializing_if = "BudgetedSection::is_empty")]
    topology_edges: BudgetedSection<ContextTopologyEdge<'a>>,
    observations: BudgetedSection<ContextObservation<'a>>,
    /// Agent-authored decisions with rationale (issue #191). Always
    /// present; empty when no decisions link to the symbol.
    decisions: BudgetedSection<ContextDecision<'a>>,
    project_state: BudgetedSection<ContextLinkedItem<'a>>,
    artifacts: BudgetedSection<ContextLinkedItem<'a>>,
    verification_evidence: BudgetedSection<ContextLinkedItem<'a>>,
    /// `SemanticDrift` records targeting this symbol, score descending
    /// (issue #108). Always present — an empty array, not an omitted field,
    /// signals "no drift recorded" (AC4).
    drift_history: BudgetedSection<ContextDrift<'a>>,
    unresolved: BudgetedSection<ContextUnresolved<'a>>,
    /// Active approved policy folded into the answer (issue #169). Always
    /// present; an empty array signals "no in-scope policy applies" (AC5).
    /// Budgeted like every other section (issue #211): the fold keeps its
    /// top-ranked (record-ID ordered) prefix and the remainder flows on.
    policy: BudgetedSection<ContextPolicyRow<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<ExcludedDiagnostic<'a>>,
    /// Corpus the current-state view read (issue #427):
    /// `union` over a scan-history store, `single_snapshot` over a plain scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
    /// Store-level trust-domain presence (issue #196): which of the five
    /// domains the store contains at least one record in. Lets a reader tell
    /// an empty section that means "domain absent from this store" apart from
    /// one that means "domain present, no records for this entity".
    store_coverage: query::StoreCoverage,
}

/// Full task context query response envelope.
#[derive(Serialize)]
pub(crate) struct TaskContextResponse<'a> {
    ok: bool,
    task_id: &'a str,
    tasks: Vec<ContextLinkedItem<'a>>,
    acceptance_criteria: Vec<ContextLinkedItem<'a>>,
    source_facts: Vec<ContextSourceFact<'a>>,
    observations: Vec<ContextObservation<'a>>,
    /// Agent-authored decisions with rationale (issue #191).
    decisions: Vec<ContextDecision<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reviews: Vec<ContextLinkedItem<'a>>,
    external_links: Vec<ContextLinkedItem<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<ExcludedDiagnostic<'a>>,
    /// Store-level trust-domain presence (issue #196): which of the five
    /// domains the store contains at least one record in. Lets a reader tell
    /// an empty section that means "domain absent from this store" apart from
    /// one that means "domain present, no records for this entity".
    store_coverage: query::StoreCoverage,
}

/// One semantic drift item in the `semantic_drift` section of a subsystem response.
#[derive(Serialize)]
pub(crate) struct SubsystemDrift<'a> {
    record_id: &'a str,
    /// Derived trust class (issue #114): always `source_derived` — a
    /// `SemanticDrift` record is a deterministic measurement, never an agent
    /// claim.
    trust: crate::query::TrustClass,
    score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_span: Option<crate::ir::SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after_git_commit: Option<&'a str>,
}

/// One in-prefix resolved backtrace frame on a `log_signatures` row (issue #325).
#[derive(Serialize)]
pub(crate) struct SubsystemLogFrame<'a> {
    frame_index: u32,
    frame_resolution: &'a str,
    target_repo_relative_path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_span: Option<crate::ir::SourceSpan>,
}

/// One runtime `ErrorSignature` in the `log_signatures` section of a subsystem
/// response (issue #325).
///
/// Rows are runtime observations — leads, not proof the subsystem is unhealthy;
/// `occurrence_count` reflects scanned log sources only.
#[derive(Serialize)]
pub(crate) struct SubsystemLogSignature<'a> {
    record_id: &'a str,
    kind: &'static str,
    /// Derived trust class (issue #114): always `runtime_observation`. Carried
    /// alongside the pre-existing `trust_class` field so every record in the
    /// envelope answers to the same `trust` key.
    trust: crate::query::TrustClass,
    trust_class: &'static str,
    schema_version: u32,
    severity: &'a str,
    occurrence_count: u64,
    template_excerpt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_seen_valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seen_valid_time: Option<&'a str>,
    resolved_frames: Vec<SubsystemLogFrame<'a>>,
}

/// Documents an active author selector on a subsystem answer (issue #195).
///
/// Present only when `--agent` / `--not-agent` was given. Records which
/// selector spelled the filter, which answer field carries the authoring agent
/// identity, and how many observations the filter matched — so an empty
/// `observations` array under an active scope is an explicit empty result,
/// not a silent fallback to unscoped recall.
#[derive(Serialize)]
pub(crate) struct AuthorScopeReport<'a> {
    /// The `--agent` value: only observations authored by this `agent_id`
    /// were returned. Omitted when the flag was not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a str>,
    /// The `--not-agent` value: observations authored by this `agent_id`
    /// were excluded. Omitted when the flag was not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    not_agent: Option<&'a str>,
    /// The observation field carrying the authoring agent identity.
    author_field: &'static str,
    /// Observations in the answer after the author filter and supersession
    /// resolution.
    observations_matched: usize,
    /// Observations under the prefix before the author filter.
    observations_total: usize,
}

/// Full subsystem context query response envelope (issue #83).
#[derive(Serialize)]
pub(crate) struct SubsystemResponse<'a> {
    ok: bool,
    prefix: &'a str,
    source_facts: Vec<ContextSourceFact<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topology_edges: Vec<ContextTopologyEdge<'a>>,
    observations: Vec<ContextObservation<'a>>,
    /// Agent-authored decisions with rationale (issue #191).
    decisions: Vec<ContextDecision<'a>>,
    /// Author selector applied to `observations` (issue #195). `None`
    /// (omitted) when recall was unscoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    author_scope: Option<AuthorScopeReport<'a>>,
    project_state: Vec<ContextLinkedItem<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    semantic_drift: Vec<SubsystemDrift<'a>>,
    // Always present (empty array when no scanned source resolves here) so the
    // section is additive per AC5 — never `skip_serializing_if` (issue #325).
    log_signatures: Vec<SubsystemLogSignature<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<ExcludedDiagnostic<'a>>,
    /// Corpus the current-state view read (issue #427):
    /// `union` over a scan-history store, `single_snapshot` over a plain scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
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
pub(crate) struct SemanticContextMatch<'a> {
    record_id: &'a str,
    /// Derived trust class of the matched record (issue #114). A semantic match
    /// anchors on a code-graph `Symbol`/`File`, so this is `source_derived`; it
    /// is emitted rather than assumed so every record the envelope returns —
    /// including the match anchor, not just the rows of its context sections —
    /// carries a label.
    trust: crate::query::TrustClass,
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
    /// Agent-authored decisions with rationale (issue #191). Always
    /// present; empty when no decisions link to the match.
    decisions: Vec<ContextDecision<'a>>,
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
pub(crate) struct SemanticContextResponse<'a> {
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
pub(crate) struct AuditClaim<'a> {
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
pub(crate) struct AuditProvenance<'a> {
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
pub(crate) struct AuditItem<'a> {
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
pub(crate) struct AuditDiagnostic<'a> {
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
pub(crate) struct AuditExcluded<'a> {
    record_id: &'a str,
    kind: &'static str,
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_handle: Option<&'a str>,
}

/// Deterministic pagination block (AC8). v1 returns full pages only.
#[derive(Serialize)]
pub(crate) struct AuditPage {
    cursor: Option<()>,
    has_more: bool,
    returned: usize,
}

/// Full memory evidence audit response envelope.
#[derive(Serialize)]
pub(crate) struct MemoryAuditResponse<'a> {
    ok: bool,
    memory_id: &'a str,
    verified_only: bool,
    memory_claim: BudgetedSection<AuditClaim<'a>>,
    direct_provenance: AuditProvenance<'a>,
    supporting_evidence: BudgetedSection<AuditItem<'a>>,
    contradicting_evidence: BudgetedSection<AuditItem<'a>>,
    superseding_records: BudgetedSection<AuditItem<'a>>,
    related_code_handles: BudgetedSection<AuditItem<'a>>,
    related_project_handles: BudgetedSection<AuditItem<'a>>,
    verification_evidence: BudgetedSection<AuditItem<'a>>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
    excluded: Vec<AuditExcluded<'a>>,
    page: AuditPage,
}

/// One prior failed attempt: a redaction-safe [`AuditItem`] plus the
/// failure-specific read-time fields (issue #63).
#[derive(Serialize)]
pub(crate) struct FailureAttemptJson<'a> {
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
pub(crate) struct FailureHistoryResponse<'a> {
    ok: bool,
    target_handle: &'a str,
    target_type: &'a str,
    target_ids: Vec<&'a str>,
    runtime_failures: BudgetedSection<FailureAttemptJson<'a>>,
    agent_failures: BudgetedSection<FailureAttemptJson<'a>>,
    superseding_successes: BudgetedSection<AuditItem<'a>>,
    patch_artifacts: BudgetedSection<AuditItem<'a>>,
    /// `AgentSession` record IDs reached via `AUTHORED_BY` from a failure — the
    /// citable provenance when a `Failure` carries no `agent_id`/`session_id`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    agent_sessions: Vec<&'a str>,
    /// `Agent` record IDs reached via `SESSION_OF`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    agents: Vec<&'a str>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
    page: AuditPage,
    /// Corpus the current-state view read (issue #427):
    /// `union` over a scan-history store, `single_snapshot` over a plain scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
}

// ---------------------------------------------------------------------------
/// One side of a recorded disagreement, projected to a bounded payload-safe
/// view (issue #232). A party is an agent-authored claim, a verification
/// execution, or a user-context record — never a deterministic code fact.
#[derive(Serialize)]
pub(crate) struct ConflictPartyJson<'a> {
    record_id: &'a str,
    kind: &'static str,
    /// Lane provenance class: `agent_authored` / `verification` / `user_context`.
    party_class: &'static str,
    /// Derived trust class label (issue #114 vocabulary).
    trust_class: &'static str,
    /// Which end of the `CONTRADICTS` edge the party sits on: `source` / `target`.
    edge_role: &'static str,
    /// The record's own ID first, then its evidence-link target IDs (sorted,
    /// deduplicated). Non-empty by construction.
    citation_handles: Vec<&'a str>,
    /// Non-empty author label (`agent_id[:session_id]`, the deciding user for
    /// user-context records, else a stable `unknown_agent` marker).
    author: String,
    /// Verification `executed_at`, else agent `observed_at`, else a stable
    /// `unknown_time` marker — never an invented timestamp.
    observed_at: &'a str,
    /// Recorded confidence, else a stable `unspecified` marker.
    confidence: &'a str,
    /// The shared scope target handle both parties were filtered by.
    target_handle: &'a str,
    /// Bounded payload-safe summary (agent-authored text is hashed, AC9).
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_hash: Option<String>,
}

/// One recorded disagreement: the unordered party pair joined by recorded
/// `CONTRADICTS` edge(s) (issue #232).
#[derive(Serialize)]
pub(crate) struct ConflictJson<'a> {
    /// Unordered pair key `[min_record_id, max_record_id]` — the stable
    /// conflict identity.
    conflict_id: [&'a str; 2],
    /// Recorded `CONTRADICTS` edge IDs joining the pair, sorted.
    edge_ids: Vec<&'a str>,
    /// The two parties, in record-ID order.
    parties: Vec<ConflictPartyJson<'a>>,
    /// True when any party was superseded by a later live record.
    resolved: bool,
    /// Superseding record IDs (sorted); empty when unresolved.
    resolved_by: Vec<&'a str>,
}

/// One stable machine-readable conflicts diagnostic.
#[derive(Serialize)]
pub(crate) struct ConflictDiagnosticJson<'a> {
    code: &'a str,
    detail: &'a str,
}

/// Deterministic tallies for a conflicts answer.
#[derive(Serialize)]
pub(crate) struct ConflictsCountsJson {
    contradicts_edges_considered: usize,
    conflicts_found: usize,
    conflicts_returned: usize,
    conflicts_resolved_excluded: usize,
    edges_skipped_ineligible_party: usize,
}

impl From<&query::ConflictsCounts> for ConflictsCountsJson {
    fn from(counts: &query::ConflictsCounts) -> Self {
        Self {
            contradicts_edges_considered: counts.contradicts_edges_considered,
            conflicts_found: counts.conflicts_found,
            conflicts_returned: counts.conflicts_returned,
            conflicts_resolved_excluded: counts.conflicts_resolved_excluded,
            edges_skipped_ineligible_party: counts.edges_skipped_ineligible_party,
        }
    }
}

/// Full conflicts response envelope (issue #232).
#[derive(Serialize)]
pub(crate) struct ConflictsResponse<'a> {
    ok: bool,
    scope: &'a str,
    /// `symbol` / `file` / `subsystem`.
    scope_kind: &'static str,
    target_handle: &'a str,
    /// Every code record ID the scope resolved to (sorted).
    target_ids: Vec<&'a str>,
    include_resolved: bool,
    conflicts: Vec<ConflictJson<'a>>,
    /// Number of conflicts returned (mirrors `conflicts.len()`).
    count: usize,
    counts: ConflictsCountsJson,
    diagnostics: Vec<ConflictDiagnosticJson<'a>>,
}

impl PrintText for ConflictsResponse<'_> {
    fn as_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "conflicts for scope '{}' ({}): {} conflict(s)",
            self.scope, self.scope_kind, self.count
        );
        for conflict in &self.conflicts {
            let _ = writeln!(
                out,
                "conflict [{}, {}] resolved: {}",
                conflict.conflict_id[0], conflict.conflict_id[1], conflict.resolved
            );
            if !conflict.resolved_by.is_empty() {
                let _ = writeln!(out, "  resolved_by: {}", conflict.resolved_by.join(", "));
            }
            let _ = writeln!(out, "  edges: {}", conflict.edge_ids.join(", "));
            for party in &conflict.parties {
                let _ = writeln!(
                    out,
                    "  - {} [{}, {}] {}",
                    party.edge_role, party.party_class, party.trust_class, party.record_id
                );
                let _ = writeln!(
                    out,
                    "    kind: {} author: {} observed_at: {} confidence: {}",
                    party.kind, party.author, party.observed_at, party.confidence
                );
                let _ = writeln!(
                    out,
                    "    target: {} citations: {}",
                    party.target_handle,
                    party.citation_handles.join(", ")
                );
                let _ = writeln!(out, "    summary: {}", party.summary);
            }
        }
        for diagnostic in &self.diagnostics {
            let _ = writeln!(out, "note: {}: {}", diagnostic.code, diagnostic.detail);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// query_cmd — dispatch
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
pub(crate) fn query_cmd(subcommand: QuerySubcommand) -> Result<()> {
    // Fail fast on a malformed `egregore.toml` even for lanes that never read
    // the store (issue #261): a bad config is never silently ignored. The
    // lanes manifest (issue #251) is the exception: it is local and pure —
    // clap metadata only — so it skips config parsing entirely and works on
    // a fresh clone with an empty data dir.
    if !matches!(subcommand, QuerySubcommand::Lanes { .. }) {
        let _ = cli_project_config();
    }
    match subcommand {
        QuerySubcommand::Churn {
            graph,
            data_dir,
            repo,
            limit,
            format,
        } => {
            // Validate the limit before touching the store so a malformed
            // bound fails fast with a machine-readable diagnostic.
            if limit == 0 || limit > query::CHURN_MAX_LIMIT {
                let diag = serde_json::json!({
                    "code": "invalid_limit",
                    "limit": limit,
                    "min": 1,
                    "max": query::CHURN_MAX_LIMIT,
                    "message": format!(
                        "--limit must be between 1 and {} (default {})",
                        query::CHURN_MAX_LIMIT,
                        query::CHURN_DEFAULT_LIMIT
                    ),
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_churn_cmd(&records, selected.as_deref(), limit, format)
        }
        QuerySubcommand::Complexity {
            graph,
            data_dir,
            repo,
            limit,
            format,
        } => {
            // Validate the limit before touching the store so a malformed
            // bound fails fast with a machine-readable diagnostic.
            if limit == 0 || limit > query::COMPLEXITY_MAX_LIMIT {
                let diag = serde_json::json!({
                    "code": "invalid_limit",
                    "limit": limit,
                    "min": 1,
                    "max": query::COMPLEXITY_MAX_LIMIT,
                    "message": format!(
                        "--limit must be between 1 and {} (default {})",
                        query::COMPLEXITY_MAX_LIMIT,
                        query::COMPLEXITY_DEFAULT_LIMIT
                    ),
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_complexity_cmd(&records, selected.as_deref(), limit, format)
        }
        QuerySubcommand::Conflicts {
            scope,
            graph,
            data_dir,
            repo,
            include_resolved,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_conflicts_cmd(
                &records,
                &scope,
                &index,
                selected.as_deref(),
                include_resolved,
                format,
            )
        }
        QuerySubcommand::Resolve {
            record_id,
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            at,
            as_of,
            format,
        } => {
            // Validate the handle before touching any store so a malformed
            // or non-codegraph id fails fast with a machine-readable
            // diagnostic (the lane itself re-validates; this keeps the
            // dispatch honest about the contract).
            query_resolve_cmd(
                &record_id,
                graph.as_deref(),
                data_dir,
                #[cfg(feature = "embedded-aletheiadb")]
                daemon,
                at.as_deref(),
                as_of.as_deref(),
                format,
            )
        }
        QuerySubcommand::Recency {
            graph,
            data_dir,
            repo,
            limit,
            format,
        } => {
            // Validate the limit before touching the store so a malformed
            // bound fails fast with a machine-readable diagnostic.
            if limit == 0 || limit > query::RECENCY_MAX_LIMIT {
                let diag = serde_json::json!({
                    "code": "invalid_limit",
                    "limit": limit,
                    "min": 1,
                    "max": query::RECENCY_MAX_LIMIT,
                    "message": format!(
                        "--limit must be between 1 and {} (default {})",
                        query::RECENCY_MAX_LIMIT,
                        query::RECENCY_DEFAULT_LIMIT
                    ),
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_recency_cmd(&records, selected.as_deref(), limit, format)
        }
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
            package,
            repo_path,
            at_head,
            all_history,
            role,
            format,
        } => {
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                reject_package_with_daemon(package.as_deref());
            }
            if let Some(tx) = tx_as_of.as_deref() {
                // The transaction-time lane returns `TxSymbolRow`, which carries
                // no crate attribution, so a scoped query here could only emit
                // UNSCOPED rows — including records owned by other packages, and
                // with no typo gate on the selector. Refuse the combination for
                // the same reason `--daemon` is refused (issue #117): silently
                // answering a different question is the one outcome ruled out.
                if package.is_some() {
                    print_tx_error(
                        "unsupported_combination",
                        "--package cannot be used with --tx-as-of; the transaction-time \
                         row shape carries no crate attribution",
                    )?;
                    std::process::exit(1);
                }
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
                        role,
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
                    role,
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
                    role,
                );
            }
            // Sidecar-index fast path (issue #447) for the plain current-state,
            // unscoped `query symbol <name>`; `--repo`/`--at`/`--as-of` and
            // freshness stamping (`--repo-path`) need global topology or the
            // commit timeline and stay cold.
            let selector = if repo.is_none()
                && package.is_none()
                && at.is_none()
                && as_of.is_none()
                && repo_path.is_none()
            {
                symbol_handle_selector(&name)
            } else {
                crate::graph_index::Selector::Whole
            };
            let records = load_records_selected(graph.as_deref(), data_dir.as_deref(), &selector)?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            let selected = selected.as_deref();
            // Package scope (issue #117) is resolved against the WHOLE corpus,
            // which is why `--package` forces `Selector::Whole` above — the same
            // conservatism `--repo` already applies.
            //
            // Both verdicts need global knowledge, and the #447 sidecar index
            // supplies only one symbol's closure. Enumerating `known_packages`
            // for a typo obviously does. So does cross-repository ambiguity, and
            // that one is the trap: a package owned by two repositories where
            // only ONE defines the queried symbol looks unambiguous inside the
            // narrowed closure, so the lane would return rows where a cold scan
            // refuses. An answer that changes depending on whether an index file
            // exists breaks the index's documented contract of being a pure
            // access-path optimization.
            let freshness_code = query_freshness_code_with_hint(
                &records,
                repo_path.as_deref(),
                &[graph.as_deref(), data_dir.as_deref()],
                selected,
            );
            // Corpus-mode selection (issue #456): the default `query symbol`
            // (no `--at`/`--as-of`) now HEAD-anchors over a scan-history store;
            // `--all-history` opts into the union. `--at`/`--as-of` stay
            // CommitPinned (handled by query_symbol_at/as_of, which stamp their
            // own row-level disclosure). This call also enforces the mutual
            // exclusion of --at-head/--all-history with each other and with any
            // temporal selector.
            let (corpus_mode, corpus_mode_source, filtered) = resolve_current_state_corpus(
                &records,
                &index,
                at.is_some() || as_of.is_some(),
                at_head,
                all_history,
            )?;
            // Package validation runs AFTER corpus resolution, over the corpus
            // the answer is computed from. Both of its verdicts are questions
            // ABOUT A CORPUS: validating against the whole record set while the
            // answer comes from a narrower one refuses an answerable HEAD query
            // whose only collision is historical, and calls a package known at
            // an instant before it existed.
            // A malformed `--as-of` is diagnosed by the lane itself. Package
            // validation must not pre-empt it: the corpus could not be narrowed
            // to the requested instant, so any verdict computed over it answers
            // a question the caller did not ask — and telling someone to
            // disambiguate a package name while silently ignoring an
            // uninterpretable timestamp points at the wrong input.
            let as_of_is_malformed = as_of
                .as_deref()
                .is_some_and(|raw| chrono::DateTime::parse_from_rfc3339(raw).is_err());
            if let Some(selector_name) = package.as_deref().filter(|_| !as_of_is_malformed) {
                let corpus = package_catalog_corpus(
                    &records,
                    &index,
                    at.as_deref(),
                    as_of.as_deref(),
                    filtered.as_deref(),
                );
                // Liveness is judged over the WHOLE record set for both
                // catalogs: a narrowed corpus can hold a record without the
                // tombstone that retracts it.
                let deleted = current_deleted_ids(&records);
                let catalog = PackageCatalog::build_with_liveness(&records, &index, &deleted);
                let answerable = PackageCatalog::build_with_liveness(&corpus, &index, &deleted);
                resolve_package_scope(&catalog, &answerable, selector_name, repo.is_some());
            }
            as_of.map_or_else(
                || {
                    at.map_or_else(
                        || {
                            let recs: &[GraphRecord] = filtered.as_deref().unwrap_or(&records);
                            query_symbol_all(
                                recs,
                                &name,
                                format,
                                &index,
                                selected,
                                package.as_deref(),
                                freshness_code.as_ref(),
                                corpus_mode,
                                corpus_mode_source,
                                role,
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
                                package.as_deref(),
                                freshness_code.as_ref(),
                                role,
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
                        package.as_deref(),
                        freshness_code.as_ref(),
                        role,
                    )
                },
            )
        }
        QuerySubcommand::Symbols {
            pattern,
            graph,
            data_dir,
            repo,
            package,
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
            // This lane always loads the whole corpus and offers no corpus
            // selector, so the whole record set IS its corpus and the catalog is
            // authoritative with no cold-reload fallback (issue #117). A package
            // two repositories held at different times is genuinely ambiguous
            // for a union answer, so the narrowing `query symbol` applies would
            // be wrong here.
            if let Some(selector_name) = package.as_deref() {
                let catalog = PackageCatalog::build(&records, &index);
                // No corpus selector on this lane, so the whole record set IS
                // the answerable corpus: one catalog serves both verdicts.
                resolve_package_scope(&catalog, &catalog, selector_name, repo.is_some());
            }
            query_symbols_matching(
                &records,
                &pattern,
                case_insensitive,
                format,
                &index,
                selected.as_deref(),
                package.as_deref(),
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

                    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
                        disclose_head_anchored_corpus(&records, at.is_some() || as_of.is_some());
                    let result = WhoResult {
                        symbol_name: &name,
                        commit_sha,
                        author_name,
                        author_email,
                        valid_time,
                        repo_relative_path,
                        freshness,
                        corpus_mode,
                        corpus_mode_source,
                        corpus_disclaimer,
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
            at,
            as_of,
            tx_as_of,
            repo,
            repo_path,
            role,
            format,
        } => {
            // Transaction-time file views are reserved: reject with the
            // documented machine-readable envelope, never silently ignore.
            if tx_as_of.is_some() {
                print_tx_error(
                    "not_implemented",
                    "--tx-as-of is not implemented for query file; the transaction-time \
                     axis currently covers query symbol only (see \
                     docs/schema/temporal-selectors.md)",
                )?;
                std::process::exit(1);
            }
            if at.is_some() || as_of.is_some() {
                #[cfg(feature = "embedded-aletheiadb")]
                if daemon {
                    eprintln!(
                        "error: --daemon is not supported with --at/--as-of for query file; \
                         run without --daemon against the same store"
                    );
                    std::process::exit(1);
                }
                if repo_path.is_some() {
                    // Freshness stamps a current-tree answer; a point-in-time
                    // snapshot has no current-tree freshness to report.
                    eprintln!(
                        "error: --repo-path cannot be used with --at/--as-of; \
                         freshness stamping applies to current-state answers only"
                    );
                    std::process::exit(1);
                }
                // Strictly read-only lane (issue #158): opening the embedded
                // engine in place re-persists its on-disk index files, so
                // `--data-dir` reads from a throwaway copy, never the live
                // store. The copy uses the *current-state* read — the same
                // view `query deltas` and `query symbol --at` resolve
                // against: it still includes every commit snapshot, but
                // collapses a re-ingested snapshot of the same
                // `(record_id, commit)` pair to its current version.
                // Superseded prior versions are a transaction-time concern
                // (issue #66), not part of a plain valid-time point query.
                // Config fallback (issue #261): explicit `--data-dir` wins; the
                // config-pinned dir applies only when neither `--graph` nor
                // `--data-dir` was passed, so `--graph` plus a pinned store never
                // reads as "both provided".
                let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
                let records = match (graph.as_deref(), data_dir.as_deref()) {
                    (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                    (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                    (Some(_), Some(_)) => {
                        anyhow::bail!("provide only one of --graph or --data-dir, not both")
                    }
                    (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
                };
                let index = query::RepositoryIndex::build(&records);
                let selected = resolve_repo_scope(&index, repo.as_deref());
                return query_file_at_point(
                    &records,
                    &path,
                    at.as_deref(),
                    as_of.as_deref(),
                    selected.as_deref(),
                    format,
                    role,
                );
            }
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
                return query_file_via_daemon(&path, dir, repo.as_deref(), format, role);
            }
            // Sidecar-index fast path (issue #447): a file's defined-symbol set
            // is a `ByPath` closure. `--repo` (scope) and `--repo-path`
            // (freshness stamping) need global topology and stay cold.
            let selector = if repo.is_none() && repo_path.is_none() {
                crate::graph_index::Selector::ByPath(path.clone())
            } else {
                crate::graph_index::Selector::Whole
            };
            let records = load_records_selected(graph.as_deref(), data_dir.as_deref(), &selector)?;
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
                role,
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
        QuerySubcommand::Clones {
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            repo,
            min_size,
            limit,
            format,
        } => {
            // Validate the bounds before touching the store so a malformed
            // bound fails fast with a machine-readable diagnostic (same
            // contract as `eg query churn`).
            if limit == 0 || limit > query::CLONES_MAX_LIMIT {
                let diag = serde_json::json!({
                    "code": "invalid_limit",
                    "limit": limit,
                    "min": 1,
                    "max": query::CLONES_MAX_LIMIT,
                    "message": format!(
                        "--limit must be between 1 and {} (default {})",
                        query::CLONES_MAX_LIMIT,
                        query::CLONES_DEFAULT_LIMIT
                    ),
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            if min_size < query::CLONES_DEFAULT_MIN_SIZE {
                let diag = serde_json::json!({
                    "code": "invalid_min_size",
                    "min_size": min_size,
                    "min": query::CLONES_DEFAULT_MIN_SIZE,
                    "message": "--min-size must be at least 2: a clone class needs two members",
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_clones_via_daemon(dir, min_size, limit, repo.as_deref(), format);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_clones_cmd(&records, selected.as_deref(), min_size, limit, format)
        }
        #[cfg(feature = "embeddings")]
        QuerySubcommand::Semantic {
            query,
            data_dir,
            daemon,
            repo,
            under,
            limit,
            format,
            embed_model,
        } => {
            if daemon {
                // `--under` conflicts with `--daemon` at the CLI layer (scoped
                // retrieval is a local-CLI surface for this slice, issue #198),
                // so the daemon path never receives a scope prefix.
                query_semantic_via_daemon(&query, &data_dir, limit, repo.as_deref(), format)
            } else {
                query_semantic(
                    &query,
                    &data_dir,
                    limit,
                    repo.as_deref(),
                    under.as_deref(),
                    format,
                    embed_model.as_deref(),
                )
            }
        }
        #[cfg(feature = "embeddings")]
        QuerySubcommand::Similar {
            handle,
            data_dir,
            limit,
            format,
        } => similar::query_similar_cmd(&handle, &data_dir, limit, format),
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
            agent,
            not_agent,
            format,
            supersession,
            collapse,
            similarity_threshold,
            collapse_mode,
            include_retired,
        } => query_semantic_memory(
            &query,
            &data_dir,
            limit,
            repo.as_deref(),
            verified_only,
            agent.as_deref(),
            not_agent.as_deref(),
            format,
            supersession,
            collapse,
            similarity_threshold,
            collapse_mode,
            include_retired,
        ),
        QuerySubcommand::Context {
            name,
            graph,
            data_dir,
            repo_path,
            at_head,
            all_history,
            supersession,
            max_records,
            candidate,
        } => {
            // Sidecar-index fast path (issue #447): the plain context bundle is a
            // `ByName` closure. With `--repo-path` the freshness hint needs the
            // full topology to attribute every source fact, so it stays cold.
            let selector = if repo_path.is_none() {
                symbol_handle_selector(&name)
            } else {
                crate::graph_index::Selector::Whole
            };
            let records = load_records_selected(graph.as_deref(), data_dir.as_deref(), &selector)?;
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
            // Issue #196: `store_coverage` must describe the whole store, not
            // the #447 sidecar-selected closure. When a valid sidecar index
            // served the closure, answer from its validated `by_kind` keys
            // (no record parsing); otherwise `records` is already the whole
            // store (cold scan, `Whole` selector, or `--data-dir`).
            let store_coverage = graph.as_deref().map_or_else(
                || query::StoreCoverage::from_records(&records),
                |path| match crate::graph_index::GraphIndex::load_for(path) {
                    Ok(index) if !index.body.has_temporal_history => {
                        query::StoreCoverage::from_index_kind_keys(
                            index.body.by_kind.keys().map(String::as_str),
                        )
                    }
                    _ => query::StoreCoverage::from_records(&records),
                },
            );
            query_context_cmd(
                &records,
                &name,
                freshness,
                supersession,
                at_head,
                all_history,
                max_records,
                store_coverage,
                candidate.as_deref(),
            )
        }
        QuerySubcommand::Bench {
            target,
            graph,
            data_dir,
            at,
            as_of,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_bench_cmd(&records, &target, at.as_deref(), as_of.as_deref(), format)
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
        QuerySubcommand::TaskReady {
            graph,
            data_dir,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_task_ready_cmd(&records, format)
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
            max_records,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_memory_cmd(&records, &id_or_handle, verified_only, max_records)
        }
        QuerySubcommand::Retirement {
            handle,
            graph,
            data_dir,
            as_of,
            format,
        } => query_retirement_cmd(
            &handle,
            graph.as_deref(),
            data_dir.as_deref(),
            as_of.as_deref(),
            format,
        ),
        QuerySubcommand::BeliefTimeline {
            target,
            graph,
            data_dir,
            repo,
            format,
        } => {
            // Strictly read-only lane (issue #235): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_belief_timeline_cmd(&records, &target, &index, selected.as_deref(), format)
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
            at_head,
            all_history,
            max_records,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_failures_cmd(
                &records,
                &handle,
                &index,
                selected.as_deref(),
                at_head,
                all_history,
                max_records,
            )
        }
        QuerySubcommand::FailureHotspots {
            graph,
            data_dir,
            limit,
            as_of,
            since,
            format,
        } => {
            // Validate the limit before touching the store so a malformed
            // bound fails fast with a machine-readable diagnostic.
            if limit == 0 || limit > query::FAILURE_HOTSPOTS_MAX_LIMIT {
                let diag = serde_json::json!({
                    "code": "invalid_limit",
                    "limit": limit,
                    "min": 1,
                    "max": query::FAILURE_HOTSPOTS_MAX_LIMIT,
                    "message": format!(
                        "--limit must be between 1 and {} (default {})",
                        query::FAILURE_HOTSPOTS_MAX_LIMIT,
                        query::FAILURE_HOTSPOTS_DEFAULT_LIMIT
                    ),
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_failure_hotspots_cmd(&records, limit, as_of.as_deref(), since.as_deref(), format)
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
            at_head,
            all_history,
            format,
            supersession,
            agent,
            not_agent,
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
            query_subsystem_cmd(
                &records,
                &prefix,
                at_head,
                all_history,
                format,
                supersession,
                agent.as_deref(),
                not_agent.as_deref(),
            )
        }
        QuerySubcommand::ChangeImpact {
            handle,
            graph,
            data_dir,
            repo,
            depth,
            at_head,
            all_history,
            format: _format,
            max_records,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_change_impact_cmd(
                &records,
                &handle,
                &index,
                selected.as_deref(),
                depth,
                at_head,
                all_history,
                max_records,
            )
        }
        QuerySubcommand::TransitiveCallers {
            handle,
            graph,
            data_dir,
            repo,
            max_depth,
            at,
            as_of,
            at_head,
            all_history,
            format,
        } => {
            // Validate the bound before any store I/O: a zero-hop walk can
            // never return the direct-caller set and is malformed input.
            if max_depth == 0 {
                let diag = serde_json::json!({
                    "code": "invalid_max_depth",
                    "max_depth": 0,
                    "message": "--max-depth must be at least 1",
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            // Strictly read-only lane (issue #424): opening the embedded engine
            // in place re-persists its on-disk index files, so `--data-dir` reads
            // from a throwaway copy, never the live store (same contract as
            // `query path`/`query implementors`). Temporal selectors need the
            // history-inclusive store view; the current-state read suffices
            // otherwise. A JSONL graph is read identically either way.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) if at.is_some() || as_of.is_some() => {
                    load_records_from_db_history_readonly(dir)?
                }
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_transitive_callers_cmd(
                &records,
                &handle,
                &index,
                selected.as_deref(),
                max_depth,
                at.as_deref(),
                as_of.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        QuerySubcommand::Diagram {
            handle,
            graph,
            data_dir,
            repo,
            depth,
            max_nodes,
            at,
            as_of,
            format,
        } => {
            // Validate the bounds before any store I/O.
            if depth == 0 {
                let diag = serde_json::json!({
                    "code": "invalid_depth",
                    "depth": 0,
                    "message": "--depth must be at least 1",
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            if max_nodes == 0 {
                let diag = serde_json::json!({
                    "code": "invalid_max_nodes",
                    "max_nodes": 0,
                    "message": "--max-nodes must be at least 1",
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            // Strictly read-only lane (issue #424): opening the embedded engine
            // in place re-persists its on-disk index files, so `--data-dir` reads
            // from a throwaway copy, never the live store (same contract as
            // `query transitive-callers`). Temporal selectors need the
            // history-inclusive store view; the current-state read suffices
            // otherwise. A JSONL graph is read identically either way.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) if at.is_some() || as_of.is_some() => {
                    load_records_from_db_history_readonly(dir)?
                }
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_diagram_cmd(
                &records,
                &handle,
                &index,
                selected.as_deref(),
                depth,
                max_nodes,
                at.as_deref(),
                as_of.as_deref(),
                format,
            )
        }
        QuerySubcommand::TransitiveCallees {
            handle,
            graph,
            data_dir,
            repo,
            max_depth,
            at,
            as_of,
            at_head,
            all_history,
            format,
        } => {
            // Validate the bound before any store I/O: a zero-hop walk can
            // never return the direct-dependency set and is malformed input.
            if max_depth == 0 {
                let diag = serde_json::json!({
                    "code": "invalid_max_depth",
                    "max_depth": 0,
                    "message": "--max-depth must be at least 1",
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            // Strictly read-only lane (issue #424): opening the embedded engine
            // in place re-persists its on-disk index files, so `--data-dir` reads
            // from a throwaway copy, never the live store (same contract as
            // `query path`/`query implementors`). Temporal selectors need the
            // history-inclusive store view; the current-state read suffices
            // otherwise. A JSONL graph is read identically either way.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) if at.is_some() || as_of.is_some() => {
                    load_records_from_db_history_readonly(dir)?
                }
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_transitive_callees_cmd(
                &records,
                &handle,
                &index,
                selected.as_deref(),
                max_depth,
                at.as_deref(),
                as_of.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        QuerySubcommand::Deps {
            handle,
            graph,
            data_dir,
            repo,
            at,
            as_of,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane (issue #424): opening the embedded engine
            // in place re-persists its on-disk index files, so `--data-dir` reads
            // from a throwaway copy, never the live store (same contract as
            // `query path`/`query implementors`). Temporal selectors need the
            // history-inclusive store view; the current-state read suffices
            // otherwise. A JSONL graph is read identically either way.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) if at.is_some() || as_of.is_some() => {
                    load_records_from_db_history_readonly(dir)?
                }
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                // Sidecar-index fast path (issue #447) for the plain
                // current-state, unscoped query; `--repo`/`--at`/`--as-of` need
                // global topology / commit timelines and stay cold.
                (Some(graph_path), None) if repo.is_none() && at.is_none() && as_of.is_none() => {
                    load_records_from_jsonl_selected(graph_path, &symbol_handle_selector(&handle))?
                }
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_deps_cmd(
                &records,
                &handle,
                &index,
                selected.as_deref(),
                at.as_deref(),
                as_of.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        QuerySubcommand::PublicApi {
            graph,
            data_dir,
            repo,
            format: _format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_public_api_cmd(&records, &index, selected.as_deref())
        }
        QuerySubcommand::Undocumented {
            graph,
            data_dir,
            repo,
            limit,
            include_private,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_undocumented_cmd(
                &records,
                &index,
                selected.as_deref(),
                limit,
                include_private,
                format,
            )
        }
        QuerySubcommand::Unreferenced {
            graph,
            data_dir,
            repo,
            format: _format,
        } => {
            // Strictly read-only lane (issue #113): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_unreferenced_cmd(&records, &index, selected.as_deref())
        }
        QuerySubcommand::Implementors {
            name,
            graph,
            data_dir,
            at,
            as_of,
            repo,
            format,
        } => {
            // Strictly read-only (issue #133 AC): opening the live embedded
            // engine re-persists its on-disk index files, so the --data-dir
            // path reads a throwaway copy of the store instead — the original
            // stays byte-for-byte untouched. --graph is a plain file read.
            // A temporal pin needs the history-inclusive store view: the
            // embedded store keeps older commit versions only there, so a
            // current-view read would wrongly lose pinned answers.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) if at.is_some() || as_of.is_some() => {
                    load_records_from_db_history_readonly(dir)?
                }
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (graph, None) => load_query_records(graph, None)?,
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_implementors_cmd(
                &records,
                &name,
                &index,
                selected.as_deref(),
                at.as_deref(),
                as_of.as_deref(),
                format,
            )
        }
        QuerySubcommand::ProducerDrift {
            graph,
            data_dir,
            repo,
            format,
        } => {
            // Opening the embedded engine in place re-persists its index
            // files; producer-drift documents a read-only guarantee, so an
            // embedded store is read through a throwaway copy. The guard
            // keeps the copy alive for the reads below.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let store_copy = data_dir.as_deref().map(readonly_audit_store).transpose()?;
            let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());
            let records = load_query_records(graph.as_deref(), effective_data_dir)?;
            // The embedded current-state view suppresses actively tombstoned
            // edge records, so a scoped run needs the store's recorded edge
            // sources to keep edge tombstones attributable; a JSONL graph
            // keeps the superseded edge in the slice and needs no supplement.
            let store_record_parents = effective_data_dir
                .map(load_tombstoned_record_parents_from_db)
                .transpose()?
                .unwrap_or_default();
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_producer_drift_cmd(
                &records,
                &index,
                selected.as_deref(),
                &store_record_parents,
                format,
            )
        }
        QuerySubcommand::Cycles {
            scope,
            graph,
            data_dir,
            repo,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane (issue #138): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_cycles_cmd(
                &records,
                scope.as_deref(),
                &index,
                selected.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        QuerySubcommand::DeadCode {
            graph,
            data_dir,
            repo,
            limit,
            format,
        } => {
            // Validate the limit before touching the store so a malformed
            // bound fails fast with a machine-readable diagnostic.
            if limit == 0 || limit > query::DEAD_CODE_MAX_LIMIT {
                let diag = serde_json::json!({
                    "code": "invalid_limit",
                    "limit": limit,
                    "min": 1,
                    "max": query::DEAD_CODE_MAX_LIMIT,
                    "message": format!(
                        "--limit must be between 1 and {} (default {})",
                        query::DEAD_CODE_MAX_LIMIT,
                        query::DEAD_CODE_DEFAULT_LIMIT
                    ),
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            // Strictly read-only lane (issue #240): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_dead_code_cmd(&records, &index, selected.as_deref(), limit, format)
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
        QuerySubcommand::Deltas {
            base,
            head,
            graph,
            data_dir,
            repo,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_deltas_cmd(&records, &base, &head, repo.as_deref())
        }
        QuerySubcommand::LogDeltas {
            base,
            head,
            graph,
            data_dir,
            repo,
        } => {
            // `data_dir.is_some()` marks the embedded read path. The
            // log-retained read (issue #363) surfaces every superseded
            // non-temporal log observation, so cross-scan coalescing IS
            // reconstructed here for differing-content scans; the envelope still
            // discloses the one residual divergence (byte-identical re-ingests
            // are deduped, not multiplied).
            let embedded_source = data_dir.is_some();
            // Strictly read-only lane (PR #356 review): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_log_retained_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            query_log_deltas_cmd(&records, &base, &head, repo.as_deref(), embedded_source)
        }
        QuerySubcommand::ErrorContext {
            handle,
            graph,
            data_dir,
            repo,
            as_of,
            at,
            supersession,
            protected_store,
        } => {
            // `--at` and `--as-of` key the same valid-time axis; combining them
            // is an unsupported workflow (mirrors the other temporal verbs).
            if at.is_some() && as_of.is_some() {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": "unsupported_combination",
                        "message": "--at cannot be combined with --as-of; pass at most one temporal pin",
                    },
                });
                println!("{}", serde_json::to_string(&envelope)?);
                std::process::exit(1);
            }
            // `--as-of` bounds the occurrence view on the valid axis but is NOT
            // routed through the commit resolver (which validates `--at`), so a
            // malformed instant would otherwise be silently no-op'd by the core's
            // `parse_instant` (returning None → no cutoff → every bucket kept).
            // Validate it up front so a bad `--as-of` fails loudly with a
            // machine-readable error, mirroring the sibling temporal verbs.
            if let Some(vt) = as_of.as_deref()
                && chrono::DateTime::parse_from_rfc3339(vt).is_err()
            {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": "invalid_as_of_timestamp",
                        "message": format!("--as-of must be an RFC 3339 instant, got '{vt}'"),
                    },
                });
                println!("{}", serde_json::to_string(&envelope)?);
                std::process::exit(1);
            }
            // Strictly read-only lane: opening the embedded engine in place
            // re-persists its index files, so `--data-dir` reads a throwaway
            // copy. `--at`/`--as-of` need the history-inclusive read (superseded
            // versions + the Commit timeline) for frame re-resolution and the
            // first_seen_range.
            // Whether records came from an embedded (`--data-dir`) store: gates
            // the embedded log-retention caveat (issue #363). The `--graph` path
            // preserves every ingested line, so it is `false`.
            let embedded_source = data_dir.is_some();
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) if at.is_some() || as_of.is_some() => {
                    // `--at`/`--as-of` need the history-inclusive read so a pinned
                    // commit/instant view can pick the version live at that point.
                    // The log-retained variant additionally collapses
                    // enrichment-only `ErrorSignature` rewrites (identical log
                    // payload, evidence links added by `resolve-frames`/
                    // `link-logs`) to a single observation, so the coalescer never
                    // double-counts `occurrence_count` on this lane, while every
                    // non-log superseded/temporal version stays intact for
                    // valid-time reconstruction (issue #363).
                    load_records_from_db_history_log_retained_readonly(dir)?
                }
                (None, Some(dir)) => load_records_from_data_dir_log_retained_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let repo_scope = resolve_repo_scope(&index, repo.as_deref());
            // Resolve `--at` ONLY to a single commit SHA for frame re-resolution
            // (reuses the shared temporal-view resolver). `--as-of` is NOT
            // resolved to a commit here: per issue #324, `--at` re-resolves frames
            // against a commit view while `--as-of` bounds ONLY the occurrence
            // view on the valid axis (applied by the core's bucket filter). Piping
            // `--as-of` through the commit resolver would (a) die with
            // `empty_history` on a commit-less log graph — the natural
            // bucket-bearing `scan-logs` input — and (b) silently re-resolve
            // frames, a behavior the AC assigns only to `--at`.
            let at_commit = if at.is_some() {
                Some(resolve_transitive_commit_view(
                    &records,
                    &index,
                    repo_scope.as_deref(),
                    at.as_deref(),
                    None,
                )?)
            } else {
                None
            };
            query_error_context_cmd(
                &records,
                &handle,
                repo_scope.as_deref(),
                at_commit.as_deref(),
                as_of.as_deref(),
                supersession,
                protected_store.as_deref(),
                embedded_source,
            )
        }
        QuerySubcommand::EvidencePath {
            source,
            target,
            graph,
            data_dir,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane (AC7): opening the embedded engine in place
            // re-persists its on-disk index files, so `--data-dir` reads from a
            // throwaway copy, never the live store (same contract as the other
            // read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            query_evidence_path_cmd(&records, &source, &target, at_head, all_history, format)
        }
        QuerySubcommand::Coupling {
            path,
            graph,
            data_dir,
            repo,
            base,
            head,
            at,
            as_of,
            min_support,
            limit,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            let options = query::CoChangeCouplingOptions {
                base: base.as_deref(),
                head: head.as_deref(),
                at: at.as_deref(),
                as_of: as_of.as_deref(),
                min_support,
                limit,
            };
            query_coupling_cmd(&records, &path, selected.as_deref(), &options, format)
        }
        QuerySubcommand::ManifestDeps {
            graph,
            data_dir,
            name,
            repo,
            format,
        } => {
            // Strictly read-only lane (PR #314 review): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_manifest_deps_cmd(
                &records,
                &index,
                selected.as_deref(),
                name.as_deref(),
                format,
            )
        }
        QuerySubcommand::PublicApiDeltas {
            base,
            head,
            graph,
            data_dir,
            repo,
            include_internal,
            callers,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let options = query::PublicApiDeltasOptions {
                include_internal,
                with_callers: callers,
            };
            query_public_api_deltas_cmd(&records, &base, &head, repo.as_deref(), options, format)
        }
        QuerySubcommand::UnwrapExpect {
            path,
            graph,
            data_dir,
            at,
            repo,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane (issue #223): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_unwrap_expect_cmd(
                &records,
                path.as_deref(),
                at.as_deref(),
                &index,
                selected.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        QuerySubcommand::DebtMarkers {
            path,
            graph,
            data_dir,
            at,
            repo,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane (issue #218): same throwaway-copy
            // `--data-dir` contract as the other read-only lanes.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_debt_markers_cmd(
                &records,
                path.as_deref(),
                at.as_deref(),
                &index,
                selected.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        QuerySubcommand::Diagnostics {
            graph,
            data_dir,
            repo,
            file,
            at_head,
            all_history,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_diagnostics_cmd(
                &records,
                &index,
                selected.as_deref(),
                file.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        QuerySubcommand::RiskMarkers {
            path,
            graph,
            data_dir,
            at,
            repo,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane (issue #210): same throwaway-copy
            // `--data-dir` contract as the other read-only lanes.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_risk_markers_cmd(
                &records,
                path.as_deref(),
                at.as_deref(),
                &index,
                selected.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        QuerySubcommand::DeprecatedSymbols {
            graph,
            data_dir,
            repo,
            file,
            format,
        } => {
            // Strictly read-only lane (issue #249): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_deprecated_symbols_cmd(
                &records,
                &index,
                selected.as_deref(),
                file.as_deref(),
                format,
            )
        }
        QuerySubcommand::RedactionAudit {
            graph,
            data_dir,
            repo,
            format,
        } => {
            // Strictly read-only lane (issue #244): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_redaction_audit_cmd(&records, &index, selected.as_deref(), format)
        }
        QuerySubcommand::TrustAudit {
            graph,
            data_dir,
            format,
        } => {
            // Strictly read-only lane (issue #236): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            query_trust_audit_cmd(&records, format)
        }
        QuerySubcommand::UnsafeSites {
            path,
            graph,
            data_dir,
            at,
            repo,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane (issue #222): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_unsafe_sites_cmd(
                &records,
                path.as_deref(),
                at.as_deref(),
                &index,
                selected.as_deref(),
                at_head,
                all_history,
                format,
            )
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
        QuerySubcommand::Ownership {
            path,
            graph,
            data_dir,
            at,
            as_of,
            repo,
            threshold,
            limit,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            let options = query::OwnershipOptions {
                path: path.as_deref(),
                at_commit: at.as_deref(),
                as_of: as_of.as_deref(),
                repo_scope: selected.as_deref(),
                threshold_percent: threshold,
                limit,
            };
            query_ownership_cmd(&records, &options, format)
        }
        QuerySubcommand::Origin {
            symbol,
            graph,
            data_dir,
            repo,
            at,
            as_of,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_origin_cmd(
                &records,
                &symbol,
                selected.as_deref(),
                at.as_deref(),
                as_of.as_deref(),
                format,
            )
        }
        QuerySubcommand::At {
            location,
            graph,
            data_dir,
            at,
            repo,
            format: _format,
        } => {
            // Validate the location before loading records so malformed input
            // fails fast with a machine-readable diagnostic, not a store I/O
            // error (same fail-fast shape as `query subsystem`).
            let (path, line) = match parse_file_line_location(&location) {
                Ok(parsed) => parsed,
                Err(message) => {
                    let envelope = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": "malformed_location",
                            "location": location,
                            "message": message,
                        }
                    });
                    println!("{}", serde_json::to_string(&envelope)?);
                    std::process::exit(1);
                }
            };
            // Strictly read-only lookup: opening the embedded engine in place
            // re-persists its on-disk index files, so `--data-dir` reads from
            // a throwaway copy, never the live store (same contract as the
            // other read-only query lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                // Sidecar-index fast path (issue #447): a file's spans are a
                // `ByPath` closure. `--repo`/`--at` need global topology / the
                // commit timeline and stay cold.
                (Some(graph_path), None) if repo.is_none() && at.is_none() => {
                    load_records_from_jsonl_selected(
                        graph_path,
                        &crate::graph_index::Selector::ByPath(path.to_owned()),
                    )?
                }
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_at_cmd(
                &records,
                path,
                line,
                at.as_deref(),
                &index,
                selected.as_deref(),
            )
        }
        QuerySubcommand::Locate {
            location,
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            at,
            as_of,
            repo,
            supersession,
            format,
        } => {
            // Validate the location before loading records so malformed input
            // fails fast with a machine-readable diagnostic (same fail-fast
            // shape as `query at`).
            let (path, line) = match parse_file_line_location(&location) {
                Ok(parsed) => parsed,
                Err(message) => {
                    let envelope = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": "malformed_location",
                            "location": location,
                            "message": message,
                        }
                    });
                    println!("{}", serde_json::to_string(&envelope)?);
                    std::process::exit(1);
                }
            };
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_locate_via_daemon(
                    path,
                    line,
                    dir,
                    at.as_deref(),
                    as_of.as_deref(),
                    repo.as_deref(),
                    supersession,
                    format,
                );
            }
            // Strictly read-only lookup: `--data-dir` reads from a throwaway
            // copy, never the live store (same contract as `query at`).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                // No sidecar-index fast path (issue #212): the locate answer
                // is the full trust-separated cross-domain bundle — the same
                // `record_context` walk `eg query context` performs (3 hops
                // over cross-domain edges plus every record's evidence_links).
                // No bounded `ByPath` closure can soundly supply that: a
                // pathless `Observation` linked to the located symbol is not in
                // the file's span closure, so the fast path silently dropped it
                // from the bundle while the cold scan kept it. The whole-graph
                // cold scan keeps an indexed answer byte-identical to the
                // unindexed one.
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_locate_cmd(
                &records,
                path,
                line,
                at.as_deref(),
                as_of.as_deref(),
                &index,
                selected.as_deref(),
                supersession,
                format,
            )
        }
        QuerySubcommand::VerificationCoverage {
            scope,
            graph,
            data_dir,
            repo,
            at,
            limit,
            format,
        } => {
            // Validate the limit before touching the store so a malformed
            // bound fails fast with a machine-readable diagnostic.
            if let Some(limit) = limit
                && (limit == 0 || limit > query::VERIFICATION_COVERAGE_MAX_LIMIT)
            {
                let diag = serde_json::json!({
                    "code": "invalid_limit",
                    "limit": limit,
                    "min": 1,
                    "max": query::VERIFICATION_COVERAGE_MAX_LIMIT,
                    "message": format!(
                        "--limit must be between 1 and {}",
                        query::VERIFICATION_COVERAGE_MAX_LIMIT
                    ),
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            // Strictly read-only lane (issue #109): `--data-dir` reads from a
            // throwaway copy, never the live store. A temporal pin needs the
            // history-inclusive store view so older commit versions are present
            // to snapshot.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) if at.is_some() => load_records_from_db_history_readonly(dir)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            // Resolve the optional `--at` commit pin to a single-commit
            // snapshot before building the surface (the surface itself is
            // temporally agnostic, matching `query public-api`). Only the `--at`
            // path needs an index over the full record set; the common path
            // builds the index once, over the snapshot.
            let snapshot = match at.as_deref() {
                None => records,
                Some(selector) => {
                    let index = query::RepositoryIndex::build(&records);
                    let selected = resolve_repo_scope(&index, repo.as_deref());
                    verification_coverage_snapshot_at_commit(
                        records,
                        selector,
                        &index,
                        selected.as_deref(),
                    )
                }
            };
            let index = query::RepositoryIndex::build(&snapshot);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_verification_coverage_cmd(
                &snapshot,
                &index,
                selected.as_deref(),
                scope.as_deref(),
                limit,
                at.is_some(),
                format,
            )
        }
        QuerySubcommand::VerificationFreshness {
            scope,
            graph,
            data_dir,
            repo,
            repo_path,
            stale_only,
            limit,
            format,
        } => {
            // Validate the limit before touching the store so a malformed
            // bound fails fast with a machine-readable diagnostic.
            if let Some(limit) = limit
                && (limit == 0
                    || limit > crate::verification_freshness::VERIFICATION_FRESHNESS_MAX_LIMIT)
            {
                let diag = serde_json::json!({
                    "code": "invalid_limit",
                    "limit": limit,
                    "min": 1,
                    "max": crate::verification_freshness::VERIFICATION_FRESHNESS_MAX_LIMIT,
                    "message": format!(
                        "--limit must be between 1 and {}",
                        crate::verification_freshness::VERIFICATION_FRESHNESS_MAX_LIMIT
                    ),
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            // The comparison needs the code version current AT the anchor
            // (possibly superseded), so this lane always reads the
            // history-inclusive view -- like `eg query evidence-freshness`
            // (issue #85) and unlike the corpus-mode-flipped lanes, since a
            // superseded pre-drift version must stay in scope to diff
            // against. `--data-dir` reads a throwaway copy (strictly
            // read-only, AC9).
            let records = load_evidence_freshness_records(graph.as_deref(), data_dir.as_deref())?;
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_verification_freshness_cmd(
                &records,
                selected.as_deref(),
                scope.as_deref(),
                repo_path.as_deref(),
                stale_only,
                limit,
                format,
            )
        }
        // Appended (issue #225); kept at the end to minimize cross-lane merge conflicts.
        QuerySubcommand::Path {
            from,
            to,
            graph,
            data_dir,
            repo,
            at,
            as_of,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only: opening the live embedded engine re-persists
            // its on-disk index files, so the --data-dir path reads a throwaway
            // copy of the store instead — the original stays byte-for-byte
            // untouched. --graph is a plain file read. A temporal pin needs the
            // history-inclusive store view; the current-state read suffices
            // otherwise. (Mirrors `query implementors`, issue #133.)
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) if at.is_some() || as_of.is_some() => {
                    load_records_from_db_history_readonly(dir)?
                }
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (graph, None) => load_query_records(graph, None)?,
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_path_cmd(
                &records,
                &from,
                &to,
                &index,
                selected.as_deref(),
                at.as_deref(),
                as_of.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        // Appended (issue #444); kept at the end to minimize cross-lane merge conflicts.
        QuerySubcommand::WhoImports {
            module_path,
            graph,
            data_dir,
            crate_name,
            repo,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane: opening the live embedded engine
            // re-persists its on-disk index files, so `--data-dir` reads a
            // throwaway copy of the store — the original stays byte-for-byte
            // untouched. `--graph` is a plain file read. No temporal selectors.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                // Sidecar-index fast path (issue #447): who-imports scans the
                // `Import` nodes, a `ByKind` closure. `--repo` needs global
                // topology for owner attribution and stays cold.
                (Some(graph_path), None) if repo.is_none() => load_records_from_jsonl_selected(
                    graph_path,
                    &crate::graph_index::Selector::ByKind("Import".to_owned()),
                )?,
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_who_imports_cmd(
                &records,
                &module_path,
                crate_name.as_deref(),
                &index,
                selected.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        // Appended (issue #471); kept at the end to minimize cross-lane merge conflicts.
        QuerySubcommand::WhoConstructs {
            handle,
            graph,
            data_dir,
            repo,
            at,
            as_of,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane: `--data-dir` reads a throwaway copy so the
            // live store stays byte-for-byte untouched. Temporal selectors need
            // the history-inclusive store view; the current-state read suffices
            // otherwise. `--graph` is a plain file read (the CONSTRUCTS edges are
            // Edge records, not a single-kind closure, so no sidecar fast path).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) if at.is_some() || as_of.is_some() => {
                    load_records_from_db_history_readonly(dir)?
                }
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_who_constructs_cmd(
                &records,
                &handle,
                &index,
                selected.as_deref(),
                at.as_deref(),
                as_of.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        // Appended (issue #258); kept at the end to minimize cross-lane merge conflicts.
        QuerySubcommand::Uses {
            path,
            graph,
            data_dir,
            repo,
            at,
            as_of,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane: `--data-dir` reads a throwaway copy so the
            // live store stays byte-for-byte untouched. Temporal selectors need
            // the history-inclusive store view; the current-state read suffices
            // otherwise. `--graph` is a plain file read. Config fallback
            // (issue #261): explicit `--data-dir` wins; the config-pinned dir
            // applies only when neither `--graph` nor `--data-dir` was passed,
            // so `--graph` plus a pinned store never reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) if at.is_some() || as_of.is_some() => {
                    load_records_from_db_history_readonly(dir)?
                }
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_dep_usage_cmd(
                &records,
                &path,
                &index,
                selected.as_deref(),
                at.as_deref(),
                as_of.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        // Appended (issue #112); kept at the end to minimize cross-lane merge conflicts.
        QuerySubcommand::Sessions {
            repo,
            graph,
            data_dir,
            limit,
            format,
        } => {
            // Validate the limit BEFORE touching the store so a malformed bound
            // fails fast with a machine-readable diagnostic and never reads a
            // byte. Wrapped in the `{ok, error}` envelope (issue #112 pins the
            // shape) so a caller can distinguish it from the bare selector
            // diagnostics `resolve_repo_scope` emits.
            let max =
                i128::try_from(query::SESSIONS_MAX_LIMIT).expect("SESSIONS_MAX_LIMIT fits i128");
            // `--limit` is a raw `String` (issue #112 review round 22): no
            // fixed-width integer type bounds this parse, so a decimal
            // literal of ANY magnitude reaches this lane's own range check
            // rather than a clap parse error. `parse::<i128>()` fails both
            // for genuinely non-numeric text AND for a well-formed integer
            // wider than i128 — both are equally "not 1..=max", so both are
            // reported identically as `invalid_limit`, never a bare clap
            // usage error.
            let parsed = limit.parse::<i128>().ok();
            let in_range = parsed.is_some_and(|n| (1..=max).contains(&n));
            if !in_range {
                // `serde_json::json!` embeds a raw `i128` via `Serialize`,
                // which PANICS for a value outside BOTH i64 and u64 range
                // (serde_json's `Number` cannot represent one without the
                // `arbitrary_precision` feature, which this crate does not
                // enable). Render as a NUMBER when representable (matching
                // every in-range case callers already rely on), falling back
                // to the exact decimal STRING otherwise — the parsed value's
                // string form when it parsed but overflowed i64/u64, or the
                // raw token verbatim when it never parsed as an integer at
                // all — never a lossy truncation and never a panic.
                let limit_value = parsed.map_or_else(
                    || serde_json::Value::String(limit),
                    |n| {
                        i64::try_from(n)
                            .map(serde_json::Value::from)
                            .or_else(|_| u64::try_from(n).map(serde_json::Value::from))
                            .unwrap_or_else(|_| serde_json::Value::String(n.to_string()))
                    },
                );
                let diag = serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": "invalid_limit",
                        "limit": limit_value,
                        "min": 1,
                        "max": query::SESSIONS_MAX_LIMIT,
                        "message": format!(
                            "--limit must be between 1 and {} (default {})",
                            query::SESSIONS_MAX_LIMIT,
                            query::SESSIONS_DEFAULT_LIMIT
                        ),
                    }
                });
                eprintln!("{diag}");
                std::process::exit(1);
            }
            // In-range at this point: 1..=SESSIONS_MAX_LIMIT fits `usize` on
            // every supported target.
            let limit = usize::try_from(parsed.expect("in_range implies Some"))
                .expect("bounded by the range check above");
            // Strictly read-only lane: `--data-dir` reads a throwaway copy so
            // the live store stays byte-for-byte untouched.
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            // The selector is REQUIRED here (a digest is always repo-scoped);
            // `resolve_repo_scope` exits 1 with the shared stable diagnostics
            // for an unknown or ambiguous selector.
            let repository_id = resolve_repo_scope(&index, Some(repo.as_str()))
                .expect("a required selector always resolves or exits");
            query_sessions_cmd(&records, &index, &repository_id, limit, format)
        }
        // Appended (issue #265); kept at the end to minimize cross-lane merge conflicts.
        QuerySubcommand::BlindSpots {
            graph,
            data_dir,
            repo,
            kind,
            format,
        } => {
            // Strictly read-only lane (issue #265): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_blind_spots_cmd(
                &records,
                &index,
                selected.as_deref(),
                kind.as_query(),
                format,
            )
        }
        // Appended (issue #262); kept at the end to minimize cross-lane merge conflicts.
        QuerySubcommand::TrackRecord {
            graph,
            data_dir,
            repo,
            format,
        } => {
            // Strictly read-only lane (issue #262): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_track_record_cmd(&records, selected.as_deref(), format)
        }
        QuerySubcommand::Session {
            id_or_handle,
            graph,
            data_dir,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_session_cmd(&records, &id_or_handle, format)
        }
        QuerySubcommand::Lanes { format } => query_lanes_cmd(format),
        QuerySubcommand::Suppressions {
            path,
            graph,
            data_dir,
            at,
            repo,
            at_head,
            all_history,
            format,
        } => {
            // Strictly read-only lane (issue #227): opening the embedded
            // engine in place re-persists its on-disk index files, so
            // `--data-dir` reads from a throwaway copy, never the live store
            // (same contract as the other read-only lanes).
            // Config fallback (issue #261): explicit `--data-dir` wins; the
            // config-pinned dir applies only when neither `--graph` nor
            // `--data-dir` was passed, so `--graph` plus a pinned store never
            // reads as "both provided".
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_lint_suppressions_cmd(
                &records,
                path.as_deref(),
                at.as_deref(),
                &index,
                selected.as_deref(),
                at_head,
                all_history,
                format,
            )
        }
        QuerySubcommand::Since {
            tx_after,
            graph,
            data_dir,
            repo,
            domain,
            limit,
            format,
        } => {
            // Transaction-time lane (issues #197, #66): every written version
            // is a feed event, so load the history-inclusive view; and the
            // strictly read-only lane (issue #227) means `--data-dir` reads
            // from a throwaway copy, never the live store. Config fallback
            // (issue #261): explicit `--data-dir` wins; the config-pinned dir
            // applies only when neither `--graph` nor `--data-dir` was passed.
            let data_dir = resolve_query_data_dir(graph.as_deref(), data_dir);
            let records = match (graph.as_deref(), data_dir.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide only one of --graph or --data-dir, not both")
                }
                (None, Some(dir)) => load_records_from_db_history_readonly(dir)?,
                (Some(graph_path), None) => load_records_from_jsonl(graph_path)?,
                (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
            };
            let index = query::RepositoryIndex::build(&records);
            let selected = resolve_repo_scope(&index, repo.as_deref());
            query_since_cmd(
                &records,
                &tx_after,
                &domain,
                selected.as_deref(),
                limit,
                format,
            )
        }
    }
}

/// Re-raises a daemon query error, except for repository-selector rejections,
/// which are printed as the same stable machine-readable stderr JSON the
/// non-daemon paths emit (`unknown_repository_selector` /
/// `ambiguous_repository_selector`) before exiting 1.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn surface_daemon_selector_rejection(
    error: anyhow::Error,
    repo: Option<&str>,
) -> anyhow::Error {
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
pub(crate) fn fail_on_unscoped_daemon_repo_collision(records: &[serde_json::Value]) {
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

/// Resolves an optional `--repo` selector to a stable repository record ID.
///
/// On an unknown or ambiguous selector this prints a stable machine-readable
/// JSON diagnostic to stderr and exits 1 — no partial rows reach stdout, and
/// ambiguity is never resolved by picking a repository implicitly (issue #67).
/// Maps a symbol handle to its sidecar-index [`Selector`](crate::graph_index::Selector)
/// for the `--graph` fast path (issue #447): a canonical `codegraph:` record id
/// resolves [`ById`](crate::graph_index::Selector::ById), any other token is
/// treated as an exact symbol name [`ByName`](crate::graph_index::Selector::ByName).
/// Both closures are supersets that reproduce the cold answer.
pub(crate) fn symbol_handle_selector(handle: &str) -> crate::graph_index::Selector {
    if handle.starts_with("codegraph:") {
        crate::graph_index::Selector::ById(handle.to_owned())
    } else {
        crate::graph_index::Selector::ByName(handle.to_owned())
    }
}

/// The epistemic caveat every `--package`-scoped row carries (issue #117).
///
/// Stated verbatim and identically in `docs/cli/crate-attribution.md`, in
/// `CLAUDE.md`, and on the `CrateAttribution` type. Scoping is the assertion
/// that needs it: an unscoped row's `manifest_repo_relative_path` already names
/// exactly what the claim rests on.
pub(crate) const CRATE_ATTRIBUTION_DISCLAIMER: &str = "attribution is nearest-enclosing-manifest directory containment, never proof the file is compiled into that package";

/// Which packages the loaded corpus carries, and which repositories own each
/// (issue #117).
///
/// Built from the `crate_attribution` stamped on records at scan time — never
/// from manifest files on disk, so a query answers over the corpus it was given.
#[derive(Debug, Default)]
pub(crate) struct PackageCatalog {
    /// Whether ANY record carried a `crate_attribution` field, owner or not.
    /// Separates "attribution never ran" from "attribution ran and found no
    /// package" — see [`PackageCatalog::is_empty`].
    attribution_observed: bool,
    by_package: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

impl PackageCatalog {
    /// Indexes every attributed record's package and its owning repository.
    ///
    /// The names indexed here are read back from a store or graph and are
    /// therefore operator-controlled, and `known_packages` echoes them into a
    /// stderr diagnostic. `manifest_deps::package_name_is_valid` gates names at
    /// PRODUCTION to the `XID_Start` / `XID_Continue` charset, but ingest does
    /// not re-validate a deserialized payload, so that gate is not a guarantee
    /// about what a reader sees — the #104 doctrine that store-read values are
    /// attacker-controlled applies here too. What keeps the diagnostic safe is
    /// that it is rendered as JSON, where `serde_json` escapes control
    /// characters; the raw-text render path fails closed on
    /// [`CrateAttribution::owning_package`] instead.
    pub(crate) fn build(records: &[GraphRecord], index: &query::RepositoryIndex) -> Self {
        Self::build_with_liveness(records, index, &current_deleted_ids(records))
    }

    /// [`Self::build`] with a caller-supplied deletion set.
    ///
    /// Split out so the ANSWERABLE catalog can be built over a narrowed corpus
    /// while liveness is still judged against the WHOLE record set — a
    /// tombstone lives in the corpus alongside the record it retracts, and a
    /// narrowed slice can contain the record without its tombstone.
    pub(crate) fn build_with_liveness(
        records: &[GraphRecord],
        index: &query::RepositoryIndex,
        deleted: &std::collections::BTreeSet<&str>,
    ) -> Self {
        let mut by_package: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
            std::collections::BTreeMap::new();
        let mut attribution_observed = false;
        for record in records {
            // FIRST statement in the loop, ahead of EVERY skip below.
            //
            // This asks whether attribution is USABLE here, not whether an
            // owner was found: a `status: unattributed` value counts, because it
            // proves attribution RAN over this corpus — which is what separates
            // an ownerless corpus from a pre-#117 one. A record that is
            // retracted, or a `Change` that no lane returns, still proves it
            // ran, and no re-scan restores a deleted package or turns a `Change`
            // into a current-state fact, so reporting the pre-#117 capability
            // gap for those sends the operator after a remedy that cannot work.
            //
            // A value the checks REFUSE does not count — including one riding a
            // node kind the resolver never stamps, which
            // `presentable_crate_attribution` refuses for both arms at once.
            // Everywhere else in this feature such a value owns nothing and
            // proves nothing, reading exactly like an absent field; counting it
            // as evidence of capability would make this the one surface where a
            // forged value still buys something — and it would answer a
            // wholly-unusable corpus with an empty `known_packages` list,
            // blaming the caller's spelling when nothing in the store can answer
            // at all.
            //
            // Placed first so a future exclusion cannot re-open that by adding
            // another `continue`: every skip below withholds OWNERSHIP only.
            attribution_observed |= record.presentable_crate_attribution().is_some();
            // A `Change` is a COMMIT EVENT, not a current-state fact: it is
            // minted once per (commit, path) and so is never superseded, which
            // means it survives every corpus narrowing including HEAD
            // anchoring. Counting it here made a package that a repository once
            // held — but does not hold at HEAD — still contribute that
            // repository to the ambiguity verdict, refusing an answerable
            // query. No lane returns `Change` rows for `--package`, so the
            // catalog describes the packages owning the records these lanes can
            // actually return.
            if matches!(
                record,
                GraphRecord::Node {
                    kind: NodeKind::Change,
                    ..
                }
            ) {
                continue;
            }
            // A tombstoned non-temporal record can never appear in an answer —
            // the lanes drop it via `current_deleted_ids` — so counting its
            // package here makes a name look owned by two repositories when
            // only one can produce a row. Mirrors the lanes' own liveness test
            // exactly, including its restriction to non-temporal records: a
            // commit-anchored version is history, not a current-state claim.
            if matches!(record, GraphRecord::Node { temporal: None, .. })
                && deleted.contains(record.id())
            {
                continue;
            }
            // Fail-closed on a read-back value the resolver could not have
            // produced: a record claiming `unattributed` while carrying a name,
            // or citing a manifest that does not ENCLOSE it, owns nothing and
            // must never become a scopable package.
            let Some((name, _manifest)) = record.owning_package() else {
                continue;
            };
            let entry = by_package.entry(name.to_owned()).or_default();
            if let Some(repository_id) = index.owner_of(record.id()) {
                entry.insert(repository_id.to_owned());
            }
        }
        Self {
            attribution_observed,
            by_package,
        }
    }

    /// `true` when at least one record in the corpus is attributed to `name`.
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.by_package.contains_key(name)
    }

    /// `true` when the corpus carries NO `crate_attribution` at all — a
    /// capability gap (a pre-#117 corpus), not an empty result.
    ///
    /// Deliberately NOT "no package was found". A freshly scanned repository
    /// with no `[package]` manifest — only a virtual workspace, or none —
    /// carries a present `status: unattributed` on every path-bearing record:
    /// attribution RAN and there is provably no owner. Re-scanning cannot
    /// create a package, so reporting that as the capability gap would repeat
    /// the absent-vs-unattributed collapse this feature exists to prevent, at
    /// the one surface an operator acts on. The two are tracked separately for
    /// exactly that reason.
    pub(crate) const fn is_empty(&self) -> bool {
        !self.attribution_observed
    }

    /// Every package owning at least one attributed record, sorted.
    pub(crate) fn names(&self) -> Vec<&str> {
        self.by_package.keys().map(String::as_str).collect()
    }

    /// The repositories owning records attributed to `name`, sorted.
    fn owners(&self, name: &str) -> Vec<&str> {
        self.by_package
            .get(name)
            .map(|owners| owners.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

/// Validates a `--package` selector against the corpus, exiting 1 on a bad one.
///
/// Two distinct failures, deliberately NOT collapsed into an empty answer:
/// a selector no package carries is a TYPO (`unknown_package_selector`, listing
/// the known packages), and a name carried by several repositories in a shared
/// store is AMBIGUOUS (`ambiguous_package_selector`) — silently merging two
/// repositories' facts would make the scoped answer's precision claim false.
/// A known, unambiguous selector that simply matches no row falls through to the
/// lane's ordinary exit-2 no-match, so a typo stays distinguishable from an
/// honest empty result.
pub(crate) fn resolve_package_scope(
    catalog: &PackageCatalog,
    answerable: &PackageCatalog,
    selector: &str,
    repo_scoped: bool,
) {
    if !catalog.contains(selector) {
        // A corpus carrying NO attribution at all is a CAPABILITY gap, not a
        // typo: the same `unknown_package_selector` shape would tell an operator
        // querying a pre-#117 store to check their spelling, when the remedy is
        // to re-scan. Reporting them identically would collapse
        // absent-vs-unattributed at the one surface where they act on it.
        if catalog.is_empty() {
            let diag = serde_json::json!({
                "code": "crate_attribution_unavailable",
                "selector": selector,
                "message": "no record in this corpus carries owning-package attribution",
                "remedy": "re-scan with a build that records crate attribution (issue #117), then re-ingest",
            });
            eprintln!("{diag}");
            std::process::exit(1);
        }
        let diag = serde_json::json!({
            "code": "unknown_package_selector",
            "selector": selector,
            "known_packages": catalog.names(),
        });
        eprintln!("{diag}");
        std::process::exit(1);
    }
    // The two verdicts answer DIFFERENT questions, so they read different
    // catalogs (issue #117).
    //
    // Known-ness is TYPO protection — "is this a package name this store carries
    // at all?" — and stays corpus-wide. Narrowing it would report a real package
    // that merely owns nothing at the queried instant as `unknown_package_
    // selector` with a `known_packages` list, which is a lie about the store and
    // steers the caller to fix a spelling that was never wrong; the lane's
    // ordinary exit-2 "known package, zero rows" already says that honestly.
    //
    // Ambiguity is SILENT-MERGE protection — "would scoping here fold two
    // repositories together?" — which is only meaningful over the corpus the
    // answer is computed from. A package two repositories held at different
    // times, but only one holds at HEAD, is not ambiguous for a HEAD answer, and
    // refusing it withheld an answerable result.
    let owners = answerable.owners(selector);
    if !repo_scoped && owners.len() > 1 {
        let diag = serde_json::json!({
            "code": "ambiguous_package_selector",
            "selector": selector,
            "candidates": owners,
            "message": "package name is owned by several repositories; rerun with --repo <SELECTOR>",
        });
        eprintln!("{diag}");
        std::process::exit(1);
    }
}

/// Refuses `--package` on the daemon lane (issue #117).
///
/// The daemon's symbol projection is an independent hand-built JSON map that
/// carries no crate attribution (it already omits `visibility`, `signature`, and
/// `doc`), so a scoped daemon query could only return UNSCOPED rows. Refusing is
/// the honest outcome; silently ignoring the flag would answer a different
/// question than the one asked.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn reject_package_with_daemon(package: Option<&str>) {
    if package.is_some() {
        let diag = serde_json::json!({
            "code": "unsupported_combination",
            "flags": ["--package", "--daemon"],
            "message": "--package is a local-CLI scope; the daemon symbol projection carries no crate attribution",
        });
        eprintln!("{diag}");
        std::process::exit(1);
    }
}

pub(crate) fn resolve_repo_scope(
    index: &query::RepositoryIndex,
    repo: Option<&str>,
) -> Option<String> {
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
pub(crate) fn exit_ambiguous_repository(groups: &std::collections::BTreeSet<Option<&str>>) -> ! {
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

// ---------------------------------------------------------------------------
// Embedding helpers
// ---------------------------------------------------------------------------

/// Generates dense embeddings for all file and symbol candidates in `records`.
///
/// Returns a `(record_id → vector, dimension, model identity)` triple ready for
/// `EmbeddedAletheiaSink::open_with_embeddings`. The identity is returned
/// alongside the vectors — rather than recomputed at each call site — so a write
/// path structurally cannot create a queryable vector index without also
/// recording which model produced it (issue #104).
#[cfg(feature = "embeddings")]
pub(crate) fn generate_embeddings(
    records: &[GraphRecord],
    model_name: &str,
) -> Result<(
    crate::embeddings::EmbeddingVectorMap,
    usize,
    crate::ir::EmbeddingModel,
)> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        EmbeddingVectorKey, EmbeddingVectorMap, aletheia_embeddings, embedding_candidates,
        embedding_model_identity,
    };

    let candidates = embedding_candidates(records);
    if candidates.is_empty() {
        // Zero candidates still creates a queryable (empty) vector index, so it
        // still needs an identity: the dimension is the model's declared one,
        // since the model was never loaded to report a measured dimension. The
        // identity records the RESOLVED model name (issue #261), not the
        // built-in default, so a config-pinned model is described honestly even
        // when nothing was embedded.
        return Ok((
            EmbeddingVectorMap::new(),
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
            embedding_model_identity(model_name, DEFAULT_EMBEDDING_MODEL_DIMENSIONS),
        ));
    }

    eprintln!(
        "Generating embeddings for {} file/symbol nodes…",
        candidates.len()
    );

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(model_name))
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

    // The identity records the MEASURED dimension the model actually produced,
    // not the declared constant, so a model whose real dimension drifts from the
    // constant is still described honestly — and the RESOLVED model name (issue
    // #261), so a config-pinned or `--embed-model` model is never misdescribed
    // as the built-in default.
    Ok((
        map,
        dimensions,
        embedding_model_identity(model_name, dimensions),
    ))
}

/// Renders one embedding-model identity as the allow-listed, bounded JSON object
/// used by the `--embed` write-path refusal (issue #104).
#[cfg(feature = "embeddings")]
fn identity_envelope_json(model: &crate::ir::EmbeddingModel) -> serde_json::Value {
    use crate::embeddings::bounded_identity_field;
    serde_json::json!({
        "provider": bounded_identity_field(&model.provider),
        "name": bounded_identity_field(&model.name),
        "version": bounded_identity_field(&model.version),
        "dim": model.dim,
        "content_hash": bounded_identity_field(&model.content_hash),
    })
}

/// Refuses an `--embed` write that would mix a second model's vectors into a
/// store's existing vector index (issue #104).
///
/// `open_with_embeddings` already refuses a DIMENSION change on an existing
/// index, but two different models can share a dimension — so without this check
/// a second `--embed` write with a different model would silently leave the index
/// holding vectors from BOTH in one un-rankable blend, while the identity record
/// (fixed ID, latest-write-wins) claimed only the newest.
///
/// Refuse-and-instruct, never mutate: the remedy is a fresh `--data-dir`, because
/// the prior model's vectors are already in the index and Egregore does not
/// re-embed a store on mismatch. Refusing here is what keeps the store in the
/// clean, actionable mismatch state instead of a silently blended one — and it is
/// why the natural in-place "fix" after an `eg` upgrade (`eg refresh --embed`)
/// reports a clear refusal rather than quietly making things worse.
///
/// # Errors
///
/// Returns an error when the store already records a different model identity.
#[cfg(feature = "embeddings")]
pub(crate) fn refuse_conflicting_index_identity(
    sink: &EmbeddedAletheiaSink,
    model: &crate::ir::EmbeddingModel,
) -> Result<()> {
    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
    let existing = crate::embeddings::indexed_identities(&records);
    if existing.is_empty() || (existing.len() == 1 && existing[0] == *model) {
        return Ok(());
    }
    let envelope = serde_json::json!({
        "ok": false,
        "error": {
            "code": "embedding_index_identity_conflict",
            "message": "this store's semantic vector index was built by a different embedding \
                        model; embedding into it again would leave the index holding vectors from \
                        both models, which no ranking can compare",
            "remedy": crate::embeddings::EMBEDDING_IDENTITY_REMEDY,
            "indexed_models": existing.iter().map(identity_envelope_json).collect::<Vec<_>>(),
            "producing_model": identity_envelope_json(model),
        }
    });
    println!("{}", serde_json::to_string(&envelope)?);
    anyhow::bail!(
        "--embed refused: this store's semantic vector index was built by a different embedding \
         model; {}",
        crate::embeddings::EMBEDDING_IDENTITY_REMEDY
    )
}

// ---------------------------------------------------------------------------
// eval-semantic command
// ---------------------------------------------------------------------------

/// Runs the semantic relevance corpus evaluation against an embedded store.
///
/// Reads each query from the corpus, embeds it with the default model, runs
/// semantic search, computes aggregate metrics, and prints the report.
pub(crate) fn parse_threshold(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a valid number"))?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(format!("threshold must be between 0.0 and 1.0, got {v}"))
    }
}

// ---------------------------------------------------------------------------
// Tombstone helpers
// ---------------------------------------------------------------------------

/// Returns the set of record IDs that have been tombstoned and not superseded.
/// Used to exclude deleted records from current-state queries (but not --at queries).
pub(crate) fn current_deleted_ids(records: &[GraphRecord]) -> std::collections::BTreeSet<&str> {
    let mut deleted: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for record in records {
        if let GraphRecord::Tombstone { deleted_id, .. } = record {
            deleted.insert(deleted_id.as_str());
        }
    }
    deleted
}

/// Returns `true` when a record participates in `repo` for the purposes of
/// the commit-prefix ambiguity scan: nodes by direct ownership, edges by the
/// ownership of either endpoint (edge records themselves carry no owner).
pub(crate) fn record_belongs_to_repo_for_commit_scan(
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

/// The records the package catalog must be built from: the corpus the answer
/// will actually be computed over (issue #117).
///
/// The catalog decides two things — `known_packages` for a typo, and
/// cross-repository ambiguity — and both are questions ABOUT A CORPUS. Building
/// it from the raw whole-corpus record set while the answer came from a narrower
/// one produced two wrong verdicts: a package two repositories held at DIFFERENT
/// times, but only one holds at HEAD, exited 1 `ambiguous_package_selector`
/// though the HEAD answer is unambiguous; and a package introduced after an
/// `--as-of` instant counted as known, yielding exit 2 "no rows" where the
/// package simply did not exist yet.
///
/// The repository index stays whole-corpus deliberately: `owner_of` maps a
/// record ID to its repository, which is an identity fact, not a corpus one.
///
/// Under a temporal selector, records carrying no resolvable time are DROPPED.
/// This corpus feeds the AMBIGUITY verdict only — known-ness is evaluated
/// corpus-wide and keeps them, which is what still makes a real package name a
/// package rather than a typo. The pinned lanes can only return records stamped
/// at the selected commit or instant (`symbols_at_commit` skips undated records
/// outright; `symbol_as_of_valid_time_by_repo` requires a parseable
/// `valid_time`), so an undated record contributes a repository that could
/// never appear in the answer — turning an answerable query into a false
/// `ambiguous_package_selector`. That is reachable on a shared graph combining
/// a `scan-history` repository with a plain-`scan` one that shares a package
/// name.
fn package_catalog_corpus<'records>(
    records: &'records [GraphRecord],
    index: &query::RepositoryIndex,
    at: Option<&str>,
    as_of: Option<&str>,
    filtered: Option<&'records [GraphRecord]>,
) -> std::borrow::Cow<'records, [GraphRecord]> {
    if let Some(prefix) = at {
        return std::borrow::Cow::Owned(
            records
                .iter()
                .filter(|record| temporal_commit_if_prefix(record, prefix).is_some())
                .cloned()
                .collect(),
        );
    }
    if let Some(raw) = as_of {
        let Ok(instant) = chrono::DateTime::parse_from_rfc3339(raw) else {
            // A malformed timestamp is the lane's error to report; do not let
            // catalog construction pre-empt it with a different diagnostic.
            return std::borrow::Cow::Borrowed(records);
        };
        // The snapshot instant per repository — the newest stamped time any
        // record reached there at or before the cutoff — mirroring how
        // `query_symbol_as_of` resolves snapshot membership.
        let mut snapshot: std::collections::HashMap<
            Option<&str>,
            chrono::DateTime<chrono::FixedOffset>,
        > = std::collections::HashMap::new();
        for record in records {
            let Some(parsed) = record_valid_time_instant(record) else {
                continue;
            };
            if parsed > instant {
                continue;
            }
            let owner = index.owner_of(record.id());
            snapshot
                .entry(owner)
                .and_modify(|latest| {
                    if parsed > *latest {
                        *latest = parsed;
                    }
                })
                .or_insert(parsed);
        }
        return std::borrow::Cow::Owned(
            records
                .iter()
                .filter(|record| {
                    record_valid_time_instant(record).is_some_and(|parsed| {
                        snapshot.get(&index.owner_of(record.id())) == Some(&parsed)
                    })
                })
                .cloned()
                .collect(),
        );
    }
    // HEAD-anchored: `non_head_current_record_ids` drops IDs with no version at
    // HEAD, but keeps every VERSION of the IDs that survive — lanes pick the
    // current one themselves. The catalog reads one field off "the current
    // record", so it needs the version-level test too: without it a superseded
    // version's stale package name still speaks for the ID, which is exactly
    // how a renamed-away package kept contributing its repository to the
    // ambiguity verdict.
    filtered.map_or_else(
        || std::borrow::Cow::Borrowed(records),
        |head_anchored| {
            std::borrow::Cow::Owned(
                query::head_current_versions(head_anchored, index)
                    .into_iter()
                    .cloned()
                    .collect(),
            )
        },
    )
}

/// A record's stamped valid time, parsed — the temporal block's when present,
/// else a node's own `valid_time`.
fn record_valid_time_instant(
    record: &GraphRecord,
) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let raw = match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        }
        | GraphRecord::Edge {
            temporal: Some(t), ..
        } => t.valid_time.as_str(),
        GraphRecord::Node {
            valid_time: Some(vt),
            ..
        } => vt.as_str(),
        _ => return None,
    };
    chrono::DateTime::parse_from_rfc3339(raw).ok()
}

pub(crate) fn temporal_commit_if_prefix<'a>(
    record: &'a GraphRecord,
    prefix: &str,
) -> Option<&'a str> {
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
// query context (issue #38)
// ---------------------------------------------------------------------------

/// One active approved policy record folded into a context answer (issue
/// #169).
///
/// Mirrors [`query::PolicyEntry`]: the authorization basis is the cited
/// approval decision (whose audit trail the fold already verified), and the
/// trust class is `other` under the #114 closed vocabulary — distinguishable
/// from source-derived code facts and unverified observations.
#[derive(Serialize)]
pub(crate) struct ContextPolicyRow<'a> {
    record_id: &'a str,
    kind: &'static str,
    body: &'a str,
    approval_decision_id: &'a str,
    active_from: &'a str,
    status: &'static str,
    trust: crate::query::TrustClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_repo: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_path_glob: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_language: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_lifecycle_phase: Option<&'a str>,
}

/// The five trust-separated context sections (plus topology edges and
/// unresolved references) rendered from a [`query::SymbolContext`].
///
/// Shared by `eg query context` and `eg query semantic-context` so both emit
/// byte-identical section shapes from the same builders.
pub(crate) struct ContextSections<'a> {
    source_facts: Vec<ContextSourceFact<'a>>,
    topology_edges: Vec<ContextTopologyEdge<'a>>,
    observations: Vec<ContextObservation<'a>>,
    decisions: Vec<ContextDecision<'a>>,
    project_state: Vec<ContextLinkedItem<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
    policy: Vec<ContextPolicyRow<'a>>,
}

/// One stable machine-readable diagnostic in the public-api response.
#[derive(Serialize)]
pub(crate) struct PublicApiDiagnosticJson<'a> {
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    record_id: Option<&'a str>,
    detail: &'a str,
}

// ---------------------------------------------------------------------------
// query memory — memory evidence audit (issue #64)
// ---------------------------------------------------------------------------

/// Maps a node kind to its trust class so an agent claim is never labelled as
/// source truth (AC3).
///
/// This is the **static, kind-only** bucket rendered as `trust_class` by the
/// non-context lanes (`eg query memory`, `eg query sessions`, `eg query
/// transaction-time`, the subsystem `log_signatures` rows). Cross-domain
/// context answers instead carry the **derived** per-row `trust` field from
/// [`crate::query::TrustClass`] (issue #114), which renames this function's
/// `source_fact` to `source_derived` and splits `agent_authored` into
/// `agent_verified` / `agent_unverified` / `agent_contradicted` using the
/// record's evidence and contradiction edges.
///
/// The correspondence between the two is pinned by
/// `query::trust::tests::derived_and_legacy_trust_vocabularies_do_not_drift`,
/// so changing one classifier without the other fails the build.
pub(crate) fn trust_class_for(record: &GraphRecord) -> &'static str {
    let Some(kind) = record.node_kind_name() else {
        return "other";
    };
    match kind {
        "Observation" | "Decision" | "Failure" | "Lesson" => "agent_authored",
        "Verification" | "CommandEvidence" | "CommandRun" | "TestRun" | "CIStatus"
        | "BenchmarkRun" | "CoverageReport" | "ProofResult" => "verification_evidence",
        "File"
        | "Symbol"
        | "Module"
        | "Import"
        | "Commit"
        | "Change"
        | "Repository"
        | "PanicRiskSite"
        | "DebtMarker"
        | "UnsafeSite"
        | "DependencyDeclaration" => "source_fact",
        "Task"
        | "AcceptanceCriterion"
        | "LocalTask"
        | "GitHubIssue"
        | "PR"
        | "Review"
        | "ExternalIdentity"
        | "ReviewStateTransition"
        | "ExternalLink"
        | "Product"
        | "Project"
        | "Plan" => "project_state",
        "Artifact" | "PatchArtifact" | "FileEdit" => "artifact",
        // Runtime log-signature observations (issues #319 / #320): a program's
        // own claim about its execution, deterministically parsed but never
        // verified — never source truth or verification evidence.
        "LogSource" | "ErrorSignature" | "LogEvent" | "LogOccurrenceBucket" => {
            "runtime_observation"
        }
        _ => "other",
    }
}
