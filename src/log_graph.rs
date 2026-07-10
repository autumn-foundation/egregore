//! Runtime log-signature extraction (`scan-logs`, issues #319 / #320).
//!
//! Turns a captured log file into deterministic, redaction-safe graph records:
//! one [`NodeKind::LogSource`] per file, one [`NodeKind::ErrorSignature`] per
//! distinct `template-v1` fingerprint, up to a capped set of
//! [`NodeKind::LogEvent`] exemplars per signature, and one hourly
//! [`NodeKind::LogOccurrenceBucket`] per (signature, hour).
//!
//! Strictly local and read-only. Raw log text never enters the graph: every
//! stored excerpt is normalized (`template-v1`), passed through the v1 redaction
//! policy, and truncated to [`EXCERPT_MAX_CHARS`]. Record IDs and content
//! hashes are BLAKE3 digests — one-way, never reversible to raw bytes. Output
//! is byte-identical across runs for a fixed `transaction_time`. See
//! `docs/schema/log-graph.md` and `docs/cli/scan-logs.md`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use chrono::{DateTime, SecondsFormat, Timelike, Utc};

use crate::ir::{
    EdgeLabel, ErrorSignaturePayload, GraphRecord, LOG_SCHEMA_VERSION, LogEventPayload,
    LogOccurrenceBucketPayload, LogPayload, LogSourcePayload, NodeKind, Producer, ProducerKind,
    log_stable_id,
};
use crate::redaction::{self, REDACTION_POLICY_VERSION};

mod template;
#[cfg(test)]
mod tests;

pub use template::normalize_template_v1;

/// Fingerprint algorithm identifier stamped on every `ErrorSignature`.
pub const FINGERPRINT_ALGORITHM: &str = "template-v1";

/// Default cap on `LogEvent` exemplars per (signature, source).
pub const DEFAULT_EXEMPLAR_CAP: usize = 5;

/// Occurrence-bucket width token (hourly).
pub const BUCKET_WIDTH: &str = "1h";

/// Maximum character length of any stored excerpt.
pub const EXCERPT_MAX_CHARS: usize = 200;

/// `plain-v1` line-oriented text format.
pub const FORMAT_PLAIN_V1: &str = "plain-v1";

/// `jsonl-v1` structured (one JSON object per line) format.
pub const FORMAT_JSONL_V1: &str = "jsonl-v1";

/// `valid_time_source` for a parsed log event timestamp.
pub const VALID_TIME_SOURCE_EVENT: &str = "log_event_timestamp";

/// `valid_time_source` for a timestamp-less line (transaction-time fallback).
pub const VALID_TIME_SOURCE_INFERRED: &str = "inferred_from_transaction_time";

/// Errors returned by [`scan_log_records`].
#[derive(Debug)]
pub enum LogScanError {
    /// The log file could not be read.
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The file is not a recognizable text log (binary bytes / invalid UTF-8).
    /// No partial output is produced.
    UnrecognizedFormat {
        /// Machine-readable detail (never raw file bytes).
        detail: String,
    },
}

impl std::fmt::Display for LogScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "failed to read log file {}: {source}", path.display())
            }
            Self::UnrecognizedFormat { detail } => {
                write!(f, "unrecognized log format: {detail}")
            }
        }
    }
}

impl std::error::Error for LogScanError {}

/// One exemplar-cap diagnostic: a signature had more distinct exemplars than
/// the cap, so extras were dropped (never silently).
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct ExemplarCapDiagnostic {
    /// Stable diagnostic code.
    pub code: &'static str,
    /// The signature whose exemplars were capped.
    pub signature_id: String,
    /// Severity class of the signature.
    pub severity: String,
    /// Number of exemplars kept.
    pub kept: u64,
    /// Number of distinct exemplars dropped.
    pub dropped: u64,
}

/// Result of a successful log scan.
#[derive(Debug, Clone)]
pub struct LogScan {
    /// The extracted graph records (unstamped by producer).
    pub records: Vec<GraphRecord>,
    /// Detected source format (`plain-v1` or `jsonl-v1`).
    pub source_format_version: &'static str,
    /// Exemplar-cap diagnostics, in canonical order.
    pub diagnostics: Vec<ExemplarCapDiagnostic>,
}

