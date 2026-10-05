//! Capture a local Verus proof run as verification-domain graph records.
//!
//! `eg capture-proof` EXECUTES a local verifier binary (unlike its capture-only
//! siblings) and records the outcome as two verification-domain nodes:
//!
//! - a `CommandRun` node holding the exact argv, exit code, redacted
//!   stdout/stderr handles, and wall-clock `started_at`/`finished_at` (the
//!   latter two are documented volatile metadata, excluded from canonical
//!   comparison);
//! - a `ProofResult` node holding the normalized proof summary in its
//!   `stdout_handle`, with an `evidence_links` `HAS_EVIDENCE` citation pointing
//!   at the `CommandRun` — the proof claim is always backed by the captured
//!   verifier command whose exit status and output support it.
//!
//! Pre-execution failures (missing/non-executable verifier binary, unsupported
//! verifier version, missing/stale/ambiguous proof target) emit a stable
//! diagnostic-only record and exit 2 — never a `ProofResult`.
//!
//! Issue #69. See `docs/cli/capture-proof.md`.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::ir::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, OutputHandle, Producer, ProducerKind,
    TemporalMetadata, VERIFICATION_SCHEMA_VERSION, verification_stable_id,
};
use crate::redaction::{detect_secret_span, redact_span};

// ── Stable diagnostic codes ─────────────────────────────────────────────────
// Every pre-execution and verifier-error condition maps to one of these codes.
// They are part of the CLI contract: scripts match on them, so they never
// change meaning.

/// The verifier binary does not exist.
pub const VERIFIER_BINARY_MISSING_CODE: &str = "verifier_binary_missing";
/// The verifier binary exists but is not executable.
pub const VERIFIER_BINARY_NOT_EXECUTABLE_CODE: &str = "verifier_binary_not_executable";
/// `verus --version` produced no recognizable version token.
pub const UNSUPPORTED_VERIFIER_VERSION_CODE: &str = "unsupported_verifier_version";
/// The proof target path does not exist.
pub const PROOF_TARGET_MISSING_CODE: &str = "proof_target_missing";
/// The proof target is a dangling symlink (stale reference).
pub const PROOF_TARGET_STALE_CODE: &str = "proof_target_stale";
/// The proof target is a directory (ambiguous: which file?).
pub const PROOF_TARGET_AMBIGUOUS_CODE: &str = "proof_target_ambiguous";
/// Clean summary but a nonzero exit code — the verifier disagrees with itself.
pub const VERUS_EXIT_CODE_MISMATCH_CODE: &str = "verus_exit_code_mismatch";
/// A `verification results::` line exists but its counts do not parse.
pub const MALFORMED_VERUS_OUTPUT_CODE: &str = "malformed_verus_output";
/// No recognizable Verus output shape at all.
pub const UNRECOGNIZED_VERUS_OUTPUT_CODE: &str = "unrecognized_verus_output";

/// Inline ceiling for verifier output, mirroring
/// `docs/schema/verification.md` (`OutputHandle` 16 KiB inline ceiling).
pub const INLINE_OUTPUT_CEILING: u64 = 16 * 1024;

/// Tag stamped into the normalized-summary JSON in `stdout_handle`.
pub const PROOF_SUMMARY_FORMAT: &str = "egregore/proof-summary#1";

/// v1 redaction policy version stamped on every emitted record.
pub const REDACTION_POLICY_VERSION: &str = "v1";

// ── Configuration ───────────────────────────────────────────────────────────

/// Everything `capture_proof` needs. All owned: the CLI builds one from its
/// flags; tests build one from fixtures.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Path to the local `verus` binary (never installed or fetched for you).
    pub verus_bin: PathBuf,
    /// Extra arguments passed to the verifier before the target.
    pub verus_args: Vec<String>,
    /// Proof target file to verify.
    pub target: PathBuf,
    /// Display form of the target (used in stable IDs and summaries).
    pub target_display: String,
    /// Operator session id, e.g. an agent session or CI run id.
    pub session_id: String,
    /// Commit SHA the target is evaluated at.
    pub commit: String,
    /// Caller-supplied execution timestamp (RFC 3339). This is what makes the
    /// capture deterministic: the producer stamps this, never a wall clock.
    pub executed_at: String,
    /// Verifier name for the normalized summary (`"verus"`).
    pub verifier_name: String,
    /// Skip the `--version` probe and use this version string instead.
    pub verifier_version_override: Option<String>,
    /// Repository identity label, e.g. from `--repo`.
    pub repo: Option<String>,
    /// Wall-clock budget for the verifier run.
    pub timeout: Duration,
    /// Wall-clock budget for the `--version` probe.
    pub probe_timeout: Duration,
}

// ── Proof target ────────────────────────────────────────────────────────────

/// A resolved proof target: raw bytes plus their BLAKE3 hash.
#[derive(Debug, Clone)]
pub struct ProofTarget {
    /// Raw target bytes (hashed, never persisted unredacted to the graph).
    pub bytes: Vec<u8>,
    /// BLAKE3 hex of the target bytes.
    pub hash: String,
}

