#![allow(missing_docs)]

//! TDD tests for issue #58: Calibrate semantic code search relevance.
//!
//! RED PHASE: These tests are written to fail until the corpus file,
//! `semantic_eval` module, and documentation are created.

use std::{collections::HashSet, path::PathBuf};

use aletheia_egregore::semantic_eval::{
    ExpectedTarget, QueryClass, QueryEvalResult, SearchHit, SemanticRelevanceCorpus, build_report,
    compute_metrics, evaluate_query, format_diagnostic, hit_matches_expected,
};

// ---------------------------------------------------------------------------
// Corpus file structure
// ---------------------------------------------------------------------------

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/semantic_relevance_corpus.json")
}

#[test]
fn corpus_file_exists() {
    assert!(
        corpus_path().exists(),
        "corpus/semantic_relevance_corpus.json must exist"
    );
}

#[test]
fn corpus_is_valid_json_with_required_top_level_fields() {
    let path = corpus_path();
    let text = std::fs::read_to_string(&path).expect("read corpus");
    let corpus: SemanticRelevanceCorpus =
        serde_json::from_str(&text).expect("corpus must be valid JSON");
    assert!(
        !corpus.corpus_version.is_empty(),
        "corpus_version must be non-empty"
    );
    assert!(
        !corpus.description.is_empty(),
        "description must be non-empty"
    );
    assert!(
        !corpus.queries.is_empty(),
        "queries array must be non-empty"
    );
}

#[test]
fn corpus_has_at_least_25_queries() {
    let text = std::fs::read_to_string(corpus_path()).expect("read corpus");
    let corpus: SemanticRelevanceCorpus = serde_json::from_str(&text).expect("parse corpus");
    assert!(
        corpus.queries.len() >= 25,
        "corpus must have >= 25 queries; found {}",
        corpus.queries.len()
    );
}

#[test]
fn corpus_queries_have_unique_ids() {
    let text = std::fs::read_to_string(corpus_path()).expect("read corpus");
    let corpus: SemanticRelevanceCorpus = serde_json::from_str(&text).expect("parse corpus");
    let ids: Vec<&str> = corpus.queries.iter().map(|q| q.id.as_str()).collect();
    let unique: HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(ids.len(), unique.len(), "every query must have a unique id");
}

#[test]
fn corpus_queries_have_non_empty_text_and_expected() {
    let text = std::fs::read_to_string(corpus_path()).expect("read corpus");
    let corpus: SemanticRelevanceCorpus = serde_json::from_str(&text).expect("parse corpus");
    for q in &corpus.queries {
        assert!(
            !q.text.is_empty(),
            "query {} must have non-empty text",
            q.id
        );
        // Ambiguous queries are allowed to have empty expected (they have no correct answer)
        if q.class != QueryClass::Ambiguous {
            assert!(
                !q.expected.is_empty(),
                "labeled query {} must have at least one expected target",
                q.id
            );
        }
    }
}

#[test]
fn corpus_has_required_query_classes() {
    let text = std::fs::read_to_string(corpus_path()).expect("read corpus");
    let corpus: SemanticRelevanceCorpus = serde_json::from_str(&text).expect("parse corpus");
    let classes: HashSet<String> = corpus
        .queries
        .iter()
        .map(|q| format!("{:?}", q.class))
        .collect();
    let required = [
        "ConceptAbsent",
        "SynonymHeavy",
        "Architecture",
        "ErrorHandling",
        "PersistenceQuery",
        "Ambiguous",
    ];
    for class in &required {
        assert!(
            classes.contains(*class),
            "corpus must contain at least one query of class {class}"
        );
    }
}

#[test]
fn corpus_has_at_least_5_ambiguous_queries() {
    let text = std::fs::read_to_string(corpus_path()).expect("read corpus");
    let corpus: SemanticRelevanceCorpus = serde_json::from_str(&text).expect("parse corpus");
    let ambiguous_count = corpus
        .queries
        .iter()
        .filter(|q| q.class == QueryClass::Ambiguous)
        .count();
    assert!(
        ambiguous_count >= 5,
        "corpus must have >= 5 ambiguous queries; found {ambiguous_count}"
    );
}

#[test]
fn corpus_labeled_queries_have_rg_substitute() {
    let text = std::fs::read_to_string(corpus_path()).expect("read corpus");
    let corpus: SemanticRelevanceCorpus = serde_json::from_str(&text).expect("parse corpus");
    for q in corpus.labeled_queries() {
        assert!(
            q.rg_substitute.is_some(),
            "labeled query {} must have an rg_substitute",
            q.id
        );
        let sub = q.rg_substitute.as_ref().unwrap();
        assert!(
            !sub.keyword.is_empty(),
            "rg_substitute for {} must have a non-empty keyword",
            q.id
        );
        assert!(
            !sub.note.is_empty(),
            "rg_substitute for {} must have a non-empty note",
            q.id
        );
    }
}

