//! Integration tests for `eg retire` / `eg reinstate` (issue #156).
//!
//! Coverage map:
//!   AC1 — retire by stable ID writes a retirement receipt (and a SUPERSEDES
//!         edge for the `superseded` reason)
//!   AC2 — default recall excludes retired records; `--include-retired`
//!         returns them labeled with their retirement state
//!   AC3 — missing reason / dangling superseding target → stable diagnostic +
//!         distinct exit code
//!   AC4 — a Failure record retires and stays retrievable
//!   AC5 — five identical retire+reinstate sequences produce byte-identical
//!         receipts, edges, envelopes, and final recall output (issue AC9)
//!   AC6 — `superseded` requires an active observation-class superseding
//!         record; `drifted` requires an unresolved, cited evidence handle
//!   AC7 — reinstate returns the record to recall; the retire/reinstate trail
//!         stays queryable
//!   AC8 — retire/reinstate receipts are queryable; original provenance is
//!         untouched
//!   AC9 — retiring a code-graph fact fails and changes zero code records
//!   AC10 — a transaction-time `--as-of` before the retirement sees the
//!         record as active

#![allow(missing_docs)]

#[cfg(feature = "embedded-aletheiadb")]
mod embedded {
    use std::{fs, path::Path};

    use aletheia_egregore::{
        GraphRecord, NodeKind,
        adapters::{EmbeddedAletheiaSink, GraphSink},
        ir::{AGENT_MEMORY_SCHEMA_VERSION, EvidenceLink, agent_memory_stable_id},
    };
    use assert_cmd::Command;

    const TX_RETIRE: &str = "2026-09-28T12:00:00Z";
    const TX_REINSTATE: &str = "2026-09-28T13:00:00Z";

    fn egregore() -> Command {
        Command::cargo_bin("egregore").expect("binary should run")
    }

