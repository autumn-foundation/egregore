// clippy::too_long_first_doc_paragraph fires on this module's doc without a span
// (nursery lint span-reporting bug in clippy 0.1.94); suppress at module level.
#![allow(clippy::too_long_first_doc_paragraph)]
//! Capture a `cargo test` / libtest JSON run as a citable, deterministic
//! verification-domain `TestRun` record (issue #165).
//!
//! This module is CAPTURE-ONLY: it parses a machine-readable libtest event
//! stream that the caller already captured to a file. It NEVER executes a test
//! runner. A captured pass is a recorded observation of one run — "no captured
//! failure" is not proof of correctness.
//!
//! Raw failing-test output never enters the graph. The only test payload that
//! reaches a record is a bounded, normalized `{name, outcome}` summary stored
//! through an [`OutputHandle`]; raw bytes are retrievable only via the
//! protected store, never inline.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::ir::{
    EdgeLabel, GraphRecord, NodeKind, OutputHandle, Producer, ProducerKind,
    VERIFICATION_SCHEMA_VERSION, stable_id, verification_stable_id,
};

/// Inline-payload ceiling shared with the typed evidence writer: an
/// [`OutputHandle`] stores content inline only when it is at or below this many
/// bytes (16 KiB), otherwise it references the content by hash only.
const INLINE_PAYLOAD_CEILING: u64 = 16 * 1024;

/// The single accepted `--format` value.
pub const LIBTEST_JSON_FORMAT: &str = "libtest-json";

/// Normalized-summary format tag stamped into the `stdout_handle` payload.
const NORMALIZED_SUMMARY_FORMAT: &str = "libtest-json-v1";

/// Diagnostic machine codes (stamped into `symbol_kind` on `Diagnostic` nodes).
pub const EMPTY_TEST_OUTPUT_CODE: &str = "empty_test_output";
/// See [`EMPTY_TEST_OUTPUT_CODE`].
pub const UNPARSEABLE_TEST_OUTPUT_CODE: &str = "unparseable_test_output";
/// See [`EMPTY_TEST_OUTPUT_CODE`].
pub const PARTIAL_TEST_OUTPUT_CODE: &str = "partial_test_output";
/// See [`EMPTY_TEST_OUTPUT_CODE`].
pub const TEST_SYMBOL_UNRESOLVED_CODE: &str = "test_symbol_unresolved";
/// See [`EMPTY_TEST_OUTPUT_CODE`].
pub const TEST_SYMBOL_AMBIGUOUS_CODE: &str = "test_symbol_ambiguous";

// ── Parsed outcome model ────────────────────────────────────────────────────

/// A single test's terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestOutcome {
    /// The test passed.
    Pass,
    /// The test failed.
    Fail,
    /// The test was ignored / filtered out.
    Ignored,
    /// The test timed out (treated as a failure at the suite level).
    Timeout,
}

impl TestOutcome {
    /// Returns the stable serialized outcome string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Ignored => "ignored",
            Self::Timeout => "timeout",
        }
    }

    /// Whether this outcome counts as a failure for suite-status derivation.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(self, Self::Fail | Self::Timeout)
    }
}

/// One parsed test and its terminal outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTest {
    /// The full test name as reported by the runner (e.g. `mycrate::mod::t`).
    pub name: String,
    /// The terminal outcome.
    pub outcome: TestOutcome,
}

/// Suite-level status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuiteStatus {
    /// No failures observed.
    Pass,
    /// At least one failure/timeout, or a terminal `suite` `failed` event.
    Fail,
}

impl SuiteStatus {
    /// Returns the stable serialized status string (`pass` / `fail`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
        }
    }
}

/// Aggregate per-outcome counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestCounts {
    /// Number of passing tests.
    pub passed: u64,
    /// Number of failing tests.
    pub failed: u64,
    /// Number of ignored tests.
    pub ignored: u64,
    /// Number of timed-out tests.
    pub timeout: u64,
    /// Number of `bench` events (out of scope; tallied, never emitted as tests).
    pub bench: u64,
    /// Total number of terminal test events.
    pub total: u64,
}

