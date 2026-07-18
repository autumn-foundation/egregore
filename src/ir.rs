use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Current schema version for code-graph records.
pub const SCHEMA_VERSION: u32 = 6;

/// Schema version for agent-memory records (`Agent`, `AgentSession`, `Observation`, etc.).
/// Documented in `docs/schema/agent-memory.md`.
pub const AGENT_MEMORY_SCHEMA_VERSION: u32 = 1;

/// Schema version for verification-domain records (`CommandRun`, `TestRun`, `Verification`, etc.).
/// Documented in `docs/schema/verification.md`.
pub const VERIFICATION_SCHEMA_VERSION: u32 = 1;

/// Schema version for artifact-domain records (`PatchArtifact`, etc.).
/// Documented in `docs/schema/agent-actions.md`.
pub const ARTIFACT_SCHEMA_VERSION: u32 = 1;

/// Schema version for project-domain records (`Task`, `AcceptanceCriterion`, etc.).
/// Documented in `docs/schema/project-graph.md`.
pub const PROJECT_SCHEMA_VERSION: u32 = 1;

/// Schema version for semantic-domain records (`SemanticDrift`, reserved
/// `EmbeddingModel`, reserved `EmbeddingVector`).
/// Documented in `docs/schema/semantic-drift.md`.
pub const SEMANTIC_SCHEMA_VERSION: u32 = 1;

/// Schema version for authorization-derived user-context records
/// (`PromoteCandidate`, `PromotionDecision`, durable preferences, etc.).
/// Documented in `docs/schema/user-context.md`.
pub const USER_CONTEXT_SCHEMA_VERSION: u32 = 1;

/// Schema version for the log-signature domain (`LogSource`, `ErrorSignature`,
/// `LogEvent`, `LogOccurrenceBucket`). Documented in `docs/schema/log-graph.md`
/// (issues #319 / #320).
///
/// v2 (issue #361): source-aware `LogOccurrenceBucket` identity — the owning
/// `LogSource` is folded into the bucket's stable ID and carried as a citable
/// `source_id` payload field, so two distinct sources observing the same
/// signature/hour mint DISTINCT bucket IDs (summed downstream) while a genuine
/// rescan of identical bytes mints the SAME bucket ID (collapsed). A breaking
/// bump: re-scan to regenerate buckets under the new identity.
///
/// v3 (issues #362 / #364): two additive capabilities fold into one breaking
/// bump. **#362** persists repository attribution as a retrievable
/// `repository_id` field on ALL four log payloads (`LogSource`,
/// `ErrorSignature`, `LogEvent`, `LogOccurrenceBucket`) so `--repo` can filter
/// log signatures in a shared multi-repository store (the value is the same
/// `Repository` record ID already hashed into every log ID). **#364** adds a
/// sorted `occurrence_timestamps` list to `LogOccurrenceBucketPayload` so window
/// counts can be bounded endpoint-exactly at an arbitrary commit instant instead
/// of hour-bucket-granular. Both new fields are `#[serde(default)]`, so a legacy
/// `log:v2:` record lacking them still deserializes and degrades honestly (empty
/// attribution / no per-occurrence data) — a deliberate divergence from #361's
/// required-field stance. The version prefix still flips (`log:v2:` → `log:v3:`)
/// so v3 and v2 IDs never collide; re-scan is the remedy to regenerate every
/// record with attribution + per-occurrence timestamps populated.
pub const LOG_SCHEMA_VERSION: u32 = 3;

/// Minimum replay tolerance for semantic drift scores.
/// Documented in `docs/schema/semantic-drift.md`.
pub const SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE: f64 = 1e-5;

/// Schema version for the producer envelope.
///
/// Adding a new `producer_kind` or a well-known `producer_components` key is
/// additive; removing or renaming an existing key is a `/v2/` bump.
/// Documented in `docs/schema/producer-version.md`.
pub const PRODUCER_ENVELOPE_SCHEMA_VERSION: u32 = 1;

// ── Producer identity types (docs/schema/producer-version.md) ─────────────────

/// Optional Git-tree provenance for a producer build.
///
/// Present when the binary was built from a git checkout; absent when built from
/// a clean release tarball. Documented in `docs/schema/producer-version.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct EgregoreGit {
    /// Short or full git commit SHA of the build tree.
    pub commit: String,
    /// `true` when the working tree had uncommitted changes at build time.
    pub dirty: bool,
}

/// Which subsystem wrote a batch of graph records.
///
/// Adding a new variant is **additive** per `docs/schema/schema-versioning.md`.
/// Removing or renaming a variant requires a producer-envelope `/v2/` bump.
/// Documented in `docs/schema/producer-version.md`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerKind {
    /// Tree-sitter-based code graph extractor (`scan` command).
    CodeGraphExtractor,
    /// Git history replay producer (`scan-history` command).
    HistoryReplay,
    /// Incremental cache update producer.
    IncrementalCache,
    /// `.traj` file importer.
    TrajImporter,
    /// Codex-format importer.
    CodexImporter,
    /// Claude Code session importer.
    ClaudeCodeImporter,
    /// Agent-memory observation writer.
    ObservationWriter,
    /// Project / task writer.
    TaskWriter,
    /// Semantic drift engine.
    DriftEngine,
    /// Log-signature importer (`scan-logs` command, issues #319 / #320).
    LogImporter,
    /// Any other producer not enumerated above, including future additive variants
    /// from newer binary versions read by an older binary.
    #[serde(other)]
    Other,
}

impl ProducerKind {
    /// Returns the serialized producer kind string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CodeGraphExtractor => "code_graph_extractor",
            Self::HistoryReplay => "history_replay",
            Self::IncrementalCache => "incremental_cache",
            Self::TrajImporter => "traj_importer",
            Self::CodexImporter => "codex_importer",
            Self::ClaudeCodeImporter => "claude_code_importer",
            Self::ObservationWriter => "observation_writer",
            Self::TaskWriter => "task_writer",
            Self::DriftEngine => "drift_engine",
            Self::LogImporter => "log_importer",
            Self::Other => "other",
        }
    }
}

/// Producer identity envelope stamped on every persisted graph record.
///
/// **Non-identity rule:** `producer` MUST NOT contribute to any stable ID
/// composition. Two records produced by different binary versions over identical
/// input MUST have identical stable IDs. See `docs/schema/producer-version.md`.
///
/// **Legacy-record policy:** records persisted before this field was introduced
/// have no `producer` field. Readers MUST treat them as `legacy_pre_v1` rather
/// than synthesizing a producer identity.
///
/// **Placement:** this is a single embedded field on `GraphRecord`, not per
/// node/edge/tombstone duplication. When a producer emits a batch of records,
/// every record in that batch carries the same `Producer` value.
///
/// Documented in `docs/schema/producer-version.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Producer {
    /// Semver string from `CARGO_PKG_VERSION`.
    pub egregore_version: String,
    /// Git build provenance; absent for clean release tarballs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egregore_git: Option<EgregoreGit>,
    /// Which subsystem produced these records.
    pub producer_kind: ProducerKind,
    /// Named version strings for the producer's components.
    ///
    /// `code_graph_extractor` MUST populate `tree_sitter` and `tree_sitter_rust`.
    /// `history_replay` MUST also populate `tree_sitter` and `tree_sitter_rust`.
    /// `incremental_cache` MUST populate `cache_format_version`.
    /// `drift_engine` MUST populate `embedding_model_id`.
    /// `traj_importer`, `codex_importer`, `claude_code_importer` MUST populate
    /// `importer_schema_version` and `source_format_version`.
    /// `log_importer` MUST populate `importer_schema_version`,
    /// `source_format_version`, and `fingerprint_algorithm` (issues #319 / #320).
    pub producer_components: std::collections::BTreeMap<String, String>,
    /// RFC 3339 wall-clock time the producer process started.
    pub producer_started_at: String,
}

/// Complete in-memory graph emitted by a scan.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    records: Vec<GraphRecord>,
}

impl Graph {
    /// Creates an empty graph.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// Creates a graph from a vector of records.
    #[must_use]
    pub const fn from_records(records: Vec<GraphRecord>) -> Self {
        Self { records }
    }

    /// Adds a record to the graph.
    pub fn push(&mut self, record: GraphRecord) {
        self.records.push(record);
    }

    /// Returns all graph records in insertion order.
    #[must_use]
    pub fn records(&self) -> &[GraphRecord] {
        &self.records
    }

    /// Returns mutable access to all graph records in insertion order.
    ///
    /// Used by post-extraction passes that annotate already-pushed records,
    /// e.g. same-file `CALLS` resolution labeling (issue #134).
    #[must_use]
    pub fn records_mut(&mut self) -> &mut [GraphRecord] {
        &mut self.records
    }

    /// Consumes the graph and returns the records vector.
    #[must_use]
    pub fn into_records(self) -> Vec<GraphRecord> {
        self.records
    }

    /// Stamps every record in the graph with the given producer envelope.
    ///
    /// When a single producer emits a batch of records, every record in that
    /// batch carries the same `Producer` value per the non-identity rule in
    /// `docs/schema/producer-version.md`.
    #[must_use]
    pub fn stamp_producer(self, producer: &Producer) -> Self {
        Self {
            records: self
                .records
                .into_iter()
                .map(|r| r.with_producer(producer.clone()))
                .collect(),
        }
    }

    /// Serializes the graph to canonically ordered JSON Lines.
    ///
    /// Ordering is based on the final serialized lines. This keeps output
    /// byte-stable even if internal scan order changes.
    ///
    /// # Errors
    ///
    /// Returns an error if any record cannot be serialized.
    pub fn to_jsonl(&self) -> Result<String> {
        let mut lines = self
            .records
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        lines.sort_unstable();
        Ok(format!("{}\n", lines.join("\n")))
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

/// Output or error handle for verification-domain `CommandRun` records.
///
/// Inline content is bounded by a 16 KiB ceiling.  When the output exceeds
/// that ceiling the `inline` field MUST be `None` and the full content is
/// referenced by `hash` only.  Documented in `docs/schema/verification.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutputHandle {
    /// Inline content (None when bytes exceeds the 16 KiB ceiling).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
    /// BLAKE3 hash of the full output content.
    pub hash: String,
    /// Total byte length of the output.
    pub bytes: u64,
}

/// Handle for raw patch bytes in an artifact-domain `PatchArtifact`.
///
/// The patch may be inlined only under the 16 KiB ceiling; otherwise the path
/// points at protected artifact storage. Documented in
/// `docs/schema/agent-actions.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PatchHandle {
    /// Path to the stored patch bytes.
    pub path: String,
    /// Redacted inline patch bytes when the payload is small enough.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
}

/// Agent-memory provenance fields for agent-authored nodes.
///
/// Documented in `docs/schema/agent-memory.md §3`.
/// These fields appear flat in `GraphRecord::Node` JSON.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct NodeProvenance {
    /// Observation body text (Observation nodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// ID of the record that supersedes this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Stable agent identity (agent-authored nodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Agent kind enum value (agent-authored nodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<String>,
    /// Agent session ID (agent-authored nodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Wall-clock time the agent observed the fact (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    /// Transaction time when the daemon committed the record (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingested_at: Option<String>,
    /// Extraction confidence `[0.0, 1.0]` (Observation, Decision, Lesson).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Artifact path or hash the record was extracted from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_handle: Option<String>,
    /// Redaction policy version when any field passed through redaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction_policy_version: Option<String>,
}

/// How a `Repository` node's stable ID was determined.
///
/// Documented in `docs/schema/repository-identity.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentitySource {
    /// Derived from the lowest-name-sorted git remote URL (normalized).
    Remote,
    /// Derived from the root commit SHA (no remote available).
    LocalRootCommit,
    /// Derived from the canonical absolute path (no git or no commits).
    /// Not stable across machines; unsafe for shared daemon stores.
    LocalPath,
    /// Supplied directly by the operator via `--repo-id-override`.
    OperatorOverride,
}

/// Identity payload carried on every `Repository` node.
///
/// Describes how the node's stable ID was derived. Present only on
/// `NodeKind::Repository` nodes; absent on all other node kinds.
///
/// Documented in `docs/schema/repository-identity.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryIdentityPayload {
    /// How the stable ID was computed.
    pub identity_source: IdentitySource,
    /// Normalized canonical remote URL (when `identity_source = remote`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    /// Root commit SHA (when `identity_source = local_root_commit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_commit_sha: Option<String>,
    /// Canonical absolute path (when `identity_source = local_path`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_path: Option<String>,
    /// Directory basename; for display only, never an identity input.
    pub basename: String,
}

/// HEAD commit state captured in a [`SourceSnapshotPayload`] at scan time.
///
/// Mirrors the repository-identity module's git-root gate: a commit SHA is only
/// recorded when the scanned path is the actual Git repository root. Sub-directories
/// of a repo and non-Git directories serialize as `no_git`; a repository with no
/// commits yet serializes as `unborn_head`. Documented in
/// `docs/schema/source-snapshot.md` (issue #82).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SnapshotHead {
    /// HEAD resolved to a commit; carries the full commit SHA.
    Commit {
        /// Full commit SHA the working tree was scanned at.
        sha: String,
    },
    /// The scanned path is not a Git repository root (or Git is unavailable).
    NoGit,
    /// The scanned path is a Git repository whose HEAD has no commits yet.
    UnbornHead,
}

/// Store-level source-snapshot identity stamped on the `Repository` node.
///
/// Records *which* working-tree snapshot a store was built from so a reader can
/// detect staleness against the live working tree without re-scanning. The
/// deterministic portion (`head` + `dirty`) is reproducible for an unchanged clean
/// tree at a fixed commit; `scanned_at` flows through the same transaction-time
/// override path as `valid_time`, so it never breaks JSONL determinism.
///
/// Documented in `docs/schema/source-snapshot.md` (issue #82).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceSnapshotPayload {
    /// HEAD commit state at scan time.
    pub head: SnapshotHead,
    /// `true` when the working tree had uncommitted or untracked changes at scan
    /// time. Always `false` when `head` is `no_git` / `unborn_head` (no committed
    /// baseline to be dirty against).
    pub dirty: bool,
    /// Stable `Repository` record ID this snapshot describes — the repository
    /// identity already used by the graph.
    pub repository_id: String,
    /// RFC 3339 wall-clock scan time, carried through the transaction-time
    /// override path for deterministic JSONL.
    pub scanned_at: String,
}

/// Declared-dependency payload stamped on `DependencyDeclaration` nodes.
///
/// Captures one directly-declared Cargo dependency exactly as written in a
/// `Cargo.toml` manifest, joined with the single resolved version from the
/// nearest `Cargo.lock` when one exists (issue #180). Parse-derived and
/// strictly local: never the output of `cargo metadata`, a network lookup, or
/// a build. All fields are additive per `docs/schema/schema-versioning.md §2`
/// and are never identity inputs beyond those hashed into the record ID.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DependencyDeclarationPayload {
    /// `[package].name` of the manifest declaring this dependency.
    pub declaring_package: String,
    /// Dependency table the declaration was written in, drawn from the closed
    /// set `normal` (`[dependencies]`), `dev` (`[dev-dependencies]`), or
    /// `build` (`[build-dependencies]`).
    pub dependency_kind: String,
    /// Manifest key the entry was declared under when it differs from the
    /// crate name (Cargo `package = "…"` rename syntax, which legitimately
    /// declares several versions of one crate). Absent for plain
    /// declarations. Additive per `docs/schema/schema-versioning.md §2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_as: Option<String>,
    /// Declared version requirement string exactly as written (`"1.0.228"`).
    /// Absent when the declaration carries no `version` key (e.g. a pure
    /// `path`/`git`/`workspace = true` dependency) — never fabricated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_requirement: Option<String>,
    /// The single resolved version from the nearest `Cargo.lock`. Present only
    /// when `resolution` is `locked`; never a guessed or fabricated version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_version: Option<String>,
    /// Lockfile resolution marker, drawn from the closed set `locked`,
    /// `no_lockfile` (no `Cargo.lock` found for this manifest),
    /// `not_in_lockfile` (lockfile exists but does not list the crate),
    /// `ambiguous_in_lockfile` (several locked versions and the declared
    /// requirement cannot select exactly one), `requirement_unsatisfied_in_lockfile`
    /// (a parseable declared requirement is satisfied by none of the locked
    /// versions; the mismatched version is never presented as resolved), or
    /// `lockfile_unreadable` (the nearest `Cargo.lock` exists but could not
    /// be read or parsed; an ancestor lockfile is never consulted in its
    /// place).
    pub resolution: String,
}

