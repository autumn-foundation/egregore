//! Unit tests for the log-signature extractor internals (issues #319 / #320).

use super::*;
use crate::ir::EdgeLabel;

// ── template-v1 normalization ────────────────────────────────────────────────

#[test]
fn normalizes_iso_timestamp() {
    assert_eq!(
        normalize_template_v1("2026-01-02T03:04:05Z boom"),
        "<TS> boom"
    );
    assert_eq!(
        normalize_template_v1("2026-01-02T03:04:05.123456+02:00 boom"),
        "<TS> boom"
    );
    assert_eq!(
        normalize_template_v1("2026-01-02 03:04:05 boom"),
        "<TS> boom"
    );
}

#[test]
fn normalizes_bare_clock_and_syslog() {
    assert_eq!(normalize_template_v1("12:34:56 tick"), "<TS> tick");
    assert_eq!(
        normalize_template_v1("Jan  9 12:34:56 host sshd"),
        "<TS> host sshd"
    );
}

#[test]
fn normalizes_uuid_ip_duration_hex_num() {
    assert_eq!(
        normalize_template_v1("id=550e8400-e29b-41d4-a716-446655440000"),
        "id=<UUID>"
    );
    assert_eq!(normalize_template_v1("from 192.168.0.1:8080"), "from <IP>");
    assert_eq!(
        normalize_template_v1("peer fe80::1ff:fe23:4567:890a"),
        "peer <IP>"
    );
    assert_eq!(normalize_template_v1("took 1.5s"), "took <DUR>");
    assert_eq!(normalize_template_v1("elapsed 1m30s"), "elapsed <DUR>");
    assert_eq!(normalize_template_v1("elapsed 250ms"), "elapsed <DUR>");
    assert_eq!(normalize_template_v1("ptr 0xDEADBEEF"), "ptr <HEX>");
    assert_eq!(normalize_template_v1("hash cafebabe1234"), "hash <HEX>");
    assert_eq!(normalize_template_v1("index 42"), "index <NUM>");
}

#[test]
fn normalizes_paths_with_line_col() {
    assert_eq!(
        normalize_template_v1("panicked at src/main.rs:42:5"),
        "panicked at <PATH>"
    );
    assert_eq!(
        normalize_template_v1("open /var/log/app.log failed"),
        "open <PATH> failed"
    );
}

#[test]
fn specifics_win_over_num() {
    // A UUID/IP must not be shredded into <NUM> fragments.
    assert_eq!(normalize_template_v1("10.0.0.255"), "<IP>");
    // A 6+ hex run wins over a decimal number; a short one is a plain number.
    assert_eq!(normalize_template_v1("123456"), "<HEX>");
    assert_eq!(normalize_template_v1("12345"), "<NUM>");
}

#[test]
fn does_not_rewrite_identifier_interiors() {
    // Interior digits/hex inside an identifier stay put.
    assert_eq!(normalize_template_v1("var_123abc"), "var_123abc");
    assert_eq!(normalize_template_v1("deadbeefx"), "deadbeefx");
}

#[test]
fn repeated_error_varying_only_volatiles_normalizes_identically() {
    let a = normalize_template_v1(
        "2026-01-02T03:04:05Z [pid 4242] request 550e8400-e29b-41d4-a716-446655440000 failed at 0xdeadbeef",
    );
    let b = normalize_template_v1(
        "2026-01-02T09:10:11Z [pid 5353] request 111e2222-e29b-41d4-a716-446655440000 failed at 0xfeedface",
    );
    assert_eq!(
        a, b,
        "volatile-only differences must collapse to one template"
    );
    assert_eq!(a, "<TS> [pid <NUM>] request <UUID> failed at <HEX>");
}

#[test]
fn normalization_is_deterministic() {
    let input = "2026-01-02T03:04:05Z ERROR db 10.0.0.1:5432 took 3ms id=550e8400-e29b-41d4-a716-446655440000";
    let first = normalize_template_v1(input);
    for _ in 0..5 {
        assert_eq!(normalize_template_v1(input), first);
    }
}

// ── edge-label predicate classification ──────────────────────────────────────

