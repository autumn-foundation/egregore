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
        repository_id: String::new(),
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
            repository_id: String::new(),
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
        is_exhaustive: None,
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

// Issue #421 (transport parity): a frame target re-ingested AFTER its own
// tombstone is live again over `--graph` — matching the embedded `--data-dir`
// current-state read — so `node_visible` (now latest-write-wins) counts it as a
// live citation, not a `MissingRequiredHandle`. Before the fix, raw tombstone
// membership hid the revived symbol and diverged from `--data-dir`.
#[test]
fn frame_target_reingested_after_tombstone_is_cited() {
    fn cited_symbol(id: &str) -> GraphRecord {
        let mut sym = GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 10,
                end_line: 20,
            }),
            None,
            "summary".to_owned(),
        );
        if let GraphRecord::Node { name, .. } = &mut sym {
            *name = Some("revived".to_owned());
        }
        sym
    }
    let revived = "codegraph:v1:revived_symbol";
    let tombstone = GraphRecord::Tombstone {
        id: "codegraph:v1:tomb_revived".to_owned(),
        schema_version: 1,
        deleted_id: revived.to_owned(),
        summary: "symbol removed".to_owned(),
        producer: None,
    };
    // Append order: symbol, Tombstone(symbol), symbol again → latest write wins.
    let records = log_deltas_frame_scenario(
        revived,
        vec![cited_symbol(revived), tombstone, cited_symbol(revived)],
    );
    let report = run_citation_audit(&records, &AuditConfig::default());

    let row = log_deltas_frame_row(&report, revived);
    assert_eq!(
        row.status,
        CitationStatus::Cited,
        "a frame target revived after its tombstone is a live citation (latest write wins)"
    );
    assert_eq!(row.trust_class, "source_fact");
    assert!(report.gate.code_gate_pass);
    assert!(report.ok);
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

// ── #328 Finding A: class-wide runtime_observation provenance ──────────────
//
// The `runtime_observation` citation requirement is CLASS-WIDE ("every row"):
// a log record surfaced through ANY workflow — not just `eg query log-deltas` —
// must carry its full log citation. Here `eg query memory` returns an
// ErrorSignature as supporting evidence; an ErrorSignature with no CAPTURED_FROM
// LogSource has no resolvable provenance and must be a citation FAILURE, never
// counted as cited by its own ID just because a non-log-deltas workflow reached
// it via the context-free catch-all classifier.
#[test]
fn runtime_observation_via_memory_requires_provenance_classwide() {
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let log_link = EvidenceLink {
        target_record_id: Some(sig_id.clone()),
        target_domain: "log".to_owned(),
        relation: "OBSERVES".to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    };
    // The Observation is itself cited (external evidence link + source handle);
    // the ErrorSignature it cites has NO CAPTURED_FROM edge → no LogSource.
    let obs = observation(
        "agent_memory:v1:obs_log",
        Some("traj/run.traj"),
        vec![log_link],
    );
    let sig = error_signature_node(&sig_id);
    let records = vec![obs, sig];
    let report = run_citation_audit(&records, &AuditConfig::default());

    let memory = report
        .workflows
        .iter()
        .find(|w| w.workflow == "memory")
        .expect("memory workflow present");
    let row = memory
        .rows
        .iter()
        .find(|r| r.record_id == sig_id)
        .expect("the ErrorSignature must surface as a memory supporting-evidence row");
    assert_eq!(row.trust_class, "runtime_observation");
    assert_eq!(
        row.status,
        CitationStatus::MissingRequiredHandle,
        "a provenance-less runtime observation is never cited, regardless of surfacing workflow"
    );
    assert!(
        !report.gate.log_gate_pass,
        "an uncited runtime observation must fail the log gate"
    );
    assert!(!report.ok);
    // #376: the `below_log_citation_threshold` diagnostic names the SPECIFIC
    // workflow whose runtime-observation rows fell short — here the `memory`
    // lane surfaced the provenance-less signature — never a hard-coded
    // `log-deltas`. (The same signature also surfaces via the `error-context`
    // lane, which names itself too.)
    let named: Vec<&str> = report
        .diagnostics
        .iter()
        .filter(|d| d.code == "below_log_citation_threshold")
        .map(|d| d.workflow)
        .collect();
    assert!(
        named.contains(&"memory"),
        "the memory lane must name itself in the below-threshold diagnostic: {named:?}"
    );
    assert!(
        !named.contains(&"log-deltas"),
        "the log-deltas lane is disabled here (no commit range) and must not be named: {named:?}"
    );
}

