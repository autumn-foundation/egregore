#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]

//! Corpus-structure and metrics tests for the agent-memory recall slice
//! (issue #91).
//!
//! The model-based recall metric runs via `eg eval-memory-recall` against a
//! store seeded from `corpus/agent_memory_recall_seed.jsonl`; that path needs
//! the local embedding model and is exercised by the operator / CI. These tests
//! validate, without any model, that:
//!
//! - the checked-in corpus and seed exist and parse (AC6),
//! - the corpus has >= 20 questions with unique IDs, each labeling >= 1 expected
//!   memory record ID that is present in the seed,
//! - a free-floating lesson (attached to no symbol) is among the targets (AC4),
//! - the metrics computation and canonical ordering are deterministic (AC7),
//! - the documented workflow file exists (AC1/AC2/AC5).

use std::{collections::HashSet, path::PathBuf};

use aletheia_egregore::memory_recall_eval::{
    MemoryHit, MemoryQuery, MemoryRecallCorpus, build_report, canonical_order, compute_metrics,
    evaluate_query,
};

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/agent_memory_recall_corpus.json")
}

fn seed_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/agent_memory_recall_seed.jsonl")
}

// ---------------------------------------------------------------------------
// Corpus + seed files
// ---------------------------------------------------------------------------

#[test]
fn corpus_and_seed_files_exist() {
    assert!(
        corpus_path().exists(),
        "corpus/agent_memory_recall_corpus.json must exist"
    );
    assert!(
        seed_path().exists(),
        "corpus/agent_memory_recall_seed.jsonl must exist"
    );
}

#[test]
fn corpus_is_valid_json_with_required_top_level_fields() {
    let corpus = MemoryRecallCorpus::from_json_file(&corpus_path()).expect("corpus parses");
    assert!(
        !corpus.corpus_version.is_empty(),
        "corpus_version non-empty"
    );
    assert!(!corpus.description.is_empty(), "description non-empty");
    assert!(!corpus.questions.is_empty(), "questions non-empty");
}

#[test]
fn corpus_has_at_least_20_questions() {
    let corpus = MemoryRecallCorpus::from_json_file(&corpus_path()).expect("corpus parses");
    assert!(
        corpus.questions.len() >= 20,
        "corpus must have >= 20 questions; found {}",
        corpus.questions.len()
    );
}

#[test]
fn corpus_questions_have_unique_ids() {
    let corpus = MemoryRecallCorpus::from_json_file(&corpus_path()).expect("corpus parses");
    let ids: Vec<&str> = corpus.questions.iter().map(|q| q.id.as_str()).collect();
    let unique: HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(
        ids.len(),
        unique.len(),
        "every question must have a unique id"
    );
}

#[test]
fn corpus_questions_have_text_and_expected_targets() {
    let corpus = MemoryRecallCorpus::from_json_file(&corpus_path()).expect("corpus parses");
    for q in &corpus.questions {
        assert!(!q.text.is_empty(), "question {} must have text", q.id);
        assert!(
            !q.expected_record_ids.is_empty(),
            "question {} must label at least one expected record id",
            q.id
        );
        for id in &q.expected_record_ids {
            assert!(
                id.starts_with("agent_memory:"),
                "question {} expected id {id} must be an agent-memory record id",
                q.id
            );
        }
    }
}

/// Parses the seed JSONL and returns the set of agent-memory node IDs it
/// defines, so we can confirm every labeled target actually exists.
fn seed_memory_ids() -> HashSet<String> {
    let text = std::fs::read_to_string(seed_path()).expect("seed reads");
    let mut ids = HashSet::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line).expect("seed line is JSON");
        if value["record_type"] == "node"
            && let Some(id) = value["id"].as_str()
            && id.starts_with("agent_memory:")
        {
            ids.insert(id.to_owned());
        }
    }
    ids
}

#[test]
fn every_expected_target_exists_in_seed() {
    let corpus = MemoryRecallCorpus::from_json_file(&corpus_path()).expect("corpus parses");
    let seed_ids = seed_memory_ids();
    assert!(
        seed_ids.len() >= 20,
        "seed must define a substantial memory corpus; found {} records",
        seed_ids.len()
    );
    for q in &corpus.questions {
        for id in &q.expected_record_ids {
            assert!(
                seed_ids.contains(id),
                "question {} expects {id}, which is absent from the seed",
                q.id
            );
        }
    }
}

