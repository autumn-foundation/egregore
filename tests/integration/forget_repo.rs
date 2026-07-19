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

#![allow(missing_docs, clippy::too_many_lines)]

#[cfg(feature = "embedded-aletheiadb")]
mod embedded {
    use std::{fs, path::Path};

    use aletheia_egregore::{
        EdgeLabel, EmbeddingModel, GraphRecord, IdentitySource, MetricKind, NodeKind,
        RepositoryIdentityPayload, SelectionBasis, SemanticDriftMetadata, SourceSpan,
        ir::{
            ARTIFACT_SCHEMA_VERSION, AGENT_MEMORY_SCHEMA_VERSION, ErrorSignaturePayload,
            Graph, LOG_SCHEMA_VERSION, LogPayload, PROJECT_SCHEMA_VERSION,
            VERIFICATION_SCHEMA_VERSION, log_stable_id, stable_id,
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
    fn two_repo_cross_domain_store() -> (tempfile::TempDir, std::path::PathBuf, RepoHandles, RepoHandles) {
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

        // Scoped query to repo A must now be a clean no-match (exit 2), never a
        // silent fallback to repo B.
        let (code, _stdout, _) = run(&store, &["query", "symbol", "widget", "--repo", "acme/widget-a"]);
        assert_eq!(code, 2, "evicted repo A resolves zero symbols");

        // The current serving view carries NONE of repo A's cross-domain records.
        let records = current_records(&store);
        for id in [
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
        assert_eq!(store_digest(&store), before, "dry-run must not mutate the store");
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

        assert_eq!(first, second, "pinned --transaction-time yields byte-identical envelopes");
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
        assert_eq!(events.len(), 1, "exactly one eviction event names the evicted repo");
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
        assert_eq!(candidates, ids.iter().map(String::as_str).collect::<Vec<_>>());
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
}