// ── #372: the shared external classifier applies runtime provenance ─────────
//
// `eg audit citations` (via `WorkflowBuilder::push_record`) and the evidence-pack
// / bundle citation gates (via `classify_record_external_with_provenance`) MUST
// agree on every `runtime_observation` row: both route through the SAME
// `classify_record_provenanced` derivation against a `LogProvenanceIndex`. A
// provenance-less `ErrorSignature` therefore classifies `MissingRequiredHandle`
// on BOTH surfaces — never `Cited`-by-its-own-ID via the context-free catch-all
// (the loophole #372 closes). Mirrors the #338/#339 single-implementation
// invariant.
#[test]
fn external_provenance_classifier_matches_audit_per_log_row() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "hash1"]);
    let cited_sig = crate::ir::log_stable_id(&["error_signature", "repo", "cited", "error"]);
    let bare_sig = crate::ir::log_stable_id(&["error_signature", "repo", "bare", "error"]);

    // Two Observations surface the two signatures through the `memory` workflow
    // (the class-wide requirement holds regardless of surfacing workflow).
    let obs_cited = observation(
        "agent_memory:v1:obs_cited",
        Some("traj/run.traj"),
        vec![EvidenceLink {
            target_record_id: Some(cited_sig.clone()),
            target_domain: "log".to_owned(),
            relation: "OBSERVES".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }],
    );
    let obs_bare = observation(
        "agent_memory:v1:obs_bare",
        Some("traj/run.traj"),
        vec![EvidenceLink {
            target_record_id: Some(bare_sig.clone()),
            target_domain: "log".to_owned(),
            relation: "OBSERVES".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }],
    );

    let records = vec![
        // (ii) properly-cited: LogSource + CAPTURED_FROM resolves provenance.
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&cited_sig),
        captured_from(&cited_sig, &src_id),
        // (i) provenance-less: no LogSource, no CAPTURED_FROM.
        error_signature_node(&bare_sig),
        obs_cited,
        obs_bare,
    ];

    let report = run_citation_audit(&records, &AuditConfig::default());
    let provenance = CitationProvenance::build(&records);

    // Gather the audit's status for every runtime_observation row, per record ID.
    let mut audit_status: std::collections::BTreeMap<String, CitationStatus> =
        std::collections::BTreeMap::new();
    for workflow in &report.workflows {
        for row in &workflow.rows {
            if row.trust_class == "runtime_observation" {
                if let Some(prev) = audit_status.get(&row.record_id) {
                    assert_eq!(
                        *prev, row.status,
                        "the audit must classify {} consistently across workflows",
                        row.record_id
                    );
                }
                audit_status.insert(row.record_id.clone(), row.status);
            }
        }
    }

    // Both signatures surfaced (via memory), and each one's audit status equals
    // the shared external sibling's status — the single-derivation invariant.
    for (id, expected) in [
        (&cited_sig, CitationStatus::Cited),
        (&bare_sig, CitationStatus::MissingRequiredHandle),
    ] {
        let sig = records
            .iter()
            .find(|r| r.id() == id.as_str())
            .expect("signature record present");
        let sibling = classify_record_external_with_provenance(sig, &provenance);
        assert_eq!(
            sibling.trust_class, "runtime_observation",
            "the sibling classifies {id} as a runtime observation"
        );
        assert_eq!(
            sibling.status, expected,
            "the sibling status for {id} must match the intended provenance verdict"
        );
        let audited = audit_status
            .get(id.as_str())
            .expect("the audit surfaced this runtime observation as a row");
        assert_eq!(
            *audited, sibling.status,
            "audit and sibling must agree on {id} — same shared derivation (#372)"
        );
    }

    // Document the gap #372 closes: the context-free `classify_record_external`
    // would (wrongly) count the provenance-less signature as Cited by its own ID.
    let bare = records
        .iter()
        .find(|r| r.id() == bare_sig.as_str())
        .expect("bare signature present");
    assert_eq!(
        classify_record_external(bare).status,
        CitationStatus::Cited,
        "the context-free classifier cites a provenance-less log row by its own ID \
         (the #372 loophole) — the provenance-aware sibling must NOT"
    );
}

// ── #376: the two remaining log query workflows are citation-gated ─────────
//
// `eg audit citations` now drives three log query workflows (`log-deltas`,
// `error-context`, `log_signatures`). Each surfaces `runtime_observation` rows
// that are gated by the SAME class-wide provenance rule, and a below-threshold
// lane names ITSELF in the `below_log_citation_threshold` diagnostic.

/// A Symbol node at `path` with a well-formed span — a live code frame target.
fn symbol_at(id: &str, path: &str) -> GraphRecord {
    let mut sym = node(id, NodeKind::Symbol);
    if let GraphRecord::Node {
        repo_relative_path,
        span,
        ..
    } = &mut sym
    {
        repo_relative_path.replace(path.to_owned());
        *span = Some(mk_span(10, 20));
    }
    sym
}

/// The `below_log_citation_threshold` diagnostic workflow names in a report.
fn below_log_workflows(report: &CitationAuditReport) -> Vec<&'static str> {
    report
        .diagnostics
        .iter()
        .filter(|d| d.code == "below_log_citation_threshold")
        .map(|d| d.workflow)
        .collect()
}