#[test]
fn seed_is_valid_record_jsonl_parsable_by_importer() {
    // The same parser the ingest path uses must accept the seed.
    let text = std::fs::read_to_string(seed_path()).expect("seed reads");
    let records = aletheia_egregore::adapters::records_from_jsonl(&text)
        .expect("seed must parse as graph records");
    let memory = records
        .iter()
        .filter(|r| {
            matches!(
                r.node_kind_name(),
                Some("Observation" | "Decision" | "Failure")
            )
        })
        .count();
    assert!(
        memory >= 20,
        "seed must contain >= 20 observation-class memory records; found {memory}"
    );
}

#[test]
fn corpus_includes_a_free_floating_lesson_target() {
    // AC4: at least one labeled target must be a free-floating lesson — a memory
    // record that cites no code handle (no OBSERVES/MENTIONS_SYMBOL link).
    let corpus = MemoryRecallCorpus::from_json_file(&corpus_path()).expect("corpus parses");
    let text = std::fs::read_to_string(seed_path()).expect("seed reads");

    let mut floating: HashSet<String> = HashSet::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line).expect("seed line is JSON");
        if value["record_type"] != "node" {
            continue;
        }
        let Some(id) = value["id"].as_str() else {
            continue;
        };
        if !id.starts_with("agent_memory:") {
            continue;
        }
        let cites_code = value["evidence_links"].as_array().is_some_and(|links| {
            links.iter().any(|l| {
                matches!(
                    l["relation"].as_str(),
                    Some("OBSERVES" | "MENTIONS_SYMBOL" | "TOUCHED_FILE")
                ) || l["target_domain"] == "codegraph"
            })
        });
        if !cites_code {
            floating.insert(id.to_owned());
        }
    }
    assert!(
        !floating.is_empty(),
        "seed must contain at least one free-floating lesson (no code link)"
    );

    let labeled: HashSet<&str> = corpus
        .questions
        .iter()
        .flat_map(|q| q.expected_record_ids.iter().map(String::as_str))
        .collect();
    assert!(
        floating.iter().any(|id| labeled.contains(id.as_str())),
        "at least one corpus question must target a free-floating lesson (AC4)"
    );
}

// ---------------------------------------------------------------------------
// Metrics + determinism (no store/model required)
// ---------------------------------------------------------------------------

fn hit(id: &str, score: f32) -> MemoryHit {
    MemoryHit {
        record_id: id.to_owned(),
        score,
    }
}

fn query(id: &str, expected: &[&str]) -> MemoryQuery {
    MemoryQuery {
        id: id.to_owned(),
        text: format!("question {id}"),
        expected_record_ids: expected.iter().map(|s| (*s).to_owned()).collect(),
        note: None,
    }
}

#[test]
fn canonical_order_breaks_score_ties_by_record_id() {
    // Equal scores must order by record id ascending for determinism (AC7).
    let hits = vec![hit("agent_memory:v1:b", 0.5), hit("agent_memory:v1:a", 0.5)];
    let ordered = canonical_order(&hits);
    assert_eq!(ordered, vec!["agent_memory:v1:a", "agent_memory:v1:b"]);
}

#[test]
fn evaluate_query_records_first_hit_rank() {
    let q = query("m1", &["agent_memory:v1:target"]);
    let hits = vec![
        hit("agent_memory:v1:other", 0.9),
        hit("agent_memory:v1:target", 0.8),
    ];
    let result = evaluate_query(&q, &hits);
    assert!(!result.top1_hit);
    assert!(result.top3_hit);
    assert!((result.reciprocal_rank - 0.5).abs() < 1e-9);
}

#[test]
fn evaluate_query_top1_when_target_ranks_first() {
    let q = query("m1", &["agent_memory:v1:target"]);
    let hits = vec![hit("agent_memory:v1:target", 0.9)];
    let result = evaluate_query(&q, &hits);
    assert!(result.top1_hit);
    assert!(result.top3_hit);
    assert!((result.reciprocal_rank - 1.0).abs() < 1e-9);
}