/// Why a proof target could not be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofTargetError {
    /// The path does not exist.
    Missing,
    /// The path is a dangling symlink.
    Stale,
    /// The path is a directory (or otherwise not a single file).
    Ambiguous,
}

impl ProofTargetError {
    /// Stable machine-readable diagnostic code for this failure.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Missing => PROOF_TARGET_MISSING_CODE,
            Self::Stale => PROOF_TARGET_STALE_CODE,
            Self::Ambiguous => PROOF_TARGET_AMBIGUOUS_CODE,
        }
    }

    fn message(self, display: &str) -> String {
        match self {
            Self::Missing => format!("proof target does not exist: {display}"),
            Self::Stale => format!("proof target is a dangling symlink: {display}"),
            Self::Ambiguous => format!("proof target is a directory, not a file: {display}"),
        }
    }
}

/// Resolves the proof target to bytes + hash.
///
/// Directories are ambiguous (which file?), dangling symlinks are stale —
/// both are pre-execution failures, not proof results.
///
/// # Errors
///
/// Returns [`ProofTargetError::Missing`] when the path does not exist,
/// [`ProofTargetError::Stale`] for a dangling symlink, and
/// [`ProofTargetError::Ambiguous`] for a directory.
pub fn resolve_proof_target(path: &Path, _display: &str) -> Result<ProofTarget, ProofTargetError> {
    let link_meta = std::fs::symlink_metadata(path).map_err(|_| ProofTargetError::Missing)?;
    if link_meta.file_type().is_symlink() && !path.exists() {
        return Err(ProofTargetError::Stale);
    }
    // Follow links for the kind check: a symlink to a directory is ambiguous.
    let meta = std::fs::metadata(path).map_err(|_| ProofTargetError::Stale)?;
    if meta.is_dir() {
        return Err(ProofTargetError::Ambiguous);
    }
    if !meta.is_file() {
        return Err(ProofTargetError::Ambiguous);
    }
    let bytes = std::fs::read(path).map_err(|_| ProofTargetError::Stale)?;
    Ok(ProofTarget {
        hash: blake3::hash(&bytes).to_hex().to_string(),
        bytes,
    })
}

// ── Verifier binary ─────────────────────────────────────────────────────────

/// Why a verifier binary was refused before execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifierBinaryError {
    /// The binary does not exist.
    Missing,
    /// The binary exists but is not executable.
    NotExecutable,
}

impl VerifierBinaryError {
    /// Stable machine-readable diagnostic code for this failure.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Missing => VERIFIER_BINARY_MISSING_CODE,
            Self::NotExecutable => VERIFIER_BINARY_NOT_EXECUTABLE_CODE,
        }
    }
}

/// Validates the verifier binary exists and is executable.
///
/// Refusing a non-executable file matters: silently failing to exec would
/// otherwise be misread as a verifier error instead of an operator error.
///
/// # Errors
///
/// Returns [`VerifierBinaryError::Missing`] when the path does not exist and
/// [`VerifierBinaryError::NotExecutable`] when it is not executable.
pub fn validate_verifier_binary(path: &Path) -> Result<(), VerifierBinaryError> {
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::metadata(path).map_err(|_| VerifierBinaryError::Missing)?;
    if !meta.is_file() {
        return Err(VerifierBinaryError::Missing);
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(VerifierBinaryError::NotExecutable);
    }
    Ok(())
}

// ── Version probe ───────────────────────────────────────────────────────────

/// Why the verifier version could not be established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionProbeError {
    /// The binary vanished between validation and the probe.
    Missing,
    /// The probe could not be spawned or timed out.
    SpawnFailed,
    /// `--version` output carried no recognizable version token.
    Unrecognized,
}

impl VersionProbeError {
    /// Stable machine-readable diagnostic code for this failure.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Missing => VERIFIER_BINARY_MISSING_CODE,
            Self::SpawnFailed | Self::Unrecognized => UNSUPPORTED_VERIFIER_VERSION_CODE,
        }
    }
}

/// Runs `<verus> --version` and extracts a `major.minor[.patch]` token.
///
/// Anything unrecognizable is `Unrecognized`: the capture must not claim a
/// proof came from a verifier it cannot name.
///
/// # Errors
///
/// Returns [`VersionProbeError`] when the binary cannot be spawned, the probe
/// times out, or the version output is unrecognizable.
pub fn probe_verifier_version(bin: &Path, timeout: Duration) -> Result<String, VersionProbeError> {
    let run =
        run_verifier(bin, &["--version".to_owned()], timeout).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => VersionProbeError::Missing,
            _ => VersionProbeError::SpawnFailed,
        })?;
    if run.timed_out {
        return Err(VersionProbeError::SpawnFailed);
    }
    let text = String::from_utf8_lossy(&run.stdout);
    text.split_whitespace()
        .find_map(|token| {
            let token = token.strip_prefix('v').unwrap_or(token);
            let mut parts = token.split('.');
            let major: u64 = parts.next()?.parse().ok()?;
            let minor: u64 = parts.next()?.parse().ok()?;
            let _ = major;
            let _ = minor;
            Some(token.to_owned())
        })
        .ok_or(VersionProbeError::Unrecognized)
}