/// Builds the `log_importer` producer envelope (issues #319 / #320).
///
/// `producer_started_at` flows through the transaction-time override path so
/// canonical JSONL stays byte-stable; the producer envelope is never an
/// identity input.
#[must_use]
pub fn log_importer_producer(source_format_version: &str, producer_started_at: &str) -> Producer {
    let producer_components = BTreeMap::from([
        (
            "importer_schema_version".to_owned(),
            LOG_SCHEMA_VERSION.to_string(),
        ),
        (
            "source_format_version".to_owned(),
            source_format_version.to_owned(),
        ),
        (
            "fingerprint_algorithm".to_owned(),
            FINGERPRINT_ALGORITHM.to_owned(),
        ),
    ]);
    Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        producer_kind: ProducerKind::LogImporter,
        producer_components,
        producer_started_at: producer_started_at.to_owned(),
    }
}

/// Severity class in the closed log-signature set.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Severity {
    Fatal,
    Error,
    Warn,
}

impl Severity {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Fatal => "fatal",
            Self::Error => "error",
            Self::Warn => "warn",
        }
    }
}

/// Maps a header/level token to the closed severity set.
///
/// Keyed on the uppercase level tokens `FATAL` / `ERROR` / `WARN`(`ING`) and the
/// lowercase `panic` marker, so ordinary `info`/`debug`/`trace` lines return
/// `None` and mint no signature. `fatal` wins over `error` wins over `warn`.
fn severity_from_text(text: &str) -> Option<Severity> {
    if text.contains("panic") || text.contains("FATAL") {
        Some(Severity::Fatal)
    } else if text.contains("ERROR") {
        Some(Severity::Error)
    } else if text.contains("WARN") {
        Some(Severity::Warn)
    } else {
        None
    }
}

/// Maps a structured (jsonl) level value to the closed severity set.
fn severity_from_level(level: &str) -> Option<Severity> {
    match level.to_ascii_lowercase().as_str() {
        "fatal" | "panic" | "critical" | "crit" => Some(Severity::Fatal),
        "error" | "err" => Some(Severity::Error),
        "warn" | "warning" => Some(Severity::Warn),
        _ => None,
    }
}

/// A single raw occurrence extracted from the log, pre-aggregation.
struct Occurrence {
    severity: Severity,
    /// Redacted, normalized template (`template-v1` → redaction). This is the
    /// signature fingerprint basis and the excerpt source.
    template: String,
    /// `true` when redaction changed the normalized template.
    redacted: bool,
    /// One-based source line the occurrence began on.
    source_line: u64,
    /// RFC 3339 UTC valid time.
    valid_time: String,
    /// Source of `valid_time`.
    valid_time_source: &'static str,
    /// RFC 3339 UTC bucket start (floored to the hour).
    bucket_start: String,
    /// `true` when the bucket start came from a parsed timestamp.
    bucket_from_timestamp: bool,
}

