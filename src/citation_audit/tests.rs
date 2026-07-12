//! Unit tests for the citation-completeness audit (issue #65).

use super::*;
use crate::ir::{EvidenceLink, NodeKind, SourceSpan};

fn mk_span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 10,
        start_line,
        end_line,
    }
}

fn node(id: &str, kind: NodeKind) -> GraphRecord {
    GraphRecord::node(id.to_owned(), kind, None, None, None, "summary".to_owned())
}

fn observation(id: &str, source_handle: Option<&str>, links: Vec<EvidenceLink>) -> GraphRecord {
    let mut rec = node(id, NodeKind::Observation);
    if let GraphRecord::Node {
        source_handle: ref mut sh,
        evidence_links: ref mut el,
        agent_id: ref mut aid,
        ..
    } = rec
    {
        *sh = source_handle.map(str::to_owned);
        *aid = Some("agent_1".to_owned());
        if !links.is_empty() {
            *el = Some(links);
        }
    }
    rec
}

fn link(target: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: "codegraph".to_owned(),
        relation: "OBSERVES".to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

// AC3/AC4: a code fact with a path and span is cited.
#[test]
fn classifies_code_fact_with_span_as_cited() {
    let mut sym = node("codegraph:v1:sym1", NodeKind::Symbol);
    if let GraphRecord::Node {
        repo_relative_path,
        span,
        ..
    } = &mut sym
    {
        repo_relative_path.replace("src/lib.rs".to_owned());
        *span = Some(mk_span(10, 20));
    }
    let result = classify_record(&sym);
    assert_eq!(result.row.status, CitationStatus::Cited);
    assert_eq!(result.row.trust_class, "source_fact");
    assert_eq!(
        result.row.primary_handle.as_deref(),
        Some("src/lib.rs:10-20")
    );
    assert!(result.diagnostic.is_none());
}

// AC4 "or documented absent-span reason": a span-less module is documented.
#[test]
fn classifies_spanless_module_as_absent_handle_documented() {
    let mut module = node("codegraph:v1:mod1", NodeKind::Module);
    if let GraphRecord::Node {
        repo_relative_path, ..
    } = &mut module
    {
        repo_relative_path.replace("src/lib.rs".to_owned());
    }
    let result = classify_record(&module);
    assert_eq!(result.row.status, CitationStatus::AbsentHandleDocumented);
    assert_eq!(
        result.row.absent_handle_reason,
        Some(AbsentHandleRule::NoSpanModuleLevel)
    );
    assert!(result.diagnostic.is_none());
}

// Issue #180 (PR #314 review): a manifest-declared dependency fact is a
// deterministic source fact, cited by its repo-relative Cargo.toml path —
// span absence is legitimate for a whole-manifest handle, mirroring `File`.
#[test]
fn classifies_dependency_declaration_as_path_cited_source_fact() {
    let mut dep = node("codegraph:v5:dep1", NodeKind::DependencyDeclaration);
    if let GraphRecord::Node {
        repo_relative_path,
        name,
        ..
    } = &mut dep
    {
        repo_relative_path.replace("crates/pkg-a/Cargo.toml".to_owned());
        name.replace("serde".to_owned());
    }
    assert_eq!(citation_trust_class(&dep), "source_fact");
    let result = classify_record(&dep);
    assert_eq!(result.row.trust_class, "source_fact");
    assert_eq!(result.row.status, CitationStatus::Cited);
    assert_eq!(
        result.row.primary_handle.as_deref(),
        Some("crates/pkg-a/Cargo.toml"),
        "the manifest path is the citation handle"
    );
    assert!(result.diagnostic.is_none());
}

// Issue #180 fail path: a dependency fact without its manifest path carries no
// usable handle and must fail the gate rather than pass as documented-absent.
#[test]
fn dependency_declaration_without_manifest_path_is_missing_required() {
    let dep = node("codegraph:v5:dep2", NodeKind::DependencyDeclaration);
    let result = classify_record(&dep);
    assert_eq!(result.row.trust_class, "source_fact");
    assert_eq!(result.row.status, CitationStatus::MissingRequiredHandle);
}

// AC4 fail path: a code fact with neither path nor span is missing + diagnosed.
#[test]
fn code_fact_without_path_or_span_is_missing_required() {
    let sym = node("codegraph:v1:sym2", NodeKind::Symbol);
    let result = classify_record(&sym);
    assert_eq!(result.row.status, CitationStatus::MissingRequiredHandle);
    assert_eq!(result.diagnostic.unwrap().0, "missing_span");
}

// AC6: an agent claim's own record ID never counts as its own evidence.
#[test]
fn agent_authored_row_not_its_own_evidence() {
    let self_only = observation(
        "agent_memory:v1:obs1",
        None,
        vec![link("agent_memory:v1:obs1")],
    );
    let result = classify_record(&self_only);
    assert_eq!(result.row.status, CitationStatus::MissingRequiredHandle);

    let external = observation(
        "agent_memory:v1:obs2",
        None,
        vec![link("codegraph:v1:sym1")],
    );
    let result = classify_record(&external);
    assert_eq!(result.row.status, CitationStatus::Cited);
    assert_eq!(
        result.row.primary_handle.as_deref(),
        Some("codegraph:v1:sym1")
    );
}

// AC5: an agent-memory row without any source handle is missing.
#[test]
fn agent_memory_without_source_handle_is_missing() {
    let bare = observation("agent_memory:v1:obs3", None, vec![]);
    assert_eq!(
        classify_record(&bare).row.status,
        CitationStatus::MissingRequiredHandle
    );
    let with_source = observation("agent_memory:v1:obs4", Some("traj/run.traj"), vec![]);
    assert_eq!(
        classify_record(&with_source).row.status,
        CitationStatus::Cited
    );
}

// AC5: a project row requires a task/source handle.
#[test]
fn project_task_requires_task_handle() {
    let bare_task = node("project:v1:task1", NodeKind::Task);
    assert_eq!(
        classify_record(&bare_task).row.status,
        CitationStatus::MissingRequiredHandle
    );
    let mut task = node("project:v1:task2", NodeKind::Task);
    if let GraphRecord::Node { entity_id, .. } = &mut task {
        entity_id.replace("task-2".to_owned());
    }
    let result = classify_record(&task);
    assert_eq!(result.row.status, CitationStatus::Cited);
    assert_eq!(result.row.trust_class, "project_state");
}

// AC5: a user-context row requires a policy-audit handle.
#[test]
fn user_context_requires_policy_handle() {
    let bare = node("user_context:v1:pref1", NodeKind::Preference);
    let result = classify_record(&bare);
    assert_eq!(result.row.trust_class, "user_context");
    assert_eq!(result.row.status, CitationStatus::MissingRequiredHandle);

    let mut pref = node("user_context:v1:pref2", NodeKind::Preference);
    if let GraphRecord::Node { user_context, .. } = &mut pref {
        user_context.approval_decision_id = Some("user_context:v1:dec1".to_owned());
    }
    assert_eq!(classify_record(&pref).row.status, CitationStatus::Cited);
}

// AC5: a verification row is inherently citable by its own evidence handle.
#[test]
fn verification_row_cited_by_own_handle() {
    let ver = node("verification:v1:v1", NodeKind::Verification);
    let result = classify_record(&ver);
    assert_eq!(result.row.trust_class, "verification_evidence");
    assert_eq!(result.row.status, CitationStatus::Cited);
}

// AC5/AC8: a row referencing a protected payload is excluded, not counted.
#[test]
fn protected_payload_row_excluded_not_counted() {
    let handle = format!(
        "{}{}",
        crate::protected::PROTECTED_HANDLE_PREFIX,
        "a".repeat(64)
    );
    let mut artifact = node("artifact:v1:patch1", NodeKind::PatchArtifact);
    if let GraphRecord::Node { source_handle, .. } = &mut artifact {
        source_handle.replace(handle);
    }
    let result = classify_record(&artifact);
    assert_eq!(result.row.status, CitationStatus::ExcludedProtected);
    assert_eq!(result.diagnostic.unwrap().0, "protected_payload");
}

// AC11: only the existing trust vocabulary (+ user_context) is emitted.
#[test]
fn trust_class_strings_match_existing_vocab() {
    let allowed = [
        "source_fact",
        "agent_authored",
        "verification_evidence",
        "project_state",
        "artifact",
        "user_context",
        "runtime_observation",
        "other",
    ];
    for kind in [
        NodeKind::Symbol,
        NodeKind::Observation,
        NodeKind::Verification,
        NodeKind::Task,
        NodeKind::PatchArtifact,
        NodeKind::Preference,
        NodeKind::Agent,
        NodeKind::EmbeddingModel,
        NodeKind::DependencyDeclaration,
        NodeKind::LogSource,
        NodeKind::ErrorSignature,
    ] {
        let rec = node("id", kind);
        assert!(
            allowed.contains(&citation_trust_class(&rec)),
            "unexpected trust class for {kind:?}"
        );
    }
}

// ── #328: runtime_observation (log-domain) classification ──────────────────

fn log_source_node(id: &str, path: &str, hash: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::LogSource,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        "log source".to_owned(),
    )
    .with_domain("log", crate::ir::LOG_SCHEMA_VERSION)
    .with_log(LogPayload::LogSource(LogSourcePayload {
        source_relative_path: path.to_owned(),
        source_format_version: "plain-v1".to_owned(),
        source_artifact_hash: hash.to_owned(),
        line_count: 10,
    }))
}