// #376 RED (error-context): a well-formed `log:v1:` ErrorSignature with NO
// CAPTURED_FROM/LogSource provenance, surfaced through the error-context lane,
// is a `MissingRequiredHandle` runtime observation that fails the log gate and
// makes the lane name itself in the diagnostic.
#[test]
fn error_context_uncited_signature_fails_log_gate() {
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let records = vec![error_signature_node(&sig_id)];
    let report = run_citation_audit(&records, &AuditConfig::default());

    let ec = report
        .workflows
        .iter()
        .find(|w| w.workflow == "error-context")
        .expect("error-context workflow present");
    assert!(
        ec.enabled,
        "error-context is enabled when a signature is present"
    );
    let row = ec
        .rows
        .iter()
        .find(|r| r.record_id == sig_id)
        .expect("the signature surfaces as an error-context row");
    assert_eq!(row.trust_class, "runtime_observation");
    assert_eq!(
        row.status,
        CitationStatus::MissingRequiredHandle,
        "a provenance-less signature is never cited via the error-context lane"
    );
    assert!(!report.gate.log_gate_pass);
    assert!(!report.ok);
    assert!(
        below_log_workflows(&report).contains(&"error-context"),
        "the error-context lane must name itself: {:?}",
        below_log_workflows(&report)
    );
}

// #376 GREEN (error-context): the same signature WITH a valid CAPTURED_FROM →
// LogSource(non-empty hash) is cited, and the lane contributes no diagnostic.
#[test]
fn error_context_cited_signature_passes() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
    ];
    let report = run_citation_audit(&records, &AuditConfig::default());

    let ec = report
        .workflows
        .iter()
        .find(|w| w.workflow == "error-context")
        .expect("error-context workflow present");
    let row = ec
        .rows
        .iter()
        .find(|r| r.record_id == sig_id)
        .expect("the signature surfaces as an error-context row");
    assert_eq!(
        row.status,
        CitationStatus::Cited,
        "a signature with CAPTURED_FROM provenance is cited"
    );
    assert!(report.gate.log_gate_pass);
    assert!(report.ok);
    assert!(
        !below_log_workflows(&report).contains(&"error-context"),
        "a fully cited error-context lane emits no below-threshold diagnostic"
    );
}

// ── #376 (Codex P2): the error-context CROSS-DOMAIN sections are gated ──────
//
// `eg query error-context` returns not only the signature + frame rows but the
// full `query context` cross-domain bundle (`source_facts`, `observations`,
// `project_state`, `artifacts`, `verification_evidence`). An EMITTED_DURING
// `CommandRun` lands in `verification_evidence`; when its target record is
// absent/tombstoned the public query still exposes the row, so the audit must
// classify it a citation failure — otherwise the gate can pass while a returned
// row is uncited.

/// An `EMITTED_DURING` edge from a signature to an agent/command run.
fn emitted_during(signature_id: &str, run_id: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::EmittedDuring,
        signature_id.to_owned(),
        run_id.to_owned(),
        None,
        "emitted during".to_owned(),
    )
}

/// The `error-context` workflow's classified row for `id`, or a panic naming the
/// gap (the row must never be silently dropped from the lane).
fn error_context_row<'a>(report: &'a CitationAuditReport, id: &str) -> &'a RowClassification {
    let workflow = report
        .workflows
        .iter()
        .find(|w| w.workflow == "error-context")
        .expect("error-context is a registered audit workflow");
    assert!(workflow.enabled, "error-context lane must be enabled");
    workflow
        .rows
        .iter()
        .find(|r| r.record_id == id)
        .unwrap_or_else(|| {
            panic!(
                "cross-domain row {id} must surface as an error-context row, never be dropped; \
                 rows: {:?}",
                workflow.rows
            )
        })
}