/// Scans one log file into deterministic log-signature graph records.
///
/// `repository_id` is the stable `Repository` record ID the facts are attributed
/// to; `transaction_time` (RFC 3339) is the capture instant threaded through the
/// deterministic override path (no wall clock enters IDs or canonical output).
///
/// # Errors
///
/// Returns [`LogScanError::Read`] when the file cannot be read and
/// [`LogScanError::UnrecognizedFormat`] when the bytes are not a UTF-8 text log
/// (binary / NUL bytes). No partial records are produced on the error path.
#[allow(clippy::too_many_lines)]
pub fn scan_log_records(
    log_path: &Path,
    repo_root: &Path,
    repository_id: &str,
    transaction_time: &str,
) -> Result<LogScan, LogScanError> {
    let raw = std::fs::read(log_path).map_err(|source| LogScanError::Read {
        path: log_path.to_path_buf(),
        source,
    })?;
    let normalized_bytes = normalize_newlines(&raw);
    if normalized_bytes.contains(&0) {
        return Err(LogScanError::UnrecognizedFormat {
            detail: "file contains NUL bytes; not a text log".to_owned(),
        });
    }
    let text = std::str::from_utf8(&normalized_bytes).map_err(|error| {
        LogScanError::UnrecognizedFormat {
            detail: format!("file is not valid UTF-8: {error}"),
        }
    })?;

    let source_artifact_hash = blake3::hash(&normalized_bytes).to_hex().to_string();
    let source_relative_path = repo_relative_path(repo_root, log_path);
    let line_count = text.lines().count() as u64;

    let source_format_version = detect_format(text);

    // Floor the transaction time once for the timestamp-less fallback bucket.
    let tx_bucket = floor_to_hour(transaction_time).unwrap_or_else(|| transaction_time.to_owned());

    let occurrences = match source_format_version {
        FORMAT_JSONL_V1 => parse_jsonl(text, transaction_time, &tx_bucket),
        _ => parse_plain(text, transaction_time, &tx_bucket),
    };

    let source_id = log_stable_id(&[
        "log_source",
        repository_id,
        &source_relative_path,
        &source_artifact_hash,
    ]);
    let mut records = Vec::new();
    let mut diagnostics = Vec::new();

    // ── LogSource node ───────────────────────────────────────────────────────
    records.push(
        GraphRecord::node(
            source_id.clone(),
            NodeKind::LogSource,
            Some(source_relative_path.clone()),
            None,
            Some(source_relative_path.clone()),
            format!(
                "Log source {source_relative_path} ({source_format_version}), {line_count} lines"
            ),
        )
        .with_domain("log", LOG_SCHEMA_VERSION)
        .with_log(LogPayload::LogSource(LogSourcePayload {
            source_relative_path,
            source_format_version: source_format_version.to_owned(),
            source_artifact_hash,
            line_count,
        }))
        .with_valid_time(transaction_time, VALID_TIME_SOURCE_INFERRED),
    );

    // ── Aggregate occurrences by signature (severity + template) ─────────────
    let mut signatures: BTreeMap<(&'static str, &str), Vec<&Occurrence>> = BTreeMap::new();
    for occ in &occurrences {
        signatures
            .entry((occ.severity.as_str(), occ.template.as_str()))
            .or_default()
            .push(occ);
    }

    for ((severity, template), occs) in &signatures {
        let signature_id = log_stable_id(&[
            "error_signature",
            repository_id,
            FINGERPRINT_ALGORITHM,
            template,
            severity,
        ]);
        let occurrence_count = occs.len() as u64;
        let redacted = occs.iter().any(|o| o.redacted);
        let excerpt = truncate_excerpt(template);
        let first_seen = occs
            .iter()
            .map(|o| o.valid_time.as_str())
            .min()
            .unwrap_or_default()
            .to_owned();
        let last_seen = occs
            .iter()
            .map(|o| o.valid_time.as_str())
            .max()
            .unwrap_or_default()
            .to_owned();
        // Valid time of the signature = its earliest occurrence.
        let sig_valid_time_source = occs
            .iter()
            .min_by(|a, b| a.valid_time.cmp(&b.valid_time))
            .map_or(VALID_TIME_SOURCE_INFERRED, |occ| occ.valid_time_source);

        let mut sig_node = GraphRecord::node(
            signature_id.clone(),
            NodeKind::ErrorSignature,
            None,
            None,
            Some(format!("{severity} signature")),
            format!("Error signature ({severity}) x{occurrence_count}: {excerpt}"),
        )
        .with_domain("log", LOG_SCHEMA_VERSION)
        .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
            fingerprint_algorithm: FINGERPRINT_ALGORITHM.to_owned(),
            template_excerpt: excerpt.clone(),
            severity: (*severity).to_owned(),
            occurrence_count,
            first_seen: first_seen.clone(),
            last_seen,
        }))
        .with_valid_time(first_seen, sig_valid_time_source);
        if redacted {
            sig_node = sig_node.with_redaction_policy_version(REDACTION_POLICY_VERSION);
        }
        records.push(sig_node);

        // ErrorSignature —CAPTURED_FROM→ LogSource
        records.push(log_edge(
            EdgeLabel::CapturedFrom,
            repository_id,
            &signature_id,
            &source_id,
            "ErrorSignature captured from LogSource",
        ));

        // ── Exemplars: distinct (valid_time, content_hash), capped ───────────
        let mut exemplars: BTreeMap<(String, String), &Occurrence> = BTreeMap::new();
        for occ in occs {
            let content_hash = blake3::hash(occ.template.as_bytes()).to_hex().to_string();
            exemplars
                .entry((occ.valid_time.clone(), content_hash))
                .and_modify(|existing| {
                    if occ.source_line < existing.source_line {
                        *existing = occ;
                    }
                })
                .or_insert(occ);
        }
        // Canonical order: (event_valid_time, event_content_hash, source_line).
        let mut ordered: Vec<((String, String), &Occurrence)> = exemplars.into_iter().collect();
        ordered.sort_by(|a, b| {
            a.0.0
                .cmp(&b.0.0)
                .then(a.0.1.cmp(&b.0.1))
                .then(a.1.source_line.cmp(&b.1.source_line))
        });
        let distinct = ordered.len();
        if distinct > DEFAULT_EXEMPLAR_CAP {
            diagnostics.push(ExemplarCapDiagnostic {
                code: "exemplar_cap_reached",
                signature_id: signature_id.clone(),
                severity: (*severity).to_owned(),
                kept: DEFAULT_EXEMPLAR_CAP as u64,
                dropped: (distinct - DEFAULT_EXEMPLAR_CAP) as u64,
            });
        }
        for ((event_valid_time, content_hash), occ) in
            ordered.into_iter().take(DEFAULT_EXEMPLAR_CAP)
        {
            let event_id = log_stable_id(&[
                "log_event",
                repository_id,
                &signature_id,
                &event_valid_time,
                &content_hash,
            ]);
            let mut event_node = GraphRecord::node(
                event_id.clone(),
                NodeKind::LogEvent,
                None,
                None,
                Some(format!("{severity} event")),
                format!(
                    "Log event ({severity}) at line {} [{event_valid_time}]",
                    occ.source_line
                ),
            )
            .with_domain("log", LOG_SCHEMA_VERSION)
            .with_log(LogPayload::LogEvent(LogEventPayload {
                event_excerpt: excerpt.clone(),
                event_content_hash: content_hash,
                source_line: occ.source_line,
                severity: (*severity).to_owned(),
            }))
            .with_valid_time(event_valid_time, occ.valid_time_source);
            if occ.redacted {
                event_node = event_node.with_redaction_policy_version(REDACTION_POLICY_VERSION);
            }
            records.push(event_node);

            // LogEvent —FINGERPRINTED_AS→ ErrorSignature
            records.push(log_edge(
                EdgeLabel::FingerprintedAs,
                repository_id,
                &event_id,
                &signature_id,
                "LogEvent fingerprinted as ErrorSignature",
            ));
            // LogEvent —CAPTURED_FROM→ LogSource
            records.push(log_edge(
                EdgeLabel::CapturedFrom,
                repository_id,
                &event_id,
                &source_id,
                "LogEvent captured from LogSource",
            ));
        }

        // ── Hourly occurrence buckets ────────────────────────────────────────
        let mut buckets: BTreeMap<String, (u64, bool)> = BTreeMap::new();
        for occ in occs {
            let entry = buckets
                .entry(occ.bucket_start.clone())
                .or_insert((0, false));
            entry.0 += 1;
            entry.1 |= occ.bucket_from_timestamp;
        }
        for (bucket_start, (count, from_ts)) in buckets {
            let bucket_id = log_stable_id(&[
                "log_occurrence_bucket",
                repository_id,
                &signature_id,
                &bucket_start,
                BUCKET_WIDTH,
            ]);
            let source = if from_ts {
                VALID_TIME_SOURCE_EVENT
            } else {
                VALID_TIME_SOURCE_INFERRED
            };
            records.push(
                GraphRecord::node(
                    bucket_id.clone(),
                    NodeKind::LogOccurrenceBucket,
                    None,
                    None,
                    Some(format!("{severity} bucket {bucket_start}")),
                    format!(
                        "Occurrence bucket {bucket_start} ({BUCKET_WIDTH}) x{count} for {severity} signature"
                    ),
                )
                .with_domain("log", LOG_SCHEMA_VERSION)
                .with_log(LogPayload::LogOccurrenceBucket(LogOccurrenceBucketPayload {
                    bucket_start: bucket_start.clone(),
                    bucket_width: BUCKET_WIDTH.to_owned(),
                    occurrence_count: count,
                }))
                .with_valid_time(bucket_start.clone(), source),
            );
            // LogOccurrenceBucket —AGGREGATES→ ErrorSignature
            records.push(log_edge(
                EdgeLabel::Aggregates,
                repository_id,
                &bucket_id,
                &signature_id,
                "LogOccurrenceBucket aggregates ErrorSignature",
            ));
        }
    }

    Ok(LogScan {
        records,
        source_format_version,
        diagnostics,
    })
}