// ── Verus output parsing ────────────────────────────────────────────────────

/// Normalized counts from a `verification results:: N verified, M errors` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerusSummary {
    /// Functions verified, per the summary line.
    pub verified: u64,
    /// Errors reported, per the summary line.
    pub errors: u64,
}

/// What the parser made of the verifier's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerusParse {
    /// A well-formed summary line was found.
    Recognized {
        /// Parsed `verified`/`errors` counts.
        summary: VerusSummary,
        /// `error`-prefixed diagnostic lines across stdout+stderr.
        error_lines: usize,
    },
    /// No summary line at all.
    Unrecognized {
        /// `error`-prefixed diagnostic lines across stdout+stderr.
        error_lines: usize,
    },
    /// A summary line exists but its counts do not parse.
    MalformedSummary,
}

/// Parses Verus output without regexes.
///
/// Finds the `verification results::` marker line and reads the two counts
/// off it. `error_lines` counts `error`-prefixed diagnostic lines across
/// stdout+stderr — the fail-closed signal when the summary line itself is
/// absent.
#[must_use]
pub fn parse_verus_output(stdout: &str, stderr: &str) -> VerusParse {
    const MARKER: &str = "verification results::";
    let error_lines = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| line.trim_start().starts_with("error"))
        .count();
    let mut summary_line = None;
    for line in stdout.lines().chain(stderr.lines()) {
        if let Some(rest) = line.find(MARKER).map(|i| &line[i + MARKER.len()..]) {
            summary_line = Some(rest);
        }
    }
    let Some(rest) = summary_line else {
        return VerusParse::Unrecognized { error_lines };
    };
    let mut verified = None;
    let mut errors = None;
    for part in rest.split(',') {
        let mut words = part.split_whitespace();
        let (Some(count), Some(label)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(count) = count.parse::<u64>() else {
            continue;
        };
        match label {
            "verified" => verified = Some(count),
            "errors" => errors = Some(count),
            _ => {}
        }
    }
    match (verified, errors) {
        (Some(verified), Some(errors)) => VerusParse::Recognized {
            summary: VerusSummary { verified, errors },
            error_lines,
        },
        _ => VerusParse::MalformedSummary,
    }
}

// ── Verifier execution ──────────────────────────────────────────────────────

/// The captured outcome of one verifier process run.
#[derive(Debug, Clone)]
pub struct VerusRun {
    /// Exact argv, for the record.
    pub argv: Vec<String>,
    /// Exit code; `None` when the child was killed (timeout).
    pub exit_code: Option<i64>,
    /// Whether the run exceeded its budget and was killed.
    pub timed_out: bool,
    /// Raw captured stdout bytes (redacted before persistence).
    pub stdout: Vec<u8>,
    /// Raw captured stderr bytes (redacted before persistence).
    pub stderr: Vec<u8>,
    /// Wall-clock start/finish (RFC 3339). Volatile metadata: documented as
    /// excluded from canonical comparison (see AC9).
    pub started_at: String,
    /// Wall-clock finish (RFC 3339); see [`VerusRun::started_at`].
    pub finished_at: String,
}

