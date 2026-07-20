//! Integration tests for `eg forget-repo <selector>` (issue #248): logically
//! evict EVERY record belonging to ONE repository from a shared multi-repo
//! embedded store, across ALL domains (code facts, semantic drift/embeddings,
//! agent memory, project/task, artifact, verification, log), leaving
//! co-resident repositories byte-identical.
//!
//! This is the SANCTIONED BULK EXCEPTION to issue #231's rule that deterministic
//! code facts are never tombstoned: the unit forgotten is the whole repository,
//! not a single fact being corrected.
//!
//! Coverage map:
//!   AC-GUARD  — evicting repo A leaves repo B's every-lane output BYTE-IDENTICAL
//!               and its embeddings/drift untouched (the wrong-delete guard).
//!   AC-LEAK   — after eviction every agent-facing query lane returns ZERO rows
//!               for evicted repo A (0% leakage).
//!   AC-DRY    — dry-run is the DEFAULT; without `--confirm` nothing mutates and
//!               the report enumerates per-domain counts + representative ids.
//!   AC-DET    — a pinned `--transaction-time` yields a byte-identical envelope.
//!   AC-EVENT  — `--confirm` writes EXACTLY ONE auditable eviction event.
//!   AC-IDEM   — a second `--confirm` on the same repo is a no-op success.
//!   AC-SEL    — unknown selector exits 2; ambiguous selector exits 2 with
//!               candidates listed.
//!   AC-BITEMP — a `--at`/historical read before eviction still sees repo A
//!               (bi-temporal honesty); documents the temporal current-state
//!               finding from DESIGN-248 §4.
//!   AC-GAP    — an unattributable record (legacy log with empty repository_id,
//!               or an orphan artifact) is REPORTED as unattributable and NEVER
//!               evicted (honest-gap contract).
//!   AC-XREPO  — a surviving repo-B record citing an evicted repo-A handle is
//!               KEPT; its dangling evidence link is reported, not dropped.
//!
//! RED phase: `eg forget-repo` and `aletheia_egregore::repo_evict` do not exist
//! yet, so this binary fails to compile at the `repo_evict::eviction_event_id`
//! reference and every `forget-repo` shell-out is an unrecognized subcommand.
//! The fixture builders below are the load-bearing scaffolding and must be sound.

#![allow(
    missing_docs,
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::struct_field_names,
    clippy::items_after_statements
)]

#[cfg(feature = "embedded-aletheiadb")]
mod embedded {
    use std::{fs, path::Path};

    use aletheia_egregore::{
        EdgeLabel, EmbeddingModel, GraphRecord, IdentitySource, MetricKind, NodeKind,
        RepositoryIdentityPayload, SelectionBasis, SemanticDriftMetadata, SourceSpan,
        TemporalMetadata,
        ir::{
            AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, ErrorSignaturePayload, Graph,
            LOG_SCHEMA_VERSION, LogPayload, PROJECT_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION,
            log_stable_id, stable_id,
        },
    };
    use assert_cmd::Command;

    const TX: &str = "2026-07-01T00:00:00Z";

    fn egregore() -> Command {
        Command::cargo_bin("egregore").expect("binary should build")
    }

    const fn span(start_line: usize, end_line: usize) -> SourceSpan {
        SourceSpan {
            start_byte: start_line * 10,
            end_byte: end_line * 10,
            start_line,
            end_line,
        }
    }

    /// Handles a fixture repository answers to, so tests can assert on stable ids.
    struct RepoHandles {
        repo_id: String,
        symbol_id: String,
        file_id: String,
        drift_id: String,
        observation_id: String,
        task_id: String,
        artifact_id: String,
        verification_id: String,
    }

    /// Pushes one repository's full cross-domain subgraph into `graph` and returns
    /// its stable handles. Every non-code record is reachable from an owned code
    /// record through an evidence-link edge, so `forget-repo`'s evidence walk can
    /// attribute it. NON-temporal on purpose: base-id tombstones suppress these
    /// from every current-state read (DESIGN-248 §4).
    fn push_repo(graph: &mut Graph, display: &str, remote: &str, drift_score: f64) -> RepoHandles {
        let repo_id = stable_id(&["repository", "remote", remote]);
        graph.push(
            GraphRecord::node(
                repo_id.clone(),
                NodeKind::Repository,
                None,
                None,
                Some(display.to_owned()),
                format!("Repository {display}"),
            )
            .with_repository_identity(RepositoryIdentityPayload {
                identity_source: IdentitySource::Remote,
                remote_url: Some(remote.to_owned()),
                root_commit_sha: None,
                canonical_path: None,
                basename: display.rsplit('/').next().unwrap_or(display).to_owned(),
            }),
        );

        // ── code facts (codegraph): Repository -CONTAINS-> File -DEFINES-> Symbol
        let file_id = stable_id(&["node", "file", &repo_id, "src/lib.rs"]);
        graph.push(GraphRecord::node(
            file_id.clone(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("src/lib.rs".to_owned()),
            format!("Rust source file src/lib.rs in {display}"),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.clone(),
            file_id.clone(),
            Some("1.0".to_owned()),
            "Repository contains source file".to_owned(),
        ));
        let symbol_id = stable_id(&["node", "symbol", "function", &repo_id, "widget"]);
        graph.push(GraphRecord::symbol(
            symbol_id.clone(),
            "function",
            "src/lib.rs".to_owned(),
            span(10, 20),
            "widget".to_owned(),
            format!("Rust function widget in {display}"),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Defines,
            file_id.clone(),
            symbol_id.clone(),
            Some("1.0".to_owned()),
            "file defines symbol".to_owned(),
        ));

        // ── semantic drift (semantic): SemanticDrift -DRIFTS_FROM-> Symbol
        // (attributed by RepositoryIndex; the store's embeddings representative).
        let drift_id = stable_id(&["node", "semantic-drift", &repo_id, "widget"]);
        graph.push(
            GraphRecord::node(
                drift_id.clone(),
                NodeKind::SemanticDrift,
                Some("src/lib.rs".to_owned()),
                None,
                Some("widget".to_owned()),
                format!("semantic drift for widget in {display}"),
            )
            .with_semantic_drift(SemanticDriftMetadata {
                embedding_model: EmbeddingModel {
                    provider: "test".to_owned(),
                    name: "test-model-v1".to_owned(),
                    version: "v1".to_owned(),
                    dim: 384,
                    content_hash: "fixture".to_owned(),
                },
                target_record_id: symbol_id.clone(),
                prior_record_id: symbol_id.clone(),
                before_git_commit: "aaaaaaaa".to_owned(),
                after_git_commit: "bbbbbbbb".to_owned(),
                before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
                after_valid_time: "2026-01-03T00:00:00Z".to_owned(),
                metric_kind: MetricKind::CosineDistance,
                score: drift_score,
                selection_threshold: 0.2,
                selection_basis: SelectionBasis::ThresholdOnly,
            }),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::DriftsFrom,
            drift_id.clone(),
            symbol_id.clone(),
            Some("1.0".to_owned()),
            "drift targets symbol".to_owned(),
        ));

        // ── agent memory (agent_memory): Observation -OBSERVES-> Symbol
        let observation_id = stable_id(&["node", "observation", &repo_id, "obs-0"]);
        graph.push(
            GraphRecord::node(
                observation_id.clone(),
                NodeKind::Observation,
                None,
                None,
                Some("observation".to_owned()),
                format!("agent observation about {display}"),
            )
            .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Observes,
            observation_id.clone(),
            symbol_id.clone(),
            Some("0.9".to_owned()),
            "observation observes symbol".to_owned(),
        ));

