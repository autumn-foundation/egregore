//! Find symbols similar to a given symbol (issue #154).
//!
//! `eg query similar <handle>` ranks the symbol/file nodes most similar to an
//! existing symbol by cosine similarity of their stored embeddings. The
//! anchor's already-stored embedding is the query vector: this lane never
//! loads an embedding model, never touches the network, and never re-scans
//! the corpus.

#![cfg(feature = "embeddings")]

use super::*;

use crate::embeddings::VectorIndexState;

/// Cosine-similarity floor: a candidate must score strictly above this to
/// count as "similar". Zero or negative cosine means orthogonal or opposed —
/// not reusable.
const SIMILAR_SCORE_FLOOR: f32 = 0.0;

/// Stable diagnostic codes for the `similar` lane.
pub(crate) const SIMILAR_MALFORMED_HANDLE_CODE: &str = "malformed_handle";
pub(crate) const SIMILAR_MALFORMED_LIMIT_CODE: &str = "malformed_limit";
pub(crate) const SIMILAR_UNKNOWN_HANDLE_CODE: &str = "unknown_symbol_handle";
pub(crate) const SIMILAR_AMBIGUOUS_HANDLE_CODE: &str = "ambiguous_symbol_handle";
pub(crate) const SIMILAR_NOT_SYMBOL_CODE: &str = "handle_not_symbol";
pub(crate) const SIMILAR_ANCHOR_NOT_EMBEDDED_CODE: &str = "anchor_not_embedded";
pub(crate) const SIMILAR_INDEX_UNREADABLE_CODE: &str = "semantic_index_unreadable";
pub(crate) const NO_SIMILAR_MATCHES_CODE: &str = "no_similar_matches";

/// Anchor-resolution failure for `eg query similar`.
#[derive(Debug)]
enum SimilarAnchorError {
    /// Blank handle.
    MalformedHandle,
    /// No live node matches the handle.
    UnknownHandle,
    /// Several live symbols share the name; disambiguation is the
    /// caller's job, never a guess.
    AmbiguousHandle { candidates: Vec<String> },
    /// A `record_id` handle resolving to a record that is not a symbol.
    /// The anchor is always a symbol; files may appear as results but never
    /// as the anchor (issue #154).
    NotSymbol { kind: String },
}

impl SimilarAnchorError {
    const fn code(&self) -> &'static str {
        match self {
            Self::MalformedHandle => SIMILAR_MALFORMED_HANDLE_CODE,
            Self::UnknownHandle => SIMILAR_UNKNOWN_HANDLE_CODE,
            Self::AmbiguousHandle { .. } => SIMILAR_AMBIGUOUS_HANDLE_CODE,
            Self::NotSymbol { .. } => SIMILAR_NOT_SYMBOL_CODE,
        }
    }

    fn message(&self, handle: &str) -> String {
        match self {
            Self::MalformedHandle => "handle must not be blank".to_owned(),
            Self::UnknownHandle => {
                format!("no live symbol node matches handle '{handle}'")
            }
            Self::AmbiguousHandle { candidates } => format!(
                "handle '{handle}' matches {} symbols; pass a record_id to disambiguate",
                candidates.len(),
            ),
            Self::NotSymbol { kind } => format!(
                "handle '{handle}' resolves to a {kind} node; similarity search needs a symbol anchor",
            ),
        }
    }
}

/// Prints the stable `{"ok":false,"error":{...}}` envelope to stdout plus a
/// one-line stderr summary, and returns the error that makes `main` exit 1.
fn report_similar_error(
    code: &str,
    handle: &str,
    message: &str,
    extra: Option<(&str, serde_json::Value)>,
) -> anyhow::Error {
    let mut error = serde_json::Map::new();
    error.insert(
        "code".to_owned(),
        serde_json::Value::String(code.to_owned()),
    );
    error.insert(
        "handle".to_owned(),
        serde_json::Value::String(handle.to_owned()),
    );
    error.insert(
        "message".to_owned(),
        serde_json::Value::String(message.to_owned()),
    );
    if let Some((key, value)) = extra {
        error.insert(key.to_owned(), value);
    }
    let envelope = serde_json::json!({ "ok": false, "error": error });
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("similar error envelope should serialize")
    );
    eprintln!("similar query failed: {message}");
    anyhow::anyhow!("{code}")
}