#[test]
fn metrics_top3_recall_and_mrr() {
    let results = vec![
        evaluate_query(
            &query("m1", &["agent_memory:v1:a"]),
            &[hit("agent_memory:v1:a", 0.9)],
        ),
        evaluate_query(
            &query("m2", &["agent_memory:v1:b"]),
            &[hit("agent_memory:v1:x", 0.9), hit("agent_memory:v1:b", 0.8)],
        ),
    ];
    let metrics = compute_metrics(&results);
    assert_eq!(metrics.question_count, 2);
    assert!((metrics.top3_recall - 1.0).abs() < 1e-9);
    assert!((metrics.top1_accuracy - 0.5).abs() < 1e-9);
    // MRR = (1.0 + 0.5) / 2 = 0.75
    assert!((metrics.mean_reciprocal_rank - 0.75).abs() < 1e-9);
}

#[test]
fn metrics_are_deterministic_across_five_runs() {
    let results = vec![
        evaluate_query(
            &query("m1", &["agent_memory:v1:a"]),
            &[hit("agent_memory:v1:a", 0.9)],
        ),
        evaluate_query(
            &query("m2", &["agent_memory:v1:b"]),
            &[hit("agent_memory:v1:x", 0.5), hit("agent_memory:v1:b", 0.5)],
        ),
    ];
    let first = compute_metrics(&results);
    for run in 1..5 {
        let repeat = compute_metrics(&results);
        assert_eq!(first, repeat, "metrics must be deterministic (run {run})");
    }
}

#[test]
fn build_report_pass_fail_threshold() {
    let pass = build_report(
        vec![evaluate_query(
            &query("m1", &["agent_memory:v1:a"]),
            &[hit("agent_memory:v1:a", 0.9)],
        )],
        0.8,
    );
    assert!(pass.passed);

    let fail = build_report(
        vec![evaluate_query(
            &query("m1", &["agent_memory:v1:a"]),
            &[hit("agent_memory:v1:other", 0.9)],
        )],
        0.8,
    );
    assert!(!fail.passed);
}

// ---------------------------------------------------------------------------
// Documentation
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Seed + corpus generator (ignored; run to (re)materialize the checked-in files)
//   cargo test --test memory_recall_corpus -- --ignored generate_seed_and_corpus
// ---------------------------------------------------------------------------

/// One memory record specification used by the generator.
struct Spec {
    slug: &'static str,
    kind: &'static str,
    text: &'static str,
    /// Repo-relative code path this memory cites, if any (None = free-floating).
    code: Option<&'static str>,
    verified: bool,
    /// When false, the record is written with no provenance (must be excluded
    /// from recall and is never a corpus target).
    provenance: bool,
}