/// RFC 3339 wall-clock timestamp, second precision. Volatile metadata only
/// (see [`VerusRun::started_at`]); never part of stable IDs or comparison.
fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Runs the verifier with piped stdout/stderr drained on reader threads (so a
/// chatty verifier can never deadlock the pipe buffer), killing the child when
/// `timeout` elapses.
/// Per-stream capture ceiling: a verifier that floods its pipes fails the run
/// instead of exhausting memory.
const VERIFIER_OUTPUT_CAP: u64 = 10 * 1024 * 1024;

/// Spawns the verifier, drains its output on background threads, and enforces
/// the wall-clock budget.
///
/// The child runs as a process-group leader (Unix): on timeout the whole
/// group is signal-killed, so grandchildren the verifier spawned (a shell
/// wrapper's `sleep`, or Verus's own `z3` workers) cannot outlive the budget
/// holding the pipes open. Reader threads therefore always observe EOF after
/// the kill and `run_verifier` always returns.
///
/// # Errors
///
/// Returns [`std::io::Error`] when the binary cannot be spawned, a pipe
/// cannot be drained, or a stream exceeds the 10 MiB capture ceiling.
pub fn run_verifier(bin: &Path, args: &[String], timeout: Duration) -> std::io::Result<VerusRun> {
    let command_line: Vec<String> = std::iter::once(bin.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .collect();
    let started_at = now_rfc3339();
    let mut command = Command::new(bin);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // New process group whose ID equals the child's PID: the group kill
        // on timeout then reaches the verifier and everything it spawned.
        command.process_group(0);
    }
    let mut child = command.spawn()?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    // Each thread drains one stream up to the capture ceiling.
    let stdout_thread = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            pipe.take(VERIFIER_OUTPUT_CAP + 1).read_to_end(&mut buf)?;
        }
        if buf.len() as u64 > VERIFIER_OUTPUT_CAP {
            return Err(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "verifier stdout exceeded the 10 MiB capture ceiling",
            ));
        }
        Ok(buf)
    });
    let stderr_thread = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            pipe.take(VERIFIER_OUTPUT_CAP + 1).read_to_end(&mut buf)?;
        }
        if buf.len() as u64 > VERIFIER_OUTPUT_CAP {
            return Err(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "verifier stderr exceeded the 10 MiB capture ceiling",
            ));
        }
        Ok(buf)
    });

    let start = Instant::now();
    let (exit_code, timed_out) = loop {
        match child.try_wait()? {
            Some(status) => break (status.code().map(i64::from), false),
            None if start.elapsed() >= timeout => {
                // Kill the whole process group: the verifier plus any
                // grandchildren holding the pipes (a shell wrapper's `sleep`,
                // Verus's own `z3` workers). The child is a group leader on
                // Unix (see spawn above), so a negative PID addresses exactly
                // that group.
                //
                // Implemented through `bash`'s builtin `kill`: the
                // standalone /usr/bin/kill on these systems is util-linux,
                // which rejects negative PIDs ("invalid argument"), and
                // `unsafe_code = "forbid"` rules out calling `killpg`
                // directly. If bash is unavailable, fall back to killing the
                // direct child; grandchildren holding the pipes may then
                // linger, which is a known limitation on bash-less systems.
                #[cfg(unix)]
                {
                    let group = format!("-{}", child.id());
                    let killed = Command::new("bash")
                        .args(["-c", "kill -KILL -- \"$0\""])
                        .arg(&group)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if !killed {
                        let _ = child.kill();
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = child.kill();
                }
                let _ = child.wait();
                break (None, true);
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // The drain threads never panic (no panicking operations inside); a join
    // failure would mean the thread itself was killed, treated as empty.
    let stdout = stdout_thread.join().unwrap_or_else(|_| Ok(Vec::new()))?;
    let stderr = stderr_thread.join().unwrap_or_else(|_| Ok(Vec::new()))?;
    Ok(VerusRun {
        argv: command_line,
        exit_code,
        timed_out,
        stdout,
        stderr,
        started_at,
        finished_at: now_rfc3339(),
    })
}

// ── Classification ──────────────────────────────────────────────────────────

/// The normalized proof status. A failing PROOF is still a successful CAPTURE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofStatus {
    /// The verifier proved the target: zero errors, clean exit.
    Pass,
    /// The verifier reported errors (or error-shaped output with a nonzero exit).
    Fail,
    /// The verifier exceeded its time budget and was killed.
    Timeout,
    /// The verifier ran but its output could not be trusted.
    Error,
}

impl ProofStatus {
    /// Machine-readable status string stored on the records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Timeout => "timeout",
            Self::Error => "error",
        }
    }
}

/// A classified proof run: the status plus, for `Error`, the stable
/// diagnostic code explaining why the output could not be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofClassification {
    /// The normalized proof status.
    pub status: ProofStatus,
    /// Stable diagnostic code, present only for `Error`.
    pub diagnostic_code: Option<&'static str>,
}

/// Fail-closed classification: anything that is not an unambiguous,
/// internally-consistent success is `Fail`, `Timeout`, or `Error` — never a
/// silent pass.
#[must_use]
pub fn classify_proof_run(run: &VerusRun, parse: &VerusParse) -> ProofClassification {
    if run.timed_out {
        return ProofClassification {
            status: ProofStatus::Timeout,
            diagnostic_code: None,
        };
    }
    match parse {
        VerusParse::Recognized { summary, .. } => {
            if summary.errors > 0 {
                // Reported errors dominate: the proof failed, whatever the
                // exit code claims.
                ProofClassification {
                    status: ProofStatus::Fail,
                    diagnostic_code: None,
                }
            } else if run.exit_code == Some(0) {
                ProofClassification {
                    status: ProofStatus::Pass,
                    diagnostic_code: None,
                }
            } else {
                // Clean summary but a nonzero exit: the verifier disagrees
                // with itself. Never a pass.
                ProofClassification {
                    status: ProofStatus::Error,
                    diagnostic_code: Some(VERUS_EXIT_CODE_MISMATCH_CODE),
                }
            }
        }
        VerusParse::MalformedSummary => ProofClassification {
            status: ProofStatus::Error,
            diagnostic_code: Some(MALFORMED_VERUS_OUTPUT_CODE),
        },
        VerusParse::Unrecognized { error_lines } => {
            if *error_lines > 0 && run.exit_code != Some(0) {
                // Error-shaped output plus a nonzero exit: the proof failed,
                // even though the summary line is missing.
                ProofClassification {
                    status: ProofStatus::Fail,
                    diagnostic_code: Some(UNRECOGNIZED_VERUS_OUTPUT_CODE),
                }
            } else {
                ProofClassification {
                    status: ProofStatus::Error,
                    diagnostic_code: Some(UNRECOGNIZED_VERUS_OUTPUT_CODE),
                }
            }
        }
    }
}