#[test]
fn corpus_expected_targets_have_repo_relative_path() {
    let text = std::fs::read_to_string(corpus_path()).expect("read corpus");
    let corpus: SemanticRelevanceCorpus = serde_json::from_str(&text).expect("parse corpus");
    for q in corpus.labeled_queries() {
        for target in &q.expected {
            assert!(
                !target.repo_relative_path.is_empty(),
                "expected target in query {} must have a non-empty repo_relative_path",
                q.id
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Metrics computation (unit tests — no store required)
// ---------------------------------------------------------------------------

fn make_hit(path: &str, name: Option<&str>, score: f32) -> SearchHit {
    SearchHit {
        record_id: format!("rec:{path}:{}", name.unwrap_or("")),
        name: name.map(str::to_owned),
        repo_relative_path: Some(path.to_owned()),
        score,
        span: None,
    }
}

fn make_expected(path: &str, sym: Option<&str>) -> ExpectedTarget {
    ExpectedTarget {
        repo_relative_path: path.to_owned(),
        symbol_name: sym.map(str::to_owned),
        note: None,
    }
}

#[test]
fn hit_matches_expected_by_path_only() {
    let hit = make_hit("src/lib.rs", None, 0.9);
    let expected = vec![make_expected("src/lib.rs", None)];
    assert!(hit_matches_expected(&hit, &expected));
}

#[test]
fn hit_matches_expected_by_path_and_symbol() {
    let hit = make_hit("src/lib.rs", Some("my_fn"), 0.9);
    let expected = vec![make_expected("src/lib.rs", Some("my_fn"))];
    assert!(hit_matches_expected(&hit, &expected));
}

#[test]
fn hit_does_not_match_wrong_path() {
    let hit = make_hit("src/other.rs", None, 0.9);
    let expected = vec![make_expected("src/lib.rs", None)];
    assert!(!hit_matches_expected(&hit, &expected));
}

#[test]
fn hit_does_not_match_wrong_symbol() {
    let hit = make_hit("src/lib.rs", Some("other_fn"), 0.9);
    let expected = vec![make_expected("src/lib.rs", Some("my_fn"))];
    assert!(!hit_matches_expected(&hit, &expected));
}

#[test]
fn metrics_top1_accuracy_when_all_hit() {
    let results = vec![
        QueryEvalResult {
            query_id: "q1".to_owned(),
            query_text: "q1".to_owned(),
            class: QueryClass::Architecture,
            top_k_results: vec![],
            top1_hit: true,
            top3_hit: true,
            reciprocal_rank: 1.0,
            is_false_positive: false,
        },
        QueryEvalResult {
            query_id: "q2".to_owned(),
            query_text: "q2".to_owned(),
            class: QueryClass::Architecture,
            top_k_results: vec![],
            top1_hit: true,
            top3_hit: true,
            reciprocal_rank: 1.0,
            is_false_positive: false,
        },
    ];
    let metrics = compute_metrics(&results);
    assert!(
        (metrics.top1_accuracy - 1.0).abs() < 1e-9,
        "top1_accuracy should be 1.0 when all queries hit at rank 1"
    );
}

#[test]
fn metrics_top3_recall_excludes_ambiguous_queries() {
    let results = vec![
        QueryEvalResult {
            query_id: "q1".to_owned(),
            query_text: "q1".to_owned(),
            class: QueryClass::Architecture,
            top_k_results: vec![],
            top1_hit: true,
            top3_hit: true,
            reciprocal_rank: 1.0,
            is_false_positive: false,
        },
        QueryEvalResult {
            query_id: "qa".to_owned(),
            query_text: "qa".to_owned(),
            class: QueryClass::Ambiguous,
            top_k_results: vec![],
            top1_hit: false,
            top3_hit: false,
            reciprocal_rank: 0.0,
            is_false_positive: true,
        },
    ];
    let metrics = compute_metrics(&results);
    assert_eq!(
        metrics.labeled_count, 1,
        "ambiguous queries excluded from labeled_count"
    );
    assert_eq!(metrics.ambiguous_count, 1);
    assert!(
        (metrics.top3_recall - 1.0).abs() < 1e-9,
        "top3_recall computed only over labeled queries"
    );
}

#[test]
fn metrics_mrr_is_harmonic_mean_of_ranks() {
    // First result at rank 1 (RR=1.0), second at rank 2 (RR=0.5)
    let results = vec![
        QueryEvalResult {
            query_id: "q1".to_owned(),
            query_text: "q1".to_owned(),
            class: QueryClass::Architecture,
            top_k_results: vec![],
            top1_hit: true,
            top3_hit: true,
            reciprocal_rank: 1.0,
            is_false_positive: false,
        },
        QueryEvalResult {
            query_id: "q2".to_owned(),
            query_text: "q2".to_owned(),
            class: QueryClass::SynonymHeavy,
            top_k_results: vec![],
            top1_hit: false,
            top3_hit: true,
            reciprocal_rank: 0.5,
            is_false_positive: false,
        },
    ];
    let metrics = compute_metrics(&results);
    let expected_mrr = 0.75_f64; // (1.0 + 0.5) / 2.0
    assert!(
        (metrics.mean_reciprocal_rank - expected_mrr).abs() < 1e-9,
        "MRR should be {expected_mrr}; got {}",
        metrics.mean_reciprocal_rank
    );
}

#[test]
fn metrics_false_positive_count_only_from_ambiguous() {
    let results = vec![
        QueryEvalResult {
            query_id: "labeled".to_owned(),
            query_text: "labeled".to_owned(),
            class: QueryClass::Architecture,
            top_k_results: vec![make_hit("src/lib.rs", None, 0.9)],
            top1_hit: false,
            top3_hit: false,
            reciprocal_rank: 0.0,
            is_false_positive: false,
        },
        QueryEvalResult {
            query_id: "ambig1".to_owned(),
            query_text: "ambig1".to_owned(),
            class: QueryClass::Ambiguous,
            top_k_results: vec![make_hit("src/lib.rs", None, 0.5)],
            top1_hit: false,
            top3_hit: false,
            reciprocal_rank: 0.0,
            is_false_positive: true,
        },
        QueryEvalResult {
            query_id: "ambig2".to_owned(),
            query_text: "ambig2".to_owned(),
            class: QueryClass::Ambiguous,
            top_k_results: vec![],
            top1_hit: false,
            top3_hit: false,
            reciprocal_rank: 0.0,
            is_false_positive: false,
        },
    ];
    let metrics = compute_metrics(&results);
    assert_eq!(
        metrics.false_positive_count, 1,
        "false_positive_count must count only ambiguous queries that returned results"
    );
}

#[test]
fn evaluate_query_records_first_hit_rank() {
    use aletheia_egregore::semantic_eval::CorpusQuery;
    let query = CorpusQuery {
        id: "q1".to_owned(),
        text: "test query".to_owned(),
        class: QueryClass::Architecture,
        expected: vec![make_expected("src/adapters/mod.rs", None)],
        rg_substitute: None,
    };
    let results = vec![
        make_hit("src/lib.rs", None, 0.9),          // not a hit
        make_hit("src/query.rs", None, 0.8),        // not a hit
        make_hit("src/adapters/mod.rs", None, 0.7), // hit at rank 3
    ];
    let eval = evaluate_query(&query, &results, 0.5);
    assert!(!eval.top1_hit, "not a top-1 hit");
    assert!(eval.top3_hit, "is a top-3 hit");
    assert!(
        (eval.reciprocal_rank - 1.0 / 3.0).abs() < 1e-9,
        "RR should be 1/3"
    );
}

#[test]
fn evaluate_query_ambiguous_never_counts_as_hit() {
    use aletheia_egregore::semantic_eval::CorpusQuery;
    let query = CorpusQuery {
        id: "ambig".to_owned(),
        text: "ambiguous query".to_owned(),
        class: QueryClass::Ambiguous,
        expected: vec![],
        rg_substitute: None,
    };
    // Even if a result happens to match something, ambiguous queries don't count as hits
    let results = vec![make_hit("src/lib.rs", None, 0.9)];
    // threshold 0.5: score 0.9 exceeds it, so this is a false positive
    let eval = evaluate_query(&query, &results, 0.5);
    assert!(!eval.top1_hit, "ambiguous queries never count as top-1 hit");
    assert!(!eval.top3_hit, "ambiguous queries never count as top-3 hit");
    assert!(
        eval.reciprocal_rank.abs() < f64::EPSILON,
        "RR always 0 for ambiguous"
    );
    assert!(
        eval.is_false_positive,
        "returning results for ambiguous query is a false positive"
    );
}

// ---------------------------------------------------------------------------
// Determinism: five identical metric computations produce identical results
// ---------------------------------------------------------------------------

#[test]
fn determinism_five_metric_computations_produce_identical_results() {
    let results = vec![
        QueryEvalResult {
            query_id: "q1".to_owned(),
            query_text: "first query".to_owned(),
            class: QueryClass::Architecture,
            top_k_results: vec![],
            top1_hit: true,
            top3_hit: true,
            reciprocal_rank: 1.0,
            is_false_positive: false,
        },
        QueryEvalResult {
            query_id: "q2".to_owned(),
            query_text: "second query".to_owned(),
            class: QueryClass::SynonymHeavy,
            top_k_results: vec![],
            top1_hit: false,
            top3_hit: true,
            reciprocal_rank: 0.5,
            is_false_positive: false,
        },
        QueryEvalResult {
            query_id: "q3".to_owned(),
            query_text: "third query".to_owned(),
            class: QueryClass::Ambiguous,
            top_k_results: vec![make_hit("src/lib.rs", None, 0.5)],
            top1_hit: false,
            top3_hit: false,
            reciprocal_rank: 0.0,
            is_false_positive: true,
        },
    ];

    let first = compute_metrics(&results);
    for run in 1..5 {
        let repeat = compute_metrics(&results);
        assert_eq!(
            first, repeat,
            "metrics computation must be deterministic (run {run} differed)"
        );
    }
}

// ---------------------------------------------------------------------------
// build_report: passed flag and diagnostic
// ---------------------------------------------------------------------------

#[test]
fn build_report_passed_when_threshold_met() {
    let results = vec![QueryEvalResult {
        query_id: "q1".to_owned(),
        query_text: "q1".to_owned(),
        class: QueryClass::Architecture,
        top_k_results: vec![],
        top1_hit: true,
        top3_hit: true,
        reciprocal_rank: 1.0,
        is_false_positive: false,
    }];
    let report = build_report(results, 0.8);
    assert!(
        report.passed,
        "report must pass when top3_recall >= threshold"
    );
}

#[test]
fn build_report_fails_when_threshold_missed() {
    let results = vec![
        QueryEvalResult {
            query_id: "q1".to_owned(),
            query_text: "q1".to_owned(),
            class: QueryClass::Architecture,
            top_k_results: vec![],
            top1_hit: false,
            top3_hit: false,
            reciprocal_rank: 0.0,
            is_false_positive: false,
        },
        QueryEvalResult {
            query_id: "q2".to_owned(),
            query_text: "q2".to_owned(),
            class: QueryClass::Architecture,
            top_k_results: vec![],
            top1_hit: false,
            top3_hit: false,
            reciprocal_rank: 0.0,
            is_false_positive: false,
        },
    ];
    let report = build_report(results, 0.8);
    assert!(
        !report.passed,
        "report must fail when top3_recall < threshold"
    );
}

#[test]
fn format_diagnostic_includes_missed_query_ids() {
    let results = vec![
        QueryEvalResult {
            query_id: "missed-q001".to_owned(),
            query_text: "find the write path".to_owned(),
            class: QueryClass::ConceptAbsent,
            top_k_results: vec![make_hit("src/wrong.rs", None, 0.5)],
            top1_hit: false,
            top3_hit: false,
            reciprocal_rank: 0.0,
            is_false_positive: false,
        },
        QueryEvalResult {
            query_id: "hit-q002".to_owned(),
            query_text: "which module is the CLI".to_owned(),
            class: QueryClass::Architecture,
            top_k_results: vec![],
            top1_hit: true,
            top3_hit: true,
            reciprocal_rank: 1.0,
            is_false_positive: false,
        },
    ];
    let report = build_report(results, 0.8);
    let diag = format_diagnostic(&report);
    assert!(
        diag.contains("missed-q001"),
        "diagnostic must list missed query id; got:\n{diag}"
    );
    assert!(
        !diag.contains("hit-q002"),
        "diagnostic must NOT list queries that hit; got:\n{diag}"
    );
    assert!(
        diag.contains("FAIL") || diag.contains("fail") || diag.contains("missed"),
        "diagnostic must indicate failure; got:\n{diag}"
    );
}

// ---------------------------------------------------------------------------
// Documentation file exists
// ---------------------------------------------------------------------------

#[test]
fn semantic_search_guidance_documentation_exists() {
    let doc_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/cli/semantic-search-guidance.md");
    assert!(
        doc_path.exists(),
        "docs/cli/semantic-search-guidance.md must exist"
    );
}

#[test]
fn semantic_search_guidance_addresses_when_to_use_semantic_vs_rg() {
    let doc_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/cli/semantic-search-guidance.md");
    let text = std::fs::read_to_string(&doc_path).expect("read docs file");
    // Must mention when semantic is appropriate
    assert!(
        text.contains("semantic") || text.contains("Semantic"),
        "guidance doc must discuss semantic search"
    );
    // Must mention rg or ripgrep or git grep as the boring substitute
    let mentions_rg = text.contains("rg ") || text.contains("ripgrep") || text.contains("git grep");
    assert!(
        mentions_rg,
        "guidance doc must mention rg/ripgrep or git grep as the boring substitute"
    );
    // Must state that semantic similarity is not verification evidence
    let mentions_not_verification = text.contains("not")
        && (text.contains("verification") || text.contains("proof") || text.contains("evidence"));
    assert!(
        mentions_not_verification,
        "guidance doc must state that semantic similarity is not verification evidence"
    );
}