fn error_signature_node(id: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::ErrorSignature,
        None,
        None,
        Some("error signature".to_owned()),
        "error signature".to_owned(),
    )
    .with_domain("log", crate::ir::LOG_SCHEMA_VERSION)
    .with_log(LogPayload::ErrorSignature(
        crate::ir::ErrorSignaturePayload {
            fingerprint_algorithm: "template-v1".to_owned(),
            template_excerpt: "boom".to_owned(),
            severity: "error".to_owned(),
            occurrence_count: 1,
            first_seen: "2026-01-02T12:00:00Z".to_owned(),
            last_seen: "2026-01-02T13:00:00Z".to_owned(),
            frames: None,
        },
    ))
}

fn captured_from(signature_id: &str, source_id: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::CapturedFrom,
        signature_id.to_owned(),
        source_id.to_owned(),
        None,
        "captured from".to_owned(),
    )
}

// #328: a LogSource is cited from its own payload (path + source_artifact_hash).
#[test]
fn log_source_cited_from_own_payload() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "hash1"]);
    let src = log_source_node(&src_id, "app.log", "abc123");
    let index = LogProvenanceIndex::build(std::slice::from_ref(&src));
    let result = classify_log_handle(&index, &src);
    assert_eq!(result.row.trust_class, "runtime_observation");
    assert_eq!(result.row.status, CitationStatus::Cited);
    assert_eq!(result.row.primary_handle.as_deref(), Some("app.log@abc123"));
}