// #376 RED (error-context cross-domain): a signature cited via CAPTURED_FROM
// carries an EMITTED_DURING edge to a TOMBSTONED CommandRun. `eg query
// error-context` returns that run in `verification_evidence`, so the audit must
// count it a `MissingRequiredHandle` non-code failure — never silently skip it.
// This test fails against the pre-fix lane (which iterated only signatures +
// frames), proving the gap.
#[test]
fn error_context_cross_domain_tombstoned_command_run_fails_gate() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let run_id = "codegraph:v1:tombstoned_command_run";
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        node(run_id, NodeKind::CommandRun),
        emitted_during(&sig_id, run_id),
        GraphRecord::Tombstone {
            id: "codegraph:v1:tomb_run".to_owned(),
            schema_version: 1,
            deleted_id: run_id.to_owned(),
            summary: "command run retracted".to_owned(),
            producer: None,
        },
    ];
    let report = run_citation_audit(&records, &AuditConfig::default());

    // The signature itself stays cited — only the cross-domain row is the defect.
    assert_eq!(
        error_context_row(&report, &sig_id).status,
        CitationStatus::Cited,
        "the CAPTURED_FROM-cited signature is not the failing row"
    );
    let row = error_context_row(&report, run_id);
    assert_eq!(
        row.status,
        CitationStatus::MissingRequiredHandle,
        "a tombstoned EMITTED_DURING CommandRun is never a valid citation"
    );
    assert_eq!(
        row.trust_class, "verification_evidence",
        "an EMITTED_DURING CommandRun is a verification-evidence row"
    );
    // The miss carries a classifying diagnostic (never an unclassified miss)…
    assert_eq!(report.gate.unclassified_missing_rows, 0);
    // …and the error-context lane names itself as the source of the miss.
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == "missing_required_handle"
                && d.workflow == "error-context"
                && d.source_record_id.as_deref() == Some(run_id)),
        "the error-context lane must name itself for the missing cross-domain row: {:?}",
        report.diagnostics
    );
    assert!(
        !report.gate.non_code_handle_gate_pass,
        "an uncited cross-domain verification row fails the non-code handle gate"
    );
    assert!(!report.ok);
}

// #376 GREEN (error-context cross-domain): the same scenario with the CommandRun
// PRESENT (not tombstoned) → the verification row is cited by its own evidence
// handle, no gate fails, and the error-context lane contributes no
// missing-handle diagnostic.
#[test]
fn error_context_cross_domain_present_command_run_is_cited() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let run_id = "codegraph:v1:live_command_run";
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        node(run_id, NodeKind::CommandRun),
        emitted_during(&sig_id, run_id),
    ];
    let report = run_citation_audit(&records, &AuditConfig::default());

    let row = error_context_row(&report, run_id);
    assert_eq!(
        row.status,
        CitationStatus::Cited,
        "a present CommandRun is cited by its own stable evidence handle"
    );
    assert_eq!(row.trust_class, "verification_evidence");
    assert!(report.gate.non_code_handle_gate_pass);
    assert!(report.ok);
    assert!(
        !report
            .diagnostics
            .iter()
            .any(|d| d.code == "missing_required_handle" && d.workflow == "error-context"),
        "a fully cited error-context cross-domain bundle emits no missing-handle diagnostic: {:?}",
        report.diagnostics
    );
}

// #376: the report over a corpus that exercises the error-context cross-domain
// sections is byte-identical across runs (deterministic serialized output).
#[test]
fn error_context_cross_domain_report_is_byte_identical() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let run_id = "codegraph:v1:tombstoned_command_run";
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        node(run_id, NodeKind::CommandRun),
        emitted_during(&sig_id, run_id),
        GraphRecord::Tombstone {
            id: "codegraph:v1:tomb_run".to_owned(),
            schema_version: 1,
            deleted_id: run_id.to_owned(),
            summary: "command run retracted".to_owned(),
            producer: None,
        },
    ];
    let first =
        serde_json::to_string(&run_citation_audit(&records, &AuditConfig::default())).unwrap();
    for _ in 0..2 {
        let again =
            serde_json::to_string(&run_citation_audit(&records, &AuditConfig::default())).unwrap();
        assert_eq!(first, again, "audit output must be deterministic");
    }
}

// ── #376 (Codex P2, third/fourth instance): every returned envelope row ──────
//
// The error-context response returns more row shapes than the signature + frame
// + cross-domain sections the first pass covered: each `SignatureBlock` also
// carries `buckets` (`LogOccurrenceBucket` runtime-observation rows), and the
// cross-domain `source_facts` preserves MULTIPLE temporal versions of one stable
// ID keyed by `(record_id, git_commit)`. The audit must classify every one of
// those rows from its OWN identity, never let an uncited bucket or a collapsed
// temporal version escape the gate.

/// A `LogOccurrenceBucket` runtime-observation node (issue #320).
fn occurrence_bucket_node(id: &str, bucket_start: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::LogOccurrenceBucket,
        None,
        None,
        None,
        "occurrence bucket".to_owned(),
    )
    .with_domain("log", crate::ir::LOG_SCHEMA_VERSION)
    .with_log(LogPayload::LogOccurrenceBucket(
        crate::ir::LogOccurrenceBucketPayload {
            bucket_start: bucket_start.to_owned(),
            bucket_width: "1h".to_owned(),
            occurrence_count: 3,
            source_id: "log:v2:fixture-source".to_owned(),
            repository_id: String::new(),
            occurrence_timestamps: Vec::new(),
        },
    ))
}

/// An `AGGREGATES` edge (bucket → signature). `error_context` reads it to attach
/// the bucket to the signature block; provenance resolves the reverse direction
/// (bucket → signature → `CAPTURED_FROM` → `LogSource`).
fn aggregates_edge(bucket_id: &str, signature_id: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Aggregates,
        bucket_id.to_owned(),
        signature_id.to_owned(),
        None,
        "aggregates".to_owned(),
    )
}