/// Produces the POST-REDACTION whole-file bytes of a log for protected capture
/// (issue #321).
///
/// Reads the log, normalizes CRLF/CR to LF (the same basis the scanner hashes),
/// and applies the v1 redaction policy so a secret-bearing line is returned
/// collapsed to its `<REDACTED:…>` marker and the raw secret never reaches the
/// protected blob.
///
/// Secret detection runs over the **whole normalized text** via
/// [`redaction::detect_secret_span`], not line by line: a multi-line secret —
/// e.g. a PEM / OpenSSH private-key block whose base64 body and `END` line are
/// not individually secret-shaped — is caught as one span that covers every
/// line it touches. Each maximal run of secret-bearing lines is then collapsed
/// to a single `<REDACTED:class:hash>` marker through [`redaction::redact_value`]
/// (the run's joined text carries the leading secret the whole-value detector
/// recognizes), and every non-secret line is preserved verbatim. Redacting each
/// line independently (this function's original form, issue #321) collapsed only
/// the `BEGIN` marker line and wrote the key body to the blob unchanged.
///
/// This materializes redacted bytes ONLY when protected capture is requested;
/// ordinary graph extraction ([`scan_log_records`]) never calls it and is
/// unchanged. Raw, unredacted bytes never leave this function.
///
/// # Errors
///
/// Returns [`LogScanError::Read`] when the file cannot be read and
/// [`LogScanError::UnrecognizedFormat`] for binary / non-UTF-8 input, matching
/// [`scan_log_records`] so capture and extraction agree on what is a valid log.
pub fn redacted_source_bytes(log_path: &Path) -> Result<Vec<u8>, LogScanError> {
    let raw = std::fs::read(log_path).map_err(|source| LogScanError::Read {
        path: log_path.to_path_buf(),
        source,
    })?;
    let normalized = normalize_newlines(&raw);
    if normalized.contains(&0) {
        return Err(LogScanError::UnrecognizedFormat {
            detail: "file contains NUL bytes; not a text log".to_owned(),
        });
    }
    let text =
        std::str::from_utf8(&normalized).map_err(|error| LogScanError::UnrecognizedFormat {
            detail: format!("file is not valid UTF-8: {error}"),
        })?;

    let secret_lines = secret_bearing_lines(text);

    // `split('\n')` (not `lines()`) preserves the exact normalized structure,
    // including a trailing empty segment when the file ends in a newline. Each
    // maximal run of secret-bearing lines collapses to ONE marker so a
    // multi-line key block does not leak its body/END lines; non-secret lines
    // pass through verbatim. Output is deterministic and byte-stable.
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut first = true;
    while i < lines.len() {
        if !first {
            out.push('\n');
        }
        first = false;
        if secret_lines.contains(&i) {
            let run_start = i;
            while i < lines.len() && secret_lines.contains(&i) {
                i += 1;
            }
            let run = lines[run_start..i].join("\n");
            out.push_str(&redaction::redact_value(&run));
        } else {
            out.push_str(lines[i]);
            i += 1;
        }
    }
    Ok(out.into_bytes())
}