// #328: an ErrorSignature is cited via an at-least-one present CAPTURED_FROM
// LogSource carrying a source_artifact_hash.
#[test]
fn error_signature_cited_via_captured_from_source() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "hash1"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
    ];
    let index = LogProvenanceIndex::build(&records);
    let result = classify_log_handle(&index, &records[1]);
    assert_eq!(result.row.status, CitationStatus::Cited);
    assert_eq!(result.row.primary_handle.as_deref(), Some("app.log@abc123"));
}

// #328: an ErrorSignature with no resolvable CAPTURED_FROM LogSource is a
// citation failure — never counted as cited by its own ID.
#[test]
fn error_signature_without_source_is_missing_required() {
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let sig = error_signature_node(&sig_id);
    let index = LogProvenanceIndex::build(std::slice::from_ref(&sig));
    let result = classify_log_handle(&index, &sig);
    assert_eq!(result.row.status, CitationStatus::MissingRequiredHandle);
    assert_eq!(result.diagnostic.unwrap().0, "missing_required_handle");
}

// #328: log node IDs exclude the source, so a signature may carry MULTIPLE
// CAPTURED_FROM edges to distinct LogSources — provenance is at-least-one, and a
// signature whose first source lacks a hash still resolves via a later one.
#[test]
fn error_signature_multiple_captured_from_at_least_one() {
    let src_a = crate::ir::log_stable_id(&["log_source", "repo", "a.log", "h"]);
    let src_b = crate::ir::log_stable_id(&["log_source", "repo", "b.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let records = vec![
        // First source has an EMPTY hash (not a valid citation on its own).
        log_source_node(&src_a, "a.log", ""),
        // Second source carries a real hash.
        log_source_node(&src_b, "b.log", "hashB"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_a),
        captured_from(&sig_id, &src_b),
    ];
    let index = LogProvenanceIndex::build(&records);
    let result = classify_log_handle(&index, &records[2]);
    assert_eq!(
        result.row.status,
        CitationStatus::Cited,
        "an at-least-one present source with a hash cites the signature"
    );
    assert_eq!(result.row.primary_handle.as_deref(), Some("b.log@hashB"));
}

// AC4: the gate fails below the threshold and passes when fully cited.
#[test]
fn gate_fails_below_threshold_passes_when_cited() {
    // A single cited symbol → 100% code completeness → pass.
    let mut sym = node("codegraph:v1:symA", NodeKind::Symbol);
    if let GraphRecord::Node {
        repo_relative_path,
        span,
        name,
        ..
    } = &mut sym
    {
        repo_relative_path.replace("src/lib.rs".to_owned());
        *span = Some(mk_span(1, 5));
        name.replace("alpha".to_owned());
    }
    let report = run_citation_audit(std::slice::from_ref(&sym), &AuditConfig::default());
    assert!(report.gate.code_gate_pass);
    assert!((report.gate.code_citation_completeness - 1.0).abs() < 1e-9);

    // Add a span-less symbol (a real code-answer miss) → completeness 0.5 → fail.
    let mut bad = node("codegraph:v1:symB", NodeKind::Symbol);
    if let GraphRecord::Node { name, .. } = &mut bad {
        name.replace("beta".to_owned());
    }
    let report = run_citation_audit(&[sym, bad], &AuditConfig::default());
    assert!(!report.gate.code_gate_pass);
    assert!(!report.ok);
}

// PR #314 review: `manifest-deps` is a registered citation-audit workflow —
// dependency rows must carry record_id + manifest handle, and a handle-less
// row must fail the gate.
#[test]
fn manifest_deps_workflow_gates_dependency_rows() {
    // Real extractor output: a plain entry plus a `package = "…"` rename pair
    // (spanless facts cited by the manifest path).
    let records = crate::manifest_deps::manifest_dependency_records(
        "repo-id",
        "Cargo.toml",
        "[package]\nname = \"pkg\"\n\n[dependencies]\nembedded-hal = \"0.2\"\nembedded-hal-1 = { package = \"embedded-hal\", version = \"1\" }\n",
        &crate::manifest_deps::LockfileStatus::Absent,
        None,
    );
    assert_eq!(records.len(), 2, "both rename-pair entries seed the audit");

    let report = run_citation_audit(&records, &AuditConfig::default());
    let workflow = report
        .workflows
        .iter()
        .find(|w| w.workflow == "manifest-deps")
        .expect("manifest-deps must be a registered audit workflow");
    assert!(workflow.enabled);
    assert_eq!(workflow.trust_class, "source_fact");
    assert_eq!(workflow.rows.len(), 2, "one row per declared entry");
    for row in &workflow.rows {
        assert_eq!(row.trust_class, "source_fact");
        assert_eq!(row.status, CitationStatus::Cited);
        assert_eq!(
            row.primary_handle.as_deref(),
            Some("Cargo.toml"),
            "rows are cited by their repo-relative manifest handle"
        );
        assert!(row.record_id.starts_with("codegraph:v"));
    }
    assert!(report.gate.code_gate_pass);

    // A dependency row missing its manifest handle is a real code-answer miss
    // and must fail the gate.
    let bad = node(
        "codegraph:v5:dep-without-handle",
        NodeKind::DependencyDeclaration,
    );
    let mut with_bad = records;
    with_bad.push(bad);
    let report = run_citation_audit(&with_bad, &AuditConfig::default());
    assert!(
        !report.gate.code_gate_pass,
        "a handle-less dependency row must fail the citation gate"
    );
    assert!(!report.ok);
}

// AC2/AC3: the semantic workflow reports a stable disabled reason over --graph.
#[test]
fn semantic_disabled_reason_stable_over_graph() {
    let report = run_citation_audit(&[], &AuditConfig::default());
    let semantic = report
        .workflows
        .iter()
        .find(|w| w.workflow == "semantic")
        .expect("semantic workflow present");
    assert!(!semantic.enabled);
    assert_eq!(semantic.disabled_reason, Some("requires_embedded_store"));
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == "unsupported_workflow" && d.workflow == "semantic")
    );
}