/// One temporal version of a code symbol under a specific commit. A `path`
/// yields a cited (path + span) version; `None` yields a handle-less version that
/// the code lane must classify `MissingRequiredHandle`.
fn symbol_version(id: &str, path: Option<&str>, git_commit: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        path.map(str::to_owned),
        path.map(|_| mk_span(10, 20)),
        Some("boom_handler".to_owned()),
        "symbol".to_owned(),
    )
    .with_temporal(crate::ir::TemporalMetadata {
        git_commit: git_commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
}

// #376 RED (buckets): a signature cited via CAPTURED_FROM carries a
// `LogOccurrenceBucket` whose AGGREGATES edge to that signature is RETRACTED, so
// the bucket's provenance to its `LogSource` is unreachable. `eg query
// error-context` still returns the bucket in `signatures[].buckets`, so the audit
// must classify it a `MissingRequiredHandle` runtime observation and fail the log
// gate. Fails against the pre-fix lane, which never classified buckets at all.
#[test]
fn error_context_uncited_bucket_fails_log_gate() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let bucket_id = crate::ir::log_stable_id(&["bucket", "repo", "sig", "2026-01-02T12"]);
    let agg = aggregates_edge(&bucket_id, &sig_id);
    let agg_id = agg.id().to_owned();
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        occurrence_bucket_node(&bucket_id, "2026-01-02T12:00:00Z"),
        agg,
        GraphRecord::Tombstone {
            id: "log:v1:tomb_agg".to_owned(),
            schema_version: 1,
            deleted_id: agg_id,
            summary: "aggregates edge retracted".to_owned(),
            producer: None,
        },
    ];
    let report = run_citation_audit(&records, &AuditConfig::default());

    // The signature itself stays cited — only the bucket is the defect.
    assert_eq!(
        error_context_row(&report, &sig_id).status,
        CitationStatus::Cited,
        "the CAPTURED_FROM-cited signature is not the failing row"
    );
    let bucket = error_context_row(&report, &bucket_id);
    assert_eq!(
        bucket.trust_class, "runtime_observation",
        "an occurrence bucket is a runtime-observation row"
    );
    assert_eq!(
        bucket.status,
        CitationStatus::MissingRequiredHandle,
        "a bucket with unreachable provenance is never a valid citation"
    );
    assert!(!report.gate.log_gate_pass);
    assert!(!report.ok);
    assert!(
        below_log_workflows(&report).contains(&"error-context"),
        "the error-context lane must name itself: {:?}",
        below_log_workflows(&report)
    );
}

// #376 GREEN (buckets): the same bucket with an INTACT AGGREGATES edge resolves
// its `LogSource` provenance through the signature and is cited; the log gate
// passes and the lane contributes no below-threshold diagnostic.
#[test]
fn error_context_cited_bucket_passes() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let bucket_id = crate::ir::log_stable_id(&["bucket", "repo", "sig", "2026-01-02T12"]);
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        occurrence_bucket_node(&bucket_id, "2026-01-02T12:00:00Z"),
        aggregates_edge(&bucket_id, &sig_id),
    ];
    let report = run_citation_audit(&records, &AuditConfig::default());

    let bucket = error_context_row(&report, &bucket_id);
    assert_eq!(
        bucket.status,
        CitationStatus::Cited,
        "a bucket that resolves LogSource provenance via AGGREGATES → signature → \
         CAPTURED_FROM is cited"
    );
    assert!(report.gate.log_gate_pass);
    assert!(report.ok);
    assert!(
        !below_log_workflows(&report).contains(&"error-context"),
        "a fully cited error-context lane emits no below-threshold diagnostic"
    );
}

// #376 RED (temporal collapse): a scan-history-style graph where the frame-target
// symbol exists in TWO temporal versions (distinct git_commits) — one cited
// (path + span), one uncited (no handle). `eg query error-context` returns BOTH
// in `source_facts` keyed by `(record_id, git_commit)`. The uncited version is
// ordered LAST-in-`by_id` (cited), so the pre-fix ID-only lookup classified only
// the cited version and let the uncited one escape the code gate entirely. This
// test pins that: BOTH versions must be classified, one a citation failure.
#[test]
fn error_context_temporal_source_fact_versions_not_collapsed() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let sym_id = "codegraph:v1:boom_handler";
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        frame_resolves_to(&sig_id, sym_id),
        // Uncited version FIRST, cited version LAST: an ID-only `by_id` keeps the
        // cited version, so pre-fix the collapsed single row is cited and the
        // uncited version silently escapes the gate.
        symbol_version(sym_id, None, "c_uncited"),
        symbol_version(sym_id, Some("src/lib.rs"), "c_cited"),
    ];
    let report = run_citation_audit(&records, &AuditConfig::default());
    let ec = report
        .workflows
        .iter()
        .find(|w| w.workflow == "error-context")
        .expect("error-context workflow present");
    let sym_rows: Vec<&RowClassification> =
        ec.rows.iter().filter(|r| r.record_id == sym_id).collect();
    assert_eq!(
        sym_rows.len(),
        2,
        "both temporal versions must be classified, never collapsed to one: {sym_rows:?}"
    );
    assert!(
        sym_rows.iter().any(|r| r.status == CitationStatus::Cited),
        "the path+span version is cited"
    );
    assert!(
        sym_rows
            .iter()
            .any(|r| r.status == CitationStatus::MissingRequiredHandle),
        "the handle-less version is a code-lane citation failure that must not escape"
    );
    assert!(!report.gate.code_gate_pass);
    assert!(!report.ok);
}

