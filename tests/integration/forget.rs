//! Integration tests for `eg forget` (issue #231).
//!
//! Coverage map:
//!   AC1 — one command retracts one record by stable handle
//!   AC2 — after retraction no read surface returns the record's content
//!         (query memory / context and the MCP tool funnel are pinned here;
//!         the semantic/vector lane is pinned in the adapter unit tests)
//!   AC3 — retraction is logical: a citable retraction event records actor,
//!         transaction time, reason, and the prior record handle
//!   AC4 — deterministic code-graph facts are refused with a machine-readable
//!         error naming `eg refresh` / re-scan
//!   AC5 — citing records survive; their evidence link to the retracted handle
//!         is reported stale rather than silently dropped
//!   AC6 — the physical record stays reconstructable for transaction-time
//!         views predating the retraction (bi-temporal honesty)
//!   AC7 — re-running on an already-retracted handle is a no-op success

#![allow(missing_docs)]

#[cfg(feature = "embedded-aletheiadb")]
mod embedded {
    use std::{fs, path::Path};

    use aletheia_egregore::{
        GraphRecord, NodeKind, TemporalMetadata,
        embeddings::{CandidateVector, EmbeddingCandidate, semantic_drift_records},
        ir::{AGENT_MEMORY_SCHEMA_VERSION, EvidenceLink, agent_memory_stable_id, stable_id},
    };
    use assert_cmd::Command;

    const TX: &str = "2026-07-01T00:00:00Z";
    const OBS_TEXT: &str = "the parser silently skips empty input";
    const CITING_TEXT: &str = "builds on the earlier parser claim";

    fn egregore() -> Command {
        Command::cargo_bin("egregore").expect("binary should run")
    }

    fn symbol_id() -> String {
        stable_id(&["node", "symbol", "repo", "src/lib.rs", "fn", "parse", "0"])
    }

    fn obs_id() -> String {
        agent_memory_stable_id(&["node", "observation", "sess-231", "0"])
    }

    fn citing_id() -> String {
        agent_memory_stable_id(&["node", "observation", "sess-231", "1"])
    }

    fn link(target: &str, domain: &str, relation: &str) -> EvidenceLink {
        EvidenceLink {
            target_record_id: Some(target.to_owned()),
            target_domain: domain.to_owned(),
            relation: relation.to_owned(),
            confidence: "0.9".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }
    }