/// Returns the set of line indices (0-based over `text.split('\n')`) that any
/// v1-policy secret span touches.
///
/// Spans are detected over the WHOLE text (via [`redaction::detect_secret_span`])
/// so a multi-line secret block marks every line its byte range covers, not just
/// the one line that is individually secret-shaped. Zero-length (empty) line
/// segments are never marked. Detection is deterministic left-to-right.
fn secret_bearing_lines(text: &str) -> std::collections::BTreeSet<usize> {
    // Absolute byte range [start, end) of each split('\n') line's content.
    let mut line_ranges = Vec::new();
    let mut offset = 0_usize;
    for line in text.split('\n') {
        let start = offset;
        let end = start + line.len();
        line_ranges.push((start, end));
        offset = end + 1; // skip the '\n' separator
    }

    let mut secret = std::collections::BTreeSet::new();
    let mut cursor = 0_usize;
    while cursor < text.len() {
        let Some((rel_start, len)) = earliest_secret_span(&text[cursor..]) else {
            break;
        };
        let span_start = cursor + rel_start;
        let span_end = span_start + len;
        for (idx, &(ls, le)) in line_ranges.iter().enumerate() {
            // A line is secret-bearing when the span overlaps its non-empty
            // content range.
            if le > ls && span_start < le && span_end > ls {
                secret.insert(idx);
            }
        }
        // Advance past this span; guard against a zero-length span.
        cursor = span_end.max(span_start + 1);
    }
    secret
}