// #376 GREEN (temporal collapse): both versions carry a path + span, so both are
// cited, the code gate passes, and the lane emits no missing-handle diagnostic.
#[test]
fn error_context_temporal_source_fact_versions_all_cited_pass() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let sym_id = "codegraph:v1:boom_handler";
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        frame_resolves_to(&sig_id, sym_id),
        symbol_version(sym_id, Some("src/lib.rs"), "c1"),
        symbol_version(sym_id, Some("src/lib.rs"), "c2"),
    ];
    let report = run_citation_audit(&records, &AuditConfig::default());
    let ec = report
        .workflows
        .iter()
        .find(|w| w.workflow == "error-context")
        .expect("error-context workflow present");
    let sym_rows: Vec<&RowClassification> =
        ec.rows.iter().filter(|r| r.record_id == sym_id).collect();
    assert_eq!(sym_rows.len(), 2, "both cited versions are classified");
    assert!(
        sym_rows.iter().all(|r| r.status == CitationStatus::Cited),
        "both path+span versions are cited: {sym_rows:?}"
    );
    assert!(report.gate.code_gate_pass);
    assert!(report.ok);
    assert!(
        !report
            .diagnostics
            .iter()
            .any(|d| d.code == "missing_required_handle" && d.workflow == "error-context"),
        "a fully cited error-context lane emits no missing-handle diagnostic: {:?}",
        report.diagnostics
    );
}