// AC9: diagnostics are canonically sorted and de-duplicated.
#[test]
fn diagnostics_sorted_and_stable() {
    let bare = observation("agent_memory:v1:obsX", None, vec![]);
    let report = run_citation_audit(std::slice::from_ref(&bare), &AuditConfig::default());
    let mut sorted = report.diagnostics.clone();
    sorted.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    assert_eq!(report.diagnostics, sorted);
}

// Review #4: a pathless `Commit` (e.g. from `eg query changes`) is documented
// spanless, not `missing_required_handle`.
#[test]
fn pathless_commit_is_documented_spanless() {
    let commit = node("codegraph:v1:commit1", NodeKind::Commit);
    let result = classify_record(&commit);
    assert_eq!(result.row.status, CitationStatus::AbsentHandleDocumented);
    assert_eq!(
        result.row.absent_handle_reason,
        Some(AbsentHandleRule::NoSpanModuleLevel)
    );
}

// Review #6: an empty primary handle is not a citation.
#[test]
fn empty_handle_is_missing_not_cited() {
    let mut task = node("project:v1:task_e", NodeKind::Task);
    if let GraphRecord::Node { entity_id, .. } = &mut task {
        entity_id.replace(String::new());
    }
    assert_eq!(
        classify_record(&task).row.status,
        CitationStatus::MissingRequiredHandle
    );

    let mut pref = node("user_context:v1:pref_e", NodeKind::Preference);
    if let GraphRecord::Node { user_context, .. } = &mut pref {
        user_context.approval_decision_id = Some(String::new());
    }
    assert_eq!(
        classify_record(&pref).row.status,
        CitationStatus::MissingRequiredHandle
    );
}