/// Scan-coverage payload stamped on the single `ScanCoverage` node a full
/// `eg scan` emits (issue #135).
///
/// Makes indexing coverage a stated, deterministic, queryable graph fact: how
/// many files the walk visited, how many were indexed, and — for every file
/// that was walked but not indexed — a per-extension skip tally. Excluded
/// directories (`.git`, `target`, nested Git worktrees) are never walked, so
/// they never dilute these counts (AC7). All fields are additive per
/// `docs/schema/schema-versioning.md §2` and carry no paths or PII — only
/// lowercased extensions, counts, and the named language scope — so the node is
/// redaction-exempt deterministic code-graph data.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScanCoveragePayload {
    /// Total files the walk visited (files under excluded directories are never
    /// counted). Equals `files_indexed + sum(skipped_by_extension.values())`
    /// whenever `coverage_complete` is true (AC4).
    pub files_walked: usize,
    /// Files that matched the indexed-source filter and became code-graph nodes.
    pub files_indexed: usize,
    /// Per-extension count of walked-but-not-indexed files, keyed on the
    /// lowercased final path extension (`""` for a file with no extension).
    /// A sorted map for byte-stable output (AC5).
    pub skipped_by_extension: std::collections::BTreeMap<String, usize>,
    /// The human-facing names of the languages `eg scan` indexes, in a stable
    /// order (AC6). Derived from [`crate::languages::Language::ALL`], never a
    /// hard-coded list, so it can never drift from the real extractor scope.
    pub indexed_languages: Vec<String>,
    /// `true` when the walk produced a complete file-level accounting (the
    /// Git-tracked-files path). `false` for the non-Git filesystem-walk
    /// fallback, which enumerates only matching source files and so has no
    /// walked/skipped denominator; there `files_walked == files_indexed` and
    /// `skipped_by_extension` is empty. Never fabricates a denominator.
    pub coverage_complete: bool,
    /// Full-precision (nanoseconds) RFC 3339 UTC instant captured at scan time,
    /// used ONLY as a recency tie-break signal when two `ScanCoverage` versions
    /// of the same repository share a seconds-precision `valid_time` — the
    /// issue #406 same-UTC-second freshness signal. `eg inspect --graph`
    /// collapses equal-ID versions to the one with the newest `valid_time`
    /// instant, then (on a tie) the newest `coverage_generation`; two full
    /// scans within one UTC second get DISTINCT nanosecond values here, so the
    /// newer one wins deterministically regardless of physical line order.
    /// Non-identity: never feeds a record's stable ID and never affects any
    /// other node's `valid_time`. Additive per
    /// `docs/schema/schema-versioning.md §2`; absent on graphs produced before
    /// this field, which fall through to the content tie-break.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_generation: Option<String>,
}

/// Per-kind payload stamped on the four log-signature node kinds (issues
/// #319 / #320).
///
/// Carried in one boxed `log` field on `GraphRecord::Node`, mirroring the
/// `dependency` payload pattern, so log records never widen the flat node
/// schema. Serialized internally-tagged on `log_kind`, so the concrete
/// variant's fields sit alongside the tag. All fields are additive per
/// `docs/schema/schema-versioning.md §2`. Only the fields listed in the
/// identity part-lists of `docs/schema/log-graph.md` feed a record's stable
/// ID; the rest are non-identity metadata.
///
/// No field ever carries raw log text beyond a bounded, post-redaction excerpt
/// (`template_excerpt` / `event_excerpt`).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "log_kind", rename_all = "snake_case")]
pub enum LogPayload {
    /// A captured log source artifact.
    LogSource(LogSourcePayload),
    /// A deduplicated error fingerprint.
    ErrorSignature(ErrorSignaturePayload),
    /// A bounded exemplar occurrence of a signature.
    LogEvent(LogEventPayload),
    /// An hourly occurrence-count bucket for a signature.
    LogOccurrenceBucket(LogOccurrenceBucketPayload),
}

impl LogPayload {
    /// Returns the persisted `Repository` record ID this log node is attributed
    /// to (issue #362, schema v3).
    ///
    /// Every log payload variant carries a `repository_id` field equal to the
    /// code-graph `Repository` node ID computed at scan time (already an identity
    /// input for the log record's stable ID). An empty string is a legacy
    /// `log:v2:` record deserialized through `#[serde(default)]`: unattributed,
    /// so consumers treat it as owner-less.
    #[must_use]
    pub fn repository_id(&self) -> &str {
        match self {
            Self::LogSource(p) => &p.repository_id,
            Self::ErrorSignature(p) => &p.repository_id,
            Self::LogEvent(p) => &p.repository_id,
            Self::LogOccurrenceBucket(p) => &p.repository_id,
        }
    }
}

/// Payload for a `LogSource` node: the captured log artifact identity.
///
/// Identity inputs (`docs/schema/log-graph.md`): `repository_id`,
/// `source_relative_path`, `source_artifact_hash`. `line_count`, capture time,
/// and producer are non-identity.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LogSourcePayload {
    /// Repository-relative path of the captured log file.
    pub source_relative_path: String,
    /// Detected source format: `plain-v1` or `jsonl-v1`.
    pub source_format_version: String,
    /// BLAKE3 hex of the newline-normalized (`\r\n` / `\r` → `\n`) file bytes.
    /// The idempotency anchor; normalizing line endings before hashing makes a
    /// CRLF and an LF checkout of the same log yield an identical hash.
    pub source_artifact_hash: String,
    /// Count of every logical line in the source, including info/debug noise
    /// that mints no signature. Non-identity.
    pub line_count: u64,
    /// Stable `Repository` record ID this log source is attributed to (issue
    /// #362, schema v3). Already an identity input for every log ID; storing it
    /// makes attribution retrievable so `--repo` can filter log signatures.
    /// `#[serde(default)]`: a legacy `log:v2:` record without it deserializes to
    /// an empty (unattributed) string.
    #[serde(default)]
    pub repository_id: String,
}

/// Payload for an `ErrorSignature` node: a deduplicated error fingerprint.
///
/// Identity inputs: `repository_id`, `fingerprint_algorithm`,
/// `normalized_template`, `severity`. `occurrence_count`, `first_seen`,
/// `last_seen`, capture time, and producer are non-identity.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ErrorSignaturePayload {
    /// Fingerprint algorithm identifier (`template-v1`).
    pub fingerprint_algorithm: String,
    /// Bounded, post-redaction excerpt of the normalized template. Never raw
    /// log text beyond the documented excerpt bound.
    pub template_excerpt: String,
    /// Closed severity class: `fatal`, `error`, or `warn`.
    pub severity: String,
    /// Total raw occurrences that fingerprinted to this signature. Non-identity.
    pub occurrence_count: u64,
    /// Valid time of the earliest occurrence. Non-identity.
    pub first_seen: String,
    /// Valid time of the latest occurrence. Non-identity.
    pub last_seen: String,
    /// Structured, redaction-safe backtrace frames captured at scan time
    /// (issue #322), when the signature carried a parseable backtrace. Frames
    /// are **non-identity**: they are never part of the signature record-ID
    /// hash preimage. Absent (`None`) when no backtrace was parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<Vec<StackFrame>>,
    /// Stable `Repository` record ID this signature is attributed to (issue
    /// #362, schema v3). Already an identity input for the signature ID; storing
    /// it makes attribution retrievable so `--repo` can filter log signatures.
    /// `#[serde(default)]`: a legacy `log:v2:` record without it deserializes to
    /// an empty (unattributed) string.
    #[serde(default)]
    pub repository_id: String,
}

/// Payload for a `LogEvent` node: one bounded exemplar occurrence.
///
/// Identity inputs: `repository_id`, `signature_id`, `event_valid_time`,
/// `event_content_hash`. `source_line`, byte offsets, capture time, and
/// producer are non-identity.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LogEventPayload {
    /// Bounded, post-redaction excerpt of the exemplar line(s).
    pub event_excerpt: String,
    /// BLAKE3 hex of the newline-normalized exemplar content.
    pub event_content_hash: String,
    /// One-based source line the exemplar began on. Non-identity.
    pub source_line: u64,
    /// Closed severity class: `fatal`, `error`, or `warn`.
    pub severity: String,
    /// Stable `Repository` record ID this exemplar is attributed to (issue #362,
    /// schema v3). Already an identity input for the event ID; storing it gives
    /// every log node uniform, retrievable attribution. `#[serde(default)]`: a
    /// legacy `log:v2:` record without it deserializes to an empty string.
    #[serde(default)]
    pub repository_id: String,
}

/// Payload for a `LogOccurrenceBucket` node: an hourly occurrence count.
///
/// Identity inputs: `repository_id`, `signature_id`, `bucket_start`,
/// `bucket_width`, and `source_id` (issue #361, schema v2). `occurrence_count`
/// and producer are non-identity. Folding the owning `LogSource` into identity
/// makes two distinct sources observing the same signature/hour mint DISTINCT
/// bucket IDs (summed downstream), while a genuine rescan of identical bytes
/// mints the SAME bucket ID (collapsed as a duplicate).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LogOccurrenceBucketPayload {
    /// RFC 3339 UTC start of the bucket, floored to the hour.
    pub bucket_start: String,
    /// Bucket width token (`1h`).
    pub bucket_width: String,
    /// Occurrences of the signature within this bucket. Non-identity.
    pub occurrence_count: u64,
    /// Stable `log:v<N>:` handle to the owning `LogSource`. Identity input
    /// (issue #361): distinguishes buckets from distinct sources sharing a
    /// signature/hour from a genuine rescan of the same source.
    pub source_id: String,
    /// Stable `Repository` record ID this bucket is attributed to (issue #362,
    /// schema v3). Already an identity input for the bucket ID; storing it makes
    /// attribution retrievable so `--repo` can filter log signatures.
    /// `#[serde(default)]`: a legacy `log:v2:` record without it deserializes to
    /// an empty (unattributed) string.
    #[serde(default)]
    pub repository_id: String,
    /// Sorted (ascending), Z-normalized RFC 3339 UTC valid times of every
    /// occurrence that fell in this hour (issue #364, schema v3). Non-identity.
    /// Its `len()` equals `occurrence_count`. Lets a consumer bound window counts
    /// endpoint-exactly at an arbitrary commit instant instead of counting the
    /// whole hour-aligned bucket. `#[serde(default)]`: a legacy `log:v2:` bucket
    /// without it deserializes to an empty vector (no per-occurrence data, so
    /// consumers fall back to the hour-bucket-granular predicate for it).
    #[serde(default)]
    pub occurrence_timestamps: Vec<String>,
}

/// A typed citation from an agent-memory node to another graph record.
///
/// Evidence links are stored both on the source node (for fast read) and as
/// graph edges (for traversal). Both representations MUST agree at write time;
/// the daemon write applier is the enforcement point.
///
/// The target may be specified either by its stable `target_record_id` or by
/// the `(target_repo_relative_path, target_span, target_git_commit)` triple
/// when the writer cannot compute the stable hash. The daemon write applier
/// resolves the triple at write time.
///
/// Documented in docs/schema/agent-memory.md.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceLink {
    /// Stable record ID of the cited graph node.
    /// Either this or the triple fields below must be present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_record_id: Option<String>,
    /// Domain of the target record (e.g. `"codegraph"`, `"agent_memory"`).
    pub target_domain: String,
    /// Cross-domain edge label (e.g. `"OBSERVES"`, `"MENTIONS_SYMBOL"`).
    pub relation: String,
    /// Extraction confidence formatted in `[0.0, 1.0]`.
    pub confidence: String,
    /// Git commit SHA anchoring a time-specific citation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of_commit: Option<String>,
    // ── Triple-based target resolution ────────────────────────────────────────
    /// Repository-relative path of the target node (triple fallback).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_repo_relative_path: Option<String>,
    /// Source span of the target node (triple fallback).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_span: Option<SourceSpan>,
    /// Git commit SHA anchoring the target node lookup (triple fallback).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_git_commit: Option<String>,
}

/// User-context scope shared by promotion candidates and durable rules.
///
/// All fields omitted means "global to this operator"; see
/// `docs/schema/user-context.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct UserContextScope {
    /// Optional repository identity from the repository-identity domain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Optional repository-relative glob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_glob: Option<String>,
    /// Optional language tag such as `rust`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Optional lifecycle phase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle_phase: Option<String>,
}

/// Flat user-context fields carried by `GraphRecord::Node`.
///
/// The fields are flattened into node JSON so the schema remains a normal
/// record shape instead of a nested metadata blob.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct UserContextFields {
    /// Candidate durable rule body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed_rule_text: Option<String>,
    /// Durable kind the candidate would materialize.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed_rule_kind: Option<String>,
    /// Scope for candidates and durable user-context records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<UserContextScope>,
    /// Evidence supporting a promotion candidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supporting_evidence: Option<Vec<EvidenceLink>>,
    /// Evidence contradicting a promotion candidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contradicting_evidence: Option<Vec<EvidenceLink>>,
    /// Promotion candidate ID for prompts and decisions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_id: Option<String>,
    /// Surface where a prompt was presented.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_surface: Option<String>,
    /// Exact redacted prompt text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_text: Option<String>,
    /// Prompt timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompted_at: Option<String>,
    /// Operator handle the prompt was shown to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompted_to: Option<String>,
    /// Prompt expiry timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Prompt ID a decision responds to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    /// Promotion decision outcome.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Decision timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    /// Operator handle that made a decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    /// Optional redacted rationale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_rationale: Option<String>,
    /// Durable record ID created or revoked by a decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub materialized_record_id: Option<String>,
    /// Edited rule body for edited approvals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited_rule_text: Option<String>,
    /// Durable preference/workflow rule body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_text: Option<String>,
    /// Approval decision that authorized a durable record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_decision_id: Option<String>,
    /// RFC3339 activation timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_from: Option<String>,
    /// RFC3339 deactivation timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_to: Option<String>,
    /// Workflow trigger enums.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggers: Option<Vec<String>>,
    /// Workflow action summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_summary: Option<String>,
    /// Naming-decision entity kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_kind: Option<String>,
    /// Canonical approved name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_name: Option<String>,
    /// Rejected naming alternatives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alternatives_rejected: Option<Vec<String>>,
    /// Constraint body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraint_text: Option<String>,
    /// Constraint enforcement level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enforcement_level: Option<String>,
}

impl UserContextFields {
    /// Empty user-context field set.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            proposed_rule_text: None,
            proposed_rule_kind: None,
            scope: None,
            supporting_evidence: None,
            contradicting_evidence: None,
            candidate_id: None,
            prompt_surface: None,
            prompt_text: None,
            prompted_at: None,
            prompted_to: None,
            expires_at: None,
            prompt_id: None,
            outcome: None,
            decided_at: None,
            decided_by: None,
            decision_rationale: None,
            materialized_record_id: None,
            edited_rule_text: None,
            rule_text: None,
            approval_decision_id: None,
            active_from: None,
            active_to: None,
            triggers: None,
            action_summary: None,
            entity_kind: None,
            canonical_name: None,
            alternatives_rejected: None,
            constraint_text: None,
            enforcement_level: None,
        }
    }

    /// Returns true when no user-context field is populated.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.proposed_rule_text.is_none()
            && self.proposed_rule_kind.is_none()
            && self.scope.is_none()
            && self.supporting_evidence.is_none()
            && self.contradicting_evidence.is_none()
            && self.candidate_id.is_none()
            && self.prompt_surface.is_none()
            && self.prompt_text.is_none()
            && self.prompted_at.is_none()
            && self.prompted_to.is_none()
            && self.expires_at.is_none()
            && self.prompt_id.is_none()
            && self.outcome.is_none()
            && self.decided_at.is_none()
            && self.decided_by.is_none()
            && self.decision_rationale.is_none()
            && self.materialized_record_id.is_none()
            && self.edited_rule_text.is_none()
            && self.rule_text.is_none()
            && self.approval_decision_id.is_none()
            && self.active_from.is_none()
            && self.active_to.is_none()
            && self.triggers.is_none()
            && self.action_summary.is_none()
            && self.entity_kind.is_none()
            && self.canonical_name.is_none()
            && self.alternatives_rejected.is_none()
            && self.constraint_text.is_none()
            && self.enforcement_level.is_none()
    }
}