#[test]
fn log_edge_labels_classify_on_both_predicates() {
    // Structural log labels: neither an evidence link nor codegraph topology.
    for label in [
        EdgeLabel::FingerprintedAs,
        EdgeLabel::CapturedFrom,
        EdgeLabel::Aggregates,
    ] {
        assert!(
            !label.is_evidence_link_label(),
            "{} is structural, not an evidence link",
            label.as_str()
        );
        assert!(
            !label.is_codegraph_topology_label(),
            "{} is a log label, never codegraph topology",
            label.as_str()
        );
    }
    // Evidence-link log labels: evidence links, never codegraph topology.
    for label in [EdgeLabel::FrameResolvesTo, EdgeLabel::EmittedDuring] {
        assert!(
            label.is_evidence_link_label(),
            "{} must be a valid evidence link",
            label.as_str()
        );
        assert!(
            !label.is_codegraph_topology_label(),
            "{} is a log label, never codegraph topology",
            label.as_str()
        );
    }
}

// ── stable-id identity contract ──────────────────────────────────────────────

#[test]
fn log_stable_id_is_deterministic_and_prefixed() {
    let a = log_stable_id(&[
        "error_signature",
        "repo",
        FINGERPRINT_ALGORITHM,
        "t",
        "error",
    ]);
    let b = log_stable_id(&[
        "error_signature",
        "repo",
        FINGERPRINT_ALGORITHM,
        "t",
        "error",
    ]);
    assert_eq!(a, b);
    assert!(a.starts_with("log:v1:"), "got {a}");
}

#[test]
fn log_stable_id_preserves_case_of_parts() {
    // Non-lowercasing: content identity is preserved exactly.
    let lower = log_stable_id(&["log_event", "repo", "template"]);
    let upper = log_stable_id(&["log_event", "repo", "TEMPLATE"]);
    assert_ne!(lower, upper);
}

// ── schema-version gate ──────────────────────────────────────────────────────

#[test]
fn schema_gate_accepts_v1_rejects_unknown() {
    use crate::schema_version::{RecordVersion, is_known_record_version};
    for kind in [
        "LogSource",
        "ErrorSignature",
        "LogEvent",
        "LogOccurrenceBucket",
    ] {
        assert!(is_known_record_version(&RecordVersion::new("log", kind, 1)));
        assert!(!is_known_record_version(&RecordVersion::new(
            "log", kind, 999
        )));
    }
    for label in ["FINGERPRINTED_AS", "CAPTURED_FROM", "AGGREGATES"] {
        assert!(is_known_record_version(&RecordVersion::new(
            "log", label, 1
        )));
    }
}

// ── severity mapping and continuation predicate ──────────────────────────────

#[test]
fn severity_mapping_is_closed() {
    assert_eq!(
        severity_from_text("thread 'main' panicked at x"),
        Some(Severity::Fatal)
    );
    assert_eq!(severity_from_text("FATAL boom"), Some(Severity::Fatal));
    assert_eq!(severity_from_text("[ERROR] boom"), Some(Severity::Error));
    assert_eq!(severity_from_text("[WARN] careful"), Some(Severity::Warn));
    assert_eq!(
        severity_from_text("[WARNING] careful"),
        Some(Severity::Warn)
    );
    assert_eq!(severity_from_text("INFO all good"), None);
    assert_eq!(severity_from_text("DEBUG detail"), None);
}

#[test]
fn severity_from_level_is_closed() {
    assert_eq!(severity_from_level("ERROR"), Some(Severity::Error));
    assert_eq!(severity_from_level("warning"), Some(Severity::Warn));
    assert_eq!(severity_from_level("fatal"), Some(Severity::Fatal));
    assert_eq!(severity_from_level("info"), None);
    assert_eq!(severity_from_level("trace"), None);
}

#[test]
fn continuation_predicate_groups_backtraces() {
    assert!(is_continuation_line("   0: core::panicking::panic"));
    assert!(is_continuation_line("\tat src/main.rs:10"));
    assert!(is_continuation_line("stack backtrace:"));
    assert!(is_continuation_line("note: run with RUST_BACKTRACE=1"));
    assert!(is_continuation_line("12: foo::bar"));
    // A new top-level severity/timestamp line is not a continuation.
    assert!(!is_continuation_line("2026-01-02T03:04:05Z [ERROR] next"));
    assert!(!is_continuation_line("[ERROR] a new error"));
    assert!(!is_continuation_line(""));
}

#[test]
fn fingerprint_redacts_secrets() {
    let (template, redacted) = fingerprint("config load failed API_KEY=hunterSECRETvalue");
    assert!(redacted, "a secret-shaped value must be redacted");
    assert!(
        template.contains("<REDACTED:"),
        "expected redaction marker, got {template}"
    );
    assert!(
        !template.contains("hunterSECRETvalue"),
        "raw secret must not survive: {template}"
    );

    let (plain, redacted2) = fingerprint("nothing secret here index 5");
    assert!(!redacted2);
    assert_eq!(plain, "nothing secret here index <NUM>");
}