// ── Redaction ───────────────────────────────────────────────────────────────

/// Per-span redaction of verifier output: each detected secret span becomes a
/// `<REDACTED:class:hash_prefix>` marker (auditable, irreversible), while the
/// surrounding output survives. Returns the redacted text and the number of
/// spans redacted.
fn redact_verifier_output(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut count = 0;
    // Modeled on `redact_code_text`'s forward-progress discipline: the cursor
    // always advances past an inserted marker, so the loop terminates.
    while let Some((class, rel_start, len)) = detect_secret_span(rest) {
        let Some(secret) = rest.get(rel_start..rel_start + len) else {
            break;
        };
        out.push_str(&rest[..rel_start]);
        out.push_str(&redact_span(class, secret));
        rest = &rest[rel_start + len..];
        count += 1;
    }
    out.push_str(rest);
    (out, count)
}

/// Builds an [`OutputHandle`] for redacted verifier output: inline only under
/// the 16 KiB ceiling, otherwise hash/bytes/counts only. The hash covers the
/// REDACTED bytes — the store never sees the raw secret.
fn output_handle(redacted: &str) -> OutputHandle {
    let bytes = redacted.len() as u64;
    OutputHandle {
        inline: (bytes <= INLINE_OUTPUT_CEILING).then(|| redacted.to_owned()),
        hash: blake3::hash(redacted.as_bytes()).to_hex().to_string(),
        bytes,
    }
}

// ── Record builders ─────────────────────────────────────────────────────────

fn proof_capture_producer(executed_at: &str) -> Producer {
    Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        // The capture workflows share the observation-writer producer kind;
        // see `test_capture_producer` for the sibling convention.
        producer_kind: ProducerKind::ObservationWriter,
        producer_components: BTreeMap::new(),
        // Deterministic: the caller-supplied executed_at, never a wall clock.
        producer_started_at: executed_at.to_owned(),
    }
}

fn proof_temporal(commit: &str, executed_at: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: executed_at.to_owned(),
        author_time: None,
        observed_at: executed_at.to_owned(),
        valid_time_source: Some("author_provided".to_owned()),
    }
}

fn stamp_verification_node(node: &mut GraphRecord, config: &CaptureConfig) {
    if let GraphRecord::Node {
        schema_version,
        domain,
        executed_at,
        redaction_policy_version,
        producer,
        temporal,
        ..
    } = node
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *domain = Some("verification".to_owned());
        *executed_at = Some(config.executed_at.clone());
        *redaction_policy_version = Some(REDACTION_POLICY_VERSION.to_owned());
        *producer = Some(proof_capture_producer(&config.executed_at));
        *temporal = Some(proof_temporal(&config.commit, &config.executed_at));
    }
}

/// The normalized proof-summary JSON stored in the `ProofResult`'s
/// `stdout_handle`. Derived from the captured evidence — never invented.
fn normalized_summary_json(
    config: &CaptureConfig,
    version: Option<&str>,
    classification: &ProofClassification,
    summary: Option<VerusSummary>,
    error_lines: usize,
    exit_code: Option<i64>,
    target_hash: &str,
) -> String {
    let mut map = serde_json::Map::new();
    map.insert(
        "format".to_owned(),
        serde_json::Value::String(PROOF_SUMMARY_FORMAT.to_owned()),
    );
    map.insert(
        "verifier".to_owned(),
        serde_json::Value::String(config.verifier_name.clone()),
    );
    if let Some(version) = version {
        map.insert(
            "verifier_version".to_owned(),
            serde_json::Value::String(version.to_owned()),
        );
    }
    map.insert(
        "status".to_owned(),
        serde_json::Value::String(classification.status.as_str().to_owned()),
    );
    if let Some(summary) = summary {
        map.insert(
            "verified".to_owned(),
            serde_json::Value::Number(summary.verified.into()),
        );
        map.insert(
            "errors".to_owned(),
            serde_json::Value::Number(summary.errors.into()),
        );
    }
    if let Some(code) = classification.diagnostic_code {
        map.insert(
            "diagnostic_code".to_owned(),
            serde_json::Value::String(code.to_owned()),
        );
    }
    map.insert(
        "error_lines".to_owned(),
        serde_json::Value::Number((error_lines as u64).into()),
    );
    if let Some(exit_code) = exit_code {
        map.insert(
            "exit_code".to_owned(),
            serde_json::Value::Number(exit_code.into()),
        );
    }
    map.insert(
        "target".to_owned(),
        serde_json::Value::String(config.target_display.clone()),
    );
    map.insert(
        "target_hash".to_owned(),
        serde_json::Value::String(target_hash.to_owned()),
    );
    serde_json::Value::Object(map).to_string()
}