/// Returns the byte span `(start, len)` of the EARLIEST-starting v1-policy secret
/// in `bytes`, or `None` when there is none.
///
/// [`redaction::detect_secret_span`] returns the first match in secret-CLASS
/// priority order, not the earliest byte offset (issue #321, Codex P1 "scan spans
/// in byte order before marking lines"): a lower-priority secret sitting earlier
/// in the byte stream loses to a later higher-priority one. Driving the
/// line-marking cursor straight off that call would advance past the later span
/// and skip the earlier secret's line entirely, leaking it into the protected
/// blob. This helper recovers the earliest start by re-probing the strict prefix
/// before each reported span until the prefix holds no further secret, so the
/// caller never advances past unprocessed secret text. It reuses
/// `detect_secret_span` unchanged. The prefix probes look only at bytes strictly
/// before the current earliest start, so a multi-line secret block (which the
/// priority order surfaces first, at its own start) is never truncated mid-span.
fn earliest_secret_span(bytes: &str) -> Option<(usize, usize)> {
    let (_, mut best_start, mut best_len) = redaction::detect_secret_span(bytes)?;
    while best_start > 0 {
        match redaction::detect_secret_span(&bytes[..best_start]) {
            Some((_, start, len)) => {
                best_start = start;
                best_len = len;
            }
            None => break,
        }
    }
    Some((best_start, best_len))
}

/// Computes the repository-relative path of a log file under the repo root,
/// exposed for protected capture so the blob handle uses the same repo-relative
/// path the `LogSource` node records (issue #321).
#[must_use]
pub fn source_relative_path(repo_root: &Path, log_path: &Path) -> String {
    repo_relative_path(repo_root, log_path)
}

/// Builds a log-domain edge record (`log:v1:` ID, schema version 1).
fn log_edge(
    label: EdgeLabel,
    repository_id: &str,
    source: &str,
    target: &str,
    summary: &str,
) -> GraphRecord {
    let id = log_stable_id(&["edge", label.as_str(), repository_id, source, target]);
    GraphRecord::Edge {
        id,
        schema_version: LOG_SCHEMA_VERSION,
        label,
        source: source.to_owned(),
        target: target.to_owned(),
        confidence: None,
        resolution: None,
        temporal: None,
        summary: summary.to_owned(),
        producer: None,
    }
}