        // ── project (project): Task -TOUCHES_FILE-> File
        let task_id = stable_id(&["node", "task", &repo_id, "task-0"]);
        graph.push(
            GraphRecord::node(
                task_id.clone(),
                NodeKind::Task,
                None,
                None,
                Some("task".to_owned()),
                format!("task for {display}"),
            )
            .with_domain("project", PROJECT_SCHEMA_VERSION),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::TouchesFile,
            task_id.clone(),
            file_id.clone(),
            Some("1.0".to_owned()),
            "task touches file".to_owned(),
        ));

        // ── artifact (artifact): Observation -PRODUCED_PATCH-> Artifact
        // (reached two hops out: Symbol <- Observation -> Artifact).
        let artifact_id = stable_id(&["node", "artifact", &repo_id, "artifact-0"]);
        graph.push(
            GraphRecord::node(
                artifact_id.clone(),
                NodeKind::Artifact,
                None,
                None,
                Some("artifact".to_owned()),
                format!("artifact for {display}"),
            )
            .with_domain("artifact", ARTIFACT_SCHEMA_VERSION),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::ProducedPatch,
            observation_id.clone(),
            artifact_id.clone(),
            Some("1.0".to_owned()),
            "observation produced patch artifact".to_owned(),
        ));

        // ── verification (verification): Symbol -HAS_EVIDENCE-> Verification
        let verification_id = stable_id(&["node", "verification", &repo_id, "verif-0"]);
        graph.push(
            GraphRecord::node(
                verification_id.clone(),
                NodeKind::Verification,
                None,
                None,
                Some("verification".to_owned()),
                format!("verification for {display}"),
            )
            .with_domain("verification", VERIFICATION_SCHEMA_VERSION),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::HasEvidence,
            symbol_id.clone(),
            verification_id.clone(),
            Some("1.0".to_owned()),
            "symbol has verification evidence".to_owned(),
        ));

        // ── log (log): ErrorSignature carrying repository_id (#362) attributes
        // via RepositoryIndex directly.
        let sig_id = log_stable_id(&["error_signature", &repo_id, "boom"]);
        graph.push(
            GraphRecord::node(
                sig_id,
                NodeKind::ErrorSignature,
                None,
                None,
                Some("error signature".to_owned()),
                format!("error signature in {display}"),
            )
            .with_domain("log", LOG_SCHEMA_VERSION)
            .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
                fingerprint_algorithm: "template-v1".to_owned(),
                template_excerpt: format!("boom in {display}"),
                severity: "error".to_owned(),
                occurrence_count: 1,
                first_seen: "2026-01-01T00:00:00Z".to_owned(),
                last_seen: "2026-01-01T00:00:00Z".to_owned(),
                frames: None,
                repository_id: repo_id.clone(),
            })),
        );

        RepoHandles {
            repo_id,
            symbol_id,
            file_id,
            drift_id,
            observation_id,
            task_id,
            artifact_id,
            verification_id,
        }
    }

    /// Builds a two-repository interleaved store. Both repos carry code +
    /// semantic drift + one agent observation + one task + one artifact + one
    /// verification + one log signature. Returns `(tempdir, store_path, repo A
    /// handles, repo B handles)`.
    fn two_repo_cross_domain_store() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        RepoHandles,
        RepoHandles,
    ) {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut graph = Graph::new();
        let a = push_repo(
            &mut graph,
            "acme/widget-a",
            "https://example.com/acme/widget-a",
            0.5,
        );
        let b = push_repo(
            &mut graph,
            "acme/widget-b",
            "https://example.com/acme/widget-b",
            0.4,
        );
        let jsonl = temp.path().join("store.jsonl");
        fs::write(&jsonl, graph.to_jsonl().expect("serialize graph")).expect("write fixture");
        (temp, jsonl, a, b)
    }

    /// Ingests a graph JSONL into a fresh embedded store at `<dir>/store`.
    fn ingest_store(dir: &Path, jsonl: &Path) -> std::path::PathBuf {
        let store = dir.join("store");
        egregore()
            .args(["ingest"])
            .arg(jsonl)
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

    /// The embedded engine logs index-restore lines to stderr, so the JSON
    /// envelope is the LAST non-empty stderr line.
    fn stderr_envelope(stderr: &str) -> serde_json::Value {
        let line = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .expect("stderr should carry a JSON envelope");
        serde_json::from_str(line).expect("machine-readable stderr envelope")
    }

    /// Runs `eg forget-repo` with the given selector and flags (dry-run unless
    /// `confirm`).
    fn forget_repo(store: &Path, selector: &str, confirm: bool) -> (i32, String, String) {
        let mut args = vec![
            "forget-repo",
            selector,
            "--reason",
            "offboarded customer repository",
            "--evicted-by",
            "op-1",
            "--transaction-time",
            TX,
        ];
        if confirm {
            args.push("--confirm");
        }
        run(store, &args)
    }

    /// Reads the current serving view directly from the embedded store.
    fn current_records(store: &Path) -> Vec<GraphRecord> {
        let sink =
            aletheia_egregore::adapters::EmbeddedAletheiaSink::open(store).expect("store opens");
        sink.read_all_records().expect("current view reads")
    }

    /// A recursive checksum of a store directory's bytes, so a test can assert a
    /// dry-run mutated nothing on disk.
    fn store_digest(store: &Path) -> Vec<(String, Vec<u8>)> {
        let mut entries = Vec::new();
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for entry in fs::read_dir(dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).expect("rel").display().to_string();
                    out.push((rel, fs::read(&path).expect("read file")));
                }
            }
        }
        walk(store, store, &mut entries);
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    // ── AC-GUARD: the wrong-delete guard (most important) ────────────────────

    /// Evicting repo A must leave repo B BYTE-IDENTICAL on every lane, and its
    /// semantic drift (embeddings representative) untouched.
    #[test]
    fn wrong_delete_guard_repo_b_byte_identical() {
        let (temp, jsonl, _a, b) = two_repo_cross_domain_store();
        let store = ingest_store(temp.path(), &jsonl);

        // Baseline repo-B outputs across every scoped lane BEFORE eviction.
        let lanes: [&[&str]; 4] = [
            &["query", "symbol", "widget", "--repo", "acme/widget-b"],
            &["query", "file", "src/lib.rs", "--repo", "acme/widget-b"],
            &["query", "drift", "--repo", "acme/widget-b"],
            &["query", "context", "widget", "--repo", "acme/widget-b"],
        ];
        let before: Vec<(i32, String)> = lanes
            .iter()
            .map(|args| {
                let (code, out, _) = run(&store, args);
                (code, out)
            })
            .collect();

        let (code, _out, stderr) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0, "eviction of repo A succeeds: {stderr}");

        for (args, baseline) in lanes.iter().zip(before.iter()) {
            let (code, out, _) = run(&store, args);
            assert_eq!(
                (code, out.as_str()),
                (baseline.0, baseline.1.as_str()),
                "repo B lane {args:?} must be byte-identical after evicting repo A"
            );
        }

        // Repo B's every record still resolves in the current serving view.
        let records = current_records(&store);
        for id in [
            &b.repo_id,
            &b.symbol_id,
            &b.file_id,
            &b.drift_id,
            &b.observation_id,
            &b.task_id,
            &b.artifact_id,
            &b.verification_id,
        ] {
            assert!(
                records.iter().any(|r| r.id() == *id),
                "repo B record {id} must survive eviction of repo A"
            );
        }
    }

    // ── AC-LEAK: 0% evicted-repo leakage across every query lane ──────────────

    #[test]
    fn evicted_repo_a_zero_leakage_all_query_lanes() {
        let (temp, jsonl, a, _b) = two_repo_cross_domain_store();
        let store = ingest_store(temp.path(), &jsonl);

        let (code, _, stderr) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0, "eviction succeeds: {stderr}");

        // Eviction tombstones the repository IDENTITY node too (catalog-clean), so
        // repo A drops off the catalog surface entirely: a `--repo acme/widget-a`
        // selector no longer resolves and the scoped lane exits 1 (unknown
        // selector), NOT the emptied-shell's exit-2 in-repo no-match. This is the
        // honest post-identity-tombstone behavior; see docs/cli/forget-repo.md.
        let (code, _stdout, _) = run(
            &store,
            &["query", "symbol", "widget", "--repo", "acme/widget-a"],
        );
        assert_eq!(
            code, 1,
            "evicted repo A no longer resolves as a --repo selector"
        );

        // Zero leakage proven WITHOUT depending on the --repo exit code: the
        // UNSCOPED symbol lane must not surface repo A's symbol, and repo A's
        // identity is gone from the catalog (no Repository node with its id).
        let (code, unscoped, _) = run(&store, &["query", "symbol", "widget"]);
        assert_eq!(code, 0, "unscoped widget lookup still resolves repo B");
        assert!(
            !unscoped.contains(&a.symbol_id) && !unscoped.contains(&a.repo_id),
            "no repo-A row may leak into the unscoped lane: {unscoped}"
        );

        // The current serving view carries NONE of repo A's cross-domain records —
        // INCLUDING its Repository identity node (the catalog entry).
        let records = current_records(&store);
        for id in [
            &a.repo_id,
            &a.symbol_id,
            &a.file_id,
            &a.drift_id,
            &a.observation_id,
            &a.task_id,
            &a.artifact_id,
            &a.verification_id,
        ] {
            assert!(
                records.iter().all(|r| r.id() != *id),
                "evicted repo A record {id} must not surface in any current-state read"
            );
        }
    }

    // ── AC-DRY: dry-run is the default and mutates nothing ────────────────────

    #[test]
    fn dry_run_default_mutates_nothing() {
        let (temp, jsonl, a, _b) = two_repo_cross_domain_store();
        let store = ingest_store(temp.path(), &jsonl);

        let before = store_digest(&store);

        // No `--confirm`: strictly read-only.
        let (code, stdout, stderr) = forget_repo(&store, "acme/widget-a", false);
        assert_eq!(code, 0, "dry-run succeeds: {stderr}");
        let envelope: serde_json::Value =
            serde_json::from_str(stdout.trim()).expect("dry-run JSON envelope");
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["action"], "dry_run");
        assert_eq!(envelope["repository"]["id"], a.repo_id);

        // The plan enumerates per-domain counts and representative ids.
        let planned = &envelope["planned"];
        assert!(planned["total"].as_u64().expect("total") >= 8);
        for domain in [
            "codegraph",
            "semantic",
            "agent_memory",
            "project",
            "artifact",
            "verification",
            "log",
        ] {
            assert!(
                planned["by_domain"][domain].is_number(),
                "dry-run plan must count domain {domain}: {envelope}"
            );
        }

        // Nothing on disk changed and repo A is still fully live.
        assert_eq!(
            store_digest(&store),
            before,
            "dry-run must not mutate the store"
        );
        let records = current_records(&store);
        assert!(records.iter().any(|r| r.id() == a.symbol_id));
    }

    // ── AC-DET: deterministic envelope under a pinned transaction time ────────

    #[test]
    fn deterministic_envelope_across_two_runs() {
        let (temp1, jsonl1, _a1, _b1) = two_repo_cross_domain_store();
        let store1 = ingest_store(temp1.path(), &jsonl1);
        let (_c1, first, _) = forget_repo(&store1, "acme/widget-a", true);

        let (temp2, jsonl2, _a2, _b2) = two_repo_cross_domain_store();
        let store2 = ingest_store(temp2.path(), &jsonl2);
        let (_c2, second, _) = forget_repo(&store2, "acme/widget-a", true);

        assert_eq!(
            first, second,
            "pinned --transaction-time yields byte-identical envelopes"
        );
    }

    // ── AC-EVENT: exactly one auditable eviction event ────────────────────────

    #[test]
    fn confirm_writes_exactly_one_eviction_event() {
        let (temp, jsonl, a, _b) = two_repo_cross_domain_store();
        let store = ingest_store(temp.path(), &jsonl);

        let (code, _, stderr) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0, "eviction succeeds: {stderr}");

        // Compile-RED anchor: the pure-core API names the deterministic event id.
        let expected_event_id = aletheia_egregore::repo_evict::eviction_event_id(&a.repo_id);

        let records = current_records(&store);
        let events: Vec<&GraphRecord> = records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Node { kind: NodeKind::Retraction, source_handle: Some(h), .. }
                        if h == &a.repo_id
                )
            })
            .collect();
        assert_eq!(
            events.len(),
            1,
            "exactly one eviction event names the evicted repo"
        );
        assert_eq!(events[0].id(), expected_event_id);
    }

    // ── AC-IDEM: idempotent second eviction is a no-op ────────────────────────

    #[test]
    fn idempotent_second_confirm_no_duplicate_event() {
        let (temp, jsonl, a, _b) = two_repo_cross_domain_store();
        let store = ingest_store(temp.path(), &jsonl);

        let (code, _, _) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0);

        let (code, stdout, stderr) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0, "second eviction is a no-op success: {stderr}");
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
        assert_eq!(envelope["action"], "already_evicted");

        let records = current_records(&store);
        let event_count = records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Node { kind: NodeKind::Retraction, source_handle: Some(h), .. }
                        if h == &a.repo_id
                )
            })
            .count();
        assert_eq!(event_count, 1, "no duplicate eviction event on re-run");
    }

    // ── AC-REPAIR: a partial prior eviction is verified-and-repaired ──────────

    /// Builds a bare eviction EVENT node for `repo_id` (no tombstones) — the
    /// on-disk shape left by a crash between the event write and the tombstone
    /// writes, matching `repo_evict::build_event_node`'s recorded fields.
    fn eviction_event_node(repo_id: &str) -> GraphRecord {
        let mut node = GraphRecord::node(
            aletheia_egregore::repo_evict::eviction_event_id(repo_id),
            NodeKind::Retraction,
            None,
            None,
            None,
            format!("Repository eviction of {repo_id}"),
        )
        .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION);
        if let GraphRecord::Node {
            source_handle,
            agent_id,
            text,
            transaction_time,
            valid_time,
            valid_time_source,
            ..
        } = &mut node
        {
            *source_handle = Some(repo_id.to_owned());
            *agent_id = Some("op-1".to_owned());
            *text = Some("offboarded customer repository".to_owned());
            *transaction_time = Some(TX.to_owned());
            *valid_time = Some(TX.to_owned());
            *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        }
        node
    }

    /// Codex #248 P1: on finding an existing eviction event, `forget-repo` must
    /// VERIFY the attributed records are still suppressed and REPAIR (re-issue
    /// tombstones for any that are currently live) instead of unconditionally
    /// reporting `already_evicted` and writing nothing. RED trigger: a store
    /// where the eviction EVENT for repo A landed but the tombstones did not (a
    /// partial write), so repo A's records are fully live. A re-run `--confirm`
    /// must suppress them again WITHOUT writing a second event.
    #[test]
    fn partial_eviction_is_verified_and_repaired_without_second_event() {
        let (temp, jsonl, a, b) = two_repo_cross_domain_store();

        // Simulate the partial write: append only the eviction event for repo A.
        let mut graph = Graph::new();
        graph.push(eviction_event_node(&a.repo_id));
        let mut store_text = fs::read_to_string(&jsonl).expect("read fixture");
        store_text.push_str(&graph.to_jsonl().expect("serialize"));
        fs::write(&jsonl, store_text).expect("append partial eviction event");
        let store = ingest_store(temp.path(), &jsonl);

        // Pre-condition: repo A is live and the event is present.
        let before = current_records(&store);
        assert!(
            before.iter().any(|r| r.id() == a.symbol_id),
            "repo A must be live before the repair run"
        );
        assert!(
            before.iter().any(|r| matches!(
                r,
                GraphRecord::Node { kind: NodeKind::Retraction, source_handle: Some(h), .. }
                    if h == &a.repo_id
            )),
            "the prior eviction event must be present before the repair run"
        );

        // Re-run `--confirm`: must REPAIR (re-issue tombstones), not falsely no-op.
        let (code, stdout, stderr) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0, "repair confirm succeeds: {stderr}");
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
        assert_eq!(
            envelope["action"], "repaired",
            "a partial prior eviction is repaired, not no-oped: {envelope}"
        );

        // Repo A's records are now suppressed from the current-state read.
        let after = current_records(&store);
        for id in [
            &a.repo_id,
            &a.symbol_id,
            &a.file_id,
            &a.drift_id,
            &a.observation_id,
            &a.task_id,
            &a.artifact_id,
            &a.verification_id,
        ] {
            assert!(
                after.iter().all(|r| r.id() != *id),
                "repo A record {id} must be suppressed after the repair"
            );
        }
        // Repo B is untouched.
        assert!(
            after.iter().any(|r| r.id() == b.symbol_id),
            "repo B must survive the repair of repo A"
        );

        // NO second eviction event was written — the original is preserved.
        let events = after
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Node { kind: NodeKind::Retraction, source_handle: Some(h), .. }
                        if h == &a.repo_id
                )
            })
            .count();
        assert_eq!(
            events, 1,
            "repair must not write a second eviction event; the original is preserved"
        );

        // A subsequent `--confirm` is now a true idempotent no-op.
        let (code, stdout, _) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0);
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
        assert_eq!(
            envelope["action"], "already_evicted",
            "after repair, the repository is fully evicted and a re-run no-ops"
        );
    }

    // ── AC-SEL: unknown / ambiguous selector taxonomy ─────────────────────────

    #[test]
    fn unknown_selector_exits_2() {
        let (temp, jsonl, _a, _b) = two_repo_cross_domain_store();
        let store = ingest_store(temp.path(), &jsonl);

        let (code, _, stderr) = forget_repo(&store, "no-such-repo", true);
        assert_eq!(code, 2, "unknown selector exits 2");
        let envelope = stderr_envelope(&stderr);
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error"]["code"], "unknown_repository_selector");
    }

    #[test]
    fn ambiguous_selector_exits_2_with_candidates() {
        // Both fixture repos share the display basename `widget-*`; the bare
        // shared basename cannot pick one. Build a store where both repos share
        // an identical basename so the selector is genuinely ambiguous.
        let temp = tempfile::tempdir().expect("temp dir");
        let mut graph = aletheia_egregore::ir::Graph::new();
        let mut ids = Vec::new();
        for owner in ["acme", "globex"] {
            let remote = format!("https://example.com/{owner}/widget");
            let repo_id = stable_id(&["repository", "remote", &remote]);
            ids.push(repo_id.clone());
            graph.push(
                GraphRecord::node(
                    repo_id,
                    NodeKind::Repository,
                    None,
                    None,
                    Some(format!("{owner}/widget")),
                    format!("Repository {owner}/widget"),
                )
                .with_repository_identity(RepositoryIdentityPayload {
                    identity_source: IdentitySource::Remote,
                    remote_url: Some(remote),
                    root_commit_sha: None,
                    canonical_path: None,
                    basename: "widget".to_owned(),
                }),
            );
        }
        let jsonl = temp.path().join("ambiguous.jsonl");
        fs::write(&jsonl, graph.to_jsonl().expect("serialize")).expect("write fixture");
        let store = ingest_store(temp.path(), &jsonl);
        ids.sort();

        let (code, _, stderr) = forget_repo(&store, "widget", true);
        assert_eq!(code, 2, "ambiguous selector exits 2");
        let envelope = stderr_envelope(&stderr);
        assert_eq!(envelope["error"]["code"], "ambiguous_repository_selector");
        let candidates: Vec<&str> = envelope["error"]["detail"]["candidates"]
            .as_array()
            .expect("candidates listed")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(
            candidates,
            ids.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    // ── AC-BITEMP: a pre-eviction historical read still sees repo A ───────────

    /// Bi-temporal honesty: the physical records stay in the store, so a
    /// transaction-time / history view predating the eviction still reflects
    /// that repo A existed. Also documents DESIGN-248 §4: temporal (scan-history)
    /// commit snapshots are re-emitted by `read_all_records` regardless of
    /// tombstones, so a pure base-id tombstone cannot suppress them from the
    /// current-state read — GREEN must confront this for scan-history stores.
    #[test]
    fn bitemporal_history_view_before_eviction_still_sees_repo_a() {
        let (temp, jsonl, a, _b) = two_repo_cross_domain_store();
        let store = ingest_store(temp.path(), &jsonl);

        let (code, _, stderr) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0, "eviction succeeds: {stderr}");

        // The history-inclusive read still holds repo A's physical bytes.
        let sink =
            aletheia_egregore::adapters::EmbeddedAletheiaSink::open(&store).expect("store opens");
        let history = sink
            .read_all_records_including_superseded()
            .expect("history view reads");
        assert!(
            history.iter().any(|r| r.id() == a.symbol_id),
            "repo A's bytes stay reconstructable for pre-eviction transaction-time views"
        );
        // The eviction event itself is preserved and citable.
        assert!(
            history.iter().any(|r| matches!(
                r,
                GraphRecord::Node { kind: NodeKind::Retraction, source_handle: Some(h), .. }
                    if h == &a.repo_id
            )),
            "the eviction event is stored and citable"
        );
    }

    // ── AC-GAP: honest-gap — unattributable records reported, not evicted ─────

    /// A legacy `log:v2:`-shaped ErrorSignature with an EMPTY `repository_id` (the
    /// canonical unattributable record) and an orphan artifact with no evidence
    /// edge must be REPORTED under `unattributable` and NEVER evicted.
    #[test]
    fn honest_gap_unattributable_record_reported_not_evicted() {
        let (temp, jsonl, _a, _b) = two_repo_cross_domain_store();

        // Append an unattributed log signature (empty repository_id) and an
        // orphan artifact with no evidence edge to any owned record.
        let mut graph = aletheia_egregore::ir::Graph::new();
        let legacy_sig_id = log_stable_id(&["error_signature", "legacy", "orphan"]);
        graph.push(
            GraphRecord::node(
                legacy_sig_id.clone(),
                NodeKind::ErrorSignature,
                None,
                None,
                Some("error signature".to_owned()),
                "legacy unattributed signature".to_owned(),
            )
            .with_domain("log", LOG_SCHEMA_VERSION)
            .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
                fingerprint_algorithm: "template-v1".to_owned(),
                template_excerpt: "legacy boom".to_owned(),
                severity: "error".to_owned(),
                occurrence_count: 1,
                first_seen: "2026-01-01T00:00:00Z".to_owned(),
                last_seen: "2026-01-01T00:00:00Z".to_owned(),
                frames: None,
                repository_id: String::new(),
            })),
        );
        let orphan_artifact_id = stable_id(&["node", "artifact", "orphan", "no-owner"]);
        graph.push(
            GraphRecord::node(
                orphan_artifact_id.clone(),
                NodeKind::Artifact,
                None,
                None,
                Some("artifact".to_owned()),
                "orphan artifact with no attribution".to_owned(),
            )
            .with_domain("artifact", ARTIFACT_SCHEMA_VERSION),
        );

        let mut store_text = fs::read_to_string(&jsonl).expect("read fixture");
        store_text.push_str(&graph.to_jsonl().expect("serialize"));
        fs::write(&jsonl, store_text).expect("append unattributed records");
        let store = ingest_store(temp.path(), &jsonl);

        // Dry-run reports the unattributable records; they are never planned for
        // eviction.
        let (code, stdout, _) = forget_repo(&store, "acme/widget-a", false);
        assert_eq!(code, 0);
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
        let unattributable = envelope["unattributable"].to_string();
        assert!(
            unattributable.contains(&legacy_sig_id) && unattributable.contains(&orphan_artifact_id),
            "unattributable records must be reported: {envelope}"
        );

        // Confirm: the unattributable records survive.
        let (code, _, _) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0);
        let records = current_records(&store);
        assert!(records.iter().any(|r| r.id() == legacy_sig_id));
        assert!(records.iter().any(|r| r.id() == orphan_artifact_id));
    }

    // ── AC-XREPO: surviving cross-repo citation kept and reported ─────────────

    /// A surviving repo-B observation that cites repo A's evicted symbol is KEPT;
    /// the dangling evidence link is REPORTED, never silently dropped.
    #[test]
    fn cross_repo_citation_survivor_kept_and_reported() {
        let (temp, jsonl, a, b) = two_repo_cross_domain_store();

        // Repo B's observation additionally OBSERVES repo A's symbol.
        let mut graph = aletheia_egregore::ir::Graph::new();
        graph.push(GraphRecord::edge(
            EdgeLabel::Observes,
            b.observation_id.clone(),
            a.symbol_id.clone(),
            Some("0.5".to_owned()),
            "repo B observation cites repo A symbol".to_owned(),
        ));
        let mut store_text = fs::read_to_string(&jsonl).expect("read fixture");
        store_text.push_str(&graph.to_jsonl().expect("serialize"));
        fs::write(&jsonl, store_text).expect("append cross-repo citation");
        let store = ingest_store(temp.path(), &jsonl);

        let (code, stdout, _) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0);
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
        let citations = envelope["cross_repo_citations"].to_string();
        assert!(
            citations.contains(&b.observation_id) && citations.contains(&a.symbol_id),
            "the surviving cross-repo citation must be reported: {envelope}"
        );

        // Repo B's observation is untouched.
        let records = current_records(&store);
        assert!(
            records.iter().any(|r| r.id() == b.observation_id),
            "the citing repo B record must survive"
        );
    }

    /// Regression for the Codex #248 P2: a TOPOLOGY edge (CALLS) FROM an evicted
    /// repo-A node TO a surviving repo-B node is part of repo A's footprint and
    /// must be tombstoned when its SOURCE is an evicted node — even though the
    /// target survives. The adapter's current-state read (`read_all_records` ->
    /// `latest_edge_versions`) only suppresses an edge whose OWN id is tombstoned,
    /// so leaving the A->B edge un-tombstoned leaks part of repo A after
    /// `forget-repo --confirm`. The OTHER direction (source SURVIVING, target
    /// evicted) is an unchanged cross-repo citation: kept and reported.
    #[test]
    fn evicted_repo_a_outbound_topology_edge_is_tombstoned() {
        let (temp, jsonl, a, b) = two_repo_cross_domain_store();

        // A->B topology edge (repo A's symbol CALLS repo B's symbol): owned by A.
        let a_to_b_calls = GraphRecord::edge(
            EdgeLabel::Calls,
            a.symbol_id.clone(),
            b.symbol_id.clone(),
            Some("1.0".to_owned()),
            "repo A symbol calls repo B symbol".to_owned(),
        );
        let a_to_b_calls_id = a_to_b_calls.id().to_owned();
        // B->A evidence citation (repo B's observation OBSERVES repo A's symbol):
        // owned by surviving B; must be KEPT and REPORTED, never tombstoned.
        let b_to_a_cite = GraphRecord::edge(
            EdgeLabel::Observes,
            b.observation_id.clone(),
            a.symbol_id.clone(),
            Some("0.5".to_owned()),
            "repo B observation cites repo A symbol".to_owned(),
        );
        let b_to_a_cite_id = b_to_a_cite.id().to_owned();

        let mut graph = aletheia_egregore::ir::Graph::new();
        graph.push(a_to_b_calls);
        graph.push(b_to_a_cite);
        let mut store_text = fs::read_to_string(&jsonl).expect("read fixture");
        store_text.push_str(&graph.to_jsonl().expect("serialize"));
        fs::write(&jsonl, store_text).expect("append cross-repo edges");
        let store = ingest_store(temp.path(), &jsonl);

        // Before eviction, both edges are present in the current serving view.
        let before = current_records(&store);
        assert!(
            before.iter().any(|r| r.id() == a_to_b_calls_id),
            "A->B CALLS edge must exist before eviction"
        );

        let (code, stdout, _) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0);

        // The A->B topology edge is GONE from current records (its source, an
        // evicted repo-A node, makes it repo A's footprint).
        let after = current_records(&store);
        assert!(
            !after.iter().any(|r| r.id() == a_to_b_calls_id),
            "A->B CALLS edge must be tombstoned out of current records after evicting A"
        );

        // The B->A citation edge (owned by surviving B) is KEPT and REPORTED.
        assert!(
            after.iter().any(|r| r.id() == b_to_a_cite_id),
            "B->A citation edge (owned by surviving repo B) must be kept"
        );
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
        let citations = envelope["cross_repo_citations"].to_string();
        assert!(
            citations.contains(&b.observation_id) && citations.contains(&a.symbol_id),
            "the surviving B->A cross-repo citation must be reported: {envelope}"
        );

        // The tombstoned A->B edge must not surface as a dangling-reference defect
        // in the current serving view: serializing that view (`read_all_records`)
        // to JSONL and running `eg validate` must name the A->B edge in NO defect.
        // (The intentionally-kept B->A citation edge legitimately dangles onto the
        // evicted target and IS reported by validate — that is the reported
        // cross-repo citation, not a regression.)
        let mut view = aletheia_egregore::ir::Graph::new();
        for record in &after {
            view.push(record.clone());
        }
        let view_path = temp.path().join("post-evict.view.jsonl");
        fs::write(&view_path, view.to_jsonl().expect("serialize view")).expect("write view");
        let validate_out = egregore()
            .args(["validate"])
            .arg(&view_path)
            .args(["--format", "text"])
            .assert()
            .get_output()
            .clone();
        let validate_text = String::from_utf8(validate_out.stdout).expect("utf8 validate");
        assert!(
            !validate_text.contains(&a_to_b_calls_id),
            "validate must flag NO defect for the tombstoned A->B edge: {validate_text}"
        );
    }

    // ── AC-TEMPORAL: scan-history commit-anchored code is a documented residual ─

    /// Handles for one temporal (scan-history) fixture repository: its
    /// commit-anchored code snapshots (retained by the shared read path) and its
    /// non-temporal records (fully suppressed by a base-ID tombstone).
    struct TemporalRepoHandles {
        repo_id: String,
        /// Commit-anchored `Symbol` snapshot record IDs (the residual).
        temporal_symbol_ids: Vec<String>,
        /// A non-temporal, non-code record (agent observation) that eviction
        /// tombstones out of every current-state lane.
        observation_id: String,
    }

    /// Pushes one TEMPORAL repository: `src/lib.rs` defines `widget` at two
    /// commits (each a distinct commit-anchored `Symbol` snapshot carrying
    /// `TemporalMetadata`), plus one non-temporal agent observation reachable
    /// through an `OBSERVES` evidence edge. Mirrors the scan-history commit-model
    /// exercised by `repo_scope.rs`.
    fn push_temporal_repo(graph: &mut Graph, display: &str, remote: &str) -> TemporalRepoHandles {
        let repo_id = stable_id(&["repository", "remote", remote]);
        graph.push(
            GraphRecord::node(
                repo_id.clone(),
                NodeKind::Repository,
                None,
                None,
                Some(display.to_owned()),
                format!("Repository {display}"),
            )
            .with_repository_identity(RepositoryIdentityPayload {
                identity_source: IdentitySource::Remote,
                remote_url: Some(remote.to_owned()),
                root_commit_sha: None,
                canonical_path: None,
                basename: display.rsplit('/').next().unwrap_or(display).to_owned(),
            }),
        );

        let file_id = stable_id(&["node", "file", &repo_id, "src/lib.rs"]);
        graph.push(GraphRecord::node(
            file_id.clone(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("src/lib.rs".to_owned()),
            format!("Rust source file src/lib.rs in {display}"),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.clone(),
            file_id.clone(),
            Some("1.0".to_owned()),
            "Repository contains source file".to_owned(),
        ));

        // Two commit-anchored snapshots of the same source symbol.
        let mut temporal_symbol_ids = Vec::new();
        let mut last_symbol_id = String::new();
        for (commit, valid_time) in [
            ("aaaa000100000000", "2026-01-01T00:00:00Z"),
            ("aaaa000200000000", "2026-01-03T00:00:00Z"),
        ] {
            let symbol_id = stable_id(&[
                "node",
                "symbol",
                "function",
                &repo_id,
                "src/lib.rs",
                "widget",
                commit,
            ]);
            graph.push(
                GraphRecord::symbol(
                    symbol_id.clone(),
                    "function",
                    "src/lib.rs".to_owned(),
                    span(10, 20),
                    "widget".to_owned(),
                    format!("Rust function widget in {display} at {commit}"),
                )
                .with_temporal(TemporalMetadata {
                    git_commit: commit.to_owned(),
                    git_parent_commits: vec![],
                    valid_time: valid_time.to_owned(),
                    author_time: None,
                    observed_at: valid_time.to_owned(),
                    valid_time_source: None,
                }),
            );
            graph.push(GraphRecord::edge(
                EdgeLabel::Defines,
                file_id.clone(),
                symbol_id.clone(),
                Some("1.0".to_owned()),
                "file defines symbol".to_owned(),
            ));
            temporal_symbol_ids.push(symbol_id.clone());
            last_symbol_id = symbol_id;
        }
        temporal_symbol_ids.sort();

        // One NON-temporal agent observation, reachable from the owned code via an
        // OBSERVES evidence edge (the base-ID-tombstone-suppressible half).
        let observation_id = stable_id(&["node", "observation", &repo_id, "obs-0"]);
        graph.push(
            GraphRecord::node(
                observation_id.clone(),
                NodeKind::Observation,
                None,
                None,
                Some("observation".to_owned()),
                format!("agent observation about {display}"),
            )
            .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Observes,
            observation_id.clone(),
            last_symbol_id,
            Some("0.9".to_owned()),
            "observation observes symbol".to_owned(),
        ));

        TemporalRepoHandles {
            repo_id,
            temporal_symbol_ids,
            observation_id,
        }
    }

    /// A two-repository TEMPORAL (scan-history) store: each repo carries
    /// commit-anchored code snapshots plus one non-temporal observation.
    fn two_repo_temporal_store() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        TemporalRepoHandles,
        TemporalRepoHandles,
    ) {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut graph = Graph::new();
        let a = push_temporal_repo(
            &mut graph,
            "acme/widget-a",
            "https://example.com/acme/widget-a",
        );
        let b = push_temporal_repo(
            &mut graph,
            "acme/widget-b",
            "https://example.com/acme/widget-b",
        );
        let jsonl = temp.path().join("history.store.jsonl");
        fs::write(&jsonl, graph.to_jsonl().expect("serialize graph")).expect("write fixture");
        (temp, jsonl, a, b)
    }

    /// Pins the DESIGN-248 §4 temporal residual: on a `scan-history` store the
    /// base-ID tombstone eviction writes CANNOT suppress commit-anchored code
    /// snapshots from current-state code lanes (the shared `read_all_records`
    /// by-commit loop re-emits every snapshot with no tombstone check — the same
    /// read path that serves #231 `forget`'s `--at` bi-temporal honesty). This
    /// test characterizes the honest current behavior so it can never silently
    /// change:
    ///   (a) repo A's NON-temporal records are gone from current-state lanes;
    ///   (b) repo A's TEMPORAL code snapshots are STILL surfaced, and the report's
    ///       `temporal_snapshots_retained` section is non-empty and names them;
    ///   (c) repo B is byte-identical on every lane before vs after;
    ///   (d) exactly one eviction event is written.
    #[test]
    fn scan_history_temporal_code_snapshots_are_documented_residual() {
        let (temp, jsonl, a, b) = two_repo_temporal_store();
        let store = ingest_store(temp.path(), &jsonl);

        // Baseline repo-B lanes BEFORE eviction (the wrong-delete guard, temporal).
        let lanes: [&[&str]; 2] = [
            &["query", "symbol", "widget", "--repo", "acme/widget-b"],
            &["query", "context", "widget", "--repo", "acme/widget-b"],
        ];
        let before: Vec<(i32, String)> = lanes
            .iter()
            .map(|args| {
                let (code, out, _) = run(&store, args);
                (code, out)
            })
            .collect();

        // Dry-run must already DISCLOSE the temporal residual (present in both
        // dry-run and --confirm output).
        let (code, stdout, _) = forget_repo(&store, "acme/widget-a", false);
        assert_eq!(code, 0);
        let dry: serde_json::Value = serde_json::from_str(stdout.trim()).expect("dry-run JSON");
        let retained = dry["temporal_snapshots_retained"].to_string();
        assert!(
            dry["temporal_snapshots_retained"]["total"]
                .as_u64()
                .expect("total")
                >= 2,
            "dry-run must disclose the temporal residual: {dry}"
        );
        for id in &a.temporal_symbol_ids {
            assert!(
                retained.contains(id),
                "temporal snapshot {id} must be named in the residual section: {dry}"
            );
        }

        // Confirm the eviction.
        let (code, stdout, stderr) = forget_repo(&store, "acme/widget-a", true);
        assert_eq!(code, 0, "eviction succeeds: {stderr}");
        let confirmed: serde_json::Value =
            serde_json::from_str(stdout.trim()).expect("confirm JSON");
        assert!(
            confirmed["temporal_snapshots_retained"]["total"]
                .as_u64()
                .expect("total")
                >= 2,
            "--confirm output carries the residual section too: {confirmed}"
        );

        let records = current_records(&store);

        // (a) Repo A's NON-temporal records (observation + identity) ARE gone.
        assert!(
            records.iter().all(|r| r.id() != a.observation_id),
            "repo A's non-temporal observation must be suppressed"
        );
        assert!(
            records.iter().all(|r| r.id() != a.repo_id),
            "repo A's identity node must be suppressed (catalog-clean)"
        );

        // (b) THE RESIDUAL: repo A's temporal code snapshots STILL surface in the
        // current-state read despite the tombstones (documented, not silent).
        for id in &a.temporal_symbol_ids {
            assert!(
                records.iter().any(|r| r.id() == *id),
                "DOCUMENTED RESIDUAL: temporal snapshot {id} is re-emitted by the \
                 shared read path despite eviction; the report discloses it"
            );
        }

        // (c) Repo B is byte-identical on every lane after evicting repo A.
        for (args, baseline) in lanes.iter().zip(before.iter()) {
            let (code, out, _) = run(&store, args);
            assert_eq!(
                (code, out.as_str()),
                (baseline.0, baseline.1.as_str()),
                "repo B lane {args:?} must be byte-identical after evicting repo A"
            );
        }
        for id in &b.temporal_symbol_ids {
            assert!(
                records.iter().any(|r| r.id() == *id),
                "repo B snapshot {id} survives"
            );
        }
        assert!(records.iter().any(|r| r.id() == b.observation_id));

        // (d) Exactly one eviction event was written.
        let events = records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Node { kind: NodeKind::Retraction, source_handle: Some(h), .. }
                        if h == &a.repo_id
                )
            })
            .count();
        assert_eq!(
            events, 1,
            "exactly one eviction event for the temporal store"
        );
    }
}