fn human_summary(
    config: &CaptureConfig,
    classification: &ProofClassification,
    summary: Option<VerusSummary>,
) -> String {
    summary.map_or_else(
        || {
            format!(
                "{} proof run on {}: {}",
                config.verifier_name,
                config.target_display,
                classification.status.as_str(),
            )
        },
        |s| {
            format!(
                "{} proof run on {}: {} verified, {} errors ({})",
                config.verifier_name,
                config.target_display,
                s.verified,
                s.errors,
                classification.status.as_str(),
            )
        },
    )
}

/// Builds the `CommandRun` node: the captured verifier command.
fn build_command_run(
    config: &CaptureConfig,
    run: &VerusRun,
    classification: &ProofClassification,
    stdout_redacted: &str,
    stderr_redacted: &str,
    target_hash: &str,
    command_id: &str,
) -> GraphRecord {
    let mut node = GraphRecord::node(
        command_id.to_owned(),
        NodeKind::CommandRun,
        None,
        None,
        Some(config.target_display.clone()),
        format!(
            "verifier command: {} (exit {:?})",
            run.argv.join(" "),
            run.exit_code
        ),
    );
    stamp_verification_node(&mut node, config);
    if let GraphRecord::Node {
        verification_kind,
        status,
        exit_code,
        source_artifact_path,
        source_artifact_hash,
        evidence_quality,
        stdout_handle,
        stderr_handle,
        started_at,
        finished_at,
        ..
    } = &mut node
    {
        *verification_kind = Some("command_run".to_owned());
        *status = Some(classification.status.as_str().to_owned());
        *exit_code = run.exit_code;
        *source_artifact_path = Some(config.target_display.clone());
        *source_artifact_hash = Some(target_hash.to_owned());
        *evidence_quality = Some("captured".to_owned());
        *stdout_handle = Some(Box::new(output_handle(stdout_redacted)));
        *stderr_handle = Some(Box::new(output_handle(stderr_redacted)));
        // Volatile wall-clock metadata: recorded, but documented as excluded
        // from canonical comparison (AC9).
        *started_at = Some(run.started_at.clone());
        *finished_at = Some(run.finished_at.clone());
    }
    node
}

/// Parameters for [`build_proof_result`]: the normalized proof claim inputs
/// grouped so the builder stays under the argument-count lint.
struct ProofResultParams<'a> {
    config: &'a CaptureConfig,
    version: Option<&'a str>,
    classification: &'a ProofClassification,
    summary: Option<VerusSummary>,
    error_lines: usize,
    exit_code: Option<i64>,
    target_hash: &'a str,
    proof_id: &'a str,
    command_id: &'a str,
}