/// Reason a libtest event stream could not be captured as a `TestRun`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestCaptureError {
    /// The input file was empty (or whitespace only).
    Empty,
    /// A non-empty line was not valid JSON, or the whole stream carried zero
    /// recognized `test`/`suite` events.
    Unparseable,
}

impl TestCaptureError {
    /// Returns the stable machine code for this error.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Empty => EMPTY_TEST_OUTPUT_CODE,
            Self::Unparseable => UNPARSEABLE_TEST_OUTPUT_CODE,
        }
    }
}

/// The deterministic result of parsing a libtest JSON event stream.
#[derive(Debug, Clone)]
pub struct TestCaptureParse {
    tests: Vec<ParsedTest>,
    suite_terminal: Option<SuiteStatus>,
    bench_count: u64,
}

impl TestCaptureParse {
    /// The parsed tests, deduplicated on name and sorted by name.
    #[must_use]
    pub fn tests(&self) -> &[ParsedTest] {
        &self.tests
    }

    /// The terminal `suite` event status, when the stream carried one.
    #[must_use]
    pub const fn suite_terminal(&self) -> Option<SuiteStatus> {
        self.suite_terminal
    }

    /// The number of `bench` events (tallied, never emitted as tests).
    #[must_use]
    pub const fn bench_count(&self) -> u64 {
        self.bench_count
    }

    /// Whether tests were parsed but no terminal `suite` event was seen. The
    /// suite status is then derived from the test outcomes and a
    /// `partial_test_output` diagnostic is emitted (still a success).
    #[must_use]
    pub const fn is_partial(&self) -> bool {
        self.suite_terminal.is_none() && !self.tests.is_empty()
    }

    /// The derived suite status: `fail` if any test failed/timed out OR the
    /// terminal `suite` event was `failed`; otherwise `pass`.
    #[must_use]
    pub fn suite_status(&self) -> SuiteStatus {
        if self.suite_terminal == Some(SuiteStatus::Fail)
            || self.tests.iter().any(|t| t.outcome.is_failure())
        {
            SuiteStatus::Fail
        } else {
            SuiteStatus::Pass
        }
    }

    /// Aggregate per-outcome counts.
    #[must_use]
    pub fn counts(&self) -> TestCounts {
        let mut counts = TestCounts {
            passed: 0,
            failed: 0,
            ignored: 0,
            timeout: 0,
            bench: self.bench_count,
            total: self.tests.len() as u64,
        };
        for t in &self.tests {
            match t.outcome {
                TestOutcome::Pass => counts.passed += 1,
                TestOutcome::Fail => counts.failed += 1,
                TestOutcome::Ignored => counts.ignored += 1,
                TestOutcome::Timeout => counts.timeout += 1,
            }
        }
        counts
    }
}

/// A tolerant view of one libtest event line. Unknown extra fields (nextest
/// carries more) are ignored by default; absent `Option` fields deserialize to
/// `None`.
#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    ty: Option<String>,
    event: Option<String>,
    name: Option<String>,
}

