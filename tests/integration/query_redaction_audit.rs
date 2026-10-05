//! Integration tests for `eg query redaction-audit` (issue #244): sweep every
//! persisted queryable string field across all domains for secret-shaped
//! values that bypassed the write-boundary redaction gate.
//!
//! The audit is READ-ONLY and must NEVER print a raw secret: findings carry
//! only `record_id`, `domain`, `field_path`, `classification`, and a BLAKE3
//! `hash_prefix`. All planted fixture values are obviously fake (they contain
//! `FAKE`), and the tests assert the raw values never appear on stdout.

#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, ErrorSignaturePayload, GraphRecord,
    LOG_SCHEMA_VERSION, LogEventPayload, LogPayload, NodeKind, OutputHandle,
    PROJECT_SCHEMA_VERSION, USER_CONTEXT_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION,
};
use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

/// Stamp the `domain` field (and its domain schema version, so the JSONL
/// loader accepts the fixture — the JSONL reader validates version tuples
/// per domain) and return the record (builder helper).
fn with_domain(mut record: GraphRecord, domain: &str) -> GraphRecord {
    if let GraphRecord::Node {
        domain: ref mut d,
        schema_version: ref mut v,
        ..
    } = record
    {
        *d = Some(domain.to_owned());
        *v = match domain {
            "agent_memory" => AGENT_MEMORY_SCHEMA_VERSION,
            "artifact" => ARTIFACT_SCHEMA_VERSION,
            "verification" => VERIFICATION_SCHEMA_VERSION,
            "project" => PROJECT_SCHEMA_VERSION,
            "user_context" => USER_CONTEXT_SCHEMA_VERSION,
            "log" => LOG_SCHEMA_VERSION,
            _ => *v,
        };
    }
    record
}

fn inline_handle(inline: &str) -> OutputHandle {
    OutputHandle {
        inline: Some(inline.to_owned()),
        hash: "00".to_owned(),
        bytes: inline.len() as u64,
    }
}

// ---------------------------------------------------------------------------
// Fixture: one planted fake secret per redactable domain + one code-graph
// symbol body + one high-entropy token. N = 8 planted secrets.
// ---------------------------------------------------------------------------