/// Resolution status carried by labeled `CALLS` edges (issues #152/#134).
///
/// Emitted by the deterministic repo-wide cross-file resolution pass (issue
/// #152) and by the same-file labeling pass over per-file `CALLS` edges
/// backed by Tree-sitter call sites (issue #134). Both slices label call
/// edges through one field:
///
/// - `resolved` — the call site's name (plus any syntactic path/receiver
///   narrowing) matched exactly one in-repo definition.
/// - `ambiguous` — the name matched two or more in-repo definitions; an edge
///   is emitted to every candidate, each labeled `ambiguous`.
/// - `unresolved` — no in-repo definition matched; the edge targets a
///   `Diagnostic` node recording the callee, never an invented symbol.
///
/// Adding this optional field is additive per
/// `docs/schema/schema-versioning.md`; legacy edges simply lack it.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallResolution {
    /// Exactly one in-repo definition matched the call site.
    Resolved,
    /// Two or more in-repo definitions matched; all candidates carry edges.
    Ambiguous,
    /// No in-repo definition matched; the target is a `Diagnostic` marker.
    Unresolved,
}

impl CallResolution {
    /// Returns the serialized resolution status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Ambiguous => "ambiguous",
            Self::Unresolved => "unresolved",
        }
    }

    /// Parses a resolution status from its wire string. Returns `None` for
    /// unknown values.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "resolved" => Some(Self::Resolved),
            "ambiguous" => Some(Self::Ambiguous),
            "unresolved" => Some(Self::Unresolved),
            _ => None,
        }
    }
}

/// Resolution class of one backtrace stack frame against the code graph
/// (issue #322). Modeled on [`CallResolution`]: a frame handle is never
/// silently bound to an invented target.
///
/// The value set is **closed and stable**:
///
/// - `resolved` — the frame's `file:line` (or its module-path name) matched
///   exactly one in-repo `Symbol`; the `FRAME_RESOLVES_TO` edge targets it.
/// - `ambiguous` — two or more in-repo `Symbol`s matched; every candidate
///   carries its own edge and no candidate is silently chosen.
/// - `path_only` — the frame's file exists in the graph but no enclosing
///   symbol contains the line (e.g. an optimized-out or macro-generated
///   frame); the edge targets the `File` node.
/// - `unresolved` — the frame names a repo-relative path that is absent from
///   the resolved view (deleted or renamed since the log); the edge targets a
///   `Diagnostic` node carrying the redacted frame text, never an invented
///   symbol.
///
/// Frames into the standard library or a dependency are classified `external`
/// in a per-signature tally and mint **no** edge, so `external` is not a member
/// of this on-edge enum.
///
/// Adding this optional edge field is additive per
/// `docs/schema/schema-versioning.md`; legacy edges simply lack it.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameResolution {
    /// Exactly one in-repo `Symbol` matched the frame.
    Resolved,
    /// Two or more in-repo `Symbol`s matched; all candidates carry edges.
    Ambiguous,
    /// The frame's file exists but no enclosing symbol contains the line; the
    /// edge targets the `File` node.
    PathOnly,
    /// The frame names a repo path absent from the resolved view; the edge
    /// targets a `Diagnostic` marker.
    Unresolved,
}

impl FrameResolution {
    /// Returns the serialized resolution status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Ambiguous => "ambiguous",
            Self::PathOnly => "path_only",
            Self::Unresolved => "unresolved",
        }
    }

    /// Parses a frame-resolution status from its wire string. Returns `None`
    /// for unknown values.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "resolved" => Some(Self::Resolved),
            "ambiguous" => Some(Self::Ambiguous),
            "path_only" => Some(Self::PathOnly),
            "unresolved" => Some(Self::Unresolved),
            _ => None,
        }
    }
}

/// Correlation basis of an `EMITTED_DURING` evidence-link edge (issue #323).
///
/// An `EMITTED_DURING` edge runs `ErrorSignature` → the agent run / command that
/// produced it. Every such edge MUST carry exactly one basis from this **closed,
/// stable** set — no edge is ever emitted without a documented basis:
///
/// - `content_hash_join` — the `LogSource` the signature was `CAPTURED_FROM`
///   carries a `source_artifact_hash` equal to a `CommandRun`'s captured
///   stdout/stderr `OutputHandle.hash`. This is exact BLAKE3 byte equality: the
///   log artifact *is* that command's output, so the join is deterministic and
///   inherently within one repository. Edges of this basis carry confidence
///   `1.0`.
/// - `temporal_correlation` — the signature's representative valid time falls
///   inside an `AgentRun` / `AgentTurn` execution window (within the configured
///   tolerance) for the **same repository**. This is a *correlation lead, never
///   causation*: overlapping in time is not proof the run produced the error.
///   Edges of this basis carry a lower confidence (`0.5`), and overlapping runs
///   each mint their own edge — no single winner is silently chosen.
///
/// Adding this optional edge field is additive per
/// `docs/schema/schema-versioning.md`; legacy edges simply lack it.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrelationBasis {
    /// The signature's `LogSource` artifact hash equals a `CommandRun` output
    /// handle hash (exact BLAKE3 byte equality). Confidence `1.0`.
    ContentHashJoin,
    /// The signature's valid time falls inside a same-repository run window.
    /// A correlation lead, never causation. Confidence `0.5`.
    TemporalCorrelation,
}

impl CorrelationBasis {
    /// Returns the serialized correlation basis.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContentHashJoin => "content_hash_join",
            Self::TemporalCorrelation => "temporal_correlation",
        }
    }

    /// The documented confidence constant carried by an edge of this basis.
    #[must_use]
    pub const fn confidence(self) -> &'static str {
        match self {
            Self::ContentHashJoin => "1.0",
            Self::TemporalCorrelation => "0.5",
        }
    }

    /// Parses a correlation basis from its wire string. Returns `None` for
    /// unknown values.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "content_hash_join" => Some(Self::ContentHashJoin),
            "temporal_correlation" => Some(Self::TemporalCorrelation),
            _ => None,
        }
    }
}

/// One structured backtrace stack frame captured from a runtime log
/// (issues #319/#320/#322).
///
/// Redaction-safe by construction: `module_path` and `file_path` pass through
/// the v1 redaction policy at capture time and `file_path` is normalized to a
/// repository-relative form (or a generalized external-toolchain form) so no
/// absolute host path or username enters the graph. Frames are **non-identity**:
/// they never participate in the `ErrorSignature` record-ID hash preimage.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct StackFrame {
    /// Zero-based position of the frame in the captured backtrace.
    pub frame_index: u32,
    /// Redacted `module::path` of the frame, when the backtrace named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_path: Option<String>,
    /// Redacted, repo-relative (or generalized external) file path, when the
    /// backtrace named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// One-based source line named by the frame, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// One JSONL graph record.
