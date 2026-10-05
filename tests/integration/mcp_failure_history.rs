// ── Issue #188: failure_history ──────────────────────────────────────────────
//
// RED phase: the `failure_history` MCP tool and its `from_records` helper do
// not exist yet. These tests pin the contract from the issue's acceptance
// criteria before the implementation lands:
// - AC1: tool accepts `handle` (+ optional `data_dir`), mirrors the
//   symbol_context/task_evidence shape.
// - AC2: same handle-resolution order as `eg query failures`, same
//   trust-separated sections (`runtime_failures` = verification_evidence,
//   `agent_failures` = agent_authored, `superseding_successes`).
// - AC3: stable record_id/evidence handle + read-time resolution_status on
//   every failure; oldest-first canonical ordering; byte-identical output.
// - AC4: resolved target with no failures → successful explicitly-empty
//   answer, distinct from unresolvable handle; no inferred failure cause.
// - AC5: unsupported/ambiguous/stale/no-match → machine-readable structured
//   results, never silent empty success or transcript-text fallback.
// - AC6: no raw transcript text, stdout/stderr, or patch hunks anywhere.

mod failure_history_tests {
    use aletheia_egregore::ir::{
        AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, OutputHandle, PatchHandle,
        SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id, artifact_stable_id,
        stable_id, verification_stable_id,
    };
    use aletheia_egregore::{
        EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
        mcp::{EgregoreMcpServer, FailureHistoryArgs, tool_failure_history_from_records},
    };
    use rmcp::handler::server::wrapper::Parameters;
    use serde_json::Value;

    /// Sentinel payloads that must NEVER appear in failure-history output.
    const RAW_TRANSCRIPT_SENTINEL: &str = "RAW_TRANSCRIPT_MCP_188_SHOULD_NOT_LEAK";
    const RAW_STDOUT_SENTINEL: &str = "RAW_STDOUT_MCP_188_SHOULD_NOT_LEAK";
    const RAW_PATCH_SENTINEL: &str = "RAW_PATCH_MCP_188_SHOULD_NOT_LEAK";