/// Normalizes CRLF and lone CR line endings to LF over raw bytes.
fn normalize_newlines(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'\r' => {
                out.push(b'\n');
                if i + 1 < raw.len() && raw[i + 1] == b'\n' {
                    i += 1;
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    out
}

/// Detects `jsonl-v1` (every non-empty line is a JSON object) vs `plain-v1`.
fn detect_format(text: &str) -> &'static str {
    let mut saw_line = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        saw_line = true;
        match serde_json::from_str::<serde_json::Value>(line.trim()) {
            Ok(serde_json::Value::Object(_)) => {}
            _ => return FORMAT_PLAIN_V1,
        }
    }
    if saw_line {
        FORMAT_JSONL_V1
    } else {
        FORMAT_PLAIN_V1
    }
}

/// True when a plain-v1 line continues the preceding logical event (indented
/// text, a backtrace frame, `stack backtrace:`, or a `note:` line).
fn is_continuation_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    // Any leading whitespace marks a continuation (indented backtrace frames,
    // wrapped messages).
    if line.starts_with([' ', '\t']) {
        return true;
    }
    let trimmed = line.trim_start();
    if trimmed == "stack backtrace:" || trimmed.starts_with("note:") || trimmed.starts_with("at ") {
        return true;
    }
    // A non-indented backtrace frame: `<digits>: ...`.
    let mut saw_digit = false;
    for c in trimmed.chars() {
        if c.is_ascii_digit() {
            saw_digit = true;
        } else {
            return saw_digit && c == ':';
        }
    }
    false
}

/// Parses a plain-v1 log into occurrences, grouping multi-line panic/backtrace
/// continuations into one logical event.
fn parse_plain(text: &str, transaction_time: &str, tx_bucket: &str) -> Vec<Occurrence> {
    // (start_line, header, joined_text)
    let mut events: Vec<(u64, String, String)> = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let one_based = (idx + 1) as u64;
        if let Some(last) = events.last_mut()
            && is_continuation_line(line)
        {
            last.2.push('\n');
            last.2.push_str(line);
            continue;
        }
        events.push((one_based, line.to_owned(), line.to_owned()));
    }

    let mut occurrences = Vec::new();
    for (start_line, header, full_text) in events {
        let Some(severity) = severity_from_text(&header) else {
            continue;
        };
        let (valid_time, valid_time_source, bucket_start, bucket_from_timestamp) =
            resolve_time(parse_timestamp(&header), transaction_time, tx_bucket);
        let (template, redacted) = fingerprint(&full_text);
        occurrences.push(Occurrence {
            severity,
            template,
            redacted,
            source_line: start_line,
            valid_time,
            valid_time_source,
            bucket_start,
            bucket_from_timestamp,
        });
    }
    occurrences
}

/// Parses a jsonl-v1 log into occurrences (one object per line).
fn parse_jsonl(text: &str, transaction_time: &str, tx_bucket: &str) -> Vec<Occurrence> {
    let mut occurrences = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(trimmed)
        else {
            continue;
        };
        let level = first_str(&obj, &["level", "severity", "lvl"]);
        let Some(severity) = level.as_deref().and_then(severity_from_level) else {
            continue;
        };
        let message = first_str(&obj, &["message", "msg", "error", "err"]).unwrap_or_default();
        if message.is_empty() {
            continue;
        }
        let timestamp = first_str(&obj, &["timestamp", "ts", "time"]);
        let (valid_time, valid_time_source, bucket_start, bucket_from_timestamp) = resolve_time(
            timestamp.as_deref().and_then(parse_timestamp),
            transaction_time,
            tx_bucket,
        );
        let (template, redacted) = fingerprint(&message);
        occurrences.push(Occurrence {
            severity,
            template,
            redacted,
            source_line: (idx + 1) as u64,
            valid_time,
            valid_time_source,
            bucket_start,
            bucket_from_timestamp,
        });
    }
    occurrences
}

/// Returns the first present string value for any of `keys`.
fn first_str(obj: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(serde_json::Value::String(s)) = obj.get(*key) {
            return Some(s.clone());
        }
    }
    None
}