/// Parses a newline-delimited libtest JSON event stream into a deterministic
/// [`TestCaptureParse`].
///
/// Recognized events: `suite` `started`/`ok`/`failed`, `test`
/// `started`/`ok`/`failed`/`ignored`/`timeout`, and `bench` (tallied only).
/// Unknown object shapes are tolerated. A per-test outcome is keyed on the
/// terminal event; `started` is ignored.
///
/// # Errors
///
/// Returns [`TestCaptureError::Empty`] for an empty / whitespace-only input, and
/// [`TestCaptureError::Unparseable`] for a non-empty non-JSON line or a stream
/// with zero recognized `test`/`suite` events.
pub fn parse_libtest_json(input: &str) -> Result<TestCaptureParse, TestCaptureError> {
    if input.trim().is_empty() {
        return Err(TestCaptureError::Empty);
    }

    let mut tests: BTreeMap<String, TestOutcome> = BTreeMap::new();
    let mut suite_terminal: Option<SuiteStatus> = None;
    let mut bench_count: u64 = 0;
    let mut recognized: u64 = 0;

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event: RawEvent =
            serde_json::from_str(trimmed).map_err(|_| TestCaptureError::Unparseable)?;

        match event.ty.as_deref() {
            Some("suite") => {
                recognized += 1;
                match event.event.as_deref() {
                    Some("ok") => suite_terminal = Some(SuiteStatus::Pass),
                    Some("failed") => suite_terminal = Some(SuiteStatus::Fail),
                    _ => {}
                }
            }
            Some("test") => {
                recognized += 1;
                let Some(name) = event.name else { continue };
                let outcome = match event.event.as_deref() {
                    Some("ok") => TestOutcome::Pass,
                    Some("failed") => TestOutcome::Fail,
                    Some("ignored") => TestOutcome::Ignored,
                    Some("timeout") => TestOutcome::Timeout,
                    // "started" and any unknown terminal are ignored.
                    _ => continue,
                };
                tests.insert(name, outcome);
            }
            Some("bench") => {
                // Out of scope: tallied but never counted as a recognized
                // test/suite event, so a bench-only stream is unparseable.
                bench_count += 1;
            }
            _ => {}
        }
    }

    if recognized == 0 {
        return Err(TestCaptureError::Unparseable);
    }

    let tests = tests
        .into_iter()
        .map(|(name, outcome)| ParsedTest { name, outcome })
        .collect();

    Ok(TestCaptureParse {
        tests,
        suite_terminal,
        bench_count,
    })
}

// ── Record building ─────────────────────────────────────────────────────────

/// Run metadata for a captured test run. All string fields are caller-supplied;
/// none are read from a wall clock.
#[derive(Debug, Clone)]
pub struct TestRunRequest<'a> {
    /// Stable session identity (part of the record ID).
    pub session_id: &'a str,
    /// Commit handle / external identifier (part of the record ID).
    pub commit: &'a str,
    /// Suite name (part of the record ID and the node `name`).
    pub suite: &'a str,
    /// The exact command that produced the stream. Stored, never executed.
    pub command: &'a str,
    /// The runner's exit status.
    pub exit_code: i64,
    /// Caller-supplied RFC 3339 timestamp (validated by the caller).
    pub executed_at: &'a str,
    /// Optional runner name (e.g. `libtest`, `cargo-nextest`).
    pub runner: Option<&'a str>,
    /// Optional runner version.
    pub runner_version: Option<&'a str>,
    /// The `--input` path, recorded as the source artifact path.
    pub source_artifact_path: &'a str,
    /// BLAKE3 hex hash of the raw input bytes.
    pub source_artifact_hash: &'a str,
}

/// Outcome of building a `TestRun` record batch.
#[derive(Debug)]
pub struct TestRunOutcome {
    /// The stable `TestRun` record ID (citable handle).
    pub record_id: String,
    /// All emitted records (the `TestRun` node, diagnostics, and edges).
    pub records: Vec<GraphRecord>,
    /// Number of tests that resolved to exactly one symbol (with `--graph`).
    pub resolved_count: u64,
    /// Number of tests that resolved to zero symbols (with `--graph`).
    pub unresolved_count: u64,
    /// Number of tests that resolved to two or more symbols (with `--graph`).
    pub ambiguous_count: u64,
    /// Whether the stream was partial (no terminal `suite` event).
    pub partial: bool,
}

#[derive(serde::Serialize)]
struct NormalizedCounts {
    passed: u64,
    failed: u64,
    ignored: u64,
    timeout: u64,
    total: u64,
}

#[derive(serde::Serialize)]
struct NormalizedTest {
    name: String,
    outcome: &'static str,
}