/// Resolves a `similar` handle to its anchor symbol: an exact `record_id`
/// first, then an exact stored symbol name (qualified names included).
///
/// Name matches are exact and case-sensitive; an ambiguous name is an error,
/// never a guess. The anchor is always a symbol — files may rank as results
/// but never anchor the search (issue #154).
fn resolve_similar_anchor<'r>(
    records: &'r [GraphRecord],
    handle: &str,
) -> Result<&'r GraphRecord, SimilarAnchorError> {
    let trimmed = handle.trim();
    if trimmed.is_empty() {
        return Err(SimilarAnchorError::MalformedHandle);
    }
    if let Some(record) = records.iter().find(|record| record.id() == trimmed) {
        return match record {
            GraphRecord::Node {
                kind: NodeKind::Symbol,
                ..
            } => Ok(record),
            GraphRecord::Node { kind, .. } => Err(SimilarAnchorError::NotSymbol {
                kind: format!("{kind:?}"),
            }),
            _ => Err(SimilarAnchorError::NotSymbol {
                kind: "non-node record".to_owned(),
            }),
        };
    }
    let mut candidates: Vec<&'r GraphRecord> = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name: Some(name),
                    ..
                } if name.as_str() == trimmed
            )
        })
        .collect();
    match candidates.len() {
        0 => Err(SimilarAnchorError::UnknownHandle),
        1 => Ok(candidates.pop().expect("one candidate")),
        _ => {
            candidates.sort_by(|a, b| a.id().cmp(b.id()));
            Err(SimilarAnchorError::AmbiguousHandle {
                candidates: candidates
                    .iter()
                    .map(|record| record.id().to_owned())
                    .collect(),
            })
        }
    }
}

/// Filters a raw similarity ranking down to the `similar` answer: drops the
/// anchor itself, drops non-symbol/file nodes, drops everything at or below
/// the similarity floor, then applies the canonical semantic total order
/// (score descending, `record_id` ascending) before the limit truncation, so
/// both the row sequence and the truncation boundary are stable across runs
/// against an unchanged store (issue #199).
fn rank_similar_matches(matches: &mut Vec<SemanticMatch>, anchor_id: &str, limit: usize) {
    matches.retain(|m| {
        m.record_id != anchor_id
            // Code search must never blend agent-authored memory hits into
            // deterministic code results (issue #91): the shared vector index
            // also embeds observation-class memory nodes.
            && m.kind.as_deref().is_some_and(|k| k == "File" || k == "Symbol")
            && m.score > SIMILAR_SCORE_FLOOR
    });
    matches.sort_by(crate::adapters::compare_semantic_matches);
    matches.truncate(limit);
}

/// Refuses a `similar` query against a store whose vector index is absent or
/// unreadable. Unlike `eg query semantic`, there is no model-identity gate:
/// the anchor's stored vector is the query, so no query-side model exists to
/// compare against.
fn check_similar_index_ready(sink: &EmbeddedAletheiaSink, data_dir: &Path) -> Result<()> {
    match sink.embedding_index_state() {
        VectorIndexState::Loaded { .. } => Ok(()),
        VectorIndexState::Absent => Err(report_similar_error(
            semantic::SEMANTIC_INDEX_ABSENT_CODE,
            &data_dir.display().to_string(),
            "the store was ingested without --embed; re-ingest with --embed to enable similarity search",
            None,
        )),
        VectorIndexState::Unreadable { artifacts } => Err(report_similar_error(
            SIMILAR_INDEX_UNREADABLE_CODE,
            &data_dir.display().to_string(),
            &format!(
                "the vector index exists on disk but was skipped at load as corrupted or unreadable (artifacts: {})",
                artifacts.join(", "),
            ),
            Some(("index_artifacts", serde_json::json!(artifacts))),
        )),
    }
}