fn specs() -> Vec<Spec> {
    vec![
        Spec {
            slug: "obs-thiserror-libraries",
            kind: "Observation",
            text: "Prefer thiserror over anyhow in library crates so callers can match on structured error variants instead of opaque strings.",
            code: None,
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "fail-empty-input-panic",
            kind: "Failure",
            text: "The JSONL parser panicked on empty input because it indexed the first byte without checking the length first.",
            code: Some("src/parser.rs"),
            verified: true,
            provenance: true,
        },
        Spec {
            slug: "dec-workspace-single-binary",
            kind: "Decision",
            text: "We chose a single-binary workspace layout; the eg alias is a thin re-export of the egregore binary.",
            code: None,
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "obs-tree-sitter-over-regex",
            kind: "Observation",
            text: "Use Tree-sitter for syntax parsing instead of ad hoc regular expressions, which miss nested generics and comments.",
            code: Some("src/parser.rs"),
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "obs-blake3-stable-ids",
            kind: "Observation",
            text: "Stable record identifiers hash null-terminated parts with blake3, so reordering the inputs changes the identity.",
            code: Some("src/ir.rs"),
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "fail-windows-acl",
            kind: "Failure",
            text: "Daemon runtime files were world-readable on Windows until access-control-list enforcement was added.",
            code: Some("src/daemon.rs"),
            verified: true,
            provenance: true,
        },
        Spec {
            slug: "obs-hnsw-nondeterministic-ties",
            kind: "Observation",
            text: "The approximate nearest-neighbour index returns equal-score ties in arbitrary order, so canonical-order results by record id for reproducibility.",
            code: None,
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "dec-embed-into-existing-store",
            kind: "Decision",
            text: "Agent memory is embedded into the existing vector store; the separation between code and memory happens at query time, not in a second index.",
            code: None,
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "obs-redaction-at-import",
            kind: "Observation",
            text: "Redaction runs at import time, so the observation text stored in the graph is already scrubbed of secrets before it is ever queried.",
            code: Some("src/redaction.rs"),
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "obs-msvc-crt-flag",
            kind: "Observation",
            text: "Keep the MSVC C++ runtime flag in the cargo config until the upstream tokenizer linking mismatch is fixed, or the build fails to link.",
            code: None,
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "fail-superseded-returned",
            kind: "Failure",
            text: "Semantic search kept returning old replaced nodes until results were filtered by the superseded_by marker.",
            code: Some("src/adapters/aletheiadb.rs"),
            verified: true,
            provenance: true,
        },
        Spec {
            slug: "obs-temporal-valid-time",
            kind: "Observation",
            text: "A current-tree scan stamps valid time with the wall-clock instant and marks the source as inferred from transaction time.",
            code: Some("src/lib.rs"),
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "dec-no-network-mvp",
            kind: "Decision",
            text: "No network crawling or remote repository fetching in the minimum viable product; keep extraction filesystem-local.",
            code: None,
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "obs-evidence-links-dual",
            kind: "Observation",
            text: "Evidence citations are stored both denormalized on the node and as separate graph edges so either traversal direction works.",
            code: Some("src/ir.rs"),
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "obs-incremental-cache-content-hash",
            kind: "Observation",
            text: "The incremental scan cache keys on path plus content hash rather than modification time, to avoid false cache hits when timestamps lie.",
            code: Some("src/incremental.rs"),
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "fail-daemon-lease-conflict",
            kind: "Failure",
            text: "Opening the store without a lease while the background daemon still held it produced corrupted index reads.",
            code: Some("src/daemon.rs"),
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "obs-confidence-is-string",
            kind: "Observation",
            text: "Confidence is stored as a string in the zero-to-one range rather than a float, to keep the serialized JSON byte-for-byte deterministic.",
            code: None,
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "dec-aletheiadb-published-crate",
            kind: "Decision",
            text: "Depend on the published database crate from the registry, never a local path dependency, so release builds are reproducible.",
            code: None,
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "obs-observation-not-truth",
            kind: "Observation",
            text: "An agent observation is a subjective claim and must never be treated as source truth without independent verification evidence.",
            code: None,
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "obs-cosine-not-verification",
            kind: "Observation",
            text: "Semantic similarity is a recall signal, not proof; a high cosine score is never verification that the code actually works.",
            code: None,
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "fail-empty-store-exit2",
            kind: "Failure",
            text: "A query against an empty store should exit with code two and a no-match message rather than panicking.",
            code: Some("src/cli.rs"),
            verified: true,
            provenance: true,
        },
        Spec {
            slug: "obs-tombstone-active",
            kind: "Observation",
            text: "Active tombstones mark deleted entities and must be excluded from semantic search results.",
            code: Some("src/adapters/aletheiadb.rs"),
            verified: false,
            provenance: true,
        },
        Spec {
            slug: "dec-traj-idempotent-anchor",
            kind: "Decision",
            text: "The trajectory importer anchors idempotency on the blake3 hash of the raw bytes plus the importer version, so re-ingesting is a no-op.",
            code: None,
            verified: false,
            provenance: true,
        },
        // Provenance-less record: never a corpus target; must be excluded from recall.
        Spec {
            slug: "obs-no-provenance",
            kind: "Observation",
            text: "This floating claim has no provenance and must never be recalled by meaning.",
            code: None,
            verified: false,
            provenance: false,
        },
    ]
}