/// Obviously-fake planted secrets (every value contains `FAKE`).
/// Each entry: (record id, domain, node kind, field path, expected class).
fn planted_records() -> Vec<GraphRecord> {
    let mut records = Vec::new();

    // 1. agent_memory — Observation.text — api_token.
    let mut r = GraphRecord::node(
        "audit:agent-memory:1".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "fixture observation".to_owned(),
    );
    if let GraphRecord::Node { text, .. } = &mut r {
        *text = Some("deployed with key sk-FAKEAUDITKEY0123456789abcdefXYZ for staging".to_owned());
    }
    records.push(with_domain(r, "agent_memory"));

    // 2. artifact — ToolCall.arguments_handle.inline — webhook_secret.
    let mut r = GraphRecord::node(
        "audit:artifact:1".to_owned(),
        NodeKind::ToolCall,
        None,
        None,
        None,
        "fixture tool call".to_owned(),
    );
    if let GraphRecord::Node {
        arguments_handle, ..
    } = &mut r
    {
        *arguments_handle = Some(Box::new(inline_handle(
            "callback=whsec_FAKEAUDIT0123456789abcdefXYZ123",
        )));
    }
    records.push(with_domain(r, "artifact"));

    // 3. verification — CommandRun.stdout_handle.inline — database_url.
    let mut r = GraphRecord::node(
        "audit:verification:1".to_owned(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "fixture command run".to_owned(),
    );
    if let GraphRecord::Node { stdout_handle, .. } = &mut r {
        *stdout_handle = Some(Box::new(inline_handle(
            "connecting to postgres://audit:FAKEAUDITPASS0123456789@db.internal:5432/app",
        )));
    }
    records.push(with_domain(r, "verification"));

    // 4. project — Task.title — cloud_credential.
    let mut r = GraphRecord::node(
        "audit:project:1".to_owned(),
        NodeKind::Task,
        None,
        None,
        None,
        "fixture task".to_owned(),
    );
    if let GraphRecord::Node { title, .. } = &mut r {
        *title = Some("rotate AKIAFAKEAUDIT0123456 before Friday".to_owned());
    }
    records.push(with_domain(r, "project"));

    // 5. user_context — Preference.rule_text — env_secret.
    let mut r = GraphRecord::node(
        "audit:user-context:1".to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "fixture preference".to_owned(),
    );
    if let GraphRecord::Node { user_context, .. } = &mut r {
        user_context.rule_text = Some("AWS_SECRET_ACCESS_KEY=FAKEAUDITsecret0123456789".to_owned());
    }
    records.push(with_domain(r, "user_context"));

    // 6. log — ErrorSignature template_excerpt — session_cookie.
    let mut r = GraphRecord::node(
        "audit:log:1".to_owned(),
        NodeKind::ErrorSignature,
        None,
        None,
        None,
        "fixture error signature".to_owned(),
    );
    if let GraphRecord::Node { log, .. } = &mut r {
        *log = Some(Box::new(LogPayload::ErrorSignature(
            ErrorSignaturePayload {
                fingerprint_algorithm: "template-v1".to_owned(),
                template_excerpt: "login failed for sessionid=FAKEAUDITsession0123456789abcdef"
                    .to_owned(),
                severity: "error".to_owned(),
                occurrence_count: 3,
                first_seen: "2026-09-01T00:00:00Z".to_owned(),
                last_seen: "2026-09-02T00:00:00Z".to_owned(),
                frames: None,
                repository_id: String::new(),
            },
        )));
    }
    records.push(with_domain(r, "log"));

    records.extend(planted_records_tail());
    records
}

/// Plants the last two fixture records for the redaction-audit test:
/// a codegraph `Symbol` (exempt from the write gate) and a high-entropy
/// `LogEvent` with no known secret pattern.
fn planted_records_tail() -> Vec<GraphRecord> {
    let mut records = Vec::new();

    // 7. codegraph — Symbol summary (body) — ssh_private_key.
    //    Code-graph records are exempt from the write gate: the prime
    //    unredacted-at-rest risk this lane exists to catch.
    let r = GraphRecord::node(
        "audit:codegraph:1".to_owned(),
        NodeKind::Symbol,
        Some("src/config.rs".to_owned()),
        None,
        Some("load_key".to_owned()),
        "fn load_key() -> String { \"-----BEGIN RSA PRIVATE KEY-----\\nFAKEAUDITKEYDATA0123456789abcdef\\n-----END RSA PRIVATE KEY-----\" }"
            .to_owned(),
    );
    records.push(with_domain(r, "codegraph"));

    // 8. high_entropy — LogEvent event_excerpt — a random-looking token with
    //    no known pattern: 48 mixed-case alphanumerics (~4.4 bits/char).
    let mut r = GraphRecord::node(
        "audit:log:2".to_owned(),
        NodeKind::LogEvent,
        None,
        None,
        None,
        "fixture log event".to_owned(),
    );
    if let GraphRecord::Node { log, .. } = &mut r {
        *log = Some(Box::new(LogPayload::LogEvent(LogEventPayload {
            event_excerpt: "saw token Fk3FAKE9c2E5b1D8f4A6c0E3b7D9a1F5c8E2b4D6a0F3e7Xy in headers"
                .to_owned(),
            event_content_hash: "00".to_owned(),
            source_line: 42,
            severity: "warn".to_owned(),
            repository_id: String::new(),
        })));
    }
    records.push(with_domain(r, "log"));

    records
}

/// The expected `(record_id, field_path, classification)` triples, in the
/// canonical output order `(record_id, field_path, classification)`.
const EXPECTED: &[(&str, &str, &str)] = &[
    ("audit:agent-memory:1", "text", "api_token"),
    (
        "audit:artifact:1",
        "arguments_handle.inline",
        "webhook_secret",
    ),
    ("audit:codegraph:1", "summary", "ssh_private_key"),
    ("audit:log:1", "log.template_excerpt", "session_cookie"),
    ("audit:log:2", "log.event_excerpt", "high_entropy"),
    ("audit:project:1", "title", "cloud_credential"),
    ("audit:user-context:1", "rule_text", "env_secret"),
    (
        "audit:verification:1",
        "stdout_handle.inline",
        "database_url",
    ),
];

/// Already-handled records that must NEVER be flagged:
/// - a record stamped `redaction_policy_version: "v1"` carrying a raw secret;
/// - a field whose only secret-shaped content is a `<REDACTED:...>` marker;
/// - documented allowlist values (public sample keys, test-fixture keys).
fn already_handled_records() -> Vec<GraphRecord> {
    let mut records = Vec::new();

    // v1-stamped record: the gate already handled it.
    let mut r = GraphRecord::node(
        "audit:handled:v1".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "fixture observation".to_owned(),
    );
    if let GraphRecord::Node {
        text,
        redaction_policy_version,
        ..
    } = &mut r
    {
        *text = Some("key sk-FAKEV1STAMPED0123456789abcdefXY was rotated".to_owned());
        *redaction_policy_version = Some("v1".to_owned());
    }
    records.push(with_domain(r, "agent_memory"));

    // Marker-only field: already redacted, never flagged.
    let mut r = GraphRecord::node(
        "audit:handled:marker".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "fixture observation".to_owned(),
    );
    if let GraphRecord::Node { text, .. } = &mut r {
        *text = Some("<REDACTED:api_token:abcdef123456>".to_owned());
    }
    records.push(with_domain(r, "agent_memory"));

    // Allowlist: AWS documentation example key (exact match).
    let mut r = GraphRecord::node(
        "audit:handled:allowlist-exact".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "fixture observation".to_owned(),
    );
    if let GraphRecord::Node { text, .. } = &mut r {
        *text = Some("example from the AWS IAM docs: AKIAIOSFODNN7EXAMPLE".to_owned());
    }
    records.push(with_domain(r, "agent_memory"));

    // Allowlist: Stripe test secret key (prefix match).
    let mut r = GraphRecord::node(
        "audit:handled:allowlist-prefix".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "fixture observation".to_owned(),
    );
    if let GraphRecord::Node { text, .. } = &mut r {
        *text = Some("test fixture key sk-test-FAKEAUDITFIXTURE0123456789abcdef for CI".to_owned());
    }
    records.push(with_domain(r, "agent_memory"));

    // Allowlist: known-safe high-entropy-looking token (exact match).
    let mut r = GraphRecord::node(
        "audit:handled:allowlist-token".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "fixture observation".to_owned(),
    );
    if let GraphRecord::Node { text, .. } = &mut r {
        *text =
            Some("ci token dGhpcy1pcy1hLXRlc3QtZml4dHVyZS10b2tlbi0wMTIz in the harness".to_owned());
    }
    records.push(with_domain(r, "agent_memory"));

    records
}

fn fixture_graph(records: Vec<GraphRecord>) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut jsonl = String::new();
    for record in records {
        jsonl.push_str(&serde_json::to_string(&record).expect("record serializes"));
        jsonl.push('\n');
    }
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_audit(graph: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = egregore();
    cmd.args(["query", "redaction-audit", "--graph"]);
    cmd.arg(graph);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.assert()
}

fn findings_of(parsed: &Value) -> Vec<&Value> {
    parsed["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .collect()
}

fn triples(parsed: &Value) -> Vec<(String, String, String)> {
    findings_of(parsed)
        .into_iter()
        .map(|f| {
            (
                f["record_id"].as_str().expect("record_id").to_owned(),
                f["field_path"].as_str().expect("field_path").to_owned(),
                f["classification"]
                    .as_str()
                    .expect("classification")
                    .to_owned(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// AC: 100% detection — N planted secrets, N findings, exit non-zero
// ---------------------------------------------------------------------------

#[test]
fn detects_every_planted_secret_with_citable_handles() {
    let (_temp, graph) = fixture_graph(planted_records());

    let assert = run_audit(&graph, &[]).code(3);
    let output = assert.get_output().stdout.clone();
    let raw = std::str::from_utf8(&output).expect("utf8");
    let parsed: Value = serde_json::from_str(raw.trim()).expect("stdout must be valid JSON");

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["lane"], "redaction_audit");

    let got = triples(&parsed);
    let want: Vec<(String, String, String)> = EXPECTED
        .iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
        .collect();
    assert_eq!(
        got, want,
        "audit must detect 8/8 planted secrets in canonical order"
    );

    for f in findings_of(&parsed) {
        let record_id = f["record_id"].as_str().expect("record_id");
        assert!(!record_id.is_empty(), "every finding cites a record id");
        let domain = f["domain"].as_str().expect("domain");
        assert!(!domain.is_empty(), "every finding names its domain");
        let prefix = f["hash_prefix"].as_str().expect("hash_prefix");
        assert_eq!(prefix.len(), 12, "hash_prefix is 12 hex chars");
        assert!(
            prefix.chars().all(|c| c.is_ascii_hexdigit()),
            "hash_prefix is lowercase hex"
        );
    }

    assert_eq!(
        parsed["counts"]["findings"], 8,
        "counts.findings tallies the audit"
    );
    assert!(
        parsed["counts"]["records_scanned"].as_u64().unwrap_or(0) >= 8,
        "counts.records_scanned covers the fixture"
    );
}

#[test]
fn never_prints_raw_secret_values() {
    let (_temp, graph) = fixture_graph(planted_records());

    for extra in [&[] as &[&str], &["--format", "text"]] {
        let assert = run_audit(&graph, extra).code(3);
        let output = assert.get_output();
        let stdout = std::str::from_utf8(&output.stdout).expect("utf8");
        let stderr = std::str::from_utf8(&output.stderr).expect("utf8");
        for leak in [
            "FAKEAUDIT",
            "FAKEV1",
            "sk-FAKE",
            "whsec_FAKE",
            "postgres://audit",
            "AKIAFAKE",
            "BEGIN RSA PRIVATE KEY",
            "Fk3FAKE9c2",
        ] {
            assert!(
                !stdout.contains(leak),
                "stdout must never carry the raw planted value {leak:?} (format {extra:?})"
            );
            assert!(
                !stderr.contains(leak),
                "stderr must never carry the raw planted value {leak:?} (format {extra:?})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// AC: already-handled values are never flagged; clean store exits 0
// ---------------------------------------------------------------------------

#[test]
fn already_handled_records_are_never_flagged() {
    let mut records = planted_records();
    records.extend(already_handled_records());
    let (_temp, graph) = fixture_graph(records);

    let assert = run_audit(&graph, &[]).code(3);
    let output = assert.get_output().stdout.clone();
    let parsed: Value = serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON");

    let got = triples(&parsed);
    let want: Vec<(String, String, String)> = EXPECTED
        .iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
        .collect();
    assert_eq!(
        got, want,
        "v1-stamped, marker-only, and allowlisted records add zero findings"
    );
}

#[test]
fn clean_store_reports_zero_findings_and_exits_0() {
    let (_temp, graph) = fixture_graph(already_handled_records());

    let assert = run_audit(&graph, &[]).code(0);
    let output = assert.get_output().stdout.clone();
    let parsed: Value = serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["findings"].as_array().expect("findings").len(), 0);
    assert_eq!(parsed["counts"]["findings"], 0);
    assert_eq!(
        parsed["empty_reason"], "no_unredacted_secrets_in_scope",
        "clean store carries the stable empty reason"
    );
}

// ---------------------------------------------------------------------------
// AC: deterministic byte-stable output; human-readable text form
// ---------------------------------------------------------------------------

#[test]
fn two_runs_are_byte_identical() {
    let (_temp, graph) = fixture_graph(planted_records());
    let first = run_audit(&graph, &[]).code(3).get_output().stdout.clone();
    let second = run_audit(&graph, &[]).code(3).get_output().stdout.clone();
    assert_eq!(first, second, "output must be byte-identical across runs");
}

#[test]
fn text_format_is_human_readable_and_cites_handles() {
    let (_temp, graph) = fixture_graph(planted_records());
    let output = run_audit(&graph, &["--format", "text"])
        .code(3)
        .get_output()
        .stdout
        .clone();
    let text = std::str::from_utf8(&output).expect("utf8");
    for (record_id, field_path, class) in EXPECTED {
        assert!(
            text.contains(record_id),
            "text cites record id {record_id}:\n{text}"
        );
        assert!(
            text.contains(field_path),
            "text cites field path {field_path}:\n{text}"
        );
        assert!(
            text.contains(class),
            "text names classification {class}:\n{text}"
        );
    }
    assert!(
        text.contains("findings: 8"),
        "text carries the findings tally:\n{text}"
    );
}

#[test]
fn text_format_clean_store_exits_0() {
    let (_temp, graph) = fixture_graph(already_handled_records());
    run_audit(&graph, &["--format", "text"]).code(0);
}

// ---------------------------------------------------------------------------
// AC: usage errors are machine-readable exit 1
// ---------------------------------------------------------------------------

#[test]
fn unknown_repo_selector_is_machine_readable_exit_1() {
    let (_temp, graph) = fixture_graph(planted_records());
    let assert = run_audit(&graph, &["--repo", "no-such-repo"]).code(1);
    let output = assert.get_output();
    assert!(
        output.stdout.is_empty(),
        "no findings on an unknown selector"
    );
    let stderr = std::str::from_utf8(&output.stderr).expect("utf8");
    let diag: Value = serde_json::from_str(stderr.trim()).expect("stderr is JSON");
    assert_eq!(diag["code"], "unknown_repository_selector");
}