    const fn span(start_line: usize, end_line: usize) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line,
            end_line,
            start_column: None,
            end_column: None,
        }
    }

    fn link(target: &str, domain: &str, relation: &str) -> EvidenceLink {
        EvidenceLink {
            target_record_id: Some(target.to_owned()),
            target_domain: domain.to_owned(),
            relation: relation.to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }
    }

    struct FailureFixture {
        records: Vec<GraphRecord>,
        symbol_id: String,
        symbol_name: String,
        file_path: String,
        quiet_symbol_name: String,
        agent_fail_early_id: String,
        agent_fail_late_id: String,
        runtime_fail_id: String,
        success_id: String,
    }

    /// Compact in-memory analogue of the `eg query failures` seeded store: one
    /// symbol with two agent `Failure` claims (one before and one after a
    /// passing verification), one runtime `TestRun` failure superseded by that
    /// passing run, a patch artifact, and one failure-free symbol.
    #[allow(clippy::too_many_lines)]
    fn fixture() -> FailureFixture {
        let file_path = "src/mcp_fail.rs".to_owned();
        let file_id = stable_id(&["node", "File", &file_path]);
        let symbol_name = "mcp_parse_target".to_owned();
        let symbol_id = stable_id(&["node", "Symbol", &file_path, &symbol_name]);
        let quiet_symbol_name = "mcp_quiet_target".to_owned();
        let quiet_symbol_id = stable_id(&["node", "Symbol", &file_path, &quiet_symbol_name]);

        let mut records = vec![
            GraphRecord::syntax_node(
                file_id.clone(),
                NodeKind::File,
                file_path.clone(),
                span(1, 100),
                "mcp_fail.rs".to_owned(),
                "rust",
                "Source file mcp_fail.rs".to_owned(),
            ),
            GraphRecord::syntax_node(
                symbol_id.clone(),
                NodeKind::Symbol,
                file_path.clone(),
                span(10, 20),
                symbol_name.clone(),
                "rust",
                "Symbol mcp_parse_target".to_owned(),
            ),
            GraphRecord::syntax_node(
                quiet_symbol_id,
                NodeKind::Symbol,
                file_path.clone(),
                span(30, 40),
                quiet_symbol_name.clone(),
                "rust",
                "Symbol mcp_quiet_target".to_owned(),
            ),
            GraphRecord::edge(
                EdgeLabel::Defines,
                file_id,
                symbol_id.clone(),
                None,
                "file defines symbol".to_owned(),
            ),
        ];

        // Agent Failure (early): observed BEFORE the passing verification →
        // since_resolved. Its raw text must never leak (AC6).
        let agent_fail_early_id = agent_memory_stable_id(&["failure", "mcp188", "early"]);
        let mut early = GraphRecord::node(
            agent_fail_early_id.clone(),
            NodeKind::Failure,
            None,
            None,
            None,
            "agent failure early".to_owned(),
        );
        if let GraphRecord::Node {
            text,
            failure_kind,
            exit_code,
            agent_id,
            session_id,
            observed_at,
            source_artifact_path,
            schema_version,
            evidence_links,
            ..
        } = &mut early
        {
            *text = Some(format!("{RAW_TRANSCRIPT_SENTINEL}: assertion failed"));
            *failure_kind = Some("command_failure".to_owned());
            *exit_code = Some(1);
            *agent_id = Some("agent_1".to_owned());
            *session_id = Some("sess_1".to_owned());
            *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
            *source_artifact_path = Some("trajectories/mcp188-early.traj".to_owned());
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *evidence_links = Some(vec![link(&symbol_id, "codegraph", "FAILED_ON")]);
        }
        records.push(early);

        // Agent Failure (late): observed AFTER the passing verification →
        // still_failing.
        let agent_fail_late_id = agent_memory_stable_id(&["failure", "mcp188", "late"]);
        let mut late = GraphRecord::node(
            agent_fail_late_id.clone(),
            NodeKind::Failure,
            None,
            None,
            None,
            "agent failure late".to_owned(),
        );
        if let GraphRecord::Node {
            text,
            failure_kind,
            agent_id,
            session_id,
            observed_at,
            schema_version,
            evidence_links,
            ..
        } = &mut late
        {
            *text = Some(format!("{RAW_TRANSCRIPT_SENTINEL}: still broken"));
            *failure_kind = Some("patch_invalid".to_owned());
            *agent_id = Some("agent_1".to_owned());
            *session_id = Some("sess_1".to_owned());
            *observed_at = Some("2026-04-01T00:00:00Z".to_owned());
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *evidence_links = Some(vec![link(&symbol_id, "codegraph", "FAILED_ON")]);
        }
        records.push(late);

        // Runtime failure: TestRun, status=fail, inline stdout must not leak.
        let runtime_fail_id = verification_stable_id(&["verification", "mcp188", "runtime"]);
        let mut runtime = GraphRecord::node(
            runtime_fail_id.clone(),
            NodeKind::TestRun,
            None,
            None,
            None,
            "cargo test failed".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            status,
            verification_kind,
            executed_at,
            stdout_handle,
            evidence_links,
            ..
        } = &mut runtime
        {
            *schema_version = VERIFICATION_SCHEMA_VERSION;
            *status = Some("fail".to_owned());
            *verification_kind = Some("test_run".to_owned());
            *executed_at = Some("2026-02-01T00:00:00Z".to_owned());
            *stdout_handle = Some(Box::new(OutputHandle {
                inline: Some(RAW_STDOUT_SENTINEL.to_owned()),
                hash: "blake3:mcp188stdout".to_owned(),
                bytes: 4096,
            }));
            *evidence_links = Some(vec![link(&symbol_id, "codegraph", "VALIDATED_BY")]);
        }
        records.push(runtime);

        // Later PASSING verification on the same symbol: supersedes the early
        // agent failure and the runtime failure, not the late agent failure.
        let success_id = verification_stable_id(&["verification", "mcp188", "success"]);
        let mut success = GraphRecord::node(
            success_id.clone(),
            NodeKind::TestRun,
            None,
            None,
            None,
            "cargo test passed".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            status,
            verification_kind,
            executed_at,
            evidence_links,
            ..
        } = &mut success
        {
            *schema_version = VERIFICATION_SCHEMA_VERSION;
            *status = Some("pass".to_owned());
            *verification_kind = Some("test_run".to_owned());
            *executed_at = Some("2026-03-01T00:00:00Z".to_owned());
            *evidence_links = Some(vec![link(&symbol_id, "codegraph", "VALIDATED_BY")]);
        }
        records.push(success);

        // Patch artifact (protected inline bytes must not leak) produced by the
        // early failure.
        let patch_id = artifact_stable_id(&["patch", "mcp188"]);
        let mut patch = GraphRecord::node(
            patch_id.clone(),
            NodeKind::PatchArtifact,
            None,
            None,
            None,
            "Patch artifact for mcp_parse_target".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            patch_status,
            patch_bytes_hash,
            patch_handle,
            target_files,
            ..
        } = &mut patch
        {
            *schema_version = ARTIFACT_SCHEMA_VERSION;
            *patch_status = Some("rejected_validation".to_owned());
            *patch_bytes_hash = Some("blake3:mcp188patch".to_owned());
            *patch_handle = Some(Box::new(PatchHandle {
                path: "protected/mcp188.patch".to_owned(),
                inline: Some(RAW_PATCH_SENTINEL.to_owned()),
            }));
            *target_files = Some(vec![file_path.clone()]);
        }
        records.push(patch);
        records.push(GraphRecord::edge(
            EdgeLabel::ProducedPatch,
            agent_fail_early_id.clone(),
            patch_id,
            None,
            "early failure produced patch".to_owned(),
        ));

        FailureFixture {
            records,
            symbol_id,
            symbol_name,
            file_path,
            quiet_symbol_name,
            agent_fail_early_id,
            agent_fail_late_id,
            runtime_fail_id,
            success_id,
        }
    }

    /// AC1: the tool router registers `failure_history`.
    #[test]
    fn failure_history_tool_is_registered() {
        let tool = EgregoreMcpServer::failure_history_tool_attr();
        assert_eq!(tool.name.as_ref(), "failure_history");
        assert!(
            tool.description.as_ref().is_some_and(|d| !d.is_empty()),
            "failure_history must have a non-empty description"
        );
        assert!(
            tool.input_schema.get("type").is_some(),
            "failure_history must have an inputSchema with a 'type' field"
        );
    }

    /// AC2/AC3: trust-separated sections, stable record IDs, read-time
    /// `resolution_status` on every failure.
    #[test]
    fn failure_history_returns_trust_separated_sections() {
        let fx = fixture();
        let payload = tool_failure_history_from_records(&fx.records, &fx.symbol_name);

        assert_eq!(payload["ok"], Value::from(true), "got {payload}");
        assert_eq!(payload["handle"], Value::from(fx.symbol_name.clone()));
        assert_eq!(payload["target_type"], Value::from("symbol"));
        assert!(
            payload["target_ids"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == &fx.symbol_id)),
            "target_ids must cite the resolved symbol; got {payload}"
        );

        // Runtime failures: trust verification_evidence, superseded by the
        // later passing run → since_resolved with a stable resolved_by.
        let runtime = payload["runtime_failures"]
            .as_array()
            .expect("runtime_failures must be an array");
        assert_eq!(runtime.len(), 1, "got {payload}");
        assert_eq!(
            runtime[0]["record_id"],
            Value::from(fx.runtime_fail_id.clone())
        );
        assert_eq!(
            runtime[0]["trust_class"],
            Value::from("verification_evidence")
        );
        assert_eq!(
            runtime[0]["resolution_status"],
            Value::from("since_resolved")
        );
        assert_eq!(
            runtime[0]["resolved_by"],
            Value::from(fx.success_id.clone())
        );

        // Agent failures: trust agent_authored.
        let agent = payload["agent_failures"]
            .as_array()
            .expect("agent_failures must be an array");
        assert_eq!(agent.len(), 2, "got {payload}");
        for row in agent {
            assert_eq!(row["trust_class"], Value::from("agent_authored"));
            assert!(
                row["resolution_status"] == "still_failing"
                    || row["resolution_status"] == "since_resolved",
                "every failure carries a read-time resolution_status; got {row}"
            );
            assert!(
                row["record_id"].as_str().is_some_and(|s| !s.is_empty()),
                "every failure carries a stable record_id; got {row}"
            );
        }

        // Superseding successes: the later passing run, in its own section.
        let successes = payload["superseding_successes"]
            .as_array()
            .expect("superseding_successes must be an array");
        assert_eq!(successes.len(), 1, "got {payload}");
        assert_eq!(successes[0]["record_id"], Value::from(fx.success_id));
    }

    /// AC2: the per-failure statuses — the early agent failure predates the
    /// pass (`since_resolved`), the late one postdates it (`still_failing`).
    #[test]
    fn failure_history_assigns_resolution_status_by_time() {
        let fx = fixture();
        let payload = tool_failure_history_from_records(&fx.records, &fx.symbol_name);
        let agent = payload["agent_failures"]
            .as_array()
            .expect("agent_failures must be an array");
        let by_id: std::collections::BTreeMap<&str, &Value> = agent
            .iter()
            .filter_map(|r| r["record_id"].as_str().map(|id| (id, r)))
            .collect();
        assert_eq!(
            by_id[fx.agent_fail_early_id.as_str()]["resolution_status"],
            Value::from("since_resolved"),
            "early failure predates the pass"
        );
        assert_eq!(
            by_id[fx.agent_fail_late_id.as_str()]["resolution_status"],
            Value::from("still_failing"),
            "late failure postdates the pass"
        );
    }

    /// AC3: sections are canonically ordered oldest-first — the early agent
    /// failure (2026-01-01) sorts before the late one (2026-04-01).
    #[test]
    fn failure_history_sections_are_ordered_oldest_first() {
        let fx = fixture();
        let payload = tool_failure_history_from_records(&fx.records, &fx.symbol_name);
        let agent = payload["agent_failures"]
            .as_array()
            .expect("agent_failures must be an array");
        let ids: Vec<&str> = agent
            .iter()
            .filter_map(|r| r["record_id"].as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                fx.agent_fail_early_id.as_str(),
                fx.agent_fail_late_id.as_str()
            ],
            "agent failures must be oldest-first; got {ids:?}"
        );
    }

    /// AC3: identical stores yield byte-identical output.
    #[test]
    fn failure_history_output_is_deterministic() {
        let fx = fixture();
        let first = serde_json::to_string(&tool_failure_history_from_records(
            &fx.records,
            &fx.symbol_name,
        ))
        .expect("payload serializes");
        let second = serde_json::to_string(&tool_failure_history_from_records(
            &fx.records,
            &fx.symbol_name,
        ))
        .expect("payload serializes");
        assert_eq!(first, second, "tool output must be byte-identical");
    }

    /// AC2: the canonical code record ID handle resolves the same target as
    /// the symbol name.
    #[test]
    fn failure_history_resolves_canonical_code_record_id() {
        let fx = fixture();
        let payload = tool_failure_history_from_records(&fx.records, &fx.symbol_id);
        assert_eq!(payload["ok"], Value::from(true), "got {payload}");
        assert_eq!(payload["target_type"], Value::from("symbol"));
        assert_eq!(payload["agent_failures"].as_array().map(Vec::len), Some(2));
    }

    /// AC2: a repo-relative file path handle resolves (file target).
    #[test]
    fn failure_history_resolves_file_path_handle() {
        let fx = fixture();
        let payload = tool_failure_history_from_records(&fx.records, &fx.file_path);
        assert_eq!(payload["ok"], Value::from(true), "got {payload}");
        assert_eq!(payload["target_type"], Value::from("file"));
    }

    /// AC4: a resolved target with no recorded failures is a successful,
    /// explicitly-empty answer — distinct from an unresolvable handle — and
    /// carries the safety semantics (no inferred failure cause).
    #[test]
    fn failure_history_resolved_target_without_failures_is_explicitly_empty() {
        let fx = fixture();
        let payload = tool_failure_history_from_records(&fx.records, &fx.quiet_symbol_name);
        assert_eq!(payload["ok"], Value::from(true), "got {payload}");
        assert_eq!(
            payload["runtime_failures"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(payload["agent_failures"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            payload["superseding_successes"].as_array().map(Vec::len),
            Some(0)
        );
        // Absence of failure is not evidence of safety: the answer says so
        // explicitly and invents no failure cause.
        let note = payload["safety_note"]
            .as_str()
            .expect("empty answer must carry a safety_note");
        assert!(
            !note.is_empty(),
            "safety_note must be non-empty; got {payload}"
        );
        let serialized = serde_json::to_string(&payload).expect("serializes");
        assert!(
            !serialized.contains("inferred_failure_cause") && !serialized.contains("failure_cause"),
            "no inferred failure cause may appear; got {serialized}"
        );
    }

    /// AC5: an unresolvable handle is a structured `no_match` — never a silent
    /// empty success.
    #[test]
    fn failure_history_no_match_returns_structured_error() {
        let fx = fixture();
        let payload = tool_failure_history_from_records(&fx.records, "does_not_exist_188");
        assert_eq!(payload["ok"], Value::from(false), "got {payload}");
        assert_eq!(payload["error"]["code"], Value::from("no_match"));
        assert_eq!(
            payload["error"]["handle"],
            Value::from("does_not_exist_188")
        );
        assert!(
            payload.get("agent_failures").is_none(),
            "an error payload must not carry sections"
        );
    }

    /// AC5: a handle naming a tombstoned record is a structured `stale_handle`.
    #[test]
    fn failure_history_stale_handle_returns_structured_error() {
        let fx = fixture();
        let tombstone = GraphRecord::Tombstone {
            id: stable_id(&["tombstone", &fx.symbol_id]),
            schema_version: SCHEMA_VERSION,
            deleted_id: fx.symbol_id.clone(),
            summary: "deleted".to_owned(),
            producer: None,
        };
        let mut records = fx.records.clone();
        records.push(tombstone);
        let payload = tool_failure_history_from_records(&records, &fx.symbol_id);
        assert_eq!(payload["ok"], Value::from(false), "got {payload}");
        assert_eq!(payload["error"]["code"], Value::from("stale_handle"));
    }

    /// AC5: a malformed canonical ID is a structured `unsupported_handle`.
    #[test]
    fn failure_history_unsupported_handle_returns_structured_error() {
        let fx = fixture();
        let payload = tool_failure_history_from_records(&fx.records, "codegraph:not-a-valid-id");
        assert_eq!(payload["ok"], Value::from(false), "got {payload}");
        assert_eq!(payload["error"]["code"], Value::from("unsupported_handle"));
        assert!(
            payload["error"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty()),
            "unsupported_handle must explain why; got {payload}"
        );
    }

    /// AC5: a name matching targets in two repositories is a structured
    /// `ambiguous_handle` with candidates — never a merged answer.
    #[test]
    fn failure_history_ambiguous_handle_returns_structured_error() {
        let mut records: Vec<GraphRecord> = Vec::new();
        for repo in ["repo-a", "repo-b"] {
            let repo_id = stable_id(&["node", "Repository", repo]);
            let file_path = format!("{repo}/src/lib.rs");
            let file_id = stable_id(&["node", "File", &file_path]);
            let symbol_id = stable_id(&["node", "Symbol", &file_path, "mcp_parse_target"]);
            records.push(GraphRecord::node(
                repo_id.clone(),
                NodeKind::Repository,
                None,
                None,
                Some(repo.to_owned()),
                format!("Repository {repo}"),
            ));
            records.push(GraphRecord::syntax_node(
                file_id.clone(),
                NodeKind::File,
                file_path.clone(),
                span(1, 100),
                "lib.rs".to_owned(),
                "rust",
                "file".to_owned(),
            ));
            records.push(GraphRecord::syntax_node(
                symbol_id.clone(),
                NodeKind::Symbol,
                file_path,
                span(1, 10),
                "mcp_parse_target".to_owned(),
                "rust",
                "symbol".to_owned(),
            ));
            records.push(GraphRecord::edge(
                EdgeLabel::Contains,
                repo_id,
                file_id.clone(),
                None,
                "repo contains file".to_owned(),
            ));
            records.push(GraphRecord::edge(
                EdgeLabel::Defines,
                file_id,
                symbol_id,
                None,
                "file defines symbol".to_owned(),
            ));
        }
        let payload = tool_failure_history_from_records(&records, "mcp_parse_target");
        assert_eq!(payload["ok"], Value::from(false), "got {payload}");
        assert_eq!(payload["error"]["code"], Value::from("ambiguous_handle"));
        let candidates = payload["error"]["candidates"]
            .as_array()
            .expect("ambiguous_handle must enumerate candidates");
        assert_eq!(candidates.len(), 2, "one candidate per repository");
    }

    /// AC6: no raw transcript text, stdout/stderr, or patch hunks appear in
    /// any field — only hashes, handles, bounded summaries, redaction markers.
    #[test]
    fn failure_history_never_leaks_raw_payloads() {
        let fx = fixture();
        let payload = tool_failure_history_from_records(&fx.records, &fx.symbol_name);
        assert_eq!(payload["ok"], Value::from(true));
        let serialized = serde_json::to_string(&payload).expect("serializes");
        for sentinel in [
            RAW_TRANSCRIPT_SENTINEL,
            RAW_STDOUT_SENTINEL,
            RAW_PATCH_SENTINEL,
        ] {
            assert!(
                !serialized.contains(sentinel),
                "sentinel `{sentinel}` leaked into the tool response"
            );
        }
        // Citation metadata survives: hashes and handles, not bytes.
        assert!(
            serialized.contains("blake3:mcp188stdout"),
            "stdout hash citation must survive; got {serialized}"
        );
        assert!(
            serialized.contains("blake3:mcp188patch"),
            "patch hash citation must survive; got {serialized}"
        );
    }

    /// AC1/AC5: an empty handle is a structured `missing_argument` error via the
    /// tool method (mirrors `symbol_context`), before any daemon lookup.
    #[test]
    fn failure_history_empty_handle_is_missing_argument() {
        let server = EgregoreMcpServer::new(std::path::PathBuf::from(
            ".egregore-nonexistent-fixture-mcp-test",
        ));
        let raw = server.failure_history(Parameters(FailureHistoryArgs {
            handle: String::new(),
            data_dir: None,
            repo_path: None,
        }));
        let payload: Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(payload["ok"], Value::from(false));
        assert_eq!(payload["error"]["code"], Value::from("missing_argument"));
        assert_eq!(payload["error"]["field"], Value::from("handle"));
    }
}