/// Normalizes then redacts a message into its fingerprint template.
///
/// Returns `(redacted_template, was_redacted)`. Redaction runs on the
/// normalized template so secrets never enter the fingerprint hash preimage.
fn fingerprint(message: &str) -> (String, bool) {
    let normalized = normalize_template_v1(message);
    let redacted = redaction::redact_value(&normalized);
    let was_redacted = redacted != normalized;
    (redacted, was_redacted)
}

/// Truncates an excerpt to [`EXCERPT_MAX_CHARS`] characters (char-safe).
fn truncate_excerpt(s: &str) -> String {
    if s.chars().count() <= EXCERPT_MAX_CHARS {
        return s.to_owned();
    }
    s.chars().take(EXCERPT_MAX_CHARS).collect()
}

/// Resolves an optional parsed timestamp into the four temporal fields.
fn resolve_time(
    parsed: Option<DateTime<Utc>>,
    transaction_time: &str,
    tx_bucket: &str,
) -> (String, &'static str, String, bool) {
    parsed.map_or_else(
        || {
            (
                transaction_time.to_owned(),
                VALID_TIME_SOURCE_INFERRED,
                tx_bucket.to_owned(),
                false,
            )
        },
        |dt| {
            let valid_time = dt.to_rfc3339_opts(SecondsFormat::Secs, true);
            let bucket_start = floor_datetime(dt).to_rfc3339_opts(SecondsFormat::Secs, true);
            (valid_time, VALID_TIME_SOURCE_EVENT, bucket_start, true)
        },
    )
}

/// Floors a UTC datetime to the top of its hour.
fn floor_datetime(dt: DateTime<Utc>) -> DateTime<Utc> {
    dt.date_naive()
        .and_hms_opt(dt.hour(), 0, 0)
        .map_or(dt, |naive| {
            DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
        })
}

/// Parses a leading timestamp from a log line header, returning UTC.
///
/// Accepts RFC 3339 (`2026-01-02T03:04:05Z`), space-separated
/// `YYYY-MM-DD HH:MM:SS[.fff]` (assumed UTC), and bare
/// `YYYY-MM-DDTHH:MM:SS`. Returns `None` when no leading timestamp is present.
fn parse_timestamp(header: &str) -> Option<DateTime<Utc>> {
    let header = header.trim_start();
    // Candidate 1: the first whitespace-delimited token as RFC 3339.
    if let Some(token) = header.split_whitespace().next()
        && let Ok(dt) = DateTime::parse_from_rfc3339(token)
    {
        return Some(dt.with_timezone(&Utc));
    }
    // Candidate 2: `YYYY-MM-DD HH:MM:SS[.fff]` (date + time tokens, UTC).
    let mut tokens = header.split_whitespace();
    if let (Some(date), Some(time)) = (tokens.next(), tokens.next()) {
        let combined = format!("{date} {time}");
        for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
            if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&combined, fmt) {
                return Some(DateTime::from_naive_utc_and_offset(naive, Utc));
            }
        }
    }
    // Candidate 3: bare `YYYY-MM-DDTHH:MM:SS` first token, UTC.
    if let Some(token) = header.split_whitespace().next() {
        for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
            if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(token, fmt) {
                return Some(DateTime::from_naive_utc_and_offset(naive, Utc));
            }
        }
    }
    None
}

/// Floors an RFC 3339 timestamp to the top of its hour (UTC), or `None` if it
/// cannot be parsed.
fn floor_to_hour(rfc3339: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(rfc3339)
        .ok()?
        .with_timezone(&Utc);
    Some(floor_datetime(dt).to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Computes the repository-relative path of a log file under the repo root.
fn repo_relative_path(repo_root: &Path, log_path: &Path) -> String {
    let abs_log = std::fs::canonicalize(log_path).unwrap_or_else(|_| log_path.to_path_buf());
    let abs_root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let rel = abs_log.strip_prefix(&abs_root).unwrap_or(&abs_log);
    let joined = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        log_path
            .file_name()
            .map_or_else(|| "log".to_owned(), |n| n.to_string_lossy().into_owned())
    } else {
        joined
    }
}