// Review #8: a verification row with a withheld output handle (no protected
// prefix string) is excluded as a protected payload, matching the public audits.
#[test]
fn withheld_output_handle_is_excluded_protected() {
    let mut cmd = node("verification:v1:cmd1", NodeKind::CommandEvidence);
    if let GraphRecord::Node { stdout_handle, .. } = &mut cmd {
        *stdout_handle = Some(Box::new(crate::ir::OutputHandle {
            inline: None,
            hash: "blake3:withheld".to_owned(),
            bytes: 2048,
        }));
    }
    let result = classify_record(&cmd);
    assert_eq!(result.row.status, CitationStatus::ExcludedProtected);
    assert_eq!(result.diagnostic.unwrap().0, "protected_payload");
}

// Review #9: an artifact lacking any source/provenance handle cannot be cited by
// its own record ID.
#[test]
fn artifact_without_source_handle_is_missing() {
    let bare = node("artifact:v1:art1", NodeKind::Artifact);
    assert_eq!(
        classify_record(&bare).row.status,
        CitationStatus::MissingRequiredHandle
    );
    let mut with_source = node("artifact:v1:art2", NodeKind::Artifact);
    if let GraphRecord::Node {
        source_artifact_hash,
        ..
    } = &mut with_source
    {
        source_artifact_hash.replace("blake3:abc".to_owned());
    }
    assert_eq!(
        classify_record(&with_source).row.status,
        CitationStatus::Cited
    );
}