/// Builds the `ProofResult` node: the normalized proof claim, linked to the
/// captured command via an `evidence_links` `HAS_EVIDENCE` citation.
fn build_proof_result(params: &ProofResultParams<'_>) -> GraphRecord {
    // All fields are `Copy`; destructuring the place moves nothing.
    let ProofResultParams {
        config,
        version,
        classification,
        summary,
        error_lines,
        exit_code,
        target_hash,
        proof_id,
        command_id,
    } = *params;
    let mut node = GraphRecord::node(
        proof_id.to_owned(),
        NodeKind::ProofResult,
        None,
        None,
        Some(config.target_display.clone()),
        human_summary(config, classification, summary),
    );
    stamp_verification_node(&mut node, config);
    let summary_json = normalized_summary_json(
        config,
        version,
        classification,
        summary,
        error_lines,
        exit_code,
        target_hash,
    );
    if let GraphRecord::Node {
        verification_kind,
        status,
        exit_code: node_exit,
        source_artifact_path,
        source_artifact_hash,
        evidence_quality,
        evidence_links,
        stdout_handle,
        ..
    } = &mut node
    {
        *verification_kind = Some("proof_result".to_owned());
        *status = Some(classification.status.as_str().to_owned());
        *node_exit = exit_code;
        *source_artifact_path = Some(config.target_display.clone());
        *source_artifact_hash = Some(target_hash.to_owned());
        *evidence_quality = Some("summarized".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(command_id.to_owned()),
            target_domain: "verification".to_owned(),
            relation: EdgeLabel::HasEvidence.as_str().to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
        *stdout_handle = Some(Box::new(output_handle(&summary_json)));
    }
    node
}

/// Builds a verifier-error `Diagnostic` node: the malformed/unrecognized
/// output shape, never a proof claim.
fn build_verifier_error_diagnostic(
    config: &CaptureConfig,
    classification: &ProofClassification,
    target_hash: &str,
    excerpt: &str,
) -> GraphRecord {
    let code = classification
        .diagnostic_code
        .unwrap_or(UNRECOGNIZED_VERUS_OUTPUT_CODE);
    let id = verification_stable_id(&[
        "proof_diagnostic",
        &config.session_id,
        &config.commit,
        &config.target_display,
        code,
    ]);
    let mut node = GraphRecord::node(
        id,
        NodeKind::Diagnostic,
        None,
        None,
        Some(config.target_display.clone()),
        format!("verifier output could not be trusted ({code}): {excerpt}"),
    );
    stamp_verification_node(&mut node, config);
    if let GraphRecord::Node {
        symbol_kind,
        status,
        source_artifact_path,
        source_artifact_hash,
        note,
        ..
    } = &mut node
    {
        *symbol_kind = Some(code.to_owned());
        *status = Some(ProofStatus::Error.as_str().to_owned());
        *source_artifact_path = Some(config.target_display.clone());
        *source_artifact_hash = Some(target_hash.to_owned());
        *note = Some(format!(
            "the verifier ran, but its output shape was not recognized; no proof claim was recorded (code {code})"
        ));
    }
    node
}

// ── Pre-execution diagnostics ───────────────────────────────────────────────

/// Builds a stable diagnostic-only record for a pre-execution failure.
///
/// The verifier never ran, so there is no `ProofResult` and no `CommandRun`.
/// Diagnostic IDs are a pure function of (session, commit, target, code) —
/// no wall clock — so repeats are byte-identical.
#[must_use]
pub fn pre_execution_diagnostic(
    session_id: &str,
    commit: &str,
    target_display: &str,
    code: &'static str,
    message: &str,
    verifier_bin: Option<&str>,
    repo: Option<&str>,
) -> GraphRecord {
    let id =
        verification_stable_id(&["proof_diagnostic", session_id, commit, target_display, code]);
    let mut node = GraphRecord::node(
        id,
        NodeKind::Diagnostic,
        None,
        None,
        Some(target_display.to_owned()),
        format!("proof capture refused before execution ({code}): {message}"),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        symbol_kind,
        status,
        source_artifact_path,
        redaction_policy_version,
        producer,
        note,
        ..
    } = &mut node
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *domain = Some("verification".to_owned());
        *symbol_kind = Some(code.to_owned());
        *status = Some(ProofStatus::Error.as_str().to_owned());
        *source_artifact_path = Some(target_display.to_owned());
        *redaction_policy_version = Some(REDACTION_POLICY_VERSION.to_owned());
        *producer = Some(Producer {
            egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
            egregore_git: None,
            producer_kind: ProducerKind::ObservationWriter,
            producer_components: BTreeMap::new(),
            // No execution happened; stamping the commit anchor keeps the
            // diagnostic deterministic. (No wall clock anywhere.)
            producer_started_at: commit.to_owned(),
        });
        let mut note_text = message.to_owned();
        if let Some(bin) = verifier_bin {
            use std::fmt::Write as _;
            let _ = write!(note_text, " (verifier binary: {bin})");
        }
        if let Some(repo) = repo {
            use std::fmt::Write as _;
            let _ = write!(note_text, " (repo: {repo})");
        }
        *note = Some(note_text);
    }
    node
}

// ── Outcome ─────────────────────────────────────────────────────────────────

/// A successful capture: the verifier ran and its outcome was recorded.
/// A failing PROOF is still a successful CAPTURE.
#[derive(Debug, Clone)]
pub struct ProofOutcome {
    /// The normalized proof status.
    pub status: ProofStatus,
    /// Stable diagnostic code, present only for `Error`.
    pub diagnostic_code: Option<&'static str>,
    /// Canonical order: `CommandRun` first, then `ProofResult`, then any
    /// verifier-error `Diagnostic`.
    pub records: Vec<GraphRecord>,
    /// Stable ID of the `ProofResult` node.
    pub proof_record_id: String,
    /// Stable ID of the `CommandRun` node.
    pub command_record_id: String,
    /// Total secret spans redacted across stdout/stderr.
    pub secrets_redacted: usize,
    /// Raw (unredacted) verifier stdout, kept in memory only: the graph
    /// holds the redacted form; the protected store may keep this audit copy.
    pub raw_stdout: Vec<u8>,
    /// Raw (unredacted) verifier stderr, kept in memory only.
    pub raw_stderr: Vec<u8>,
}

/// A pre-execution failure: the verifier never ran. Carries the stable
/// diagnostic code and the diagnostic-only record.
#[derive(Debug, Clone)]
pub struct ProofCaptureError {
    /// Stable machine-readable diagnostic code for the pre-execution failure.
    pub code: &'static str,
    /// Diagnostic-only record (no `ProofResult` was emitted). Boxed: a
    /// `GraphRecord` is several kilobytes, and this type is the `Err`
    /// variant of [`capture_proof`]'s `Result`.
    pub diagnostic: Box<GraphRecord>,
}

fn pre_execution_failure(
    config: &CaptureConfig,
    code: &'static str,
    message: &str,
) -> ProofCaptureError {
    ProofCaptureError {
        code,
        diagnostic: Box::new(pre_execution_diagnostic(
            &config.session_id,
            &config.commit,
            &config.target_display,
            code,
            message,
            Some(&config.verus_bin.to_string_lossy()),
            config.repo.as_deref(),
        )),
    }
}

/// Validated inputs for a capture: the resolved proof target plus the
/// established verifier version (override or probed).
struct CaptureInputs {
    target: ProofTarget,
    version: Option<String>,
}