    fn observation(id: &str, text: &str, links: Vec<EvidenceLink>) -> GraphRecord {
        let mut node = GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            Some("observation".to_owned()),
            "agent observation".to_owned(),
        );
        if let GraphRecord::Node {
            ref mut schema_version,
            text: ref mut node_text,
            ref mut agent_id,
            ref mut session_id,
            ref mut observed_at,
            ref mut source_handle,
            ref mut evidence_links,
            ..
        } = node
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *node_text = Some(text.to_owned());
            *agent_id = Some("agent-1".to_owned());
            *session_id = Some("sess-231".to_owned());
            *observed_at = Some("2026-06-01T00:00:00Z".to_owned());
            *source_handle = Some("session-sess-231.jsonl".to_owned());
            *evidence_links = Some(links);
        }
        node
    }

    /// Builds one deterministic `SemanticDrift` node (plus its `DRIFTS_FROM` /
    /// `DRIFTS_PRIOR` edges) for the seeded symbol, exactly as the embeddings
    /// pipeline emits them: temporal metadata included, so the record lands in
    /// the embedded store's per-commit (temporal) lookup.
    fn drift_records() -> Vec<GraphRecord> {
        let candidate = |commit: &str, valid_time: &str, values: Vec<f32>| CandidateVector {
            candidate: EmbeddingCandidate {
                record_id: symbol_id(),
                target: "symbol".to_owned(),
                text: "symbol parse".to_owned(),
                repo_relative_path: Some("src/lib.rs".to_owned()),
                name: Some("parse".to_owned()),
                temporal: Some(TemporalMetadata {
                    git_commit: commit.to_owned(),
                    git_parent_commits: Vec::new(),
                    valid_time: valid_time.to_owned(),
                    author_time: None,
                    observed_at: valid_time.to_owned(),
                    valid_time_source: Some("git_commit_committer_date".to_owned()),
                }),
            },
            vector: values,
        };
        semantic_drift_records(
            &[
                candidate("aaaaaaaa", "2026-01-01T00:00:00Z", vec![1.0, 0.0]),
                candidate("bbbbbbbb", "2026-01-02T00:00:00Z", vec![0.0, 1.0]),
            ],
            "sentence-transformers/all-MiniLM-L6-v2",
            0.4,
        )
    }

    fn drift_node_id() -> String {
        drift_records()
            .iter()
            .find(|record| matches!(record, GraphRecord::Node { .. }))
            .expect("drift fixture emits one node")
            .id()
            .to_owned()
    }

    fn seed_records() -> Vec<GraphRecord> {
        let symbol = GraphRecord::node(
            symbol_id(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some("parse".to_owned()),
            "fn parse".to_owned(),
        );
        let obs = observation(
            &obs_id(),
            OBS_TEXT,
            vec![link(&symbol_id(), "codegraph", "MENTIONS_SYMBOL")],
        );
        let citing = observation(
            &citing_id(),
            CITING_TEXT,
            vec![link(&obs_id(), "agent_memory", "RELATES_TO")],
        );
        let mut records = vec![symbol, obs, citing];
        records.extend(drift_records());
        records
    }

    /// Writes the seed fixture to JSONL and ingests it into a fresh embedded
    /// store at `<dir>/store`, returning the store path.
    fn seed_store(dir: &Path) -> std::path::PathBuf {
        let graph = dir.join("seed.jsonl");
        let mut lines = String::new();
        for record in seed_records() {
            lines.push_str(&serde_json::to_string(&record).expect("serializable"));
            lines.push('\n');
        }
        fs::write(&graph, lines).expect("seed JSONL written");
        let store = dir.join("store");
        egregore()
            .args(["ingest"])
            .arg(&graph)
            .args(["--adapter", "embedded", "--data-dir"])
            .arg(&store)
            .assert()
            .success();
        store
    }

    fn run(store: &Path, args: &[&str]) -> (i32, String, String) {
        let output = egregore()
            .args(args)
            .args(["--data-dir"])
            .arg(store)
            .assert()
            .get_output()
            .clone();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8(output.stdout).expect("utf8 stdout"),
            String::from_utf8(output.stderr).expect("utf8 stderr"),
        )
    }

    /// Parses the machine-readable JSON envelope from stderr. The embedded
    /// engine logs index-restoration lines to stderr on open, so the envelope
    /// is the last non-empty line.
    fn stderr_envelope(stderr: &str) -> serde_json::Value {
        let line = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .expect("stderr should carry a JSON envelope");
        serde_json::from_str(line).expect("machine-readable stderr envelope")
    }

    fn forget(store: &Path, handle: &str) -> (i32, String, String) {
        run(
            store,
            &[
                "forget",
                handle,
                "--reason",
                "false claim about parser behavior",
                "--retracted-by",
                "op-1",
                "--transaction-time",
                TX,
            ],
        )
    }

    /// AC1 + AC2 + AC3: forgetting a memory record removes its content from
    /// the query lanes and leaves exactly one auditable retraction event.
    #[test]
    fn forget_retracts_memory_record_from_query_lanes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = seed_store(temp.path());

        // `query memory` output is redaction-safe (handles and summaries, not
        // raw claim text), so the pre-retraction check pins the record ID.
        let (code, stdout, _) = run(&store, &["query", "memory", &obs_id()]);
        assert_eq!(code, 0, "seeded claim resolves before retraction");
        assert!(
            stdout.contains(&obs_id()),
            "pre-retraction read cites the record: {stdout}"
        );

        let (code, stdout, stderr) = forget(&store, &obs_id());
        assert_eq!(code, 0, "retraction succeeds: {stderr}");
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["action"], "retracted");
        let event = &envelope["retraction"];
        assert_eq!(event["retracted_record_id"], obs_id());
        assert_eq!(event["retracted_by"], "op-1");
        assert_eq!(event["retracted_at"], TX);
        assert_eq!(event["reason"], "false claim about parser behavior");
        assert!(
            event["retraction_id"]
                .as_str()
                .expect("retraction_id")
                .starts_with("agent_memory:v1:"),
            "the retraction event is a citable agent-memory record: {event}"
        );

        // `query memory` reports the handle as retracted, never the content.
        let (code, stdout, _) = run(&store, &["query", "memory", &obs_id()]);
        assert_eq!(code, 2, "retracted handles are stale, not live");
        assert!(
            stdout.contains("stale_handle"),
            "stale verdict expected: {stdout}"
        );
        assert!(
            !stdout.contains(OBS_TEXT),
            "content must not leak: {stdout}"
        );

        // The symbol-context lane no longer surfaces the claim either.
        let (code, stdout, _) = run(&store, &["query", "context", "parse"]);
        assert_eq!(code, 0, "context query still works");
        assert!(
            !stdout.contains(OBS_TEXT),
            "context must exclude content: {stdout}"
        );
        assert!(
            !stdout.contains(&obs_id()),
            "context must not cite the record: {stdout}"
        );
    }

    /// AC2 (MCP funnel): the MCP tools read the same current-state record view
    /// the daemon serves; after retraction that view carries neither the
    /// record nor its content.
    #[test]
    fn forget_excludes_record_from_mcp_tool_funnel() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = seed_store(temp.path());
        let (code, _, stderr) = forget(&store, &obs_id());
        assert_eq!(code, 0, "retraction succeeds: {stderr}");

        let sink =
            aletheia_egregore::adapters::EmbeddedAletheiaSink::open(&store).expect("store opens");
        let records = sink.read_all_records().expect("current view reads");
        assert!(
            records.iter().all(|r| r.id() != obs_id()),
            "current-state view excludes the retracted record"
        );

        let context = aletheia_egregore::mcp::tool_symbol_context_from_records(&records, "parse");
        let rendered = serde_json::to_string(&context).expect("serializable");
        assert!(
            !rendered.contains(OBS_TEXT),
            "MCP symbol_context must exclude retracted content: {rendered}"
        );
    }

    /// AC6: the retraction is logical. The physical record remains in the
    /// history-inclusive view, so a transaction-time query predating the
    /// retraction still reflects that the record existed then.
    #[test]
    fn forget_preserves_pre_retraction_history_view() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = seed_store(temp.path());
        let (code, _, stderr) = forget(&store, &obs_id());
        assert_eq!(code, 0, "retraction succeeds: {stderr}");

        let sink =
            aletheia_egregore::adapters::EmbeddedAletheiaSink::open(&store).expect("store opens");
        let history = sink
            .read_all_records_including_superseded()
            .expect("history view reads");
        let preserved = history.iter().any(|record| {
            matches!(
                record,
                GraphRecord::Node { id, text: Some(text), .. }
                    if id == &obs_id() && text == OBS_TEXT
            )
        });
        assert!(
            preserved,
            "the retracted record's bytes stay reconstructable for pre-retraction views"
        );
        let event_present = history.iter().any(|record| {
            matches!(
                record,
                GraphRecord::Node { kind: NodeKind::Retraction, source_handle: Some(handle), .. }
                    if handle == &obs_id()
            )
        });
        assert!(event_present, "the retraction event is stored and citable");
    }

    /// AC7: idempotent re-run, plus deterministic output across identical
    /// stores when the transaction time is pinned.
    #[test]
    fn forget_is_idempotent_and_deterministic() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = seed_store(temp.path());
        let (code, first_stdout, _) = forget(&store, &obs_id());
        assert_eq!(code, 0);
        let first: serde_json::Value =
            serde_json::from_str(first_stdout.trim()).expect("JSON envelope");

        // Re-running with a different reason and instant is a no-op success
        // that returns the original event — never a duplicate.
        let (code, stdout, stderr) = run(
            &store,
            &[
                "forget",
                &obs_id(),
                "--reason",
                "a different reason",
                "--retracted-by",
                "op-2",
                "--transaction-time",
                "2026-07-02T09:00:00Z",
            ],
        );
        assert_eq!(code, 0, "idempotent no-op success: {stderr}");
        let rerun: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
        assert_eq!(rerun["ok"], true);
        assert_eq!(rerun["action"], "already_retracted");
        assert_eq!(
            rerun["retraction"], first["retraction"],
            "the original retraction event is preserved verbatim"
        );

        // Determinism: an identical store with the same pinned transaction
        // time produces a byte-identical envelope.
        let temp2 = tempfile::tempdir().expect("tempdir");
        let store2 = seed_store(temp2.path());
        let (code, second_stdout, _) = forget(&store2, &obs_id());
        assert_eq!(code, 0);
        assert_eq!(first_stdout, second_stdout, "byte-identical across runs");
    }

    /// A record revived by a later re-ingest must be suppressed again by
    /// re-running `eg forget`: the repair tombstone has to land as a fresh
    /// write instead of no-oping against the stale tombstone left over from
    /// the first retraction (which would leave the target live while the
    /// envelope claims `retracted`).
    #[test]
    fn forget_rerun_suppresses_record_revived_by_reingest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = seed_store(temp.path());
        let (code, _, stderr) = forget(&store, &obs_id());
        assert_eq!(code, 0, "first retraction succeeds: {stderr}");

        // Revive the retracted record: re-ingest an updated version of the
        // same stable ID. The newer write supersedes the retraction
        // tombstone, so the record is live again on current read surfaces.
        let revived = observation(
            &obs_id(),
            "revised parser claim after retraction",
            vec![link(&symbol_id(), "codegraph", "MENTIONS_SYMBOL")],
        );
        let graph = temp.path().join("revive.jsonl");
        let line = serde_json::to_string(&revived).expect("serializable");
        fs::write(&graph, format!("{line}\n")).expect("revive JSONL written");
        egregore()
            .args(["ingest"])
            .arg(&graph)
            .args(["--adapter", "embedded", "--data-dir"])
            .arg(&store)
            .assert()
            .success();
        let (code, stdout, _) = run(&store, &["query", "memory", &obs_id()]);
        assert_eq!(code, 0, "the re-ingested record is live again: {stdout}");

        // Re-running forget must actually suppress the revived record.
        let (code, stdout, stderr) = forget(&store, &obs_id());
        assert_eq!(code, 0, "repair retraction succeeds: {stderr}");
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["action"], "retracted");

        let (code, stdout, _) = run(&store, &["query", "memory", &obs_id()]);
        assert_eq!(
            code, 2,
            "the revived record must be retracted again, not left live: {stdout}"
        );
        assert!(
            stdout.contains("stale_handle"),
            "stale verdict expected: {stdout}"
        );
    }

    /// Derived semantic measurements are refused like code facts: a
    /// `SemanticDrift` node is a temporal record the embedded current-state
    /// read deliberately re-emits (for `--at` views), so a retraction
    /// tombstone would never actually suppress it from `eg query drift` —
    /// accepting the handle reported success while the record stayed
    /// queryable and re-runs falsely no-oped as `already_retracted`.
    #[test]
    fn forget_refuses_derived_semantic_drift_record() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = seed_store(temp.path());
        let drift_id = drift_node_id();

        let (code, stdout, _) = run(&store, &["query", "drift"]);
        assert_eq!(code, 0, "the seeded drift record is queryable: {stdout}");
        assert!(stdout.contains(&drift_id), "drift row present: {stdout}");

        let (code, _, stderr) = forget(&store, &drift_id);
        assert_eq!(code, 1, "derived semantic records are refused: {stderr}");
        let envelope = stderr_envelope(&stderr);
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error"]["code"], "derived_semantic_record");
        assert_eq!(envelope["error"]["detail"]["kind"], "SemanticDrift");
        assert_eq!(envelope["error"]["detail"]["record_id"], drift_id);
        let message = envelope["error"]["detail"]["message"]
            .as_str()
            .expect("message");
        assert!(
            message.contains("re-scan"),
            "refusal names the re-derivation path: {message}"
        );

        // The refusal writes nothing: the drift record stays queryable and a
        // re-run is the same refusal, never an `already_retracted` no-op.
        let (code, stdout, _) = run(&store, &["query", "drift"]);
        assert_eq!(code, 0);
        assert!(stdout.contains(&drift_id), "drift row untouched: {stdout}");
        let (code, _, stderr) = forget(&store, &drift_id);
        assert_eq!(code, 1, "re-run refuses again, never no-ops: {stderr}");
        assert_eq!(
            stderr_envelope(&stderr)["error"]["code"],
            "derived_semantic_record"
        );
    }

    /// AC4: deterministic code-graph facts are refused with a machine-readable
    /// error naming the correction path, and the store stays untouched.
    #[test]
    fn forget_refuses_deterministic_code_fact() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = seed_store(temp.path());

        let (code, _, stderr) = forget(&store, &symbol_id());
        assert_eq!(code, 1, "code facts are refused");
        let envelope = stderr_envelope(&stderr);
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error"]["code"], "deterministic_code_fact");
        assert_eq!(envelope["error"]["detail"]["kind"], "Symbol");
        let message = envelope["error"]["detail"]["message"]
            .as_str()
            .expect("message");
        assert!(
            message.contains("eg refresh"),
            "refusal names the remedy: {message}"
        );

        // Nothing was retracted: the symbol and the memory claim both stay live.
        let (code, stdout, _) = run(&store, &["query", "memory", &obs_id()]);
        assert_eq!(code, 0);
        assert!(stdout.contains(&obs_id()));
    }

    /// Unknown handles exit 2 with a machine-readable `not_found` envelope.
    #[test]
    fn forget_unknown_handle_exits_2() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = seed_store(temp.path());
        let (code, _, stderr) = forget(&store, "agent_memory:v1:doesnotexist");
        assert_eq!(code, 2);
        let envelope = stderr_envelope(&stderr);
        assert_eq!(envelope["error"]["code"], "not_found");
    }

    /// An empty `--reason` is refused: the retraction event must be auditable.
    #[test]
    fn forget_empty_reason_is_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = seed_store(temp.path());
        let (code, _, stderr) = run(
            &store,
            &[
                "forget",
                &obs_id(),
                "--reason",
                "",
                "--transaction-time",
                TX,
            ],
        );
        assert_eq!(code, 1);
        let envelope = stderr_envelope(&stderr);
        assert_eq!(envelope["error"]["code"], "missing_reason");
    }

    /// AC5: records that merely cite the retracted handle are not deleted;
    /// their evidence link is reported stale rather than silently dropped.
    #[test]
    fn forget_reports_citing_evidence_as_stale_not_dropped() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = seed_store(temp.path());
        let (code, _, stderr) = forget(&store, &obs_id());
        assert_eq!(code, 0, "retraction succeeds: {stderr}");

        let (code, stdout, _) = run(&store, &["query", "memory", &citing_id()]);
        assert_eq!(code, 0, "the citing record stays live: {stdout}");
        let audit: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
        let diagnostics = audit["diagnostics"].as_array().expect("diagnostics");
        assert!(
            diagnostics.iter().any(|d| {
                d["code"] == "stale_evidence_target" && d["target_handle"] == obs_id()
            }),
            "the dangling link is reported, not dropped: {audit}"
        );
        assert!(
            !stdout.contains(OBS_TEXT),
            "the retracted content must not resurface through citations: {stdout}"
        );
    }
}
