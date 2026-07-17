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
        basis: None,
        temporal: None,
        summary: "frame resolves to symbol".to_owned(),
        producer: None,
    };
    let version = record_version(&edge);
    assert_eq!(version.domain, "log");
    assert_eq!(version.kind, "FRAME_RESOLVES_TO");
    assert_eq!(version.version, 3);
    assert!(
        is_known_record_version(&version),
        "(log, FRAME_RESOLVES_TO, 3) must be an accepted schema tuple"
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
    assert!(a.starts_with("log:v3:"), "got {a}");
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
fn schema_gate_accepts_v3_rejects_unknown() {
    use crate::schema_version::{RecordVersion, is_known_record_version};
    // Log domain is at schema v3 since issues #362/#364 (repository_id +
    // occurrence_timestamps); v1 and v2 are superseded and no longer accepted.
    for kind in [
        "LogSource",
        "ErrorSignature",
        "LogEvent",
        "LogOccurrenceBucket",
    ] {
        assert!(is_known_record_version(&RecordVersion::new("log", kind, 3)));
        assert!(!is_known_record_version(&RecordVersion::new(
            "log", kind, 2
        )));
        assert!(!is_known_record_version(&RecordVersion::new(
            "log", kind, 1
        )));
        assert!(!is_known_record_version(&RecordVersion::new(
            "log", kind, 999
        )));
    }
    for label in ["FINGERPRINTED_AS", "CAPTURED_FROM", "AGGREGATES"] {
        assert!(is_known_record_version(&RecordVersion::new(
            "log", label, 3
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

#[test]
fn safety_net_collapses_line_when_primary_pass_leaves_secret() {
    // The whole-value backstop must catch a secret the structure-preserving primary
    // pass leaves partially unredacted. An UNBALANCED quote around a whitespace-bearing
    // env secret is exactly such a shape: the quote-aware value boundary finds no
    // matching close and falls back to the delimiter boundary (so a stray quote can't
    // swallow the line), truncating the span at the first space and leaking the tail.
    let leaking = "PASSWORD=\"correct horse LEAKWORD staple'";
    // Prove the PRIMARY pass alone leaves the tail — this is the miss the net exists
    // to backstop.
    let (primary_only, _counts) =
        crate::redaction::redact_code_text(leaking.to_owned(), "<REDACTED:secret>");
    assert!(
        primary_only.contains("LEAKWORD"),
        "precondition: the structure-preserving pass alone must leave the tail: {primary_only}"
    );
    // The full capture path (primary pass + per-line safety net) must leave no secret
    // bytes: the net collapses the whole line.
    let netted = String::from_utf8(redacted_source_bytes(leaking)).expect("utf8");
    assert!(
        !netted.contains("LEAKWORD") && !netted.contains("horse") && !netted.contains("staple"),
        "the safety net must collapse the line the primary pass left leaking: {netted}"
    );
    assert!(
        netted.contains("<REDACTED:"),
        "the collapsed line carries a redaction marker: {netted}"
    );
}

#[test]
fn safety_net_leaves_secret_free_and_already_redacted_lines_unchanged() {
    // A normal secret-free line is untouched by the net.
    let clean = "2026-01-02T03:00:00Z INFO service starting up nominally\n";
    assert_eq!(
        String::from_utf8(redacted_source_bytes(clean)).expect("utf8"),
        clean,
        "a secret-free line must pass through the net verbatim"
    );
    // An already-redacted unquoted env value begins with the placeholder, so
    // `find_env_secret` skips it and `redact_value` returns it unchanged — the net must
    // NOT fire (no gratuitous over-redaction of an already-safe line).
    let already = "API_KEY=<REDACTED:secret>\n";
    assert_eq!(
        String::from_utf8(redacted_source_bytes(already)).expect("utf8"),
        already,
        "an already-redacted placeholder line must pass through the net unchanged"
    );
}

// ── issue #361: source-aware LogOccurrenceBucket identity ────────────────────

/// Scans `body` written to `dir/name` and returns the raw scan records.
fn scan_records(dir: &std::path::Path, name: &str, body: &str) -> Vec<GraphRecord> {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write log fixture");
    scan_log_records(&path, dir, "repo_test", "2026-01-02T03:00:00Z", false)
        .expect("scan should succeed")
        .records
}

/// Returns the single occurrence bucket's `(record_id, payload.source_id)`.
fn bucket_id_and_source(records: &[GraphRecord]) -> (String, String) {
    let mut found: Option<(String, String)> = None;
    for r in records {
        if let GraphRecord::Node {
            id,
            log: Some(payload),
            ..
        } = r
            && let LogPayload::LogOccurrenceBucket(b) = payload.as_ref()
        {
            assert!(found.is_none(), "expected exactly one occurrence bucket");
            found = Some((id.clone(), b.source_id.clone()));
        }
    }
    found.expect("scan produced one occurrence bucket")
}

/// Returns the single `ErrorSignature` record ID.
fn signature_id_of(records: &[GraphRecord]) -> String {
    records
        .iter()
        .find_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::ErrorSignature,
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .expect("scan produced one ErrorSignature")
}

const ONE_ERROR: &str = "2026-01-02T03:00:00Z [ERROR] widget checkout failed for order\n";

#[test]
fn rescanning_identical_source_yields_identical_bucket_ids() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = scan_records(dir.path(), "app.log", ONE_ERROR);
    let second = scan_records(dir.path(), "app.log", ONE_ERROR);
    let (id_a, src_a) = bucket_id_and_source(&first);
    let (id_b, src_b) = bucket_id_and_source(&second);
    assert_eq!(
        id_a, id_b,
        "a rescan of identical bytes mints the same bucket ID"
    );
    assert_eq!(src_a, src_b, "a rescan carries the same source_id");
}

#[test]
fn distinct_sources_same_signature_yield_distinct_bucket_ids() {
    let dir = tempfile::tempdir().expect("temp dir");
    // Two DIFFERENT log files (distinct paths) carrying the SAME error shape → the
    // SAME ErrorSignature, but distinct LogSource identities (two app instances).
    let a = scan_records(dir.path(), "instance-a.log", ONE_ERROR);
    let b = scan_records(dir.path(), "instance-b.log", ONE_ERROR);
    assert_eq!(
        signature_id_of(&a),
        signature_id_of(&b),
        "distinct sources with the same error share one signature ID"
    );
    let (id_a, src_a) = bucket_id_and_source(&a);
    let (id_b, src_b) = bucket_id_and_source(&b);
    assert_ne!(src_a, src_b, "distinct sources carry distinct source_id");
    assert_ne!(
        id_a, id_b,
        "distinct sources mint distinct bucket IDs (source-aware identity, #361)"
    );
}

#[test]
fn bucket_payload_carries_its_log_source_id() {
    let dir = tempfile::tempdir().expect("temp dir");
    let records = scan_records(dir.path(), "app.log", ONE_ERROR);
    let source_id = records
        .iter()
        .find_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::LogSource,
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .expect("scan produced a LogSource");
    let (_, bucket_source) = bucket_id_and_source(&records);
    assert_eq!(
        bucket_source, source_id,
        "a bucket's source_id is a handle to its LogSource"
    );
}

// ── issues #362 / #364: LOG_SCHEMA v3 — repository_id + occurrence_timestamps ──

#[test]
fn log_schema_version_is_three() {
    // Breaking bump 2 → 3 folding in #362 (repository_id) + #364
    // (occurrence_timestamps).
    assert_eq!(crate::ir::LOG_SCHEMA_VERSION, 3);
}

#[test]
fn log_ids_are_minted_with_v3_prefix() {
    // The version prefix flips automatically via the const, so every log record
    // ID is minted under `log:v3:`.
    let id = log_stable_id(&[
        "error_signature",
        "repo",
        FINGERPRINT_ALGORITHM,
        "t",
        "error",
    ]);
    assert!(id.starts_with("log:v3:"), "got {id}");
}

#[test]
fn repository_id_round_trips_on_all_four_payloads() {
    // #362: every log payload carries a retrievable `repository_id`.
    let src = LogSourcePayload {
        source_relative_path: "app.log".to_owned(),
        source_format_version: "plain-v1".to_owned(),
        source_artifact_hash: "hash".to_owned(),
        line_count: 3,
        repository_id: "acme/widget".to_owned(),
    };
    let back: LogSourcePayload =
        serde_json::from_str(&serde_json::to_string(&src).unwrap()).unwrap();
    assert_eq!(src, back);
    assert_eq!(back.repository_id, "acme/widget");

    let sig = ErrorSignaturePayload {
        fingerprint_algorithm: FINGERPRINT_ALGORITHM.to_owned(),
        template_excerpt: "boom".to_owned(),
        severity: "error".to_owned(),
        occurrence_count: 1,
        first_seen: "2026-01-02T03:00:00Z".to_owned(),
        last_seen: "2026-01-02T03:00:00Z".to_owned(),
        frames: None,
        repository_id: "acme/widget".to_owned(),
    };
    let back: ErrorSignaturePayload =
        serde_json::from_str(&serde_json::to_string(&sig).unwrap()).unwrap();
    assert_eq!(sig, back);
    assert_eq!(back.repository_id, "acme/widget");

    let ev = LogEventPayload {
        event_excerpt: "boom".to_owned(),
        event_content_hash: "ch".to_owned(),
        source_line: 1,
        severity: "error".to_owned(),
        repository_id: "acme/widget".to_owned(),
    };
    let back: LogEventPayload = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
    assert_eq!(ev, back);
    assert_eq!(back.repository_id, "acme/widget");

    let bucket = LogOccurrenceBucketPayload {
        bucket_start: "2026-01-02T03:00:00Z".to_owned(),
        bucket_width: "1h".to_owned(),
        occurrence_count: 2,
        source_id: "log:v3:abc".to_owned(),
        repository_id: "acme/widget".to_owned(),
        occurrence_timestamps: vec![
            "2026-01-02T03:00:00Z".to_owned(),
            "2026-01-02T03:45:00Z".to_owned(),
        ],
    };
    let back: LogOccurrenceBucketPayload =
        serde_json::from_str(&serde_json::to_string(&bucket).unwrap()).unwrap();
    assert_eq!(bucket, back);
    assert_eq!(back.repository_id, "acme/widget");
}

#[test]
fn occurrence_timestamps_round_trip_on_bucket() {
    // #364: the sorted per-occurrence timestamps round-trip through serde.
    let bucket = LogOccurrenceBucketPayload {
        bucket_start: "2026-01-02T12:00:00Z".to_owned(),
        bucket_width: "1h".to_owned(),
        occurrence_count: 3,
        source_id: "log:v3:src".to_owned(),
        repository_id: "acme/widget".to_owned(),
        occurrence_timestamps: vec![
            "2026-01-02T12:05:00Z".to_owned(),
            "2026-01-02T12:15:00Z".to_owned(),
            "2026-01-02T12:45:00Z".to_owned(),
        ],
    };
    let back: LogOccurrenceBucketPayload =
        serde_json::from_str(&serde_json::to_string(&bucket).unwrap()).unwrap();
    assert_eq!(
        back.occurrence_timestamps.len() as u64,
        back.occurrence_count
    );
    assert_eq!(
        back.occurrence_timestamps,
        vec![
            "2026-01-02T12:05:00Z".to_owned(),
            "2026-01-02T12:15:00Z".to_owned(),
            "2026-01-02T12:45:00Z".to_owned(),
        ]
    );
}

#[test]
fn legacy_v2_log_payloads_deserialize_with_serde_defaults() {
    // Back-compat (#362/#364): a legacy `log:v2:` JSON line lacking the new
    // fields still deserializes and degrades honestly (empty attribution / no
    // per-occurrence data), rather than a hard read failure.
    let legacy_src = r#"{"source_relative_path":"app.log","source_format_version":"plain-v1","source_artifact_hash":"h","line_count":3}"#;
    let src: LogSourcePayload = serde_json::from_str(legacy_src).unwrap();
    assert_eq!(src.repository_id, "");

    let legacy_sig = r#"{"fingerprint_algorithm":"template-v1","template_excerpt":"boom","severity":"error","occurrence_count":1,"first_seen":"2026-01-02T03:00:00Z","last_seen":"2026-01-02T03:00:00Z"}"#;
    let sig: ErrorSignaturePayload = serde_json::from_str(legacy_sig).unwrap();
    assert_eq!(sig.repository_id, "");

    let legacy_ev =
        r#"{"event_excerpt":"boom","event_content_hash":"ch","source_line":1,"severity":"error"}"#;
    let ev: LogEventPayload = serde_json::from_str(legacy_ev).unwrap();
    assert_eq!(ev.repository_id, "");

    let legacy_bucket = r#"{"bucket_start":"2026-01-02T03:00:00Z","bucket_width":"1h","occurrence_count":2,"source_id":"log:v2:abc"}"#;
    let bucket: LogOccurrenceBucketPayload = serde_json::from_str(legacy_bucket).unwrap();
    assert_eq!(bucket.repository_id, "");
    assert!(bucket.occurrence_timestamps.is_empty());
}
