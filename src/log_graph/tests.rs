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

// ── frame-resolution closed set (issue #322) ─────────────────────────────────

#[test]
fn frame_resolution_as_str_and_from_wire_round_trip() {
    use crate::ir::FrameResolution;
    for value in [
        FrameResolution::Resolved,
        FrameResolution::Ambiguous,
        FrameResolution::PathOnly,
        FrameResolution::Unresolved,
    ] {
        assert_eq!(
            FrameResolution::from_wire(value.as_str()),
            Some(value),
            "{} must round-trip through as_str/from_wire",
            value.as_str()
        );
    }
    // Closed set: an unknown token is never coerced into a member.
    assert_eq!(FrameResolution::from_wire("external"), None);
    assert_eq!(FrameResolution::from_wire("nonsense"), None);
    // Explicit wire spellings are stable.
    assert_eq!(FrameResolution::PathOnly.as_str(), "path_only");
    assert_eq!(FrameResolution::Unresolved.as_str(), "unresolved");
}

#[test]
fn frame_resolves_to_schema_tuple_is_known() {
    use crate::ir::{EdgeLabel, GraphRecord, LOG_SCHEMA_VERSION};
    use crate::schema_version::{is_known_record_version, record_version};
    let edge = GraphRecord::Edge {
        id: log_stable_id(&["edge", "FRAME_RESOLVES_TO", "repo", "a", "b"]),
        schema_version: LOG_SCHEMA_VERSION,
        label: EdgeLabel::FrameResolvesTo,
        source: "a".to_owned(),
        target: "b".to_owned(),
        confidence: None,
        resolution: None,
        frame_resolution: Some(crate::ir::FrameResolution::Resolved),
        frame_index: Some(0),
        temporal: None,
        summary: "frame resolves to symbol".to_owned(),
        producer: None,
    };
    let version = record_version(&edge);
    assert_eq!(version.domain, "log");
    assert_eq!(version.kind, "FRAME_RESOLVES_TO");
    assert_eq!(version.version, 1);
    assert!(
        is_known_record_version(&version),
        "(log, FRAME_RESOLVES_TO, 1) must be an accepted schema tuple"
    );
}

// ── structured backtrace frame capture (issue #322) ──────────────────────────

#[test]
fn parse_frames_captures_rust_backtrace_shape() {
    let repo = std::path::Path::new(".");
    let text = "thread 'main' panicked at 'boom', src/alpha.rs:10:5\n\
                stack backtrace:\n\
                   0: myapp::alpha::do_thing\n\
                             at src/alpha.rs:10\n\
                   1: myapp::shared::helper\n\
                             at src/shared.rs:5:9\n\
                   2: core::panicking::panic\n\
                             at /rustc/abc123/library/core/src/panicking.rs:50";
    let frames = parse_frames(text, repo);
    assert_eq!(frames.len(), 3, "three backtrace frames parsed");

    assert_eq!(frames[0].frame_index, 0);
    assert_eq!(
        frames[0].module_path.as_deref(),
        Some("myapp::alpha::do_thing")
    );
    assert_eq!(frames[0].file_path.as_deref(), Some("src/alpha.rs"));
    assert_eq!(frames[0].line, Some(10));

    assert_eq!(frames[1].frame_index, 1);
    assert_eq!(frames[1].file_path.as_deref(), Some("src/shared.rs"));
    assert_eq!(frames[1].line, Some(5));

    // External toolchain path is generalized from the /rustc/ anchor: no
    // absolute host prefix, and the module root is a stdlib crate.
    assert_eq!(frames[2].frame_index, 2);
    assert_eq!(
        frames[2].file_path.as_deref(),
        Some("rustc/abc123/library/core/src/panicking.rs")
    );
    assert_eq!(
        frames[2].module_path.as_deref(),
        Some("core::panicking::panic")
    );
}

#[test]
fn parse_frames_strips_absolute_repo_prefix() {
    // An absolute in-repo path under the repo root normalizes to repo-relative.
    let temp = std::env::temp_dir();
    let root = temp.join("egregore_frame_test_root");
    let _ = std::fs::create_dir_all(root.join("src"));
    let abs = format!("{}/src/beta.rs", root.to_string_lossy().replace('\\', "/"));
    let text =
        format!("ERROR crash\nstack backtrace:\n   0: app::beta::run\n             at {abs}:7");
    let frames = parse_frames(&text, &root);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].file_path.as_deref(), Some("src/beta.rs"));
    assert_eq!(frames[0].line, Some(7));
}

#[test]
fn parse_frames_absent_without_backtrace() {
    let frames = parse_frames(
        "ERROR just a one-line error, no backtrace",
        std::path::Path::new("."),
    );
    assert!(frames.is_empty(), "no frame lines means no frames");
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