/// Steps 1–3 of [`capture_proof`]: the verifier binary must exist and be
/// executable, the proof target must resolve, and the verifier version must
/// be recognizable (or overridden). Any failure here is a pre-execution
/// diagnostic — the verifier never runs.
fn resolve_capture_inputs(config: &CaptureConfig) -> Result<CaptureInputs, ProofCaptureError> {
    if let Err(e) = validate_verifier_binary(&config.verus_bin) {
        return Err(pre_execution_failure(
            config,
            e.code(),
            &format!(
                "verifier binary {}: {}",
                config.verus_bin.display(),
                match e {
                    VerifierBinaryError::Missing => "not found",
                    VerifierBinaryError::NotExecutable => "not executable",
                }
            ),
        ));
    }
    let target = match resolve_proof_target(&config.target, &config.target_display) {
        Ok(target) => target,
        Err(e) => {
            return Err(pre_execution_failure(
                config,
                e.code(),
                &e.message(&config.target_display),
            ));
        }
    };
    let version = match config.verifier_version_override.clone() {
        Some(version) => Some(version),
        None => match probe_verifier_version(&config.verus_bin, config.probe_timeout) {
            Ok(version) => Some(version),
            Err(e) => {
                return Err(pre_execution_failure(
                    config,
                    e.code(),
                    &format!(
                        "could not establish verifier version from {}: {}",
                        config.verus_bin.display(),
                        match e {
                            VersionProbeError::Missing => "binary vanished",
                            VersionProbeError::SpawnFailed => "probe failed",
                            VersionProbeError::Unrecognized =>
                                "no recognizable version token in --version output",
                        }
                    ),
                ));
            }
        },
    };
    Ok(CaptureInputs { target, version })
}

/// Runs the full capture: validate → resolve → probe version → execute →
/// parse → classify → redact → emit records.
///
/// Pre-execution failures return `Err(ProofCaptureError)` with a stable
/// diagnostic code and a diagnostic-only record — never a `ProofResult`.
///
/// # Errors
///
/// Returns [`ProofCaptureError`] when the verifier binary is missing or not
/// executable, the proof target is missing/stale/ambiguous, the verifier
/// version cannot be established, or the verifier cannot be spawned.
pub fn capture_proof(config: &CaptureConfig) -> Result<ProofOutcome, ProofCaptureError> {
    let CaptureInputs { target, version } = resolve_capture_inputs(config)?;

    // 4. Execute.
    let mut argv = config.verus_args.clone();
    argv.push(config.target_display.clone());
    let run = run_verifier(&config.verus_bin, &argv, config.timeout).map_err(|e| {
        pre_execution_failure(
            config,
            VERIFIER_BINARY_MISSING_CODE,
            &format!("verifier failed to spawn: {e}"),
        )
    })?;

    // 5. Parse + classify (fail-closed).
    let stdout_text = String::from_utf8_lossy(&run.stdout);
    let stderr_text = String::from_utf8_lossy(&run.stderr);
    let parse = parse_verus_output(&stdout_text, &stderr_text);
    let classification = classify_proof_run(&run, &parse);
    let (summary, error_lines) = match &parse {
        VerusParse::Recognized {
            summary,
            error_lines,
        } => (Some(*summary), *error_lines),
        VerusParse::Unrecognized { error_lines } => (None, *error_lines),
        VerusParse::MalformedSummary => (None, 0),
    };

    // 6. Redact before persistence. Hashes cover the redacted bytes.
    let (stdout_redacted, stdout_secrets) = redact_verifier_output(&stdout_text);
    let (stderr_redacted, stderr_secrets) = redact_verifier_output(&stderr_text);
    let secrets_redacted = stdout_secrets + stderr_secrets;

    // 7. Emit records in canonical order.
    let command_id = verification_stable_id(&[
        "command_run",
        &config.session_id,
        &config.commit,
        &config.target_display,
    ]);
    let proof_id = verification_stable_id(&[
        "proof_result",
        &config.session_id,
        &config.commit,
        &config.target_display,
    ]);
    let mut records = vec![
        build_command_run(
            config,
            &run,
            &classification,
            &stdout_redacted,
            &stderr_redacted,
            &target.hash,
            &command_id,
        ),
        build_proof_result(&ProofResultParams {
            config,
            version: version.as_deref(),
            classification: &classification,
            summary,
            error_lines,
            exit_code: run.exit_code,
            target_hash: &target.hash,
            proof_id: &proof_id,
            command_id: &command_id,
        }),
    ];
    if classification.status == ProofStatus::Error {
        // The excerpt comes from the REDACTED stream: summaries persist.
        let excerpt: String = stdout_redacted
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        records.push(build_verifier_error_diagnostic(
            config,
            &classification,
            &target.hash,
            &excerpt,
        ));
    }

    Ok(ProofOutcome {
        status: classification.status,
        diagnostic_code: classification.diagnostic_code,
        records,
        proof_record_id: proof_id,
        command_record_id: command_id,
        secrets_redacted,
        raw_stdout: run.stdout,
        raw_stderr: run.stderr,
    })
}