fn questions() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
    vec![
        (
            "m01",
            "which error-handling crate should I use inside a library?",
            &["agent_memory:v1:obs-thiserror-libraries"],
        ),
        (
            "m02",
            "what breaks when the input file is empty?",
            &["agent_memory:v1:fail-empty-input-panic"],
        ),
        (
            "m03",
            "how is the project laid out as crates and binaries?",
            &["agent_memory:v1:dec-workspace-single-binary"],
        ),
        (
            "m04",
            "why shouldn't I use regular expressions to parse source code?",
            &["agent_memory:v1:obs-tree-sitter-over-regex"],
        ),
        (
            "m05",
            "how are stable record identities computed?",
            &["agent_memory:v1:obs-blake3-stable-ids"],
        ),
        (
            "m06",
            "were there file permission problems on Windows?",
            &["agent_memory:v1:fail-windows-acl"],
        ),
        (
            "m07",
            "how do we make vector search results reproducible across runs?",
            &["agent_memory:v1:obs-hnsw-nondeterministic-ties"],
        ),
        (
            "m08",
            "where does agent memory get indexed so it can be recalled?",
            &["agent_memory:v1:dec-embed-into-existing-store"],
        ),
        (
            "m09",
            "is the stored memory text already scrubbed of secrets?",
            &["agent_memory:v1:obs-redaction-at-import"],
        ),
        (
            "m10",
            "what compiler flag is needed for tokenizer linking on Windows?",
            &["agent_memory:v1:obs-msvc-crt-flag"],
        ),
        (
            "m11",
            "did replaced code ever show up in search after being superseded?",
            &["agent_memory:v1:fail-superseded-returned"],
        ),
        (
            "m12",
            "how is valid time set for an ordinary working-tree scan?",
            &["agent_memory:v1:obs-temporal-valid-time"],
        ),
        (
            "m13",
            "can the tool fetch remote repositories over the network?",
            &["agent_memory:v1:dec-no-network-mvp"],
        ),
        (
            "m14",
            "how are citations stored between a memory note and code?",
            &["agent_memory:v1:obs-evidence-links-dual"],
        ),
        (
            "m15",
            "what does the incremental scan cache key on?",
            &["agent_memory:v1:obs-incremental-cache-content-hash"],
        ),
        (
            "m16",
            "what happens if I open the store while the daemon is running?",
            &["agent_memory:v1:fail-daemon-lease-conflict"],
        ),
        (
            "m17",
            "what data type is the confidence value stored as?",
            &["agent_memory:v1:obs-confidence-is-string"],
        ),
        (
            "m18",
            "may I use a local path dependency for the database crate?",
            &["agent_memory:v1:dec-aletheiadb-published-crate"],
        ),
        (
            "m19",
            "should I trust an agent's note as established fact?",
            &["agent_memory:v1:obs-observation-not-truth"],
        ),
        (
            "m20",
            "is a high semantic similarity score proof that something works?",
            &["agent_memory:v1:obs-cosine-not-verification"],
        ),
        (
            "m21",
            "what should querying an empty store do?",
            &["agent_memory:v1:fail-empty-store-exit2"],
        ),
        (
            "m22",
            "are deleted nodes kept out of search results?",
            &["agent_memory:v1:obs-tombstone-active"],
        ),
        (
            "m23",
            "a general lesson about structured errors that is tied to no particular file or symbol",
            &["agent_memory:v1:obs-thiserror-libraries"],
        ),
    ]
}