// #376 structural completeness invariant: build a MAXIMAL error-context envelope
// — every section the response can return populated (signatures with buckets AND
// frames, all five cross-domain sections including a source-fact record_id in TWO
// temporal versions, unresolved, and excluded) — then walk every row the envelope
// ACTUALLY contains and assert the audit's error-context lane classifies EXACTLY
// that set: no returned row missing (completeness), none invented (soundness). If
// a future field is added to `ErrorContext` and returned to users but not
// classified, this test fails rather than letting the row silently escape.
#[test]
#[allow(clippy::too_many_lines)]
fn error_context_audit_classifies_every_envelope_row() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let bucket_id = crate::ir::log_stable_id(&["bucket", "repo", "sig", "2026-01-02T12"]);
    let sym_id = "codegraph:v1:boom_handler";
    let task_id = "project:v1:task_boom";
    let cmd_id = "verification:v1:cmd_boom";
    let run_id = "agent_memory:v1:run_boom";
    let art_id = "artifact:v1:patch_boom";
    let obs_new_id = "agent_memory:v1:obs_new";
    let obs_old_id = "agent_memory:v1:obs_old";
    let missing_evidence = "verification:v1:absent_evidence";

    // obs_new OBSERVES the symbol and carries an evidence link to an ABSENT
    // verification record → unresolved; obs_old is superseded by obs_new →
    // excluded.
    let validated_by_missing = EvidenceLink {
        target_record_id: Some(missing_evidence.to_owned()),
        target_domain: "verification".to_owned(),
        relation: "VALIDATED_BY".to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    };
    let obs_new = observation(
        obs_new_id,
        Some("agent_1:sess"),
        vec![link(sym_id), validated_by_missing],
    );
    let obs_old = observation(obs_old_id, Some("agent_1:sess"), vec![link(sym_id)]);
    let supersedes = GraphRecord::agent_memory_edge(
        EdgeLabel::Supersedes,
        obs_new_id.to_owned(),
        obs_old_id.to_owned(),
        None,
        "supersedes".to_owned(),
    );
    let observes_new = GraphRecord::agent_memory_edge(
        EdgeLabel::Observes,
        obs_new_id.to_owned(),
        sym_id.to_owned(),
        None,
        "observes".to_owned(),
    );
    let observes_old = GraphRecord::agent_memory_edge(
        EdgeLabel::Observes,
        obs_old_id.to_owned(),
        sym_id.to_owned(),
        None,
        "observes".to_owned(),
    );
    // EMITTED_DURING(sig → agent_run) → observations; run PRODUCED_PATCH artifact
    // → artifacts. EMITTED_DURING(sig → command_run) → verification_evidence.
    // REFERENCES_TASK(sig → task) → project_state.
    let produced_patch = GraphRecord::agent_memory_edge(
        EdgeLabel::ProducedPatch,
        run_id.to_owned(),
        art_id.to_owned(),
        None,
        "produced patch".to_owned(),
    );
    let references_task = GraphRecord::edge(
        EdgeLabel::ReferencesTask,
        sig_id.clone(),
        task_id.to_owned(),
        None,
        "references task".to_owned(),
    );

    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        occurrence_bucket_node(&bucket_id, "2026-01-02T12:00:00Z"),
        aggregates_edge(&bucket_id, &sig_id),
        frame_resolves_to(&sig_id, sym_id),
        symbol_version(sym_id, Some("src/lib.rs"), "c1"),
        symbol_version(sym_id, Some("src/lib.rs"), "c2"),
        obs_new,
        obs_old,
        supersedes,
        observes_new,
        observes_old,
        node(cmd_id, NodeKind::CommandRun),
        emitted_during(&sig_id, cmd_id),
        node(run_id, NodeKind::AgentRun),
        emitted_during(&sig_id, run_id),
        node(art_id, NodeKind::PatchArtifact),
        produced_patch,
        node(task_id, NodeKind::Task),
        references_task,
    ];

    // Walk the ACTUAL public envelope the query returns.
    let ctx = error_context(
        &records,
        &sig_id,
        None,
        None,
        None,
        SupersessionMode::Exclude,
        None,
        false,
    )
    .expect("resolve maximal envelope");

    // Every section the mandate enumerates is populated.
    assert!(
        ctx.signatures.iter().any(|b| !b.buckets.is_empty()),
        "signatures carry buckets"
    );
    assert!(
        ctx.signatures.iter().any(|b| !b.frames.is_empty()),
        "signatures carry frames"
    );
    assert!(!ctx.source_facts.is_empty(), "source_facts populated");
    assert!(!ctx.observations.is_empty(), "observations populated");
    assert!(!ctx.project_state.is_empty(), "project_state populated");
    assert!(!ctx.artifacts.is_empty(), "artifacts populated");
    assert!(
        !ctx.verification_evidence.is_empty(),
        "verification_evidence populated"
    );
    assert!(!ctx.unresolved.is_empty(), "unresolved populated");
    assert!(!ctx.excluded.is_empty(), "excluded populated");
    assert_eq!(
        ctx.source_facts
            .iter()
            .filter(|r| r.record_id == sym_id)
            .count(),
        2,
        "source_facts returns the symbol in two temporal versions"
    );

    // Expected ROW record-id set: every row the envelope actually contains.
    // `unresolved` becomes a diagnostic (not a row), so it is excluded here.
    let mut expected: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for block in &ctx.signatures {
        expected.insert(block.record_id.clone());
        for bucket in &block.buckets {
            expected.insert(bucket.record_id.clone());
        }
        for frame in &block.frames {
            expected.insert(frame.target_record_id.clone());
        }
    }
    for row in ctx
        .source_facts
        .iter()
        .chain(&ctx.observations)
        .chain(&ctx.project_state)
        .chain(&ctx.artifacts)
        .chain(&ctx.verification_evidence)
    {
        expected.insert(row.record_id.clone());
    }
    for excluded in &ctx.excluded {
        expected.insert(excluded.record_id.clone());
    }

    let report = run_citation_audit(&records, &AuditConfig::default());
    let ec = report
        .workflows
        .iter()
        .find(|w| w.workflow == "error-context")
        .expect("error-context workflow present");
    assert!(ec.enabled, "error-context lane must be enabled");
    let classified: std::collections::BTreeSet<String> =
        ec.rows.iter().map(|r| r.record_id.clone()).collect();

    assert_eq!(
        classified, expected,
        "the audit must classify EXACTLY the rows the envelope returns — no returned \
         row missing, none invented"
    );
    // Precision: the two temporal versions of the symbol are two classified rows.
    assert_eq!(
        ec.rows.iter().filter(|r| r.record_id == sym_id).count(),
        2,
        "temporal versions must not collapse to one classified row"
    );
}

// #376 RED (log_signatures): a signature whose FRAME_RESOLVES_TO edge resolves
// under a driven subsystem prefix (so it enters the `log_signatures` section)
// but carries NO CAPTURED_FROM provenance is a `MissingRequiredHandle` runtime
// observation that fails the log gate and makes the lane name itself.
#[test]
fn log_signatures_uncited_signature_fails_log_gate() {
    let sym_id = "codegraph:v1:frame_target";
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let records = vec![
        symbol_at(sym_id, "src/lib.rs"),
        error_signature_node(&sig_id),
        frame_resolves_to(&sig_id, sym_id),
    ];
    let report = run_citation_audit(&records, &AuditConfig::default());

    let ls = report
        .workflows
        .iter()
        .find(|w| w.workflow == "log_signatures")
        .expect("log_signatures workflow present");
    assert!(
        ls.enabled,
        "log_signatures is enabled when a signature resolves under a prefix"
    );
    let row = ls
        .rows
        .iter()
        .find(|r| r.record_id == sig_id)
        .expect("the signature surfaces as a log_signatures row");
    assert_eq!(row.trust_class, "runtime_observation");
    assert_eq!(row.status, CitationStatus::MissingRequiredHandle);
    assert!(!report.gate.log_gate_pass);
    assert!(!report.ok);
    assert!(
        below_log_workflows(&report).contains(&"log_signatures"),
        "the log_signatures lane must name itself: {:?}",
        below_log_workflows(&report)
    );
}