// Review #1 (P1): a `File` source fact from a real scan has a repo-relative path
// but no span, and must be path-cited rather than `missing_required_handle`.
#[test]
fn file_source_fact_is_path_cited() {
    let mut file = node("codegraph:v1:file1", NodeKind::File);
    if let GraphRecord::Node {
        repo_relative_path, ..
    } = &mut file
    {
        repo_relative_path.replace("src/lib.rs".to_owned());
    }
    let result = classify_record(&file);
    assert_eq!(result.row.status, CitationStatus::Cited);
    assert_eq!(result.row.trust_class, "source_fact");
    assert_eq!(result.row.primary_handle.as_deref(), Some("src/lib.rs"));
    assert!(result.diagnostic.is_none());
}

// Review round 4: an agent claim is citable by source-artifact provenance or by
// its agent/session handle (matching public memory/context output), but a row
// with no provenance at all is missing.
#[test]
fn agent_provenance_handles_are_accepted() {
    // source_artifact_path provenance.
    let mut by_artifact = node("agent_memory:v1:obs_a", NodeKind::Observation);
    if let GraphRecord::Node {
        source_artifact_path,
        ..
    } = &mut by_artifact
    {
        source_artifact_path.replace("trajectories/run.traj".to_owned());
    }
    assert_eq!(
        classify_record(&by_artifact).row.status,
        CitationStatus::Cited
    );

    // agent_id + session_id session-provenance handle.
    let mut by_session = node("agent_memory:v1:obs_b", NodeKind::Observation);
    if let GraphRecord::Node {
        agent_id,
        session_id,
        ..
    } = &mut by_session
    {
        *agent_id = Some("agent_1".to_owned());
        *session_id = Some("sess_1".to_owned());
    }
    let result = classify_record(&by_session);
    assert_eq!(result.row.status, CitationStatus::Cited);
    assert_eq!(result.row.primary_handle.as_deref(), Some("agent_1:sess_1"));

    // No provenance at all → missing.
    let bare = node("agent_memory:v1:obs_c", NodeKind::Observation);
    assert_eq!(
        classify_record(&bare).row.status,
        CitationStatus::MissingRequiredHandle
    );
}

// Round-9 review: `protected_payload_diagnostics` byte-filters only stdout/stderr;
// a zero-byte `body`/`diff_hunk`/`arguments`/`result` handle is still emitted as a
// protected payload, so the audit must exclude it as protected rather than count it
// as an ordinary returned row.
#[test]
fn zero_byte_body_handle_is_excluded_protected() {
    let mut task = node("project:v1:task_zp", NodeKind::Task);
    if let GraphRecord::Node { body_handle, .. } = &mut task {
        *body_handle = Some(Box::new(crate::ir::OutputHandle {
            inline: None,
            hash: "blake3:zero-body".to_owned(),
            bytes: 0,
        }));
    }
    assert_eq!(
        referenced_protected_handle(&task).as_deref(),
        Some("blake3:zero-body")
    );

    // A zero-byte stdout stream stays an ordinary row (byte-filtered like the public
    // diagnostics), so it is not treated as a protected payload.
    let mut cmd = node("verification:v1:cmd_zs", NodeKind::CommandEvidence);
    if let GraphRecord::Node { stdout_handle, .. } = &mut cmd {
        *stdout_handle = Some(Box::new(crate::ir::OutputHandle {
            inline: None,
            hash: "blake3:zero-stdout".to_owned(),
            bytes: 0,
        }));
    }
    assert_eq!(referenced_protected_handle(&cmd), None);
}

// Round-9 review: `eg query subsystem <prefix>` accepts a bare file path as a
// prefix (exact-path match is "under" the prefix), so a repo-root source fact with
// no `/` is a real subsystem entry point and must stay in the audited prefix set.
#[test]
fn subsystem_prefixes_include_root_level_paths() {
    let file = |id: &str, path: &str| {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::File,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            format!("file {path}"),
        )
    };
    let records = vec![
        file("codegraph:v1:f_root", "build.rs"),
        file("codegraph:v1:f_nested", "src/lib.rs"),
    ];
    let prefixes = subsystem_prefixes(&records);
    assert!(
        prefixes.contains("build.rs"),
        "root-level file kept as its own prefix: {prefixes:?}"
    );
    assert!(
        prefixes.contains("src"),
        "nested file keeps its parent-dir prefix: {prefixes:?}"
    );
}