#[derive(serde::Serialize)]
struct NormalizedSummary<'a> {
    format: &'a str,
    command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_version: Option<&'a str>,
    suite: &'a str,
    suite_status: &'a str,
    counts: NormalizedCounts,
    bench_count: u64,
    tests: Vec<NormalizedTest>,
}

/// Builds the deterministic normalized-summary JSON stored in `stdout_handle`.
fn normalized_summary_json(req: &TestRunRequest, parse: &TestCaptureParse) -> String {
    let counts = parse.counts();
    let summary = NormalizedSummary {
        format: NORMALIZED_SUMMARY_FORMAT,
        command: req.command,
        runner: req.runner,
        runner_version: req.runner_version,
        suite: req.suite,
        suite_status: parse.suite_status().as_str(),
        counts: NormalizedCounts {
            passed: counts.passed,
            failed: counts.failed,
            ignored: counts.ignored,
            timeout: counts.timeout,
            total: counts.total,
        },
        bench_count: counts.bench,
        tests: parse
            .tests()
            .iter()
            .map(|t| NormalizedTest {
                name: t.name.clone(),
                outcome: t.outcome.as_str(),
            })
            .collect(),
    };
    serde_json::to_string(&summary).expect("normalized summary serialization is infallible")
}

/// Builds an [`OutputHandle`] for the bounded normalized summary.
fn output_handle(content: &str) -> OutputHandle {
    let bytes = content.len() as u64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(content.as_bytes());
    OutputHandle {
        inline: (bytes <= INLINE_PAYLOAD_CEILING).then(|| content.to_owned()),
        hash: hasher.finalize().to_hex().to_string(),
        bytes,
    }
}

/// A deterministic producer whose `producer_started_at` is the caller-supplied
/// `executed_at` (never a wall clock), so identical input yields byte-identical
/// output.
fn test_capture_producer(executed_at: &str) -> Producer {
    Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        producer_kind: ProducerKind::ObservationWriter,
        producer_components: BTreeMap::new(),
        producer_started_at: executed_at.to_owned(),
    }
}

/// Human-readable one-line summary.
fn human_summary(req: &TestRunRequest, parse: &TestCaptureParse) -> String {
    use std::fmt::Write as _;
    let c = parse.counts();
    let mut s = format!(
        "cargo test run: {} passed, {} failed, {} ignored",
        c.passed, c.failed, c.ignored
    );
    if c.timeout > 0 {
        let _ = write!(s, ", {} timed out", c.timeout);
    }
    let _ = write!(s, " (suite {})", req.suite);
    s
}

/// Builds the stable `TestRun` node.
fn build_test_run_node(
    req: &TestRunRequest,
    parse: &TestCaptureParse,
    record_id: &str,
) -> GraphRecord {
    let mut node = GraphRecord::node(
        record_id.to_owned(),
        NodeKind::TestRun,
        None,
        None,
        Some(req.suite.to_owned()),
        human_summary(req, parse),
    );
    let handle = output_handle(&normalized_summary_json(req, parse));
    if let GraphRecord::Node {
        schema_version,
        domain,
        verification_kind,
        status,
        exit_code,
        executed_at,
        source_artifact_path,
        source_artifact_hash,
        evidence_quality,
        stdout_handle,
        producer,
        ..
    } = &mut node
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *domain = Some("verification".to_owned());
        *verification_kind = Some("test_run".to_owned());
        *status = Some(parse.suite_status().as_str().to_owned());
        *exit_code = Some(req.exit_code);
        *executed_at = Some(req.executed_at.to_owned());
        *source_artifact_path = Some(req.source_artifact_path.to_owned());
        *source_artifact_hash = Some(req.source_artifact_hash.to_owned());
        *evidence_quality = Some("summarized".to_owned());
        *stdout_handle = Some(Box::new(handle));
        *producer = Some(test_capture_producer(req.executed_at));
    }
    node
}