#[test]
#[ignore = "generator: run explicitly to (re)write the checked-in corpus + seed"]
fn generate_seed_and_corpus() {
    use aletheia_egregore::ir::{AGENT_MEMORY_SCHEMA_VERSION, Graph, VERIFICATION_SCHEMA_VERSION};
    use aletheia_egregore::{EvidenceLink, GraphRecord, NodeKind, SourceSpan, stable_id};

    fn span() -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line: 1,
            end_line: 100,
        }
    }
    fn kind_of(name: &str) -> NodeKind {
        match name {
            "Decision" => NodeKind::Decision,
            "Failure" => NodeKind::Failure,
            _ => NodeKind::Observation,
        }
    }

    let mut graph = Graph::new();
    let mut code_files: std::collections::BTreeMap<&str, String> =
        std::collections::BTreeMap::new();

    // Code-graph file targets for OBSERVES links.
    for spec in specs() {
        if let Some(path) = spec.code {
            code_files.entry(path).or_insert_with(|| {
                let id = stable_id(&["node", "File", path]);
                graph.push(GraphRecord::syntax_node(
                    id.clone(),
                    NodeKind::File,
                    path.to_owned(),
                    span(),
                    path.to_owned(),
                    "rust",
                    format!("Source file {path}"),
                ));
                id
            });
        }
    }

    for spec in specs() {
        let id = format!("agent_memory:v1:{}", spec.slug);
        let mut links: Vec<EvidenceLink> = Vec::new();
        if let Some(path) = spec.code {
            links.push(EvidenceLink {
                target_record_id: Some(code_files[path].clone()),
                target_domain: "codegraph".to_owned(),
                relation: "OBSERVES".to_owned(),
                confidence: "0.9".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            });
        }
        if spec.verified {
            let ver_id = format!("verification:v1:ver-{}", spec.slug);
            let mut ver = GraphRecord::node(
                ver_id.clone(),
                NodeKind::Verification,
                None,
                None,
                None,
                "Verification pass".to_owned(),
            );
            if let GraphRecord::Node {
                schema_version,
                status,
                verification_kind,
                ..
            } = &mut ver
            {
                *schema_version = VERIFICATION_SCHEMA_VERSION;
                *status = Some("pass".to_owned());
                *verification_kind = Some("command_run".to_owned());
            }
            graph.push(ver);
            links.push(EvidenceLink {
                target_record_id: Some(ver_id),
                target_domain: "verification".to_owned(),
                relation: "VALIDATED_BY".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            });
        }

        let mut node = GraphRecord::node(
            id,
            kind_of(spec.kind),
            None,
            None,
            None,
            format!("{} by agent_1:sess_1", spec.kind),
        );
        if let GraphRecord::Node {
            text,
            schema_version,
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            ingested_at,
            confidence,
            source_handle,
            evidence_links,
            domain,
            ..
        } = &mut node
        {
            *text = Some(spec.text.to_owned());
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *domain = Some("agent_memory".to_owned());
            if spec.provenance {
                *agent_id = Some("agent_1".to_owned());
                *agent_kind = Some("claude-code".to_owned());
                *session_id = Some("sess_1".to_owned());
                *observed_at = Some("2026-06-03T12:00:00Z".to_owned());
                *ingested_at = Some("2026-06-03T12:00:01Z".to_owned());
                *confidence = Some("0.8".to_owned());
                *source_handle = Some("trajectories/run-1.traj".to_owned());
            }
            if !links.is_empty() {
                *evidence_links = Some(links);
            }
        }
        graph.push(node);
    }

    let seed_jsonl = graph.to_jsonl().expect("seed serializes");
    std::fs::write(seed_path(), seed_jsonl).expect("write seed");

    let questions_json: Vec<serde_json::Value> = questions()
        .into_iter()
        .map(|(id, text, expected)| {
            serde_json::json!({
                "id": id,
                "text": text,
                "expected_record_ids": expected,
            })
        })
        .collect();
    let corpus = serde_json::json!({
        "corpus_version": "1.0",
        "description": "Agent-memory recall corpus (issue #91): natural-language questions over a seeded store of imported observations, decisions, and failures, each labeling the reviewed expected memory record id(s).",
        "questions": questions_json,
    });
    std::fs::write(
        corpus_path(),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&corpus).expect("corpus serializes")
        ),
    )
    .expect("write corpus");
}

#[test]
fn semantic_memory_recall_documentation_exists() {
    let doc = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/cli/semantic-memory-recall.md");
    assert!(
        doc.exists(),
        "docs/cli/semantic-memory-recall.md must exist"
    );
    let text = std::fs::read_to_string(&doc).expect("doc reads");
    assert!(
        text.contains("semantic-memory"),
        "doc must describe the eg query semantic-memory workflow"
    );
    assert!(
        text.contains("verified-only") || text.contains("--verified-only"),
        "doc must document the verified-only trust filter"
    );
    // Recall is not verification: the doc must say a memory hit is not source truth.
    assert!(
        text.contains("not") && (text.contains("truth") || text.contains("verification")),
        "doc must state recalled memory is not source truth / not verification"
    );
}