// ── #328 AC3: dangling / tombstoned FRAME_RESOLVES_TO targets ──────────────
//
// A resolved backtrace frame is a CODE row audited under the code-handle rule.
// When log-deltas surfaces a frame whose target record is DANGLING (absent from
// the record set) or TOMBSTONED-and-unsuperseded, the frame is still a public
// code row with no resolvable citation handle: AC3 ("dangling never counts as
// cited") requires it be counted as a `MissingRequiredHandle` code-lane failure,
// never silently dropped.

fn commit_node(sha: &str, parents: &[&str], valid_time: &str) -> GraphRecord {
    GraphRecord::node(
        format!("codegraph:v1:commit_{sha}"),
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("commit {sha}"),
    )
    .with_temporal(crate::ir::TemporalMetadata {
        git_commit: sha.to_owned(),
        git_parent_commits: parents.iter().map(|p| (*p).to_owned()).collect(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    })
}

fn frame_resolves_to(signature_id: &str, target: &str) -> GraphRecord {
    GraphRecord::Edge {
        id: crate::ir::log_stable_id(&[
            "edge",
            "FRAME_RESOLVES_TO",
            signature_id,
            "0",
            target,
            "resolved",
        ]),
        schema_version: crate::ir::LOG_SCHEMA_VERSION,
        label: EdgeLabel::FrameResolvesTo,
        source: signature_id.to_owned(),
        target: target.to_owned(),
        confidence: Some("1.0".to_owned()),
        resolution: None,
        frame_resolution: Some(crate::ir::FrameResolution::Resolved),
        frame_index: Some(0),
        basis: None,
        temporal: None,
        summary: format!("frame 0 resolves to {target}"),
        producer: None,
    }
}

// A two-commit range plus one `new`-in-window ErrorSignature (default first_seen
// 2026-01-02T12:00:00Z falls inside the [c1, c3] window) cited by its
// CAPTURED_FROM LogSource, plus a FRAME_RESOLVES_TO edge onto `frame_target`.
// `extra` carries any additional records (e.g. the frame target node + tombstone).
fn log_deltas_frame_scenario(frame_target: &str, extra: Vec<GraphRecord>) -> Vec<GraphRecord> {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let mut records = vec![
        commit_node("c1sha", &[], "2026-01-01T00:00:00Z"),
        commit_node("c3sha", &["c1sha"], "2026-01-03T00:00:00Z"),
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        frame_resolves_to(&sig_id, frame_target),
    ];
    records.extend(extra);
    records
}

fn log_deltas_frame_row<'a>(
    report: &'a CitationAuditReport,
    target: &str,
) -> &'a RowClassification {
    let workflow = report
        .workflows
        .iter()
        .find(|w| w.workflow == "log-deltas")
        .expect("log-deltas is a registered audit workflow");
    assert!(
        workflow.enabled,
        "log-deltas lane must be enabled for the seeded range"
    );
    workflow
        .rows
        .iter()
        .find(|r| r.record_id == target)
        .unwrap_or_else(|| {
            panic!(
                "frame target {target} must surface as a log-deltas row, never be dropped; rows: {:?}",
                workflow.rows
            )
        })
}

// #328 AC3 (the bug): a FRAME_RESOLVES_TO edge whose target record is ABSENT
// from the graph must count as a missing-handle CODE-lane citation failure, not
// be silently skipped out of the totals.
#[test]
fn dangling_frame_target_counts_as_missing_citation() {
    let ghost = "codegraph:v1:ghost_symbol";
    let records = log_deltas_frame_scenario(ghost, vec![]);
    let report = run_citation_audit(&records, &AuditConfig::default());

    let row = log_deltas_frame_row(&report, ghost);
    assert_eq!(
        row.status,
        CitationStatus::MissingRequiredHandle,
        "a dangling frame target is never cited"
    );
    assert_eq!(
        row.trust_class, "source_fact",
        "a frame row is a code row and lands in the code-citation lane"
    );
    // The missing row carries a classifying diagnostic (never an unclassified miss).
    assert_eq!(report.gate.unclassified_missing_rows, 0);
    // The dangling code row drags the code lane below the default gate.
    assert!(
        !report.gate.code_gate_pass,
        "an uncited public code row must fail the code gate"
    );
    assert!(!report.ok);
}