/// The final `::`-segment of a test name — the conservative resolution key.
fn final_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Indexes a code graph: symbol name → the set of matching Symbol records, and
/// repo-relative path → the File record id.
struct CodeGraphIndex<'a> {
    symbols_by_name: BTreeMap<&'a str, Vec<&'a GraphRecord>>,
    file_id_by_path: BTreeMap<&'a str, &'a str>,
}

impl<'a> CodeGraphIndex<'a> {
    fn build(records: &'a [GraphRecord]) -> Self {
        let mut symbols_by_name: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
        let mut file_id_by_path: BTreeMap<&str, &str> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node {
                id,
                kind,
                name,
                repo_relative_path,
                ..
            } = record
            {
                match kind {
                    NodeKind::Symbol => {
                        if let Some(name) = name.as_deref() {
                            symbols_by_name.entry(name).or_default().push(record);
                        }
                    }
                    NodeKind::File => {
                        if let Some(path) = repo_relative_path.as_deref() {
                            // First writer wins for a given path (deterministic).
                            file_id_by_path.entry(path).or_insert(id.as_str());
                        }
                    }
                    _ => {}
                }
            }
        }
        Self {
            symbols_by_name,
            file_id_by_path,
        }
    }
}

/// Reads `(id, repo_relative_path)` off a Symbol record.
fn symbol_handles(record: &GraphRecord) -> (&str, Option<&str>) {
    match record {
        GraphRecord::Node {
            id,
            repo_relative_path,
            ..
        } => (id.as_str(), repo_relative_path.as_deref()),
        _ => unreachable!("index only stores Node records"),
    }
}

/// Builds a codegraph-domain resolution `Diagnostic` for a test that resolved to
/// zero or two-plus symbols. Non-orphan by doctrine: `Diagnostic` markers are
/// permitted to stand alone (`eg validate`), matching `manifest_deps`.
fn resolution_diagnostic(
    req: &TestRunRequest,
    test_name: &str,
    code: &str,
    matched: usize,
) -> GraphRecord {
    let mut record = GraphRecord::node(
        stable_id(&[
            "node",
            "diagnostic",
            code,
            req.session_id,
            req.commit,
            req.suite,
            test_name,
        ]),
        NodeKind::Diagnostic,
        None,
        None,
        Some(test_name.to_owned()),
        format!(
            "test {test_name} resolved to {matched} candidate symbols ({code}): no anchoring edge emitted"
        ),
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(code.to_owned());
    }
    record
}

/// Builds the error-path `Diagnostic` node for an empty or unparseable stream.
///
/// This is the ONLY record written on the empty / unparseable paths — never a
/// `TestRun`. `Diagnostic` markers legitimately stand alone under `eg validate`.
#[must_use]
pub fn error_diagnostic(
    session_id: &str,
    commit: &str,
    suite: &str,
    source_artifact_path: &str,
    error: TestCaptureError,
) -> GraphRecord {
    let code = error.code();
    let message = match error {
        TestCaptureError::Empty => {
            format!("empty test output for suite {suite}: no TestRun captured")
        }
        TestCaptureError::Unparseable => format!(
            "unparseable test output for suite {suite}: no recognized libtest JSON events; no TestRun captured"
        ),
    };
    let mut record = GraphRecord::node(
        stable_id(&["node", "diagnostic", code, session_id, commit, suite]),
        NodeKind::Diagnostic,
        Some(source_artifact_path.to_owned()),
        None,
        Some(suite.to_owned()),
        message,
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(code.to_owned());
    }
    record
}

/// Builds the `partial_test_output` diagnostic.
fn partial_diagnostic(req: &TestRunRequest) -> GraphRecord {
    let mut record = GraphRecord::node(
        stable_id(&[
            "node",
            "diagnostic",
            PARTIAL_TEST_OUTPUT_CODE,
            req.session_id,
            req.commit,
            req.suite,
        ]),
        NodeKind::Diagnostic,
        None,
        None,
        Some(req.suite.to_owned()),
        format!(
            "test stream for suite {} carried no terminal suite event: status derived from test outcomes",
            req.suite
        ),
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(PARTIAL_TEST_OUTPUT_CODE.to_owned());
    }
    record
}