// #376 GREEN (log_signatures): the same frame-resolved signature WITH valid
// CAPTURED_FROM provenance is cited and the lane contributes no diagnostic.
#[test]
fn log_signatures_cited_signature_passes() {
    let sym_id = "codegraph:v1:frame_target";
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let records = vec![
        symbol_at(sym_id, "src/lib.rs"),
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        frame_resolves_to(&sig_id, sym_id),
    ];
    let report = run_citation_audit(&records, &AuditConfig::default());

    let ls = report
        .workflows
        .iter()
        .find(|w| w.workflow == "log_signatures")
        .expect("log_signatures workflow present");
    let row = ls
        .rows
        .iter()
        .find(|r| r.record_id == sig_id)
        .expect("the signature surfaces as a log_signatures row");
    assert_eq!(
        row.status,
        CitationStatus::Cited,
        "a frame-resolved signature with CAPTURED_FROM provenance is cited"
    );
    assert!(report.gate.log_gate_pass);
    assert!(report.ok);
    assert!(
        !below_log_workflows(&report).contains(&"log_signatures"),
        "a fully cited log_signatures lane emits no below-threshold diagnostic"
    );
}

// #376: the report over a combined log corpus driving all three log lanes is
// byte-identical across runs (deterministic serialized output).
#[test]
fn three_log_lanes_report_is_byte_identical() {
    let sym_id = "codegraph:v1:frame_target";
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let records = vec![
        symbol_at(sym_id, "src/lib.rs"),
        commit_node("c1sha", &[], "2026-01-01T00:00:00Z"),
        commit_node("c3sha", &["c1sha"], "2026-01-03T00:00:00Z"),
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        frame_resolves_to(&sig_id, sym_id),
    ];
    let first =
        serde_json::to_string(&run_citation_audit(&records, &AuditConfig::default())).unwrap();
    for _ in 0..3 {
        let again =
            serde_json::to_string(&run_citation_audit(&records, &AuditConfig::default())).unwrap();
        assert_eq!(first, again, "audit output must be deterministic");
    }
}

// ── #328 Finding B: tombstoned log provenance is not reachable ─────────────
//
// `LogProvenanceIndex` must ignore a TOMBSTONED CAPTURED_FROM/AGGREGATES edge and
// a TOMBSTONED-and-unsuperseded LogSource target, mirroring the node/frame paths.
#[test]
fn tombstoned_captured_from_edge_is_not_provenance() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let edge = captured_from(&sig_id, &src_id);
    let edge_id = edge.id().to_owned();
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        edge,
        GraphRecord::Tombstone {
            id: "log:v1:tomb_edge".to_owned(),
            schema_version: crate::ir::LOG_SCHEMA_VERSION,
            deleted_id: edge_id,
            summary: "edge removed".to_owned(),
            producer: None,
        },
    ];
    let index = LogProvenanceIndex::build(&records);
    let result = classify_log_handle(&index, &records[1]);
    assert_eq!(
        result.row.status,
        CitationStatus::MissingRequiredHandle,
        "a signature whose only CAPTURED_FROM edge is tombstoned has no reachable provenance"
    );
}

#[test]
fn tombstoned_log_source_target_is_not_provenance() {
    let src_id = crate::ir::log_stable_id(&["log_source", "repo", "app.log", "h"]);
    let sig_id = crate::ir::log_stable_id(&["error_signature", "repo", "tpl", "error"]);
    let records = vec![
        log_source_node(&src_id, "app.log", "abc123"),
        error_signature_node(&sig_id),
        captured_from(&sig_id, &src_id),
        GraphRecord::Tombstone {
            id: "log:v1:tomb_src".to_owned(),
            schema_version: crate::ir::LOG_SCHEMA_VERSION,
            deleted_id: src_id.clone(),
            summary: "log source removed".to_owned(),
            producer: None,
        },
    ];
    let index = LogProvenanceIndex::build(&records);
    let result = classify_log_handle(&index, &records[1]);
    assert_eq!(
        result.row.status,
        CitationStatus::MissingRequiredHandle,
        "a CAPTURED_FROM edge to a tombstoned-and-unsuperseded LogSource is not reachable provenance"
    );
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