/// Entry point for `eg query similar`.
pub(crate) fn query_similar_cmd(
    handle: &str,
    data_dir: &Path,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    if limit == 0 {
        return Err(report_similar_error(
            SIMILAR_MALFORMED_LIMIT_CODE,
            handle.trim(),
            "--limit must be at least 1",
            None,
        ));
    }
    // Read-only: matches the other query lanes' lease-free store open.
    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)?;
    check_similar_index_ready(&sink, data_dir)?;
    let records = sink.read_all_records()?;
    let anchor = resolve_similar_anchor(&records, handle).map_err(|error| {
        let extra = match &error {
            SimilarAnchorError::AmbiguousHandle { candidates } => {
                Some(("candidate_record_ids", serde_json::json!(candidates)))
            }
            _ => None,
        };
        report_similar_error(
            error.code(),
            handle.trim(),
            &error.message(handle.trim()),
            extra,
        )
    })?;
    let anchor_id = anchor.id().to_owned();
    let Some(anchor_vector) = sink.stored_embedding_vector(anchor) else {
        return Err(report_similar_error(
            SIMILAR_ANCHOR_NOT_EMBEDDED_CODE,
            handle.trim(),
            &format!(
                "anchor '{}' has no stored embedding; the store may predate its ingestion or the symbol was never embedded",
                handle.trim(),
            ),
            None,
        ));
    };
    // The anchor's stored vector is the query: no model load, no re-scan.
    let mut matches = sink.semantic_search(&anchor_vector, records.len().max(1))?;
    rank_similar_matches(&mut matches, &anchor_id, limit);
    if matches.is_empty() {
        eprintln!(
            "{NO_SIMILAR_MATCHES_CODE}: no nodes similar to '{}' scored above {SIMILAR_SCORE_FLOOR}",
            handle.trim(),
        );
        std::process::exit(2);
    }
    let index = query::RepositoryIndex::build(&records);
    for m in &matches {
        print_result(&SemanticResult::from_match(m, &index), format)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_span() -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line: 1,
            end_line: 1,
            start_column: None,
            end_column: None,
        }
    }

    fn test_symbol(id: &str, name: &str) -> GraphRecord {
        GraphRecord::symbol(
            id.to_owned(),
            "function",
            "src/lib.rs".to_owned(),
            test_span(),
            name.to_owned(),
            format!("symbol {name}"),
        )
    }

    fn test_match(id: &str, name: &str, score: f32) -> SemanticMatch {
        SemanticMatch {
            record_id: id.to_owned(),
            kind: Some("Symbol".to_owned()),
            name: Some(name.to_owned()),
            repo_relative_path: Some("src/lib.rs".to_owned()),
            span: Some(test_span()),
            score,
        }
    }

    #[test]
    fn resolve_anchor_prefers_exact_record_id_over_name() {
        let first = test_symbol("id-first", "alpha");
        let second = test_symbol("id-second", "alpha");
        let records = vec![first, second];

        let anchor = resolve_similar_anchor(&records, "id-second").expect("id should resolve");
        assert_eq!(anchor.id(), "id-second");
    }

    #[test]
    fn resolve_anchor_matches_qualified_symbol_name() {
        let records = vec![test_symbol("id-1", "nested::Widget::new")];

        let anchor =
            resolve_similar_anchor(&records, "nested::Widget::new").expect("name should resolve");
        assert_eq!(anchor.id(), "id-1");
    }

    #[test]
    fn resolve_anchor_rejects_blank_handle() {
        let records = vec![test_symbol("id-1", "alpha")];

        let error = resolve_similar_anchor(&records, "   ").expect_err("blank should fail");
        assert!(matches!(error, SimilarAnchorError::MalformedHandle));
        assert_eq!(error.code(), "malformed_handle");
    }

    #[test]
    fn resolve_anchor_unknown_handle() {
        let records = vec![test_symbol("id-1", "alpha")];

        let error = resolve_similar_anchor(&records, "nope").expect_err("unknown should fail");
        assert!(matches!(error, SimilarAnchorError::UnknownHandle));
        assert_eq!(error.code(), "unknown_symbol_handle");
    }

    #[test]
    fn resolve_anchor_ambiguous_name_sorts_candidate_ids() {
        let records = vec![test_symbol("id-b", "dup"), test_symbol("id-a", "dup")];

        let error = resolve_similar_anchor(&records, "dup").expect_err("ambiguous should fail");
        match &error {
            SimilarAnchorError::AmbiguousHandle { candidates } => {
                assert_eq!(*candidates, vec!["id-a".to_owned(), "id-b".to_owned()]);
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
        assert_eq!(error.code(), "ambiguous_symbol_handle");
    }

    #[test]
    fn resolve_anchor_rejects_non_symbol_record_id() {
        let repo = GraphRecord::node(
            "id-repo".to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some("repo".to_owned()),
            "repository".to_owned(),
        );
        let file = GraphRecord::node(
            "id-file".to_owned(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("repo".to_owned()),
            "file src/lib.rs".to_owned(),
        );
        let records = vec![repo, file];

        // A repository record_id is not a symbol anchor.
        let error =
            resolve_similar_anchor(&records, "id-repo").expect_err("repository id should fail");
        assert!(matches!(error, SimilarAnchorError::NotSymbol { .. }));
        assert_eq!(error.code(), "handle_not_symbol");

        // A file record_id is not a symbol anchor either: files rank as
        // results but never anchor the search (issue #154).
        let error = resolve_similar_anchor(&records, "id-file").expect_err("file id should fail");
        assert!(matches!(error, SimilarAnchorError::NotSymbol { .. }));
        assert_eq!(error.code(), "handle_not_symbol");
    }

    #[test]
    fn rank_filters_anchor_floor_and_other_kinds_then_orders() {
        let mut matches = vec![
            test_match("id-anchor", "anchor", 0.99),
            test_match("id-low", "low", 0.0),
            test_match("id-neg", "neg", -0.5),
            test_match("id-b", "b", 0.8),
            test_match("id-a", "a", 0.8),
            test_match("id-top", "top", 0.9),
        ];
        // A repository-typed candidate must never surface.
        matches.push(SemanticMatch {
            record_id: "id-repo".to_owned(),
            kind: Some("Repository".to_owned()),
            name: Some("repo".to_owned()),
            repo_relative_path: None,
            span: Some(test_span()),
            score: 0.95,
        });

        rank_similar_matches(&mut matches, "id-anchor", 10);

        let ids: Vec<&str> = matches.iter().map(|m| m.record_id.as_str()).collect();
        assert_eq!(ids, vec!["id-top", "id-a", "id-b"]);
    }

    #[test]
    fn rank_truncates_after_ordering() {
        let mut matches = vec![
            test_match("id-c", "c", 0.7),
            test_match("id-a", "a", 0.9),
            test_match("id-b", "b", 0.8),
        ];

        rank_similar_matches(&mut matches, "id-anchor", 2);

        let ids: Vec<&str> = matches.iter().map(|m| m.record_id.as_str()).collect();
        assert_eq!(ids, vec!["id-a", "id-b"]);
    }
}