/// Builds a `TestRun` record batch from a parsed stream and optional code graph.
///
/// Without a code graph the batch is exactly one self-contained `TestRun` node
/// (plus a `partial_test_output` diagnostic when the stream was partial). With a
/// code graph, each test whose final `::`-segment resolves to EXACTLY ONE
/// `Symbol` by name mints a `MENTIONS_SYMBOL` edge, a `TOUCHED_FILE` edge (when
/// the symbol's `File` is present), and — for a failing/timed-out test — a
/// `FAILED_ON` edge. Zero or two-plus matches mint NO edge and a resolution
/// `Diagnostic` instead. The code graph's own nodes are not re-emitted; union
/// the batch with the code graph to resolve edge endpoints.
#[must_use]
pub fn build_test_run_records(
    req: &TestRunRequest,
    parse: &TestCaptureParse,
    code_graph: Option<&[GraphRecord]>,
) -> TestRunOutcome {
    let record_id = verification_stable_id(&["test_run", req.session_id, req.commit, req.suite]);

    let mut records: Vec<GraphRecord> = Vec::new();
    records.push(build_test_run_node(req, parse, &record_id));

    let partial = parse.is_partial();
    if partial {
        records.push(partial_diagnostic(req));
    }

    let mut resolved_count = 0u64;
    let mut unresolved_count = 0u64;
    let mut ambiguous_count = 0u64;

    if let Some(graph) = code_graph {
        let index = CodeGraphIndex::build(graph);
        for test in parse.tests() {
            let segment = final_segment(&test.name);
            let matches = index.symbols_by_name.get(segment);
            match matches.map(Vec::as_slice) {
                Some([symbol]) => {
                    resolved_count += 1;
                    let (symbol_id, symbol_path) = symbol_handles(symbol);

                    // MENTIONS_SYMBOL: the test names this symbol.
                    records.push(GraphRecord::edge(
                        EdgeLabel::MentionsSymbol,
                        record_id.clone(),
                        symbol_id.to_owned(),
                        None,
                        format!("test run {} MENTIONS_SYMBOL {symbol_id}", req.suite),
                    ));

                    // TOUCHED_FILE: the symbol's File, when present in the graph.
                    if let Some(path) = symbol_path
                        && let Some(file_id) = index.file_id_by_path.get(path)
                    {
                        records.push(GraphRecord::edge(
                            EdgeLabel::TouchedFile,
                            record_id.clone(),
                            (*file_id).to_owned(),
                            None,
                            format!("test run {} TOUCHED_FILE {file_id}", req.suite),
                        ));
                    }

                    // FAILED_ON: only for a failing/timed-out test.
                    if test.outcome.is_failure() {
                        records.push(GraphRecord::edge(
                            EdgeLabel::FailedOn,
                            record_id.clone(),
                            symbol_id.to_owned(),
                            None,
                            format!("test run {} FAILED_ON {symbol_id}", req.suite),
                        ));
                    }
                }
                Some(many) if many.len() >= 2 => {
                    ambiguous_count += 1;
                    records.push(resolution_diagnostic(
                        req,
                        &test.name,
                        TEST_SYMBOL_AMBIGUOUS_CODE,
                        many.len(),
                    ));
                }
                _ => {
                    unresolved_count += 1;
                    records.push(resolution_diagnostic(
                        req,
                        &test.name,
                        TEST_SYMBOL_UNRESOLVED_CODE,
                        0,
                    ));
                }
            }
        }
    }

    // Deduplicate by record ID (stable, keep first) so a symbol named by several
    // tests contributes one MENTIONS/TOUCHED/FAILED edge, and identical
    // diagnostics collapse.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    records.retain(|r| seen.insert(r.id().to_owned()));

    TestRunOutcome {
        record_id,
        records,
        resolved_count,
        unresolved_count,
        ambiguous_count,
        partial,
    }
}