// Node carries 10 optional provenance strings for agent-memory nodes.
// These are None for all code-graph nodes, so the memory cost is only
// paid by agent-memory records that actually populate them.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub enum GraphRecord {
    /// A graph node.
    Node {
        /// Stable record ID.
        id: String,
        /// Node kind.
        kind: NodeKind,
        /// Schema version that produced the record.
        schema_version: u32,
        /// Repository-relative path for file-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        repo_relative_path: Option<String>,
        /// Source span for syntax-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<SourceSpan>,
        /// Human-readable node name.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Language for syntax-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        /// Language-specific symbol category.
        #[serde(skip_serializing_if = "Option::is_none")]
        symbol_kind: Option<String>,
        /// Source-order ordinal scoped to `(repo_relative_path, symbol_kind, name)`.
        ///
        /// Present on `Symbol` nodes. This is an identity component for symbols,
        /// but `span` is not; see `docs/adr/0004-symbol-identity.md`.
        #[serde(skip_serializing_if = "Option::is_none")]
        disambiguator: Option<u64>,
        // ── Declaration-surface fields (issues #124 / #213 / #257) ────────────
        /// Declaration visibility class for `Symbol` nodes (and Rust `Module`
        /// nodes, issue #213), drawn from the closed set `public` / `crate` /
        /// `restricted` / `private`. Additive per
        /// `docs/schema/schema-versioning.md §2`; never an identity input.
        #[serde(skip_serializing_if = "Option::is_none")]
        visibility: Option<String>,
        /// Normalized declaration header for `Symbol` nodes: item keyword
        /// through the end of the parameter list / return type / where-clause
        /// for callables (or the item header for type-defining items), with the
        /// body excluded and interior whitespace collapsed deterministically.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// Doc-comment text (`///`, `/** */`, or `#[doc = "..."]`) after
        /// passing through redaction policy v1. Present on `Symbol` nodes
        /// (issue #124) and on Rust `Import` nodes when a doc comment sits at
        /// a `pub use` re-export site (issue #257) — consumers and validators
        /// MUST preserve the field on both kinds. Omitted entirely when the
        /// item has no doc comment — never an empty string.
        #[serde(skip_serializing_if = "Option::is_none")]
        doc: Option<String>,
        // ── Panic-risk call-site fields (issue #223) ──────────────────────────
        /// Production-vs-test context class for `PanicRiskSite` nodes, drawn
        /// from the closed set `production` / `test`. Absent on all other node
        /// kinds. Additive per `docs/schema/schema-versioning.md §2`; never an
        /// identity input.
        #[serde(skip_serializing_if = "Option::is_none")]
        call_context: Option<String>,
        // ── Debt-comment marker fields (issue #218) ───────────────────────────
        /// Trimmed single-line note text following the marker token on
        /// `DebtMarker` nodes, after passing through redaction policy v1.
        /// Absent on all other node kinds. Additive per
        /// `docs/schema/schema-versioning.md §2`; never an identity input.
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        // ── Body content signature (issue #206) ───────────────────────────────
        /// Compact BLAKE3 handle over the normalized source body of nodes whose
        /// display `summary` is name-only, so a body change with an unchanged
        /// name/path is still content-detectable by evidence-freshness drift
        /// (issue #206). Present on Rust `Module` and `Import` nodes; absent on
        /// every other kind, whose `summary` already embeds the body. Additive
        /// per `docs/schema/schema-versioning.md §2`; never an identity input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_signature: Option<String>,
        /// Git and bitemporal provenance for history-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        temporal: Option<TemporalMetadata>,
        /// Semantic drift details for drift marker nodes.
        #[serde(skip_serializing_if = "Option::is_none")]
        semantic_drift: Option<Box<SemanticDriftMetadata>>,
        /// Evidence citations for agent-memory nodes.
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence_links: Option<Vec<EvidenceLink>>,
        /// Identity payload for `Repository` nodes; absent on all other kinds.
        #[serde(skip_serializing_if = "Option::is_none")]
        repository_identity: Option<Box<RepositoryIdentityPayload>>,
        /// Source-snapshot identity for `Repository` nodes (issue #82); absent on
        /// all other kinds and on stores produced before snapshot stamping.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_snapshot: Option<Box<SourceSnapshotPayload>>,
        /// Declared-dependency payload for `DependencyDeclaration` nodes
        /// (issue #180); absent on all other kinds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dependency: Option<Box<DependencyDeclarationPayload>>,
        /// Log-signature payload for the four log-domain node kinds
        /// (`LogSource`, `ErrorSignature`, `LogEvent`, `LogOccurrenceBucket`);
        /// absent on all other kinds (issues #319 / #320).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        log: Option<Box<LogPayload>>,
        /// Scan-coverage payload for the single `ScanCoverage` node (issue
        /// #135); absent on all other kinds and on graphs produced before
        /// coverage stamping.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scan_coverage: Option<Box<ScanCoveragePayload>>,
        // ── Agent-memory provenance fields (absent for code-graph nodes) ─────
        /// Observation body text (Observation nodes).
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// ID of the record that supersedes this one.
        #[serde(skip_serializing_if = "Option::is_none")]
        superseded_by: Option<String>,
        /// Stable agent identity (agent-authored nodes).
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// Agent kind enum value (agent-authored nodes).
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_kind: Option<String>,
        /// Agent session ID (agent-authored nodes).
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Wall-clock time the agent observed the fact (RFC 3339).
        #[serde(skip_serializing_if = "Option::is_none")]
        observed_at: Option<String>,
        /// Transaction time when the daemon committed the record (RFC 3339).
        #[serde(skip_serializing_if = "Option::is_none")]
        ingested_at: Option<String>,
        /// Extraction confidence `[0.0, 1.0]` (Observation, Decision, Lesson).
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<String>,
        /// Artifact path or hash the record was extracted from.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_handle: Option<String>,
        /// Redaction policy version when any field passed through redaction.
        #[serde(skip_serializing_if = "Option::is_none")]
        redaction_policy_version: Option<String>,
        /// Display name of the Git author (present on Commit nodes).
        #[serde(skip_serializing_if = "Option::is_none")]
        author_name: Option<String>,
        /// Email address of the Git author (present on Commit nodes).
        #[serde(skip_serializing_if = "Option::is_none")]
        author_email: Option<String>,
        /// RFC 3339 valid time for current-tree (non-history) scan records.
        /// For history-backed records use `temporal.valid_time` instead.
        #[serde(skip_serializing_if = "Option::is_none")]
        valid_time: Option<String>,
        /// Source of the `valid_time` field for current-tree records.
        /// For history-backed records see `temporal.valid_time_source`.
        #[serde(skip_serializing_if = "Option::is_none")]
        valid_time_source: Option<String>,
        // ── Project-domain fields (docs/schema/project-graph.md) ─────────────
        /// Stable entity ID within the project domain across mutations.
        #[serde(skip_serializing_if = "Option::is_none")]
        entity_id: Option<String>,
        /// Task title.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Redacted body handle for task body text.
        #[serde(skip_serializing_if = "Option::is_none")]
        body_handle: Option<Box<OutputHandle>>,
        /// Project task source kind.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_kind: Option<String>,
        /// Record ID of the `ExternalLink` carrying the source-system handle.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_external_link_id: Option<String>,
        /// Opaque assignee identifiers.
        #[serde(skip_serializing_if = "Option::is_none")]
        assignees: Option<Vec<String>>,
        /// Project labels.
        #[serde(skip_serializing_if = "Option::is_none")]
        labels: Option<Vec<String>>,
        /// Project priority enum value.
        #[serde(skip_serializing_if = "Option::is_none")]
        priority: Option<String>,
        /// Owning `Task` record ID for an `AcceptanceCriterion`.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_task_id: Option<String>,
        /// Position within the parent task's AC list.
        #[serde(skip_serializing_if = "Option::is_none")]
        ordinal: Option<u32>,
        /// Optional verification-domain record that closed an AC.
        #[serde(skip_serializing_if = "Option::is_none")]
        verification_link_id: Option<String>,
        // ── GitHub PR-promoted fields (issue #333; consumed by #334/#338) ────
        // Optional first-class flat fields promoted from the redacted body blob
        // so queries/joins/citations can reach them. Set only on PR-derived
        // Tasks (`source_kind = github_pr`); absent on issue Tasks. Plaintext
        // query substrate per `docs/schema/import-github.md` §8; never redacted.
        /// Head (source-branch) commit SHA of a pull request.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        head_sha: Option<String>,
        /// Head (source-branch) ref name of a pull request.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        head_ref: Option<String>,
        /// Base (target-branch) ref name of a pull request.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        base_ref: Option<String>,
        /// Merge commit SHA of a pull request; `Some` only when merged.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        merge_commit_sha: Option<String>,
        /// Merge timestamp (recorded string form); `Some` means merged.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        merged_at: Option<String>,
        /// Draft flag of a pull request.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        draft: Option<bool>,
        /// External system enum value for `ExternalLink`.
        #[serde(skip_serializing_if = "Option::is_none")]
        system: Option<String>,
        /// Canonical URL or `file://` path for `ExternalLink`.
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Source-system-native ID for `ExternalLink`.
        #[serde(skip_serializing_if = "Option::is_none")]
        system_native_id: Option<String>,
        /// VCS remote when applicable.
        #[serde(skip_serializing_if = "Option::is_none")]
        repository_remote: Option<String>,
        /// RFC3339 timestamp when an importer first saw this external handle.
        #[serde(skip_serializing_if = "Option::is_none")]
        discovered_at: Option<String>,
        /// RFC3339 store transaction time for project-domain mutations.
        #[serde(skip_serializing_if = "Option::is_none")]
        transaction_time: Option<String>,
        /// Agent-facing summary.
        summary: String,
        // ── Importer provenance fields (absent for non-imported records) ─────
        /// Domain identifier (`"agent_memory"`, `"codegraph"`).
        #[serde(skip_serializing_if = "Option::is_none")]
        domain: Option<String>,
        /// Importer identity for trajectory-imported records (e.g. `"traj-importer"`).
        #[serde(skip_serializing_if = "Option::is_none")]
        importer_id: Option<String>,
        /// Importer version string (e.g. `"0.1.0"`).
        #[serde(skip_serializing_if = "Option::is_none")]
        importer_version: Option<String>,
        /// Repo-relative or fixture-relative path to the raw source artifact.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_artifact_path: Option<String>,
        /// BLAKE3 hex hash of the raw source artifact bytes.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_artifact_hash: Option<String>,
        // ── Record-type-specific fields (M2 trajectory import) ───────────────
        /// Patch validation status for `PatchArtifact` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        patch_status: Option<String>,
        /// Git SHA the patch was authored against, when known.
        #[serde(skip_serializing_if = "Option::is_none")]
        base_commit: Option<String>,
        /// Reason `base_commit` is absent.
        #[serde(skip_serializing_if = "Option::is_none")]
        unknown_base_reason: Option<String>,
        /// Repo-relative files touched by the patch.
        #[serde(skip_serializing_if = "Option::is_none")]
        target_files: Option<Vec<String>>,
        /// BLAKE3 hash of the raw patch bytes.
        #[serde(skip_serializing_if = "Option::is_none")]
        patch_bytes_hash: Option<String>,
        /// Raw patch byte length.
        #[serde(skip_serializing_if = "Option::is_none")]
        patch_bytes_size: Option<u64>,
        /// Storage handle for raw patch bytes.
        #[serde(skip_serializing_if = "Option::is_none")]
        patch_handle: Option<Box<PatchHandle>>,
        /// Human-readable validation reason, redacted by policy.
        #[serde(skip_serializing_if = "Option::is_none")]
        validation_summary: Option<String>,
        /// `AgentSession` record ID that produced this patch.
        #[serde(skip_serializing_if = "Option::is_none")]
        producer_session_id: Option<String>,
        /// File edit operation kind.
        #[serde(skip_serializing_if = "Option::is_none")]
        edit_kind: Option<String>,
        /// BLAKE3 hash before a file edit.
        #[serde(skip_serializing_if = "Option::is_none")]
        before_hash: Option<String>,
        /// BLAKE3 hash after a file edit.
        #[serde(skip_serializing_if = "Option::is_none")]
        after_hash: Option<String>,
        /// Rename target path for rename edits.
        #[serde(skip_serializing_if = "Option::is_none")]
        rename_to: Option<String>,
        /// Number of hunks in the edit.
        #[serde(skip_serializing_if = "Option::is_none")]
        hunk_count: Option<u32>,
        /// `PatchArtifact` record ID containing this edit's bytes.
        #[serde(skip_serializing_if = "Option::is_none")]
        linked_patch_id: Option<String>,
        /// `AgentTurn` record ID associated with this action.
        #[serde(skip_serializing_if = "Option::is_none")]
        linked_turn_id: Option<String>,
        /// Invoked tool name.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        /// Producer-classified tool behavior kind.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_kind: Option<String>,
        /// Redacted one-line argument summary.
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments_summary: Option<String>,
        /// Raw arguments handle.
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments_handle: Option<Box<OutputHandle>>,
        /// Tool output handle for non-verification output.
        #[serde(skip_serializing_if = "Option::is_none")]
        result_handle: Option<Box<OutputHandle>>,
        /// Verification record produced by this tool call.
        #[serde(skip_serializing_if = "Option::is_none")]
        produced_evidence_id: Option<String>,
        /// RFC 3339 tool start time.
        #[serde(skip_serializing_if = "Option::is_none")]
        started_at: Option<String>,
        /// RFC 3339 tool finish time; absent when interrupted.
        #[serde(skip_serializing_if = "Option::is_none")]
        finished_at: Option<String>,
        /// Failure classification for `Failure` records: `"command_failure"`, `"patch_invalid"`, etc.
        #[serde(skip_serializing_if = "Option::is_none")]
        failure_kind: Option<String>,
        /// Shell exit code for `CommandRun` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i64>,
        /// Zero-based turn index within an `AgentRun` for `AgentTurn` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
        // ── Verification-domain fields (docs/schema/verification.md) ─────────
        /// Standard output handle for verification-domain `CommandRun` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        stdout_handle: Option<Box<OutputHandle>>,
        /// Standard error handle for verification-domain `CommandRun` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        stderr_handle: Option<Box<OutputHandle>>,
        /// Evidence quality enum for verification-domain records:
        /// `verbatim`, `summarized`, or `referenced_only`.
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence_quality: Option<String>,
        /// RFC 3339 timestamp when the evidence was produced (verification domain).
        #[serde(skip_serializing_if = "Option::is_none")]
        executed_at: Option<String>,
        /// Verification kind for `Verification` umbrella records:
        /// `test_run`, `command_run`, `ci_status`, etc.
        #[serde(skip_serializing_if = "Option::is_none")]
        verification_kind: Option<String>,
        /// Pass/fail status for verification-domain records:
        /// `passed`, `failed`, `errored`, `skipped`, or `inconclusive`.
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        // ── GitHub project.Review fields (docs/schema/import-github.md §6) ────
        /// Review kind for `project.Review` records imported from GitHub:
        /// `issue_comment`, `pr_review`, or `pr_review_comment`.
        #[serde(skip_serializing_if = "Option::is_none")]
        review_kind: Option<String>,
        /// PR review summary state for `pr_review` records:
        /// `approved`, `changes_requested`, `commented`, `dismissed`, or `pending`.
        #[serde(skip_serializing_if = "Option::is_none")]
        review_state: Option<String>,
        /// Parent review-comment record ID for threaded `pr_review_comment`
        /// records (`in_reply_to_id` chain); absent on thread roots.
        #[serde(skip_serializing_if = "Option::is_none")]
        in_reply_to_id: Option<String>,
        /// Author login for GitHub-sourced records. Plaintext query substrate
        /// per `docs/schema/import-github.md` §8; never redacted.
        #[serde(skip_serializing_if = "Option::is_none")]
        author: Option<String>,
        /// Redacted diff-hunk handle for `pr_review_comment` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        diff_hunk_handle: Option<Box<OutputHandle>>,
        /// Diff side for `pr_review_comment` records: `LEFT` (old file) or
        /// `RIGHT` (new file). Disambiguates the source location a path/line
        /// pair refers to. Plaintext query substrate; never redacted.
        #[serde(skip_serializing_if = "Option::is_none")]
        review_side: Option<String>,
        /// Commit SHA the review was anchored to for `pr_review` and
        /// `pr_review_comment` records — the GitHub payload's `commit_id`, the
        /// exact commit the reviewer looked at (issue #334). Populated whenever
        /// the payload carries `commit_id`, with no `merged_at`-style gate: a
        /// review commit is a real observed commit, not a throwaway test-merge.
        /// Absent on `issue_comment` reviews (general PR-conversation comments
        /// are not anchored to a commit). Plaintext query substrate per
        /// `docs/schema/import-github.md` §8 (the plaintext-SHA carve-out);
        /// never redacted. Legacy records lacking the field deserialize to
        /// `None`.
        #[serde(skip_serializing_if = "Option::is_none")]
        review_commit_sha: Option<String>,
        /// Source system for an `ExternalIdentity` node (issue #335). Always
        /// `"github"` for importer-minted identities; the identity's `author`
        /// field carries the login. Together `(identity_system, author)` are the
        /// node's stable-identity parts. No email, display name, avatar, or
        /// profile URL is ever stored. Plaintext query substrate per
        /// `docs/schema/import-github.md` §8 (the author-login carve-out); never
        /// redacted. Legacy records lacking the field deserialize to `None`.
        #[serde(skip_serializing_if = "Option::is_none")]
        identity_system: Option<String>,
        /// Transition kind for a `ReviewStateTransition` node (issue #336). One
        /// of the closed set `review_dismissed` / `review_requested` /
        /// `review_request_removed`, sourced from a GitHub PR-timeline event.
        /// A `ReviewStateTransition` is an append-only history record: the
        /// current `review_state` on a `Review` is a last-write-wins summary,
        /// while each state TRANSITION is preserved here so a dismissal never
        /// erases that an approval once existed. Plaintext query substrate (the
        /// event kind is a closed vocabulary, never free text); never redacted.
        /// The actor login rides in `author`; the timeline event id in
        /// `system_native_id`; the dismissal message (when present) is redacted
        /// into `body_handle`. Legacy/other-kind records deserialize to `None`.
        #[serde(skip_serializing_if = "Option::is_none")]
        transition_kind: Option<String>,
        /// User-context domain fields, flattened into node JSON.
        #[serde(flatten)]
        user_context: UserContextFields,
        /// Producer identity envelope. `None` for legacy records written before
        /// this field was introduced; see `docs/schema/producer-version.md`.
        #[serde(skip_serializing_if = "Option::is_none")]
        producer: Option<Producer>,
    },
    /// A graph edge.
    Edge {
        /// Stable record ID.
        id: String,
        /// Schema version that produced the record.
        schema_version: u32,
        /// Edge label.
        label: EdgeLabel,
        /// Source node ID.
        source: String,
        /// Target node ID.
        target: String,
        /// Optional extraction confidence.
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<String>,
        /// Call resolution status (issues #152/#134); present on `CALLS`
        /// edges emitted by the repo-wide resolution pass and on same-file
        /// `CALLS` edges backed by a Tree-sitter call site, absent elsewhere
        /// (absence means "outside the resolution contract", not "resolved").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolution: Option<CallResolution>,
        /// Backtrace-frame resolution status (issue #322); present only on
        /// `FRAME_RESOLVES_TO` edges minted by `eg resolve-frames`, absent
        /// elsewhere (absence means "outside the frame-resolution contract").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame_resolution: Option<FrameResolution>,
        /// Zero-based index of the resolved backtrace frame (issue #322);
        /// present alongside `frame_resolution` on `FRAME_RESOLVES_TO` edges,
        /// absent elsewhere.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame_index: Option<u32>,
        /// Correlation basis (issue #323); present only on `EMITTED_DURING`
        /// edges minted by `eg link-logs`, drawn from the closed
        /// [`CorrelationBasis`] set, absent elsewhere. Every `EMITTED_DURING`
        /// edge carries exactly one basis — none is emitted without one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        basis: Option<CorrelationBasis>,
        /// Git and bitemporal provenance for history-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        temporal: Option<TemporalMetadata>,
        /// Agent-facing summary.
        summary: String,
        /// Producer identity envelope. `None` for legacy records written before
        /// this field was introduced; see `docs/schema/producer-version.md`.
        #[serde(skip_serializing_if = "Option::is_none")]
        producer: Option<Producer>,
    },
    /// A deleted graph entity marker emitted by incremental scans.
    Tombstone {
        /// Stable record ID for the tombstone.
        id: String,
        /// Schema version that produced the record.
        schema_version: u32,
        /// ID of the graph entity that no longer exists.
        deleted_id: String,
        /// Agent-facing summary.
        summary: String,
        /// Producer identity envelope. `None` for legacy records written before
        /// this field was introduced; see `docs/schema/producer-version.md`.
        #[serde(skip_serializing_if = "Option::is_none")]
        producer: Option<Producer>,
    },
}