// #328 AC3: a FRAME_RESOLVES_TO edge whose target is present but
// TOMBSTONED-and-unsuperseded is likewise not a valid citation.
#[test]
fn tombstoned_frame_target_counts_as_missing_citation() {
    let dead = "codegraph:v1:retracted_symbol";
    let mut dead_symbol = node(dead, NodeKind::Symbol);
    if let GraphRecord::Node {
        repo_relative_path,
        span,
        ..
    } = &mut dead_symbol
    {
        // Even with a well-formed file/span, a tombstoned-and-unsuperseded target
        // is not a live citation.
        repo_relative_path.replace("src/gone.rs".to_owned());
        *span = Some(mk_span(1, 5));
    }
    let tombstone = GraphRecord::Tombstone {
        id: "codegraph:v1:tomb_retracted".to_owned(),
        schema_version: 1,
        deleted_id: dead.to_owned(),
        summary: "symbol removed".to_owned(),
        producer: None,
    };
    let records = log_deltas_frame_scenario(dead, vec![dead_symbol, tombstone]);
    let report = run_citation_audit(&records, &AuditConfig::default());

    let row = log_deltas_frame_row(&report, dead);
    assert_eq!(
        row.status,
        CitationStatus::MissingRequiredHandle,
        "a tombstoned-and-unsuperseded frame target is never cited"
    );
    assert_eq!(row.trust_class, "source_fact");
    assert!(!report.gate.code_gate_pass);
    assert!(!report.ok);
}

// #328 AC3 (guard the correct case): a frame resolving to a PRESENT node is
// audited under the code-handle rule and stays cited — the fix for the dangling
// case must not regress a genuinely-resolved frame.
#[test]
fn present_frame_target_stays_cited() {
    let live = "codegraph:v1:live_symbol";
    let mut live_symbol = node(live, NodeKind::Symbol);
    if let GraphRecord::Node {
        repo_relative_path,
        span,
        ..
    } = &mut live_symbol
    {
        repo_relative_path.replace("src/lib.rs".to_owned());
        *span = Some(mk_span(10, 20));
    }
    let records = log_deltas_frame_scenario(live, vec![live_symbol]);
    let report = run_citation_audit(&records, &AuditConfig::default());

    let row = log_deltas_frame_row(&report, live);
    assert_eq!(
        row.status,
        CitationStatus::Cited,
        "a resolved frame onto a present symbol is cited by its file/span"
    );
    assert_eq!(row.trust_class, "source_fact");
    assert!(report.gate.code_gate_pass);
    assert!(report.ok);
}

// Round-9 review: with two disconnected commit chains in one store, pairing
// root/tip extrema across chains yields a `NoPath` that disables the whole lane.
// `changes_range` must return a base/head pair proven connected by parent topology.
#[test]
fn changes_range_picks_a_connected_pair() {
    let commit = |sha: &str, parents: &[&str]| -> GraphRecord {
        GraphRecord::node(
            format!("codegraph:v1:commit_{sha}"),
            NodeKind::Commit,
            None,
            None,
            Some(sha.to_owned()),
            format!("commit {sha}"),
        )
        .with_temporal(crate::ir::TemporalMetadata {
            git_commit: sha.to_owned(),
            git_parent_commits: parents.iter().map(|s| (*s).to_owned()).collect(),
            valid_time: "2026-01-01T00:00:00Z".to_owned(),
            author_time: Some("2026-01-01T00:00:00Z".to_owned()),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_time_source: None,
        })
    };
    // Chain A: a0 → a1.  Chain B: b0 → b1.  No edge connects the chains.
    let records = vec![
        commit("a0", &[]),
        commit("a1", &["a0"]),
        commit("b0", &[]),
        commit("b1", &["b0"]),
    ];
    let (base, head) = changes_range(&records).expect("a connected pair exists");
    let connected = (base == "a0" && head == "a1") || (base == "b0" && head == "b1");
    assert!(
        connected,
        "expected a parent-connected in-chain pair, got ({base}, {head})"
    );
}