    fn obs_id(name: &str) -> String {
        agent_memory_stable_id(&["node", "observation", "sess-156", name])
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
            schema_version,
            text: node_text,
            agent_id,
            session_id,
            observed_at,
            source_handle,
            transaction_time,
            valid_time,
            valid_time_source,
            evidence_links,
            ..
        } = &mut node
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *node_text = Some(text.to_owned());
            *agent_id = Some("agent-1".to_owned());
            *session_id = Some("sess-156".to_owned());
            *observed_at = Some("2026-09-01T00:00:00Z".to_owned());
            *source_handle = Some("session-sess-156.jsonl".to_owned());
            *transaction_time = Some("2026-09-01T00:00:00Z".to_owned());
            *valid_time = Some("2026-09-01T00:00:00Z".to_owned());
            *valid_time_source = Some("inferred_from_transaction_time".to_owned());
            *evidence_links = Some(links);
        }
        node
    }

    fn failure_node(id: &str, text: &str) -> GraphRecord {
        let mut node = GraphRecord::node(
            id.to_owned(),
            NodeKind::Failure,
            None,
            None,
            Some("failure".to_owned()),
            "agent failure".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            text: node_text,
            agent_id,
            failure_kind,
            transaction_time,
            source_handle,
            ..
        } = &mut node
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *node_text = Some(text.to_owned());
            *agent_id = Some("agent-1".to_owned());
            *failure_kind = Some("timeout".to_owned());
            *transaction_time = Some("2026-09-01T00:00:00Z".to_owned());
            *source_handle = Some("session-sess-156.jsonl".to_owned());
        }
        node
    }

    fn symbol_node() -> GraphRecord {
        GraphRecord::node(
            "codegraph:v1:repo:src/main.rs:main".to_owned(),
            NodeKind::Symbol,
            Some("src/main.rs".to_owned()),
            None,
            Some("main".to_owned()),
            "fn main".to_owned(),
        )
    }

    fn run_node() -> GraphRecord {
        let mut node = GraphRecord::node(
            "agent_memory:v1:run-156".to_owned(),
            NodeKind::AgentRun,
            None,
            None,
            None,
            "agent run".to_owned(),
        );
        if let GraphRecord::Node { schema_version, .. } = &mut node {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        }
        node
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

    fn seed_records() -> Vec<GraphRecord> {
        vec![
            observation(
                &obs_id("old"),
                "the old belief about parser behavior",
                vec![],
            ),
            observation(
                &obs_id("new"),
                "the new belief about parser behavior",
                vec![],
            ),
            failure_node(&obs_id("fail"), "the build failed with a timeout"),
            observation(
                &obs_id("drifted"),
                "the stale belief citing a deleted file",
                vec![link(
                    "codegraph:v1:repo:src/deleted.rs",
                    "codegraph",
                    "OBSERVES",
                )],
            ),
            symbol_node(),
            run_node(),
        ]
    }

    /// Writes the seed fixture to JSONL and ingests it into a fresh embedded
    /// store at `<dir>/store`, returning the store path.
    ///
    /// The `drifted` fixture cites a handle that resolves to nothing; the
    /// ingest CLI would quarantine it under the default dangling-citation
    /// policy, so it is written directly through the sink instead. That is
    /// exactly the production shape: the citation was valid at write time and
    /// the target vanished later.
    fn seed_store(dir: &Path) -> std::path::PathBuf {
        let graph = dir.join("seed.jsonl");
        let drifted_id = obs_id("drifted");
        let mut lines = String::new();
        let mut drifted = None;
        for record in seed_records() {
            if record.id() == drifted_id {
                drifted = Some(record);
                continue;
            }
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
        if let Some(record) = drifted {
            let mut sink = EmbeddedAletheiaSink::open(&store).expect("store opens");
            GraphSink::write_record(&mut sink, &record).expect("drifted record writes");
            sink.persist_indexes().expect("indexes persist");
        }
        store
    }

    fn record_count(store: &Path) -> usize {
        let sink = EmbeddedAletheiaSink::open(store).expect("store opens");
        sink.read_all_records().expect("reads").len()
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

    /// First non-empty stdout line as JSON (success envelopes print to stdout).
    fn stdout_envelope(stdout: &str) -> serde_json::Value {
        let line = stdout
            .lines()
            .find(|line| !line.trim().is_empty())
            .expect("stdout should carry a JSON envelope");
        serde_json::from_str(line).expect("machine-readable stdout envelope")
    }

    /// Last non-empty stderr line as JSON (the embedded engine logs
    /// index-restoration lines to stderr on open).
    fn stderr_envelope(stderr: &str) -> serde_json::Value {
        let line = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .expect("stderr should carry a JSON envelope");
        serde_json::from_str(line).expect("machine-readable stderr envelope")
    }

    // -- AC1: retire writes a receipt -------------------------------------------

    #[test]
    fn retire_superseded_writes_receipt_and_supersedes_edge() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let old = obs_id("old");
        let new = obs_id("new");
        let before = record_count(&store);

        let (code, stdout, _) = run(
            &store,
            &[
                "retire",
                &old,
                "--reason",
                "superseded",
                "--superseded-by",
                &new,
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 0, "retire should succeed");
        let envelope = stdout_envelope(&stdout);
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["action"], "retired");
        assert_eq!(envelope["receipt"]["retired_record_id"], old);
        assert_eq!(envelope["receipt"]["reason"], "superseded");
        assert_eq!(envelope["receipt"]["retired_by"], "op-1");
        assert_eq!(envelope["receipt"]["retired_at"], TX_RETIRE);
        assert_eq!(envelope["receipt"]["superseded_by"], new);
        assert!(
            !envelope["receipt"]["receipt_id"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "receipt id must be populated"
        );

        // Receipt node + SUPERSEDES edge = two new records.
        assert_eq!(record_count(&store), before + 2);
    }

    // -- AC3: stable diagnostics + distinct exit codes ----------------------------

    #[test]
    fn retire_without_reason_fails_with_usage_error() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let old = obs_id("old");
        let (code, _, stderr) = run(
            &store,
            &[
                "retire",
                &old,
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 2, "missing --reason is a clap usage error (exit 2)");
        assert!(
            stderr.contains("--reason"),
            "usage error should name --reason"
        );
        assert_eq!(record_count(&store), 6, "refusal must write nothing");
    }

    #[test]
    fn retire_accepts_operator_decision_underscore_spelling() {
        // The issue text spells the fourth reason `operator_decision`; the CLI
        // accepts it as an alias and canonicalizes to `operator-decision`.
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let old = obs_id("old");
        let (code, stdout, _) = run(
            &store,
            &[
                "retire",
                &old,
                "--reason",
                "operator_decision",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 0);
        assert_eq!(
            stdout_envelope(&stdout)["receipt"]["reason"],
            "operator-decision"
        );
    }

    #[test]
    fn retire_dangling_superseder_fails_with_stable_diagnostic() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let old = obs_id("old");
        let (code, _, stderr) = run(
            &store,
            &[
                "retire",
                &old,
                "--reason",
                "superseded",
                "--superseded-by",
                "agent_memory:v1:ghost",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 2, "dangling superseding target exits 2");
        let envelope = stderr_envelope(&stderr);
        assert_eq!(envelope["ok"], false);
        assert_eq!(
            envelope["error"]["code"],
            "retire_dangling_superseding_record"
        );
        assert_eq!(envelope["error"]["superseded_by"], "agent_memory:v1:ghost");
        assert_eq!(record_count(&store), 6, "refusal must write nothing");
    }

    // -- AC6: per-reason gates ------------------------------------------------------

    #[test]
    fn retire_superseded_requires_active_observation_superseder() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let old = obs_id("old");

        // Superseder outside the observation class.
        let (code, _, stderr) = run(
            &store,
            &[
                "retire",
                &old,
                "--reason",
                "superseded",
                "--superseded-by",
                "agent_memory:v1:run-156",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr_envelope(&stderr)["error"]["code"],
            "retire_superseding_record_not_observation"
        );

        // Superseder is itself retired.
        let new = obs_id("new");
        let (code, _, _) = run(
            &store,
            &[
                "retire",
                &new,
                "--reason",
                "operator-decision",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 0);
        let (code, _, stderr) = run(
            &store,
            &[
                "retire",
                &old,
                "--reason",
                "superseded",
                "--superseded-by",
                &new,
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr_envelope(&stderr)["error"]["code"],
            "retire_superseding_record_retired"
        );
    }

    #[test]
    fn retire_drifted_requires_unresolved_cited_evidence() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let old = obs_id("old");

        // No handle at all.
        let (code, _, stderr) = run(
            &store,
            &[
                "retire",
                &old,
                "--reason",
                "drifted",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr_envelope(&stderr)["error"]["code"],
            "retire_missing_evidence_handle"
        );

        // Handle that still resolves to a live record.
        let new = obs_id("new");
        let (code, _, stderr) = run(
            &store,
            &[
                "retire",
                &old,
                "--reason",
                "drifted",
                "--evidence-handle",
                &new,
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 1);
        // `new` resolves; drifted requires an unresolved handle. It is also
        // not cited by `old`, and the cited-check fires first.
        let err_code = stderr_envelope(&stderr)["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            err_code == "retire_unknown_evidence_handle"
                || err_code == "retire_evidence_still_resolves",
            "unexpected code {err_code}"
        );
    }

    #[test]
    fn retire_drifted_happy_path_with_unresolved_cited_handle() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        // The `drifted` fixture cites `codegraph:v1:repo:src/deleted.rs`,
        // which no record resolves: a genuine drifted retirement.
        let drifted = obs_id("drifted");

        let (code, stdout, _) = run(
            &store,
            &[
                "retire",
                &drifted,
                "--reason",
                "drifted",
                "--evidence-handle",
                "codegraph:v1:repo:src/deleted.rs",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 0, "drifted retire should succeed");
        let envelope = stdout_envelope(&stdout);
        assert_eq!(envelope["receipt"]["reason"], "drifted");
        assert_eq!(
            envelope["receipt"]["evidence_handle"],
            "codegraph:v1:repo:src/deleted.rs"
        );

        // The retired record is excluded from default recall.
        let rows = semantic_memory_rows(&store, &[]);
        let recalled = rows
            .iter()
            .filter_map(|row| row["record_id"].as_str())
            .any(|id| id == drifted);
        assert!(!recalled);
    }

    // -- AC2: recall exclusion + --include-retired labeling --------------------------

    fn semantic_memory_rows(store: &Path, extra: &[&str]) -> Vec<serde_json::Value> {
        let mut args = vec![
            "query",
            "semantic-memory",
            "what did past sessions learn?",
            "--collapse",
            "--collapse-mode",
            "normalized-text",
            "--format",
            "json",
            "--limit",
            "25",
        ];
        args.extend(extra);
        let (code, stdout, stderr) = run(store, &args);
        assert_eq!(code, 0, "recall should succeed: {stderr}");
        // Rows carry record_id + kind; exclusion diagnostics carry record_id
        // only and must not be mistaken for rows.
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|value| value.get("record_id").is_some() && value.get("kind").is_some())
            .collect()
    }

    #[test]
    fn retired_record_excluded_by_default_labeled_with_include_retired() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let old = obs_id("old");
        let new = obs_id("new");

        let (code, _, _) = run(
            &store,
            &[
                "retire",
                &old,
                // operator-decision writes no SUPERSEDES edge, so only the
                // retirement filter itself can exclude the record below.
                "--reason",
                "operator-decision",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 0);

        // Default recall: the retired record is gone.
        let rows = semantic_memory_rows(&store, &[]);
        let recalled = rows
            .iter()
            .filter_map(|row| row["record_id"].as_str())
            .any(|id| id == old);
        assert!(!recalled, "retired record must be excluded");
        let active_stays = rows
            .iter()
            .filter_map(|row| row["record_id"].as_str())
            .any(|id| id == new);
        assert!(active_stays, "active record must stay");

        // --include-retired: both appear, each labeled with retirement state.
        let rows = semantic_memory_rows(&store, &["--include-retired"]);
        let by_id: std::collections::BTreeMap<&str, &serde_json::Value> = rows
            .iter()
            .filter_map(|row| row["record_id"].as_str().map(|id| (id, row)))
            .collect();
        let retired_row = by_id.get(old.as_str()).expect("retired row present");
        assert_eq!(retired_row["retirement_state"]["state"], "retired");
        assert_eq!(
            retired_row["retirement_state"]["reason"],
            "operator-decision"
        );
        let active_row = by_id.get(new.as_str()).expect("active row present");
        assert_eq!(active_row["retirement_state"]["state"], "active");

        // Determinism (AC9): the same recall query twice is byte-identical.
        let recall_args = [
            "query",
            "semantic-memory",
            "what did past sessions learn?",
            "--collapse",
            "--collapse-mode",
            "normalized-text",
            "--format",
            "json",
            "--limit",
            "25",
            "--include-retired",
        ];
        let (code_a, out_a, _) = run(&store, &recall_args);
        let (code_b, out_b, _) = run(&store, &recall_args);
        assert_eq!((code_a, code_b), (0, 0));
        assert_eq!(out_a, out_b, "recall output must be byte-identical");
    }

    // -- AC7: reinstate round-trip ------------------------------------------------------

    #[test]
    fn reinstate_roundtrip_restores_recall_and_keeps_trail() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let old = obs_id("old");

        let (code, _, _) = run(
            &store,
            &[
                "retire",
                &old,
                "--reason",
                "operator-decision",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 0);

        let (code, stdout, _) = run(
            &store,
            &[
                "reinstate",
                &old,
                "--reason",
                "the operator re-checked and the belief holds",
                "--reinstated-by",
                "op-1",
                "--transaction-time",
                TX_REINSTATE,
            ],
        );
        assert_eq!(code, 0);
        let envelope = stdout_envelope(&stdout);
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["action"], "reinstated");
        assert_eq!(envelope["receipt"]["reinstated_record_id"], old);

        // Default recall sees the record again.
        let rows = semantic_memory_rows(&store, &[]);
        let recalled = rows
            .iter()
            .filter_map(|row| row["record_id"].as_str())
            .any(|id| id == old);
        assert!(recalled, "reinstated record must recall");

        // The full trail is queryable: one retirement + one reinstatement.
        let (code, stdout, _) = run(&store, &["query", "retirement", &old]);
        assert_eq!(code, 0);
        let envelope = stdout_envelope(&stdout);
        assert_eq!(envelope["retirement_state"]["state"], "active");
        let receipts = envelope["receipts"].as_array().expect("receipts array");
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0]["action"], "retired");
        assert_eq!(receipts[1]["action"], "reinstated");
    }

    // -- AC8: queryable receipts, untouched provenance ------------------------------------

    #[test]
    fn receipts_are_queryable_and_provenance_is_untouched() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let old = obs_id("old");
        let new = obs_id("new");

        // Snapshot the target's provenance bytes before retirement.
        let sink = EmbeddedAletheiaSink::open(&store).expect("store opens");
        let before: Vec<String> = sink
            .read_all_records()
            .expect("reads")
            .iter()
            .filter(|record| record.id() == old)
            .map(|record| serde_json::to_string(record).expect("serializable"))
            .collect();
        drop(sink);
        assert_eq!(before.len(), 1);

        let (code, _, _) = run(
            &store,
            &[
                "retire",
                &old,
                "--reason",
                "superseded",
                "--superseded-by",
                &new,
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 0);

        // The original record's bytes are unchanged.
        let sink = EmbeddedAletheiaSink::open(&store).expect("store opens");
        let after: Vec<String> = sink
            .read_all_records()
            .expect("reads")
            .iter()
            .filter(|record| record.id() == old)
            .map(|record| serde_json::to_string(record).expect("serializable"))
            .collect();
        drop(sink);
        assert_eq!(before, after, "retirement must not mutate the target");

        // The receipt is queryable.
        let (code, stdout, _) = run(&store, &["query", "retirement", &old]);
        assert_eq!(code, 0);
        let envelope = stdout_envelope(&stdout);
        assert_eq!(envelope["retirement_state"]["state"], "retired");
        assert_eq!(envelope["retirement_state"]["reason"], "superseded");
        assert_eq!(envelope["retirement_state"]["retired_by"], "op-1");
        let receipts = envelope["receipts"].as_array().expect("receipts array");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0]["action"], "retired");
        assert_eq!(receipts[0]["reason"], "superseded");
        assert_eq!(receipts[0]["superseded_by"], new);
    }

    // -- AC9: code-graph facts are refused; zero code records change ------------------------

    #[test]
    fn retire_refuses_codegraph_fact_and_writes_nothing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let before = record_count(&store);

        let (code, _, stderr) = run(
            &store,
            &[
                "retire",
                "codegraph:v1:repo:src/main.rs:main",
                "--reason",
                "operator-decision",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 1, "code-graph facts must be refused");
        let envelope = stderr_envelope(&stderr);
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error"]["code"], "retire_codegraph_fact");
        assert_eq!(record_count(&store), before, "zero records may change");
    }

    // -- AC10: transaction-time as_of ---------------------------------------------------------

    #[test]
    fn as_of_before_retirement_sees_record_active() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let old = obs_id("old");

        let (code, _, _) = run(
            &store,
            &[
                "retire",
                &old,
                "--reason",
                "operator-decision",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 0);

        // Pinned before the retirement: active.
        let (code, stdout, _) = run(
            &store,
            &[
                "query",
                "retirement",
                &old,
                "--as-of",
                "2026-09-28T11:59:59Z",
            ],
        );
        assert_eq!(code, 0);
        let envelope = stdout_envelope(&stdout);
        assert_eq!(envelope["retirement_state"]["state"], "active");
        // The record itself is retrievable at the pin: the past stays
        // queryable (AC2).
        assert_eq!(envelope["target"]["id"], serde_json::json!(old));

        // Unpinned (now): retired.
        let (code, stdout, _) = run(&store, &["query", "retirement", &old]);
        assert_eq!(code, 0);
        assert_eq!(
            stdout_envelope(&stdout)["retirement_state"]["state"],
            "retired"
        );
    }

    // -- AC5: determinism -------------------------------------------------------------------------

    /// Every artifact one retire→reinstate sequence produces, compared
    /// byte-for-byte across runs.
    struct SequenceArtifacts {
        retire_receipt: String,
        edge: String,
        retire_envelope: String,
        reinstate_receipt: String,
        reinstate_envelope: String,
        recall_output: String,
    }

    /// Runs one retire→reinstate sequence on a fresh seeded graph at fixed
    /// transaction times and returns every artifact compared across runs.
    fn run_sequence(graph: &Path, old: &str, new: &str) -> SequenceArtifacts {
        let retire_output = egregore()
            .args([
                "retire",
                old,
                "--reason",
                "superseded",
                "--superseded-by",
                new,
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
                "--graph",
            ])
            .arg(graph)
            .assert()
            .get_output()
            .clone();
        assert!(
            retire_output.status.success(),
            "retire should succeed: {}",
            String::from_utf8_lossy(&retire_output.stderr)
        );
        let reinstate_output = egregore()
            .args([
                "reinstate",
                old,
                "--reason",
                "belief restored",
                "--reinstated-by",
                "op-2",
                "--transaction-time",
                TX_REINSTATE,
                "--graph",
            ])
            .arg(graph)
            .assert()
            .get_output()
            .clone();
        assert!(
            reinstate_output.status.success(),
            "reinstate should succeed: {}",
            String::from_utf8_lossy(&reinstate_output.stderr)
        );
        // Three records appended to the seed of six: retirement receipt,
        // SUPERSEDES edge, reinstatement receipt.
        let written = fs::read_to_string(graph).expect("graph readable");
        let appended: Vec<&str> = written.lines().collect();
        assert_eq!(
            appended.len(),
            9,
            "three records appended to the seed of six"
        );
        // Final recall output: `query retirement` sees the record active
        // again with the full retire+reinstate trail.
        let recall_output = egregore()
            .args(["query", "retirement", old, "--graph"])
            .arg(graph)
            .assert()
            .get_output()
            .clone();
        assert!(
            recall_output.status.success(),
            "query retirement should succeed: {}",
            String::from_utf8_lossy(&recall_output.stderr)
        );
        SequenceArtifacts {
            retire_receipt: appended[6].to_owned(),
            edge: appended[7].to_owned(),
            retire_envelope: String::from_utf8_lossy(&retire_output.stdout).into_owned(),
            reinstate_receipt: appended[8].to_owned(),
            reinstate_envelope: String::from_utf8_lossy(&reinstate_output.stdout).into_owned(),
            recall_output: String::from_utf8_lossy(&recall_output.stdout).into_owned(),
        }
    }

    /// Issue AC9: an identical retire+reinstate sequence is byte-identical
    /// across five runs — the retirement receipt, the SUPERSEDES edge, the
    /// reinstatement receipt, both stdout envelopes and exit codes, and the
    /// final recall output.
    #[test]
    fn five_identical_retire_reinstate_sequences_are_byte_identical() {
        let mut runs = Vec::new();
        for _ in 0..5 {
            let temp = tempfile::tempdir().expect("temp dir");
            let graph = temp.path().join("graph.jsonl");
            let mut lines = String::new();
            for record in seed_records() {
                lines.push_str(&serde_json::to_string(&record).expect("serializable"));
                lines.push('\n');
            }
            fs::write(&graph, lines).expect("seed JSONL written");
            runs.push(run_sequence(&graph, &obs_id("old"), &obs_id("new")));
        }
        // Everything byte-identical across the five runs, including the
        // exit-0 successes asserted per run inside `run_sequence`.
        let first = &runs[0];
        for run in runs.iter().skip(1) {
            assert_eq!(
                run.retire_receipt, first.retire_receipt,
                "retirement receipts must be byte-identical"
            );
            assert_eq!(run.edge, first.edge, "edges must be byte-identical");
            assert_eq!(
                run.retire_envelope, first.retire_envelope,
                "retire envelopes must be byte-identical"
            );
            assert_eq!(
                run.reinstate_receipt, first.reinstate_receipt,
                "reinstatement receipts must be byte-identical"
            );
            assert_eq!(
                run.reinstate_envelope, first.reinstate_envelope,
                "reinstate envelopes must be byte-identical"
            );
            assert_eq!(
                run.recall_output, first.recall_output,
                "final recall output must be byte-identical"
            );
        }
        // Spot-check the parsed shapes: receipt kinds and the final state.
        let receipt: GraphRecord =
            serde_json::from_str(&first.retire_receipt).expect("receipt parses");
        assert!(matches!(
            receipt,
            GraphRecord::Node {
                kind: NodeKind::RetirementReceipt,
                ..
            }
        ));
        let reinstatement: GraphRecord =
            serde_json::from_str(&first.reinstate_receipt).expect("reinstatement parses");
        assert!(matches!(
            reinstatement,
            GraphRecord::Node {
                kind: NodeKind::ReinstatementReceipt,
                ..
            }
        ));
        let recall: serde_json::Value =
            serde_json::from_str(&first.recall_output).expect("recall output parses");
        assert_eq!(recall["retirement_state"]["state"], "active");
        assert_eq!(recall["receipts"].as_array().map(Vec::len), Some(2));
    }

    // -- AC4: failure lineage ------------------------------------------------------------------------

    #[test]
    fn failure_record_retires_and_is_retrievable() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = seed_store(temp.path());
        let fail = obs_id("fail");

        let (code, stdout, _) = run(
            &store,
            &[
                "retire",
                &fail,
                "--reason",
                "contradicted",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
            ],
        );
        assert_eq!(code, 0);
        assert_eq!(
            stdout_envelope(&stdout)["receipt"]["reason"],
            "contradicted"
        );

        // Retired failure is excluded from default recall...
        let rows = semantic_memory_rows(&store, &[]);
        let recalled = rows
            .iter()
            .filter_map(|row| row["record_id"].as_str())
            .any(|id| id == fail);
        assert!(!recalled);

        // ...but retrievable with --include-retired, labeled.
        let rows = semantic_memory_rows(&store, &["--include-retired"]);
        let row = rows
            .iter()
            .find(|row| row["record_id"].as_str() == Some(fail.as_str()))
            .expect("retired failure retrievable");
        assert_eq!(row["retirement_state"]["state"], "retired");
        assert_eq!(row["kind"], "Failure");
    }

    // -- --graph write path -----------------------------------------------------------------------------

    #[test]
    fn graph_file_write_path_appends_receipt_line() {
        let temp = tempfile::tempdir().expect("temp dir");
        let graph = temp.path().join("graph.jsonl");
        let mut lines = String::new();
        for record in seed_records() {
            lines.push_str(&serde_json::to_string(&record).expect("serializable"));
            lines.push('\n');
        }
        fs::write(&graph, &lines).expect("seed JSONL written");
        let old = obs_id("old");

        let output = egregore()
            .args([
                "retire",
                &old,
                "--reason",
                "operator-decision",
                "--retired-by",
                "op-1",
                "--transaction-time",
                TX_RETIRE,
                "--graph",
            ])
            .arg(&graph)
            .assert()
            .get_output()
            .clone();
        assert!(
            output.status.success(),
            "retire --graph should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let written = fs::read_to_string(&graph).expect("graph readable");
        let appended: Vec<&str> = written.lines().collect();
        assert_eq!(appended.len(), 7, "one receipt line appended");
        let receipt: GraphRecord = serde_json::from_str(appended[6]).expect("receipt parses");
        match receipt {
            GraphRecord::Node {
                kind,
                source_handle,
                agent_id,
                ..
            } => {
                assert_eq!(kind, NodeKind::RetirementReceipt);
                assert_eq!(source_handle.as_deref(), Some(old.as_str()));
                assert_eq!(agent_id.as_deref(), Some("op-1"));
            }
            _ => panic!("appended record must be the receipt node"),
        }
    }

    // -- backend exclusivity -------------------------------------------------------------------

    #[test]
    fn graph_and_data_dir_together_are_refused() {
        let temp = tempfile::tempdir().expect("temp dir");
        let graph = temp.path().join("graph.jsonl");
        let mut lines = String::new();
        for record in seed_records() {
            lines.push_str(&serde_json::to_string(&record).expect("serializable"));
            lines.push('\n');
        }
        fs::write(&graph, lines).expect("seed JSONL written");
        let store = temp.path().join("store");
        let old = obs_id("old");

        // An explicit --data-dir alongside --graph is refused: explicitness is
        // tracked, never inferred from the default value.
        let output = egregore()
            .args(["retire", &old, "--reason", "operator-decision", "--graph"])
            .arg(&graph)
            .args(["--data-dir"])
            .arg(&store)
            .assert()
            .get_output()
            .clone();
        assert_eq!(
            output.status.code(),
            Some(1),
            "both backends should be refused with exit 1: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("provide only one of --graph or --data-dir, not both"),
            "stable diagnostic, got: {stderr}"
        );
        // Zero writes: the graph file is untouched.
        let written = fs::read_to_string(&graph).expect("graph readable");
        assert_eq!(written.lines().count(), 6, "no receipt appended");
    }
}