impl GraphRecord {
    /// Returns the stable record ID.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Node { id, .. } | Self::Edge { id, .. } | Self::Tombstone { id, .. } => id,
        }
    }

    /// Returns the node kind name when this record is a node.
    #[must_use]
    pub const fn node_kind_name(&self) -> Option<&'static str> {
        match self {
            Self::Node { kind, .. } => Some(kind.as_str()),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Returns this record's importer `source_kind` attribution: a node's
    /// `source_kind` (e.g. `github_pr`, `github_issue`), or `None` for a node
    /// with no attribution and for every non-node record.
    ///
    /// This is the single classification arm shared by the daemon's in-batch
    /// `source_kind` resolution and the offline `eg validate` reviewer-identity
    /// parity check (issue #369), so the two can never drift on how one record's
    /// attribution is read.
    #[must_use]
    pub fn source_kind_ref(&self) -> Option<&str> {
        match self {
            Self::Node { source_kind, .. } => source_kind.as_deref(),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Resolves a record ID's importer `source_kind` within one record batch by
    /// last-write-wins over forward order — equivalently, the first match in
    /// reverse order — the EXACT semantics of the daemon's in-batch
    /// `lookup_node_source_kind` reverse scan (`src/daemon.rs`).
    ///
    /// The outer `Option` distinguishes "a record with this id exists in the
    /// batch" (`Some`) from "no record with this id" (`None`, which the daemon
    /// resolves through its store `read_back` fallback). The inner `Option` is
    /// the matched record's [`source_kind_ref`](Self::source_kind_ref): a
    /// trailing record whose attribution is absent (a node with no `source_kind`,
    /// or any non-node record sharing the id) resolves to `Some(None)` and thus
    /// SHADOWS an earlier attribution — matching the daemon exactly (issue #369).
    #[must_use]
    pub fn resolve_source_kind_in_batch<'a>(
        id: &str,
        records: &'a [Self],
    ) -> Option<Option<&'a str>> {
        records
            .iter()
            .rev()
            .find(|record| record.id() == id)
            .map(Self::source_kind_ref)
    }

    /// Returns this record's resolved node kind: a node's [`NodeKind`], or `None`
    /// for every non-node record (an `Edge` or `Tombstone`).
    ///
    /// This is the single classification arm shared by the daemon's in-batch
    /// node-kind resolution (`lookup_node_kind`, `src/daemon.rs`) and the offline
    /// `eg validate` kind gates (issue #391), so the two can never drift on how
    /// one record's kind is read: a non-node record sharing an id resolves to
    /// `None` and thus shadows an earlier node.
    #[must_use]
    pub const fn node_kind_ref(&self) -> Option<NodeKind> {
        match self {
            Self::Node { kind, .. } => Some(*kind),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Resolves a record ID's node kind within one record batch by last-write-wins
    /// over forward order — equivalently, the first match in reverse order — the
    /// EXACT semantics of the daemon's in-batch `lookup_node_kind` reverse scan
    /// (`src/daemon.rs`).
    ///
    /// The outer `Option` distinguishes "a record with this id exists in the
    /// batch" (`Some`) from "no record with this id" (`None`, which the daemon
    /// resolves through its store `read_back` fallback). The inner `Option` is the
    /// matched record's [`node_kind_ref`](Self::node_kind_ref): a trailing non-node
    /// record sharing the id resolves to `Some(None)` and thus SHADOWS an earlier
    /// node kind — matching the daemon exactly (issue #391).
    #[must_use]
    pub fn resolve_node_kind_in_batch(id: &str, records: &[Self]) -> Option<Option<NodeKind>> {
        records
            .iter()
            .rev()
            .find(|record| record.id() == id)
            .map(Self::node_kind_ref)
    }

    /// Creates a graph node record.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub const fn node(
        id: String,
        kind: NodeKind,
        repo_relative_path: Option<String>,
        span: Option<SourceSpan>,
        name: Option<String>,
        summary: String,
    ) -> Self {
        Self::Node {
            id,
            kind,
            schema_version: SCHEMA_VERSION,
            repo_relative_path,
            span,
            name,
            language: None,
            symbol_kind: None,
            disambiguator: None,
            visibility: None,
            signature: None,
            doc: None,
            call_context: None,
            note: None,
            content_signature: None,
            temporal: None,
            semantic_drift: None,
            evidence_links: None,
            repository_identity: None,
            source_snapshot: None,
            text: None,
            superseded_by: None,
            agent_id: None,
            agent_kind: None,
            session_id: None,
            observed_at: None,
            ingested_at: None,
            confidence: None,
            source_handle: None,
            redaction_policy_version: None,
            author_name: None,
            author_email: None,
            valid_time: None,
            valid_time_source: None,
            entity_id: None,
            title: None,
            body_handle: None,
            source_kind: None,
            source_external_link_id: None,
            assignees: None,
            labels: None,
            priority: None,
            parent_task_id: None,
            ordinal: None,
            verification_link_id: None,
            head_sha: None,
            head_ref: None,
            base_ref: None,
            merge_commit_sha: None,
            merged_at: None,
            draft: None,
            system: None,
            url: None,
            system_native_id: None,
            repository_remote: None,
            discovered_at: None,
            transaction_time: None,
            summary,
            domain: None,
            importer_id: None,
            importer_version: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            patch_status: None,
            base_commit: None,
            unknown_base_reason: None,
            target_files: None,
            patch_bytes_hash: None,
            patch_bytes_size: None,
            patch_handle: None,
            validation_summary: None,
            producer_session_id: None,
            edit_kind: None,
            before_hash: None,
            after_hash: None,
            rename_to: None,
            hunk_count: None,
            linked_patch_id: None,
            linked_turn_id: None,
            tool_name: None,
            tool_kind: None,
            arguments_summary: None,
            arguments_handle: None,
            result_handle: None,
            produced_evidence_id: None,
            started_at: None,
            finished_at: None,
            failure_kind: None,
            exit_code: None,
            turn_index: None,
            stdout_handle: None,
            stderr_handle: None,
            evidence_quality: None,
            executed_at: None,
            verification_kind: None,
            status: None,
            review_kind: None,
            review_state: None,
            in_reply_to_id: None,
            author: None,
            diff_hunk_handle: None,
            review_side: None,
            review_commit_sha: None,
            identity_system: None,
            transition_kind: None,
            dependency: None,
            log: None,
            scan_coverage: None,
            user_context: UserContextFields::empty(),
            producer: None,
        }
    }

    /// Creates a syntax-backed node record with language metadata.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn syntax_node(
        id: String,
        kind: NodeKind,
        repo_relative_path: String,
        span: SourceSpan,
        name: String,
        language: &str,
        summary: String,
    ) -> Self {
        Self::Node {
            id,
            kind,
            schema_version: SCHEMA_VERSION,
            repo_relative_path: Some(repo_relative_path),
            span: Some(span),
            name: Some(name),
            language: Some(language.to_owned()),
            symbol_kind: None,
            disambiguator: None,
            visibility: None,
            signature: None,
            doc: None,
            call_context: None,
            note: None,
            content_signature: None,
            temporal: None,
            semantic_drift: None,
            evidence_links: None,
            repository_identity: None,
            source_snapshot: None,
            text: None,
            superseded_by: None,
            agent_id: None,
            agent_kind: None,
            session_id: None,
            observed_at: None,
            ingested_at: None,
            confidence: None,
            source_handle: None,
            redaction_policy_version: None,
            author_name: None,
            author_email: None,
            valid_time: None,
            valid_time_source: None,
            entity_id: None,
            title: None,
            body_handle: None,
            source_kind: None,
            source_external_link_id: None,
            assignees: None,
            labels: None,
            priority: None,
            parent_task_id: None,
            ordinal: None,
            verification_link_id: None,
            head_sha: None,
            head_ref: None,
            base_ref: None,
            merge_commit_sha: None,
            merged_at: None,
            draft: None,
            system: None,
            url: None,
            system_native_id: None,
            repository_remote: None,
            discovered_at: None,
            transaction_time: None,
            summary,
            domain: None,
            importer_id: None,
            importer_version: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            patch_status: None,
            base_commit: None,
            unknown_base_reason: None,
            target_files: None,
            patch_bytes_hash: None,
            patch_bytes_size: None,
            patch_handle: None,
            validation_summary: None,
            producer_session_id: None,
            edit_kind: None,
            before_hash: None,
            after_hash: None,
            rename_to: None,
            hunk_count: None,
            linked_patch_id: None,
            linked_turn_id: None,
            tool_name: None,
            tool_kind: None,
            arguments_summary: None,
            arguments_handle: None,
            result_handle: None,
            produced_evidence_id: None,
            started_at: None,
            finished_at: None,
            failure_kind: None,
            exit_code: None,
            turn_index: None,
            stdout_handle: None,
            stderr_handle: None,
            evidence_quality: None,
            executed_at: None,
            verification_kind: None,
            status: None,
            review_kind: None,
            review_state: None,
            in_reply_to_id: None,
            author: None,
            diff_hunk_handle: None,
            review_side: None,
            review_commit_sha: None,
            identity_system: None,
            transition_kind: None,
            dependency: None,
            log: None,
            scan_coverage: None,
            user_context: UserContextFields::empty(),
            producer: None,
        }
    }

    /// Creates a syntax-backed symbol record.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn symbol(
        id: String,
        symbol_kind: &str,
        repo_relative_path: String,
        span: SourceSpan,
        name: String,
        summary: String,
    ) -> Self {
        Self::Node {
            id,
            kind: NodeKind::Symbol,
            schema_version: SCHEMA_VERSION,
            repo_relative_path: Some(repo_relative_path),
            span: Some(span),
            name: Some(name),
            language: Some("rust".to_owned()),
            symbol_kind: Some(symbol_kind.to_owned()),
            disambiguator: Some(0),
            visibility: None,
            signature: None,
            doc: None,
            call_context: None,
            note: None,
            content_signature: None,
            temporal: None,
            semantic_drift: None,
            evidence_links: None,
            repository_identity: None,
            source_snapshot: None,
            text: None,
            superseded_by: None,
            agent_id: None,
            agent_kind: None,
            session_id: None,
            observed_at: None,
            ingested_at: None,
            confidence: None,
            source_handle: None,
            redaction_policy_version: None,
            author_name: None,
            author_email: None,
            valid_time: None,
            valid_time_source: None,
            entity_id: None,
            title: None,
            body_handle: None,
            source_kind: None,
            source_external_link_id: None,
            assignees: None,
            labels: None,
            priority: None,
            parent_task_id: None,
            ordinal: None,
            verification_link_id: None,
            head_sha: None,
            head_ref: None,
            base_ref: None,
            merge_commit_sha: None,
            merged_at: None,
            draft: None,
            system: None,
            url: None,
            system_native_id: None,
            repository_remote: None,
            discovered_at: None,
            transaction_time: None,
            summary,
            domain: None,
            importer_id: None,
            importer_version: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            patch_status: None,
            base_commit: None,
            unknown_base_reason: None,
            target_files: None,
            patch_bytes_hash: None,
            patch_bytes_size: None,
            patch_handle: None,
            validation_summary: None,
            producer_session_id: None,
            edit_kind: None,
            before_hash: None,
            after_hash: None,
            rename_to: None,
            hunk_count: None,
            linked_patch_id: None,
            linked_turn_id: None,
            tool_name: None,
            tool_kind: None,
            arguments_summary: None,
            arguments_handle: None,
            result_handle: None,
            produced_evidence_id: None,
            started_at: None,
            finished_at: None,
            failure_kind: None,
            exit_code: None,
            turn_index: None,
            stdout_handle: None,
            stderr_handle: None,
            evidence_quality: None,
            executed_at: None,
            verification_kind: None,
            status: None,
            review_kind: None,
            review_state: None,
            in_reply_to_id: None,
            author: None,
            diff_hunk_handle: None,
            review_side: None,
            review_commit_sha: None,
            identity_system: None,
            transition_kind: None,
            dependency: None,
            log: None,
            scan_coverage: None,
            user_context: UserContextFields::empty(),
            producer: None,
        }
    }

    /// Creates a syntax-backed symbol record for any language.
    ///
    /// Prefer this over [`Self::symbol`] when the language and source-order
    /// disambiguator are known at construction time.
    #[must_use]
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn syntax_symbol(
        id: String,
        symbol_kind: &str,
        repo_relative_path: String,
        span: SourceSpan,
        name: String,
        language: &str,
        disambiguator: u64,
        summary: String,
    ) -> Self {
        Self::Node {
            id,
            kind: NodeKind::Symbol,
            schema_version: SCHEMA_VERSION,
            repo_relative_path: Some(repo_relative_path),
            span: Some(span),
            name: Some(name),
            language: Some(language.to_owned()),
            symbol_kind: Some(symbol_kind.to_owned()),
            disambiguator: Some(disambiguator),
            visibility: None,
            signature: None,
            doc: None,
            call_context: None,
            note: None,
            content_signature: None,
            temporal: None,
            semantic_drift: None,
            evidence_links: None,
            repository_identity: None,
            source_snapshot: None,
            text: None,
            superseded_by: None,
            agent_id: None,
            agent_kind: None,
            session_id: None,
            observed_at: None,
            ingested_at: None,
            confidence: None,
            source_handle: None,
            redaction_policy_version: None,
            author_name: None,
            author_email: None,
            valid_time: None,
            valid_time_source: None,
            entity_id: None,
            title: None,
            body_handle: None,
            source_kind: None,
            source_external_link_id: None,
            assignees: None,
            labels: None,
            priority: None,
            parent_task_id: None,
            ordinal: None,
            verification_link_id: None,
            head_sha: None,
            head_ref: None,
            base_ref: None,
            merge_commit_sha: None,
            merged_at: None,
            draft: None,
            system: None,
            url: None,
            system_native_id: None,
            repository_remote: None,
            discovered_at: None,
            transaction_time: None,
            summary,
            domain: None,
            importer_id: None,
            importer_version: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            patch_status: None,
            base_commit: None,
            unknown_base_reason: None,
            target_files: None,
            patch_bytes_hash: None,
            patch_bytes_size: None,
            patch_handle: None,
            validation_summary: None,
            producer_session_id: None,
            edit_kind: None,
            before_hash: None,
            after_hash: None,
            rename_to: None,
            hunk_count: None,
            linked_patch_id: None,
            linked_turn_id: None,
            tool_name: None,
            tool_kind: None,
            arguments_summary: None,
            arguments_handle: None,
            result_handle: None,
            produced_evidence_id: None,
            started_at: None,
            finished_at: None,
            failure_kind: None,
            exit_code: None,
            turn_index: None,
            stdout_handle: None,
            stderr_handle: None,
            evidence_quality: None,
            executed_at: None,
            verification_kind: None,
            status: None,
            review_kind: None,
            review_state: None,
            in_reply_to_id: None,
            author: None,
            diff_hunk_handle: None,
            review_side: None,
            review_commit_sha: None,
            identity_system: None,
            transition_kind: None,
            dependency: None,
            log: None,
            scan_coverage: None,
            user_context: UserContextFields::empty(),
            producer: None,
        }
    }

    /// Creates an agent-memory graph edge with the `agent_memory:v1:` ID prefix.
    #[must_use]
    pub fn agent_memory_edge(
        label: EdgeLabel,
        source: String,
        target: String,
        confidence: Option<String>,
        summary: String,
    ) -> Self {
        let id = agent_memory_stable_id(&["edge", label.as_str(), &source, &target]);
        Self::Edge {
            id,
            schema_version: AGENT_MEMORY_SCHEMA_VERSION,
            label,
            source,
            target,
            confidence,
            resolution: None,
            frame_resolution: None,
            frame_index: None,
            basis: None,
            temporal: None,
            summary,
            producer: None,
        }
    }

    /// Creates a graph edge record.
    #[must_use]
    pub fn edge(
        label: EdgeLabel,
        source: String,
        target: String,
        confidence: Option<String>,
        summary: String,
    ) -> Self {
        let label_text = label.as_str();
        let id = stable_id(&["edge", label_text, &source, &target]);
        Self::Edge {
            id,
            schema_version: SCHEMA_VERSION,
            label,
            source,
            target,
            confidence,
            resolution: None,
            frame_resolution: None,
            frame_index: None,
            basis: None,
            temporal: None,
            summary,
            producer: None,
        }
    }

    /// Creates a project-domain graph edge record.
    ///
    /// Unlike [`GraphRecord::edge`], which stamps a `codegraph:v{SCHEMA_VERSION}`
    /// ID and the code-graph schema version, this mints a `project:v1:` ID and
    /// [`PROJECT_SCHEMA_VERSION`] so the edge serializes under the project domain.
    /// Project-graph consumers (which filter on the `project:v1:` prefix) and the
    /// daemon project-edge validator only see edges that carry this identity. The
    /// ID is derived solely from `(label, source, target)`, matching the daemon's
    /// synthesized project-edge ID scheme, so it stays byte-identical across runs.
    #[must_use]
    pub fn project_edge(
        label: EdgeLabel,
        source: String,
        target: String,
        confidence: Option<String>,
        summary: String,
    ) -> Self {
        let id = project_stable_id(&["project", "edge", label.as_str(), &source, &target]);
        Self::Edge {
            id,
            schema_version: PROJECT_SCHEMA_VERSION,
            label,
            source,
            target,
            confidence,
            resolution: None,
            frame_resolution: None,
            frame_index: None,
            basis: None,
            temporal: None,
            summary,
            producer: None,
        }
    }

    /// Attaches a cross-file call resolution status to an edge record (issue #152).
    ///
    /// No-op on node and tombstone records.
    #[must_use]
    pub const fn with_resolution(mut self, call_resolution: CallResolution) -> Self {
        if let Self::Edge { resolution, .. } = &mut self {
            *resolution = Some(call_resolution);
        }
        self
    }

    /// Returns the cross-file call resolution status when this record is an
    /// edge carrying one; `None` otherwise (issue #152).
    #[must_use]
    pub const fn resolution(&self) -> Option<CallResolution> {
        match self {
            Self::Edge { resolution, .. } => *resolution,
            Self::Node { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Attaches a backtrace-frame resolution status to an edge record
    /// (issue #322). No-op on node and tombstone records.
    #[must_use]
    pub const fn with_frame_resolution(mut self, resolution: FrameResolution) -> Self {
        if let Self::Edge {
            frame_resolution, ..
        } = &mut self
        {
            *frame_resolution = Some(resolution);
        }
        self
    }

    /// Attaches the zero-based backtrace frame index to an edge record
    /// (issue #322). No-op on node and tombstone records.
    #[must_use]
    pub const fn with_frame_index(mut self, frame_index: u32) -> Self {
        if let Self::Edge {
            frame_index: fi, ..
        } = &mut self
        {
            *fi = Some(frame_index);
        }
        self
    }

    /// Attaches a correlation basis to an edge record (issue #323). No-op on
    /// node and tombstone records.
    #[must_use]
    pub const fn with_basis(mut self, correlation_basis: CorrelationBasis) -> Self {
        if let Self::Edge { basis, .. } = &mut self {
            *basis = Some(correlation_basis);
        }
        self
    }

    /// Returns the correlation basis when this record is an edge carrying one;
    /// `None` otherwise (issue #323).
    #[must_use]
    pub const fn basis(&self) -> Option<CorrelationBasis> {
        match self {
            Self::Edge { basis, .. } => *basis,
            Self::Node { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Returns the backtrace-frame resolution status when this record is an
    /// edge carrying one; `None` otherwise (issue #322).
    #[must_use]
    pub const fn frame_resolution(&self) -> Option<FrameResolution> {
        match self {
            Self::Edge {
                frame_resolution, ..
            } => *frame_resolution,
            Self::Node { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Returns the zero-based backtrace frame index when this record is an edge
    /// carrying one; `None` otherwise (issue #322).
    #[must_use]
    pub const fn frame_index(&self) -> Option<u32> {
        match self {
            Self::Edge { frame_index, .. } => *frame_index,
            Self::Node { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Attaches Git and bitemporal provenance to a node or edge record.
    #[must_use]
    pub fn with_temporal(mut self, temporal_metadata: TemporalMetadata) -> Self {
        match &mut self {
            Self::Node { temporal, .. } | Self::Edge { temporal, .. } => {
                *temporal = Some(temporal_metadata);
            }
            Self::Tombstone { .. } => {}
        }
        self
    }

    /// Attaches author name and email to a node record.
    #[must_use]
    pub fn with_author(mut self, name: Option<String>, email: Option<String>) -> Self {
        if let Self::Node {
            author_name,
            author_email,
            ..
        } = &mut self
        {
            *author_name = name;
            *author_email = email;
        }
        self
    }

    /// Attaches declaration-surface metadata to a node record (issue #124):
    /// visibility class, normalized signature header, and redacted
    /// doc-comment text. Used by `Symbol` nodes for the full surface, and by
    /// Rust `Import` nodes to carry the doc comment written at a `pub use`
    /// re-export site (issue #257; doc only — visibility and signature stay
    /// absent there).
    ///
    /// The fields are additive per `docs/schema/schema-versioning.md §2` and
    /// MUST NOT contribute to stable ID composition. No-op on non-node records.
    #[must_use]
    pub fn with_declaration_surface(
        mut self,
        symbol_visibility: Option<String>,
        symbol_signature: Option<String>,
        symbol_doc: Option<String>,
    ) -> Self {
        if let Self::Node {
            visibility,
            signature,
            doc,
            ..
        } = &mut self
        {
            *visibility = symbol_visibility;
            *signature = symbol_signature;
            *doc = symbol_doc;
        }
        self
    }

    /// Stamps the trimmed single-line note text on a `DebtMarker` node record
    /// (issue #218). The value is additive metadata per
    /// `docs/schema/schema-versioning.md §2` and MUST NOT contribute to
    /// stable ID composition. No-op on non-node records.
    #[must_use]
    pub fn with_note(mut self, marker_note: &str) -> Self {
        if let Self::Node { note, .. } = &mut self {
            *note = Some(marker_note.to_owned());
        }
        self
    }

    /// Returns the debt-marker note text when present.
    #[must_use]
    pub fn note(&self) -> Option<&str> {
        match self {
            Self::Node { note, .. } => note.as_deref(),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Stamps the compact body content signature on a node record (issue #206).
    /// Used for `Module` nodes whose display `summary` is name-only and whose
    /// stable ID is keyed on the qualified name only, so an unchanged-name body
    /// edit keeps the ID and is still content-detectable by evidence-freshness
    /// drift. (`Import` nodes do NOT use this — their stable ID already encodes
    /// the full `use ...;` declaration, so a body change mints a new ID and
    /// surfaces as a handle-identity change, never a content drift.) The value
    /// is additive metadata per `docs/schema/schema-versioning.md §2` and MUST
    /// NOT contribute to stable ID composition. No-op on non-node records.
    #[must_use]
    pub fn with_content_signature(mut self, signature: impl Into<String>) -> Self {
        if let Self::Node {
            content_signature, ..
        } = &mut self
        {
            *content_signature = Some(signature.into());
        }
        self
    }

    /// Returns the body content signature when present (issue #206).
    #[must_use]
    pub fn content_signature(&self) -> Option<&str> {
        match self {
            Self::Node {
                content_signature, ..
            } => content_signature.as_deref(),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Attaches semantic drift metadata to a node record.
    #[must_use]
    pub fn with_semantic_drift(mut self, drift: SemanticDriftMetadata) -> Self {
        if let Self::Node { semantic_drift, .. } = &mut self {
            *semantic_drift = Some(Box::new(drift));
        }
        self
    }

    /// Stamps the production-vs-test context class on a `PanicRiskSite` node
    /// record (issue #223). The value is drawn from the closed set
    /// `production` / `test`; it is additive metadata and MUST NOT contribute
    /// to stable ID composition. No-op on non-node records.
    #[must_use]
    pub fn with_call_context(mut self, context: &str) -> Self {
        if let Self::Node { call_context, .. } = &mut self {
            *call_context = Some(context.to_owned());
        }
        self
    }

    /// Returns the panic-risk call-site context class when present.
    #[must_use]
    pub fn call_context(&self) -> Option<&str> {
        match self {
            Self::Node { call_context, .. } => call_context.as_deref(),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Sets an explicit domain and schema version on a node record.
    #[must_use]
    pub fn with_domain(mut self, domain_name: &str, schema: u32) -> Self {
        if let Self::Node {
            domain,
            schema_version,
            ..
        } = &mut self
        {
            *domain = Some(domain_name.to_owned());
            *schema_version = schema;
        }
        self
    }

    /// Attaches repository identity payload to a `Repository` node.
    #[must_use]
    pub fn with_repository_identity(mut self, payload: RepositoryIdentityPayload) -> Self {
        if let Self::Node {
            repository_identity,
            ..
        } = &mut self
        {
            *repository_identity = Some(Box::new(payload));
        }
        self
    }

    /// Stamps a store-level [`SourceSnapshotPayload`] on a `Repository` node (issue #82).
    ///
    /// No-op on non-node records. The snapshot records the HEAD commit, dirty
    /// flag, repository identity, and scan time so readers can classify store
    /// freshness against the live working tree.
    #[must_use]
    pub fn with_source_snapshot(mut self, snapshot: SourceSnapshotPayload) -> Self {
        if let Self::Node {
            source_snapshot, ..
        } = &mut self
        {
            *source_snapshot = Some(Box::new(snapshot));
        }
        self
    }

    /// Stamps a [`DependencyDeclarationPayload`] on a `DependencyDeclaration`
    /// node (issue #180). No-op on non-node records.
    #[must_use]
    pub fn with_dependency(mut self, payload: DependencyDeclarationPayload) -> Self {
        if let Self::Node { dependency, .. } = &mut self {
            *dependency = Some(Box::new(payload));
        }
        self
    }

    /// Returns the declared-dependency payload when this record is a
    /// `DependencyDeclaration` node carrying one; `None` otherwise (issue #180).
    #[must_use]
    pub fn dependency(&self) -> Option<&DependencyDeclarationPayload> {
        match self {
            Self::Node { dependency, .. } => dependency.as_deref(),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Stamps a [`LogPayload`] on a log-domain node (issues #319 / #320).
    /// No-op on non-node records.
    #[must_use]
    pub fn with_log(mut self, payload: LogPayload) -> Self {
        if let Self::Node { log, .. } = &mut self {
            *log = Some(Box::new(payload));
        }
        self
    }

    /// Stamps a [`ScanCoveragePayload`] on the `ScanCoverage` node (issue #135).
    /// No-op on non-node records.
    #[must_use]
    pub fn with_scan_coverage(mut self, payload: ScanCoveragePayload) -> Self {
        if let Self::Node { scan_coverage, .. } = &mut self {
            *scan_coverage = Some(Box::new(payload));
        }
        self
    }

    /// Returns the scan-coverage payload when this record is a `ScanCoverage`
    /// node carrying one; `None` otherwise (issue #135).
    #[must_use]
    pub fn scan_coverage(&self) -> Option<&ScanCoveragePayload> {
        match self {
            Self::Node { scan_coverage, .. } => scan_coverage.as_deref(),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Sets the evidence-link citation list on a node record. No-op on non-node
    /// records. An empty list clears the field back to `None`.
    #[must_use]
    pub fn with_evidence_links(mut self, links: Vec<EvidenceLink>) -> Self {
        if let Self::Node { evidence_links, .. } = &mut self {
            *evidence_links = if links.is_empty() { None } else { Some(links) };
        }
        self
    }

    /// Returns the evidence-link citation list when this record is a node
    /// carrying one; `None` otherwise.
    #[must_use]
    pub fn evidence_links(&self) -> Option<&[EvidenceLink]> {
        match self {
            Self::Node { evidence_links, .. } => evidence_links.as_deref(),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Returns the log-signature payload when this record is a log-domain node
    /// carrying one; `None` otherwise (issues #319 / #320).
    #[must_use]
    pub fn log_payload(&self) -> Option<&LogPayload> {
        match self {
            Self::Node { log, .. } => log.as_deref(),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Returns the source-snapshot payload when this record is a `Repository` node
    /// carrying one; `None` otherwise (issue #82).
    #[must_use]
    pub fn source_snapshot(&self) -> Option<&SourceSnapshotPayload> {
        match self {
            Self::Node {
                source_snapshot, ..
            } => source_snapshot.as_deref(),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Stamps an explicit `valid_time` and `valid_time_source` on a node record
    /// without touching `ingested_at`. No-op on non-node records.
    ///
    /// Used by the log-signature extractor (issues #319 / #320) to record a
    /// parsed event timestamp (`valid_time_source = "log_event_timestamp"`) or
    /// the transaction-time fallback for a timestamp-less line.
    #[must_use]
    pub fn with_valid_time(
        mut self,
        node_valid_time: impl Into<String>,
        node_valid_time_source: impl Into<String>,
    ) -> Self {
        if let Self::Node {
            valid_time,
            valid_time_source,
            ..
        } = &mut self
        {
            *valid_time = Some(node_valid_time.into());
            *valid_time_source = Some(node_valid_time_source.into());
        }
        self
    }

    /// Stamps inferred `valid_time` and `valid_time_source` on current-tree scan records.
    ///
    /// Used by `scan_repository_at` to satisfy the rule from
    /// `docs/schema/temporal-selectors.md`: when no commit anchors the record,
    /// set `valid_time` = `transaction_time` and `valid_time_source` =
    /// `"inferred_from_transaction_time"`.
    #[must_use]
    pub fn with_valid_time_inferred(mut self, transaction_time: &str) -> Self {
        if let Self::Node {
            valid_time,
            valid_time_source,
            ..
        } = &mut self
        {
            *valid_time = Some(transaction_time.to_owned());
            *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        }
        self
    }

    /// Stamps explicit base time fields on a node record.
    #[must_use]
    pub fn with_node_time(
        mut self,
        node_valid_time: impl Into<String>,
        node_valid_time_source: impl Into<String>,
        node_ingested_at: impl Into<String>,
    ) -> Self {
        if let Self::Node {
            valid_time,
            valid_time_source,
            ingested_at,
            ..
        } = &mut self
        {
            *valid_time = Some(node_valid_time.into());
            *valid_time_source = Some(node_valid_time_source.into());
            *ingested_at = Some(node_ingested_at.into());
        }
        self
    }

    /// Stamps an explicit store `transaction_time` (RFC 3339) on a node record.
    ///
    /// The transaction-time axis records *when the store first committed* the
    /// fact, independent of `valid_time` (when the fact was true). Used by
    /// transaction-time queries (`--tx-as-of` / `as_of.transaction_time`) per
    /// `docs/schema/temporal-selectors.md`.
    #[must_use]
    pub fn with_transaction_time(mut self, tx_time: impl Into<String>) -> Self {
        if let Self::Node {
            transaction_time, ..
        } = &mut self
        {
            *transaction_time = Some(tx_time.into());
        }
        self
    }

    /// Marks a node record as superseded by another record (sets `superseded_by`).
    #[must_use]
    pub fn with_superseded_by(mut self, target_id: impl Into<String>) -> Self {
        if let Self::Node { superseded_by, .. } = &mut self {
            *superseded_by = Some(target_id.into());
        }
        self
    }

    /// Stamps the redaction policy version on a node record.
    ///
    /// Called by importers after applying the v1 redaction policy; see
    /// `docs/schema/redaction.md` and `crate::redaction::REDACTION_POLICY_VERSION`.
    #[must_use]
    pub fn with_redaction_policy_version(mut self, policy_version: impl Into<String>) -> Self {
        if let Self::Node {
            redaction_policy_version,
            ..
        } = &mut self
        {
            *redaction_policy_version = Some(policy_version.into());
        }
        self
    }

    /// Stamps the producer identity envelope on this record.
    ///
    /// The `producer` field is a non-identity envelope: it MUST NOT contribute
    /// to any stable ID composition. See `docs/schema/producer-version.md`.
    #[must_use]
    pub fn with_producer(mut self, producer: Producer) -> Self {
        match &mut self {
            Self::Node { producer: p, .. }
            | Self::Edge { producer: p, .. }
            | Self::Tombstone { producer: p, .. } => {
                *p = Some(producer);
            }
        }
        self
    }

    /// Returns a clone with `producer.producer_started_at` set to an empty string.
    ///
    /// Use for store idempotency comparisons: version fields (`egregore_version`,
    /// `producer_components`) are still compared so an extractor upgrade triggers
    /// a rewrite of `producer_json`, while the per-run wall-clock timestamp is
    /// excluded to avoid rewriting unchanged facts on every invocation.
    #[must_use]
    pub fn with_cleared_producer_started_at(&self) -> Self {
        let mut cloned = self.clone();
        match &mut cloned {
            Self::Node { producer: p, .. }
            | Self::Edge { producer: p, .. }
            | Self::Tombstone { producer: p, .. } => {
                if let Some(prod) = p {
                    prod.producer_started_at = String::new();
                }
            }
        }
        cloned
    }

    /// Returns the producer identity envelope if present.
    ///
    /// `None` indicates a legacy record written before the producer envelope was
    /// introduced; see `docs/schema/producer-version.md §Legacy-Record Policy`.
    #[must_use]
    pub const fn producer(&self) -> Option<&Producer> {
        match self {
            Self::Node { producer, .. }
            | Self::Edge { producer, .. }
            | Self::Tombstone { producer, .. } => producer.as_ref(),
        }
    }
}

/// Git and bitemporal provenance attached to history-backed records.
#[derive(Debug, Clone, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub struct TemporalMetadata {
    /// Git commit SHA that supplied the valid-time source tree.
    pub git_commit: String,
    /// Parent commit SHAs observed for this commit.
    pub git_parent_commits: Vec<String>,
    /// Valid time for this record, derived from Git committer time.
    pub valid_time: String,
    /// Git author timestamp when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_time: Option<String>,
    /// Observation timestamp for the replay artifact.
    pub observed_at: String,
    /// Source of the `valid_time` field. See `docs/schema/temporal-selectors.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_time_source: Option<String>,
}

/// Reserved graph domains.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    /// Source-derived code facts.
    CodeGraph,
    /// Agent-authored observations and session state.
    AgentMemory,
    /// Runtime, test, CI, benchmark, and proof evidence.
    Verification,
    /// Durable generated or external artifacts.
    Artifact,
    /// Product, project, task, and acceptance-criterion state.
    Project,
    /// Semantic measurements that require source bytes plus model bytes.
    Semantic,
    /// Authorization-derived operator preference and workflow-policy records.
    UserContext,
    /// Runtime log-signature observations (issues #319 / #320).
    Log,
}

impl Domain {
    /// Returns the serialized domain name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CodeGraph => "codegraph",
            Self::AgentMemory => "agent_memory",
            Self::Verification => "verification",
            Self::Artifact => "artifact",
            Self::Project => "project",
            Self::Semantic => "semantic",
            Self::UserContext => "user_context",
            Self::Log => "log",
        }
    }
}

/// Structured identity for an embedding model.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingModel {
    /// Provider boundary that supplied the model.
    pub provider: String,
    /// Model name from the provider registry.
    pub name: String,
    /// Provider or crate version pin.
    pub version: String,
    /// Dense vector dimensionality.
    pub dim: u32,
    /// BLAKE3 hash of model weights, or `unknown` when unavailable.
    pub content_hash: String,
}

/// Semantic distance metric used by a drift measurement.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// One minus cosine similarity.
    CosineDistance,
    /// Reserved Euclidean distance metric.
    L2Distance,
    /// Reserved learned semantic delta scorer.
    LearnedDeltaV1,
}

impl MetricKind {
    /// Returns the serialized metric kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CosineDistance => "cosine_distance",
            Self::L2Distance => "l2_distance",
            Self::LearnedDeltaV1 => "learned_delta_v1",
        }
    }
}

/// Selection policy that caused a drift record to be emitted.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionBasis {
    /// Emit every drift whose score is greater than or equal to the threshold.
    ThresholdOnly,
    /// Reserved policy: top K drifts per compared pair.
    TopKPerPair,
    /// Reserved policy: top K drifts per symbol.
    TopKPerSymbol,
}

impl SelectionBasis {
    /// Returns the serialized selection basis.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ThresholdOnly => "threshold_only",
            Self::TopKPerPair => "top_k_per_pair",
            Self::TopKPerSymbol => "top_k_per_symbol",
        }
    }
}

/// Structured metadata for a semantic drift measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticDriftMetadata {
    /// Structured embedding model identity.
    pub embedding_model: EmbeddingModel,
    /// Stable graph record ID for the later drift target.
    pub target_record_id: String,
    /// Stable graph record ID for the prior drift target.
    pub prior_record_id: String,
    /// Commit SHA for the earlier embedding.
    pub before_git_commit: String,
    /// Commit SHA for the later embedding.
    pub after_git_commit: String,
    /// Valid time for the earlier embedding.
    pub before_valid_time: String,
    /// Valid time for the later embedding.
    pub after_valid_time: String,
    /// Semantic distance metric.
    pub metric_kind: MetricKind,
    /// Drift score as a JSON number.
    pub score: f64,
    /// Producer threshold that selected this record.
    pub selection_threshold: f64,
    /// Producer selection policy.
    pub selection_basis: SelectionBasis,
}

impl Eq for SemanticDriftMetadata {}

/// Initial graph node kinds.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum NodeKind {
    /// Indexed repository root.
    Repository,
    /// Source file.
    File,
    /// Language module namespace.
    Module,
    /// Language symbol.
    Symbol,
    /// Import or use declaration.
    Import,
    /// Extractor warning or unsupported construct.
    Diagnostic,
    /// Deterministic `.unwrap()` / `.expect()` panic-risk method-call site
    /// (issue #223). The `name` field carries the closed category (`unwrap`
    /// or `expect`) and `call_context` carries the production-vs-test class.
    PanicRiskSite,
    /// Deterministic human-authored debt-comment marker (issue #218): a
    /// `TODO` / `FIXME` / `HACK` / `XXX` token inside a Tree-sitter comment
    /// node. The `name` field carries the closed lowercase category and
    /// `note` carries the trimmed single-line note text.
    DebtMarker,
    /// Deterministic `unsafe`-surface site (issue #222): an `unsafe { .. }`
    /// block expression, an `unsafe fn` declaration, or an `unsafe impl`
    /// block. The `name` field carries the closed site kind
    /// (`block` / `fn` / `impl`).
    UnsafeSite,
    /// Directly-declared Cargo manifest dependency (issue #180).
    DependencyDeclaration,
    /// File-level scan-coverage summary (issue #135): one node per full
    /// `eg scan`, carrying a [`ScanCoveragePayload`] (files walked/indexed and a
    /// per-extension skip tally). Attached to its `Repository` by a `CONTAINS`
    /// edge so coverage is citable and never an orphan.
    ScanCoverage,
    /// Git commit observed during history replay.
    Commit,
    /// File-level change observed in a commit.
    Change,
    /// Semantic movement for a file or symbol over time.
    SemanticDrift,
    /// Reserved per-model registry record in the semantic domain.
    EmbeddingModel,
    /// Reserved persisted embedding vector record in the semantic domain.
    EmbeddingVector,
    /// Agent process or human actor writing observations.
    Agent,
    /// One agent run or conversation session.
    AgentSession,
    /// Agent-authored memory or discovery.
    Observation,
    /// Project-domain work item.
    Task,
    /// Falsifiable requirement attached to a task.
    AcceptanceCriterion,
    /// Source-system handle for project-domain records.
    ExternalLink,
    /// Long-lived product/repository initiative (project domain, reserved).
    Product,
    /// Bounded area of work under a product (project domain, reserved).
    Project,
    /// Strategy, milestone, or implementation plan (project domain, reserved).
    Plan,
    /// GitHub-specific issue metadata (project domain, reserved).
    GitHubIssue,
    /// GitHub pull-request metadata (project domain, reserved).
    PR,
    /// Review comment, finding, approval, or requested change (project domain, reserved).
    Review,
    /// A source-system participant identity — a GitHub login (issue #335).
    /// Project domain, trust class `project_state`. One node per distinct
    /// `(system, login)` pair, keyed on those two parts alone (deliberately
    /// NOT repo-scoped: identities are global across repositories). Carries
    /// ONLY the login (in `author`) and `identity_system` — never email,
    /// display name, avatar, or profile URL. Consumed by #338/#339;
    /// beneficiaries #245/#262.
    ExternalIdentity,
    /// One append-only review-state TRANSITION event (issue #336). Project
    /// domain, trust class `project_state`. Minted from a GitHub PR-timeline
    /// event (`review_dismissed` / `review_requested` / `review_request_removed`)
    /// and keyed on that event's server-native id, so it never participates in
    /// the parent `Review`'s identity. The `Review.review_state` field is a
    /// last-write-wins current-state SUMMARY; these transitions are the HISTORY,
    /// so a dismissal recorded here never erases that an approval once existed.
    /// A `review_dismissed` transition carries a `TRANSITIONS_REVIEW` edge to the
    /// dismissed `Review`; `review_requested` / `review_request_removed`
    /// transitions stand alone. Consumers needing "state as of T" join
    /// transitions rather than reading the summary field.
    ReviewStateTransition,
    /// Local project/task JSONL work item (project domain, reserved).
    LocalTask,
    /// File, patch, report, or generated output linked to work.
    Artifact,
    /// Evidence for a claim, test, or check.
    Verification,
    /// Command output or terminal evidence.
    CommandEvidence,
    // ── M2 trajectory-importer node kinds (docs/schema/agent-memory.md §4) ───
    /// A bounded attempt to complete a task (M2 trajectory import).
    AgentRun,
    /// One turn within an agent run: assistant action + observation (M2).
    AgentTurn,
    /// Structured tool invocation and result handle (M2).
    ToolCall,
    /// Shell command with exit status and output handle (M2).
    CommandRun,
    /// File path, diff handle, and edit provenance (M2).
    FileEdit,
    /// Patch content, validation status, and source trajectory (M2).
    PatchArtifact,
    /// Failed command, invalid patch, or blocked workflow (M2).
    Failure,
    /// Durable decision inferred from explicit context (reserved, agent-memory §4a).
    Decision,
    // ── Verification-domain node kinds (docs/schema/verification.md) ─────────
    /// One test command invocation that ran one or more tests (verification domain).
    TestRun,
    /// CI/CD run status — reserved for M9-adjacent project-graph integration.
    CIStatus,
    /// Criterion/cargo-bench/perf benchmark result — reserved.
    BenchmarkRun,
    /// Coverage tool output with per-file/per-line coverage data — reserved.
    CoverageReport,
    /// Verus/Kani/CBMC/Lean/Coq proof outcome — reserved.
    ProofResult,
    // ── User-context node kinds (docs/schema/user-context.md) ────────────────
    /// Proposal to promote repeated evidence into a durable user-context rule.
    PromoteCandidate,
    /// Append-only prompt shown to an operator for a promotion candidate.
    PromotionPrompt,
    /// Append-only operator decision on a promotion prompt.
    PromotionDecision,
    /// Approved durable preference.
    Preference,
    /// Approved durable workflow rule.
    WorkflowRule,
    /// Approved durable naming convention.
    NamingDecision,
    /// Approved durable constraint.
    Constraint,
    // ── M3 Codex importer node kinds (docs/schema/agent-memory.md) ────────────
    /// Token and cost accounting record for an agent turn (M3 Codex import).
    CostUsage,
    // ── Retraction node kind (issue #231) ─────────────────────────────────────
    /// Auditable operator retraction event for a persisted record. Records who
    /// retracted the target, when (transaction time), the reason, and the prior
    /// record handle, so logical retraction never leaves a silent hole.
    Retraction,
    // ── Log-signature node kinds (docs/schema/log-graph.md, issues #319/#320) ─
    /// A captured log source artifact (one scanned log file). Carries the
    /// artifact hash, format, and line count in its `log` payload.
    LogSource,
    /// A deduplicated error fingerprint. One `ErrorSignature` per distinct
    /// `template-v1` normalization; carries the redacted template excerpt,
    /// severity, and total occurrence count.
    ErrorSignature,
    /// A bounded exemplar occurrence of a signature (capped per signature/source).
    LogEvent,
    /// An hourly occurrence-count bucket aggregating a signature over time.
    LogOccurrenceBucket,
}

impl NodeKind {
    /// Returns the serialized node kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Repository => "Repository",
            Self::File => "File",
            Self::Module => "Module",
            Self::Symbol => "Symbol",
            Self::Import => "Import",
            Self::Diagnostic => "Diagnostic",
            Self::PanicRiskSite => "PanicRiskSite",
            Self::DebtMarker => "DebtMarker",
            Self::UnsafeSite => "UnsafeSite",
            Self::DependencyDeclaration => "DependencyDeclaration",
            Self::ScanCoverage => "ScanCoverage",
            Self::Commit => "Commit",
            Self::Change => "Change",
            Self::SemanticDrift => "SemanticDrift",
            Self::EmbeddingModel => "EmbeddingModel",
            Self::EmbeddingVector => "EmbeddingVector",
            Self::Agent => "Agent",
            Self::AgentSession => "AgentSession",
            Self::Observation => "Observation",
            Self::Task => "Task",
            Self::AcceptanceCriterion => "AcceptanceCriterion",
            Self::ExternalLink => "ExternalLink",
            Self::Product => "Product",
            Self::Project => "Project",
            Self::Plan => "Plan",
            Self::GitHubIssue => "GitHubIssue",
            Self::PR => "PR",
            Self::Review => "Review",
            Self::ExternalIdentity => "ExternalIdentity",
            Self::ReviewStateTransition => "ReviewStateTransition",
            Self::LocalTask => "LocalTask",
            Self::Artifact => "Artifact",
            Self::Verification => "Verification",
            Self::CommandEvidence => "CommandEvidence",
            Self::AgentRun => "AgentRun",
            Self::AgentTurn => "AgentTurn",
            Self::ToolCall => "ToolCall",
            Self::CommandRun => "CommandRun",
            Self::FileEdit => "FileEdit",
            Self::PatchArtifact => "PatchArtifact",
            Self::Failure => "Failure",
            Self::Decision => "Decision",
            Self::TestRun => "TestRun",
            Self::CIStatus => "CIStatus",
            Self::BenchmarkRun => "BenchmarkRun",
            Self::CoverageReport => "CoverageReport",
            Self::ProofResult => "ProofResult",
            Self::PromoteCandidate => "PromoteCandidate",
            Self::PromotionPrompt => "PromotionPrompt",
            Self::PromotionDecision => "PromotionDecision",
            Self::Preference => "Preference",
            Self::WorkflowRule => "WorkflowRule",
            Self::NamingDecision => "NamingDecision",
            Self::Constraint => "Constraint",
            Self::CostUsage => "CostUsage",
            Self::Retraction => "Retraction",
            Self::LogSource => "LogSource",
            Self::ErrorSignature => "ErrorSignature",
            Self::LogEvent => "LogEvent",
            Self::LogOccurrenceBucket => "LogOccurrenceBucket",
        }
    }
}

/// Initial graph edge labels.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeLabel {
    /// Hierarchical ownership.
    Contains,
    /// Definition ownership.
    Defines,
    /// Import declaration ownership.
    Imports,
    /// Best-effort syntactic reference.
    References,
    /// Best-effort call relationship.
    Calls,
    /// Syntactically resolvable implementation relationship.
    Implements,
    /// Weaker unresolved mention relationship.
    Mentions,
    /// Entity changed in a Git commit.
    ChangedIn,
    /// Git commit ancestry.
    ParentOf,
    /// Semantic drift measurement target.
    DriftsFrom,
    /// Semantic drift prior-version target.
    DriftsPrior,
    /// Semantic drift measurement model edge.
    MeasuredBy,
    /// Agent session belongs to an agent.
    SessionOf,
    /// Entity was authored by an agent session.
    AuthoredBy,
    /// Entity has supporting evidence.
    HasEvidence,
    // ── Cross-domain edge registry (docs/schema/agent-memory.md) ─────────────
    /// Agent-memory node observes a code-graph entity.
    Observes,
    /// Agent-memory node mentions a specific symbol.
    MentionsSymbol,
    /// Agent-memory node cites a file that was touched.
    TouchedFile,
    /// Agent-memory node produced a patch artifact.
    ProducedPatch,
    /// Agent-memory tool call produced verification evidence.
    ProducedEvidence,
    /// Agent-memory node is validated by an evidence record.
    ValidatedBy,
    /// Project acceptance criterion is closed by verification evidence.
    ClosesAcceptanceCriterion,
    /// Project acceptance criterion belongs to a task.
    OwnedByTask,
    /// Project record points to its external source handle.
    ExternalHandle,
    /// Project task intends to touch a code-graph file.
    TouchesFile,
    /// Project PR `Task` was merged as a specific code-graph `Commit`
    /// (issue #333; consumed by #334/#338). FROM `project.Task` TO
    /// `codegraph.Commit`; emitted only when a seeded code graph resolves the
    /// PR's `merge_commit_sha` to exactly one `Commit`.
    MergedAs,
    /// Project `Review` was anchored to a specific code-graph `Commit`
    /// (issue #334; the review-side mirror of [`Self::MergedAs`]). FROM
    /// `project.Review` TO `codegraph.Commit`; emitted only when a seeded code
    /// graph resolves the review's `review_commit_sha` (the GitHub payload's
    /// `commit_id`, the exact commit the reviewer looked at) to exactly one
    /// `Commit`. "Anchored at this SHA" is never "approved all changes in a
    /// range": it names the tree the review observed, not a verdict on it.
    ReviewsCommit,
    /// Project `Review` was authored by a source-system `ExternalIdentity`
    /// (issue #335). FROM `project.Review` TO `project.ExternalIdentity`;
    /// emitted for every `Review` whose payload carries an author login. A
    /// binding names who wrote the review, never a verdict on its content.
    ReviewedBy,
    /// Project PR `Task` requested review from a source-system
    /// `ExternalIdentity` (issue #335). FROM `project.Task` TO
    /// `project.ExternalIdentity`; one edge per requested-reviewer login on the
    /// PR. A request is an invitation to review, never proof a review happened.
    RequestedReviewFrom,
    /// A `ReviewStateTransition` transitions a `Review` (issue #336). FROM
    /// `project.ReviewStateTransition` TO `project.Review`; emitted only for a
    /// `review_dismissed` timeline event (which names the dismissed review). The
    /// edge names WHICH review a transition acted on — it never re-writes the
    /// review's current `review_state` summary and never asserts the transition
    /// was correct. `review_requested` / `review_request_removed` transitions
    /// name no review and mint no edge.
    TransitionsReview,
    /// Agent-memory node describes a failure on a code entity.
    FailedOn,
    /// Agent-memory node explains a code change.
    ExplainsChange,
    /// Agent-memory node references a task record.
    ReferencesTask,
    /// Agent-memory node contradicts another record.
    Contradicts,
    /// Agent-memory node supersedes another record.
    Supersedes,
    /// User-context candidate was proposed by an agent-memory observation.
    ProposedBy,
    /// Prompt was issued for a promotion candidate.
    PromptedFor,
    /// Decision was made on a promotion candidate.
    DecidedOn,
    /// Approval materialized a durable user-context record.
    MaterializedAs,
    /// Durable user-context record was revoked by a decision.
    RevokedBy,
    /// Durable user-context record is scoped to a repository.
    ScopedToRepo,
    /// Generic weak relationship between any two records.
    RelatesTo,
    // ── Log-signature edge labels (docs/schema/log-graph.md, issues #319/#320) ─
    /// A `LogEvent` exemplar is fingerprinted as an `ErrorSignature` (structural).
    FingerprintedAs,
    /// A `LogEvent` / `ErrorSignature` was captured from a `LogSource` (structural).
    CapturedFrom,
    /// A `LogOccurrenceBucket` aggregates an `ErrorSignature` (structural).
    Aggregates,
    /// A log backtrace frame resolves to a code-graph `Symbol` (evidence link,
    /// reserved for #322/#323; declared here as schema groundwork, not emitted
    /// by `scan-logs`).
    FrameResolvesTo,
    /// An `ErrorSignature` was emitted during a verification/agent run (evidence
    /// link, reserved for #322/#323; declared here, not emitted by `scan-logs`).
    EmittedDuring,
}

impl EdgeLabel {
    /// Parses an edge label from its wire string.  Returns `None` for unknown labels.
    #[must_use]
    pub fn from_relation(s: &str) -> Option<Self> {
        match s {
            "CONTAINS" => Some(Self::Contains),
            "DEFINES" => Some(Self::Defines),
            "IMPORTS" => Some(Self::Imports),
            "REFERENCES" => Some(Self::References),
            "CALLS" => Some(Self::Calls),
            "IMPLEMENTS" => Some(Self::Implements),
            "MENTIONS" => Some(Self::Mentions),
            "CHANGED_IN" => Some(Self::ChangedIn),
            "PARENT_OF" => Some(Self::ParentOf),
            "DRIFTS_FROM" => Some(Self::DriftsFrom),
            "DRIFTS_PRIOR" => Some(Self::DriftsPrior),
            "MEASURED_BY" => Some(Self::MeasuredBy),
            "SESSION_OF" => Some(Self::SessionOf),
            "AUTHORED_BY" => Some(Self::AuthoredBy),
            "HAS_EVIDENCE" => Some(Self::HasEvidence),
            "OBSERVES" => Some(Self::Observes),
            "MENTIONS_SYMBOL" => Some(Self::MentionsSymbol),
            "TOUCHED_FILE" => Some(Self::TouchedFile),
            "PRODUCED_PATCH" => Some(Self::ProducedPatch),
            "PRODUCED_EVIDENCE" => Some(Self::ProducedEvidence),
            "VALIDATED_BY" => Some(Self::ValidatedBy),
            "CLOSES_ACCEPTANCE_CRITERION" => Some(Self::ClosesAcceptanceCriterion),
            "OWNED_BY_TASK" => Some(Self::OwnedByTask),
            "EXTERNAL_HANDLE" => Some(Self::ExternalHandle),
            "TOUCHES_FILE" => Some(Self::TouchesFile),
            "MERGED_AS" => Some(Self::MergedAs),
            "REVIEWS_COMMIT" => Some(Self::ReviewsCommit),
            "REVIEWED_BY" => Some(Self::ReviewedBy),
            "REQUESTED_REVIEW_FROM" => Some(Self::RequestedReviewFrom),
            "TRANSITIONS_REVIEW" => Some(Self::TransitionsReview),
            "FAILED_ON" => Some(Self::FailedOn),
            "EXPLAINS_CHANGE" => Some(Self::ExplainsChange),
            "REFERENCES_TASK" => Some(Self::ReferencesTask),
            "CONTRADICTS" => Some(Self::Contradicts),
            "SUPERSEDES" => Some(Self::Supersedes),
            "PROPOSED_BY" => Some(Self::ProposedBy),
            "PROMPTED_FOR" => Some(Self::PromptedFor),
            "DECIDED_ON" => Some(Self::DecidedOn),
            "MATERIALIZED_AS" => Some(Self::MaterializedAs),
            "REVOKED_BY" => Some(Self::RevokedBy),
            "SCOPED_TO_REPO" => Some(Self::ScopedToRepo),
            "RELATES_TO" => Some(Self::RelatesTo),
            "FINGERPRINTED_AS" => Some(Self::FingerprintedAs),
            "CAPTURED_FROM" => Some(Self::CapturedFrom),
            "AGGREGATES" => Some(Self::Aggregates),
            "FRAME_RESOLVES_TO" => Some(Self::FrameResolvesTo),
            "EMITTED_DURING" => Some(Self::EmittedDuring),
            _ => None,
        }
    }

    /// Returns `true` when this label is permitted in an evidence link citation.
    ///
    /// Code-graph-internal labels (`CONTAINS`, `DEFINES`, `CALLS`, etc.) are
    /// reserved for the extractor and must not appear in evidence links.
    #[must_use]
    pub const fn is_evidence_link_label(self) -> bool {
        matches!(
            self,
            Self::HasEvidence
                | Self::Observes
                | Self::MentionsSymbol
                | Self::TouchedFile
                | Self::ProducedPatch
                | Self::ProducedEvidence
                | Self::ValidatedBy
                | Self::ClosesAcceptanceCriterion
                | Self::OwnedByTask
                | Self::ExternalHandle
                | Self::TouchesFile
                | Self::MergedAs
                | Self::ReviewsCommit
                | Self::ReviewedBy
                | Self::RequestedReviewFrom
                | Self::TransitionsReview
                | Self::FailedOn
                | Self::ExplainsChange
                | Self::ReferencesTask
                | Self::Contradicts
                | Self::Supersedes
                | Self::RelatesTo
                | Self::FrameResolvesTo
                | Self::EmittedDuring
        )
    }

    /// Returns `true` when this label belongs to the codegraph topology set and
    /// must not appear on agent-memory (`agent_memory:v1:`) edge records.
    ///
    /// Agent-memory structural labels (`SESSION_OF`, `AUTHORED_BY`) and the full
    /// evidence-link registry are permitted; only extractor-specific topology labels
    /// such as `CONTAINS`, `CALLS`, `DEFINES`, etc. are rejected.
    #[must_use]
    pub const fn is_codegraph_topology_label(self) -> bool {
        matches!(
            self,
            Self::Contains
                | Self::Defines
                | Self::Imports
                | Self::References
                | Self::Calls
                | Self::Implements
                | Self::Mentions
                | Self::ChangedIn
                | Self::ParentOf
                | Self::DriftsFrom
                | Self::DriftsPrior
                | Self::MeasuredBy
        )
    }

    /// Returns the serialized edge label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "CONTAINS",
            Self::Defines => "DEFINES",
            Self::Imports => "IMPORTS",
            Self::References => "REFERENCES",
            Self::Calls => "CALLS",
            Self::Implements => "IMPLEMENTS",
            Self::Mentions => "MENTIONS",
            Self::ChangedIn => "CHANGED_IN",
            Self::ParentOf => "PARENT_OF",
            Self::DriftsFrom => "DRIFTS_FROM",
            Self::DriftsPrior => "DRIFTS_PRIOR",
            Self::MeasuredBy => "MEASURED_BY",
            Self::SessionOf => "SESSION_OF",
            Self::AuthoredBy => "AUTHORED_BY",
            Self::HasEvidence => "HAS_EVIDENCE",
            Self::Observes => "OBSERVES",
            Self::MentionsSymbol => "MENTIONS_SYMBOL",
            Self::TouchedFile => "TOUCHED_FILE",
            Self::ProducedPatch => "PRODUCED_PATCH",
            Self::ProducedEvidence => "PRODUCED_EVIDENCE",
            Self::ValidatedBy => "VALIDATED_BY",
            Self::ClosesAcceptanceCriterion => "CLOSES_ACCEPTANCE_CRITERION",
            Self::OwnedByTask => "OWNED_BY_TASK",
            Self::ExternalHandle => "EXTERNAL_HANDLE",
            Self::TouchesFile => "TOUCHES_FILE",
            Self::MergedAs => "MERGED_AS",
            Self::ReviewsCommit => "REVIEWS_COMMIT",
            Self::ReviewedBy => "REVIEWED_BY",
            Self::RequestedReviewFrom => "REQUESTED_REVIEW_FROM",
            Self::TransitionsReview => "TRANSITIONS_REVIEW",
            Self::FailedOn => "FAILED_ON",
            Self::ExplainsChange => "EXPLAINS_CHANGE",
            Self::ReferencesTask => "REFERENCES_TASK",
            Self::Contradicts => "CONTRADICTS",
            Self::Supersedes => "SUPERSEDES",
            Self::ProposedBy => "PROPOSED_BY",
            Self::PromptedFor => "PROMPTED_FOR",
            Self::DecidedOn => "DECIDED_ON",
            Self::MaterializedAs => "MATERIALIZED_AS",
            Self::RevokedBy => "REVOKED_BY",
            Self::ScopedToRepo => "SCOPED_TO_REPO",
            Self::RelatesTo => "RELATES_TO",
            Self::FingerprintedAs => "FINGERPRINTED_AS",
            Self::CapturedFrom => "CAPTURED_FROM",
            Self::Aggregates => "AGGREGATES",
            Self::FrameResolvesTo => "FRAME_RESOLVES_TO",
            Self::EmittedDuring => "EMITTED_DURING",
        }
    }
}

/// Source byte and line span for syntax-backed records.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct SourceSpan {
    /// Start byte, inclusive.
    pub start_byte: usize,
    /// End byte, exclusive.
    pub end_byte: usize,
    /// One-based start line.
    pub start_line: usize,
    /// One-based end line.
    pub end_line: usize,
}

/// Builds a stable code-graph ID from semantic, repo-relative inputs.
#[must_use]
pub fn stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!("codegraph:v{SCHEMA_VERSION}:{}", hasher.finalize().to_hex())
}

/// Builds a code-graph ID using an explicit schema version rather than the current one.
///
/// Used when tombstoning records that were produced by an older version of the extractor;
/// the deleted ID must match the prefix that was in use when the record was first written.
#[must_use]
pub(crate) fn versioned_stable_id(version: u32, parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!("codegraph:v{version}:{}", hasher.finalize().to_hex())
}

/// Parses the schema version and hash suffix from a versioned codegraph ID.
///
/// Expected format: `codegraph:v<version>:<suffix>`.
#[must_use]
pub fn parse_codegraph_id(id: &str) -> Option<(u32, &str)> {
    if !id.starts_with("codegraph:v") {
        return None;
    }
    let rest = &id["codegraph:v".len()..];
    let colon_idx = rest.find(':')?;
    let version_str = &rest[..colon_idx];
    let version = version_str.parse::<u32>().ok()?;
    let suffix = &rest[colon_idx + 1..];
    Some((version, suffix))
}

/// Builds a stable verification-domain record ID.
///
/// Uses the `verification:v1:` prefix so verification IDs cannot collide with
/// code-graph or agent-memory IDs. Documented in `docs/schema/verification.md`.
#[must_use]
pub fn verification_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.to_ascii_lowercase().as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "verification:v{VERIFICATION_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

/// Builds a stable artifact-domain record ID.
///
/// Uses the `artifact:v1:` prefix so artifact IDs cannot collide with
/// code-graph, agent-memory, or verification IDs. Documented in
/// `docs/schema/agent-actions.md`.
#[must_use]
pub fn artifact_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "artifact:v{ARTIFACT_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

/// Builds a stable project-domain record ID.
///
/// Uses the `project:v1:` prefix so project IDs cannot collide with code-graph,
/// agent-memory, artifact, or verification IDs. Documented in
/// `docs/schema/project-graph.md`.
#[must_use]
pub fn project_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "project:v{PROJECT_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

/// Builds a stable semantic-domain record ID.
///
/// Uses the `semantic:v1:` prefix so semantic records cannot collide with
/// source-derived code-graph IDs. Documented in `docs/schema/semantic-drift.md`.
#[must_use]
pub fn semantic_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "semantic:v{SEMANTIC_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

/// Builds a stable user-context record ID.
///
/// Uses the `user_context:v1:` prefix so authorization-derived records cannot
/// collide with code, memory, verification, artifact, project, or semantic IDs.
/// Documented in `docs/schema/user-context.md`.
#[must_use]
pub fn user_context_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "user_context:v{USER_CONTEXT_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

/// Builds a stable log-signature record ID (issues #319 / #320).
///
/// Uses the `log:v<N>:` prefix (the schema version is [`LOG_SCHEMA_VERSION`]) so
/// runtime log-signature records cannot collide with code, memory, verification,
/// artifact, project, semantic, or user-context IDs. Parts are hashed verbatim (no lowercasing) so log content
/// identity is preserved exactly; the producer envelope and its version fields
/// are never identity inputs, so two binary versions over identical input mint
/// identical IDs. Documented in `docs/schema/log-graph.md`.
#[must_use]
pub fn log_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!("log:v{LOG_SCHEMA_VERSION}:{}", hasher.finalize().to_hex())
}

/// Strips a version-agnostic `log:v<N>:` stable-ID prefix (see [`log_stable_id`]),
/// returning the hex tail (which may be a partial prefix for prefix-matching).
///
/// Accepts ANY positive integer schema version so both superseded `log:v1:`
/// handles and current `log:v2:` handles (issue #361 bumped the log schema) both
/// resolve. Returns `None` for a non-`log` domain or a malformed version segment;
/// the hex tail is returned verbatim without hex-digit validation, so callers that
/// need it (e.g. citation auditing) apply their own tail checks.
#[must_use]
pub fn strip_log_id_prefix(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("log:v")?;
    let (version, hex) = rest.split_once(':')?;
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(hex)
}

/// Builds a stable agent-memory record ID.
///
/// Uses the `agent_memory:v1:` prefix so agent-memory IDs cannot collide with
/// code-graph `codegraph:v2:` IDs even when the content hashes are identical.
/// Documented in docs/schema/agent-memory.md.
#[must_use]
pub fn agent_memory_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "agent_memory:v{AGENT_MEMORY_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

#[cfg(test)]
mod strip_prefix_tests {
    use super::strip_log_id_prefix;

    #[test]
    fn strip_log_id_prefix_is_version_agnostic() {
        // Superseded v1/v2 and current v3 (issues #362/#364) prefixes all
        // resolve, returning the hex tail unchanged so exact-ID and prefix
        // resolution work across schema bumps.
        assert_eq!(strip_log_id_prefix("log:v1:deadbeef"), Some("deadbeef"));
        assert_eq!(strip_log_id_prefix("log:v2:deadbeef"), Some("deadbeef"));
        assert_eq!(strip_log_id_prefix("log:v3:deadbeef"), Some("deadbeef"));
        // Multi-digit versions are accepted (future-proof).
        assert_eq!(strip_log_id_prefix("log:v10:abc"), Some("abc"));
        // A partial hex tail (prefix-resolution needle) round-trips.
        assert_eq!(strip_log_id_prefix("log:v3:dead"), Some("dead"));
    }

    #[test]
    fn strip_log_id_prefix_rejects_non_log_and_malformed() {
        assert_eq!(strip_log_id_prefix("codegraph:v5:abc"), None);
        assert_eq!(strip_log_id_prefix("log:abc"), None);
        assert_eq!(strip_log_id_prefix("log:v:abc"), None);
        assert_eq!(strip_log_id_prefix("log:vx:abc"), None);
        assert_eq!(strip_log_id_prefix("some_symbol"), None);
    }
}
