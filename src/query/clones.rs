//! Exact-duplicate symbol-body clone classes (issue #216).
//!
//! Groups live Rust `Symbol` records whose normalized bodies are byte-identical
//! into citable clone classes, so a fix or extraction lands on every copy of
//! duplicated logic instead of the one the agent happened to open. This is the
//! deterministic counterpart to the fuzzy embedding-similarity lane (#154):
//! no threshold, no guess is ever presented as a clone.
//!
//! The grouping key is the same `normalize_code` + BLAKE3 framing the scan
//! pipeline uses for symbol summaries (`content_signature`, issue #206), so a
//! class hash is directly comparable to the hash the extractor computes.
//! Symbols from other languages are excluded in this slice (Rust only, per the
//! issue's extraction scope).

use std::collections::BTreeMap;

use super::RepositoryIndex;
use super::liveness::Liveness;
use crate::ir::{GraphRecord, NodeKind, SourceSpan};
use crate::languages::rust::normalize_code;

/// Default `--limit` for `eg query clones`.
pub const CLONES_DEFAULT_LIMIT: usize = 50;

/// Maximum accepted `--limit` for `eg query clones`.
pub const CLONES_MAX_LIMIT: usize = 500;

/// Default `--min-size` for `eg query clones`: a class needs two members.
pub const CLONES_DEFAULT_MIN_SIZE: usize = 2;

/// Content-hash framing for clone classes: `blake3:` followed by the hex
/// digest of the normalized symbol body (same framing as the extractor's
/// `content_signature`, issue #206).
pub const BLAKE3_HASH_PREFIX: &str = "blake3:";

/// Separator between the extractor's summary header and the normalized body in
/// a symbol summary (`"{Lang} {kind} {name}\nSource:\n{normalized}"`).
const SYMBOL_BODY_MARKER: &str = "\nSource:\n";

/// Machine-readable marker for the well-formed empty answer (issue #196).
pub const CLONES_EMPTY_REASON: &str = "no_clone_classes";

/// One citable member of a clone class: the existing citable handle
/// (record ID, repo-relative path, span, qualified name). No raw source body
/// is emitted.
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct CloneMember {
    /// Stable record ID of the symbol node.
    pub record_id: String,
    /// Qualified symbol name.
    pub qualified_name: String,
    /// Repo-relative file handle.
    pub repo_relative_path: String,
    /// Source span handle.
    pub span: SourceSpan,
}

/// One clone class: the set of symbol records sharing one normalized-body
/// content hash.
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct CloneClass {
    /// `blake3:` content hash of the shared normalized body — the class identity.
    pub content_hash: String,
    /// Class size; always `members.len()` and `>= min_size`.
    pub size: usize,
    /// Members sorted by `record_id` ascending.
    pub members: Vec<CloneMember>,
}

/// Deterministic clone-class report for `eg query clones`.
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct CloneClassReport {
    /// Minimum class size applied.
    pub min_size: usize,
    /// Class-list limit applied.
    pub limit: usize,
    /// Clone classes before truncation.
    pub total_class_count: usize,
    /// Clone classes returned after truncation.
    pub returned_class_count: usize,
    /// Completeness signal: whether `limit` cut the class list. Never silent.
    pub truncated: bool,
    /// Present as `"no_clone_classes"` when the scope holds no clone class: a
    /// well-formed empty answer (issue #196), never an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empty_reason: Option<String>,
    /// Classes ordered by size descending, then `content_hash` ascending.
    pub classes: Vec<CloneClass>,
}

/// Extracts the normalized body from an extractor-produced symbol summary and
/// hashes it with the scan pipeline's `normalize_code` + BLAKE3 framing.
///
/// The body is everything after the first `"\nSource:\n"` marker; the
/// re-normalization is idempotent over the extractor's already-normalized
/// body, so this agrees with the `content_signature` framing (issue #206).
///
/// Returns `None` when the summary carries no normalized body (no marker) or
/// the normalized body is empty — such records cannot join a clone class.
#[must_use]
pub fn symbol_body_hash(summary: &str) -> Option<String> {
    let body_start = summary.find(SYMBOL_BODY_MARKER)? + SYMBOL_BODY_MARKER.len();
    let normalized = normalize_code(&summary[body_start..]);
    if normalized.is_empty() {
        return None;
    }
    Some(format!(
        "{BLAKE3_HASH_PREFIX}{}",
        blake3::hash(normalized.as_bytes())
    ))
}

/// Groups live Rust symbol records with byte-identical normalized bodies into
/// deterministic clone classes.
///
/// * Only `Symbol` nodes with `language == "rust"` and a recoverable
///   normalized body participate; everything else is skipped, never guessed.
/// * Tombstoned records are excluded via latest-write-wins liveness, and a
///   record ID re-scanned with a new body contributes only its newest write —
///   matching the embedded current-state read on both transports.
/// * Classes with fewer than `min_size` members are dropped (singletons never
///   surface). Classes order by size descending, then `content_hash`
///   ascending; members order by `record_id` ascending. `limit` truncates the
///   class list after ordering and the report says so explicitly.
/// * An empty class list is a well-formed report with
///   `empty_reason == "no_clone_classes"`, not an error.
#[must_use]
pub fn clone_classes(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    min_size: usize,
    limit: usize,
) -> CloneClassReport {
    let liveness = Liveness::new(records);
    let repo_index = RepositoryIndex::build(records);

    // Group by exact normalized-body hash. BTreeMap keeps the base iteration
    // in hash order; the final ordering pass applies size-desc, hash-asc.
    let mut by_hash: BTreeMap<String, Vec<CloneMember>> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        let GraphRecord::Node {
            id,
            kind,
            repo_relative_path,
            span,
            name,
            language,
            summary,
            content_signature,
            ..
        } = record
        else {
            continue;
        };
        if *kind != NodeKind::Symbol {
            continue;
        }
        // Latest-write-wins liveness: a stale earlier version of a re-scanned
        // symbol never shadows its newest write, and tombstoned symbols leave
        // their class — matching the embedded current-state read.
        if !liveness.is_latest_node_version(id, index) || liveness.deleted(id) {
            continue;
        }
        if language.as_deref() != Some("rust") {
            continue;
        }
        if let Some(repo) = repo_id {
            if repo_index.owner_of(id) != Some(repo) {
                continue;
            }
        }
        // Prefer the extractor-stamped content signature when it carries the
        // blake3: framing; otherwise recover the body from the summary.
        let hash = match content_signature.as_deref() {
            Some(signature) if signature.starts_with(BLAKE3_HASH_PREFIX) => signature.to_owned(),
            _ => match symbol_body_hash(summary) {
                Some(hash) => hash,
                None => continue,
            },
        };
        // Citable handles only: skip records lacking a qualified name,
        // repo-relative path, or span rather than fabricating handles.
        let (Some(qualified_name), Some(repo_relative_path), Some(span)) =
            (name.clone(), repo_relative_path.clone(), *span)
        else {
            continue;
        };
        by_hash.entry(hash).or_default().push(CloneMember {
            record_id: id.clone(),
            qualified_name,
            repo_relative_path,
            span,
        });
    }

    // Drop classes below the minimum size, then order deterministically.
    let mut classes: Vec<CloneClass> = by_hash
        .into_iter()
        .filter_map(|(content_hash, mut members)| {
            if members.len() < min_size {
                return None;
            }
            members.sort_by(|a, b| a.record_id.cmp(&b.record_id));
            Some(CloneClass {
                content_hash,
                size: members.len(),
                members,
            })
        })
        .collect();
    classes.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then_with(|| a.content_hash.cmp(&b.content_hash))
    });

    let total_class_count = classes.len();
    let truncated = classes.len() > limit;
    classes.truncate(limit);
    let returned_class_count = classes.len();

    CloneClassReport {
        min_size,
        limit,
        total_class_count,
        returned_class_count,
        truncated,
        empty_reason: if total_class_count == 0 {
            Some(CLONES_EMPTY_REASON.to_owned())
        } else {
            None
        },
        classes,
    }
}

#[cfg(test)]
mod tests {
    //! RED suite for issue #216: written before the implementation.

    use super::*;
    use crate::fs::SourceFile;
    use crate::ir::{Graph, SCHEMA_VERSION};

    /// Extracts `source` through the REAL Rust extractor so summaries carry
    /// genuine normalized bodies (no hand-built summaries).
    fn extract(path: &str, source: &str) -> Vec<GraphRecord> {
        let file = SourceFile {
            path: std::path::PathBuf::from(path),
            repo_relative_path: path.to_owned(),
        };
        let mut graph = Graph::default();
        crate::languages::rust::extract_file_source(
            &file,
            source,
            &format!("file:{path}"),
            "repo:test",
            &mut graph,
        )
        .expect("fixture source should parse");
        graph.records().to_vec()
    }

    fn tombstone(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v6:tomb_{deleted_id}"),
            schema_version: SCHEMA_VERSION,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    const BODY_A: &str = "pub fn compute(x: u32) -> u32 {\n    x * 2 + 1\n}\n";
    const BODY_B: &str = "pub fn other() -> &'static str {\n    \"hello\"\n}\n";

    /// K exact-duplicate bodies across files group into one class of size K.
    #[test]
    fn groups_exact_duplicate_bodies_across_files() {
        let mut records = extract("src/a.rs", BODY_A);
        records.extend(extract("src/b.rs", BODY_A));
        records.extend(extract("src/c.rs", BODY_B));

        let report = clone_classes(&records, None, 2, 50);

        assert_eq!(report.total_class_count, 1);
        assert_eq!(report.returned_class_count, 1);
        assert!(!report.truncated);
        assert!(report.empty_reason.is_none());
        let class = &report.classes[0];
        assert_eq!(class.size, 2);
        assert_eq!(class.members.len(), 2);
        assert!(class.content_hash.starts_with("blake3:"));
        let names: Vec<&str> = class
            .members
            .iter()
            .map(|m| m.qualified_name.as_str())
            .collect();
        assert!(names.iter().all(|n| n.contains("compute")));
        // Citable handles only: record id, path, span, qualified name.
        for member in &class.members {
            assert!(member.record_id.starts_with("codegraph:"));
            assert!(!member.repo_relative_path.is_empty());
            assert!(!member.qualified_name.is_empty());
        }
        // Members sorted by record_id.
        let ids: Vec<&str> = class.members.iter().map(|m| m.record_id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
    }

    /// The class hash equals the extractor's normalize + BLAKE3 framing.
    #[test]
    fn class_hash_matches_normalize_plus_blake3_framing() {
        let records = extract("src/a.rs", BODY_A);
        let report = clone_classes(&records, None, 1, 50);
        assert_eq!(report.total_class_count, 1);
        let expected = format!(
            "blake3:{}",
            blake3::hash(normalize_code(BODY_A).as_bytes()).to_hex()
        );
        assert_eq!(report.classes[0].content_hash, expected);
    }

    /// Comment/whitespace-only differences still clone (normalization).
    #[test]
    fn whitespace_and_comment_differences_still_clone() {
        let variant = "// a comment\npub fn compute( x: u32 ) -> u32 { x*2+1 }\n";
        let mut records = extract("src/a.rs", BODY_A);
        records.extend(extract("src/b.rs", variant));

        let report = clone_classes(&records, None, 2, 50);
        assert_eq!(
            report.total_class_count, 1,
            "comment/whitespace variants must share one class"
        );
        assert_eq!(report.classes[0].size, 2);
    }

    /// Genuinely distinct bodies produce zero false clone classes.
    #[test]
    fn distinct_bodies_produce_no_classes() {
        let mut records = extract("src/a.rs", BODY_A);
        records.extend(extract("src/b.rs", BODY_B));

        let report = clone_classes(&records, None, 2, 50);
        assert_eq!(report.total_class_count, 0);
        assert!(report.classes.is_empty());
        assert_eq!(report.empty_reason.as_deref(), Some(CLONES_EMPTY_REASON));
    }

    /// Singletons are excluded; the empty answer is well-formed, not an error.
    #[test]
    fn singleton_bodies_are_excluded_with_well_formed_empty() {
        let records = extract("src/a.rs", BODY_A);
        let report = clone_classes(&records, None, 2, 50);
        assert!(report.classes.is_empty());
        assert_eq!(report.total_class_count, 0);
        assert_eq!(report.returned_class_count, 0);
        assert!(!report.truncated);
        assert_eq!(report.empty_reason.as_deref(), Some("no_clone_classes"));
        // The report serializes to a well-formed envelope payload.
        let json = serde_json::to_value(&report).expect("report serializes");
        assert_eq!(json["empty_reason"], "no_clone_classes");
        assert_eq!(json["classes"].as_array().unwrap().len(), 0);
    }

    /// Ordering: size desc, then content_hash asc; byte-stable across runs.
    #[test]
    fn classes_order_by_size_desc_then_hash() {
        let mut records = extract("src/a1.rs", BODY_A);
        records.extend(extract("src/a2.rs", BODY_A));
        records.extend(extract("src/a3.rs", BODY_A));
        records.extend(extract("src/b1.rs", BODY_B));
        records.extend(extract("src/b2.rs", BODY_B));

        let first = clone_classes(&records, None, 2, 50);
        let second = clone_classes(&records, None, 2, 50);
        assert_eq!(first.total_class_count, 2);
        assert_eq!(first.classes[0].size, 3);
        assert_eq!(first.classes[1].size, 2);
        // Byte-for-byte stability across repeated runs.
        assert_eq!(
            serde_json::to_string(&first).expect("serializes"),
            serde_json::to_string(&second).expect("serializes")
        );
    }

    /// `--min-size` filters classes below the threshold.
    #[test]
    fn min_size_filters_small_classes() {
        let mut records = extract("src/a1.rs", BODY_A);
        records.extend(extract("src/a2.rs", BODY_A));

        let report = clone_classes(&records, None, 3, 50);
        assert!(report.classes.is_empty());
        assert_eq!(report.min_size, 3);
        assert_eq!(report.empty_reason.as_deref(), Some("no_clone_classes"));
    }

    /// `--limit` truncates after ordering and says so (issue #121).
    #[test]
    fn limit_truncation_is_signaled_not_silent() {
        let mut records = extract("src/a1.rs", BODY_A);
        records.extend(extract("src/a2.rs", BODY_A));
        records.extend(extract("src/b1.rs", BODY_B));
        records.extend(extract("src/b2.rs", BODY_B));

        let report = clone_classes(&records, None, 2, 1);
        assert_eq!(report.total_class_count, 2);
        assert_eq!(report.returned_class_count, 1);
        assert!(report.truncated);
        // The surviving class is the deterministic first one.
        let full = clone_classes(&records, None, 2, 50);
        assert_eq!(report.classes[0], full.classes[0]);
    }

    /// Non-Rust symbols never join a class, even with an identical body.
    #[test]
    fn non_rust_symbols_are_excluded() {
        let mut records = extract("src/a.rs", BODY_A);
        // Forge a Go symbol whose summary carries the same normalized body.
        let go_summary = format!("Go function compute\nSource:\n{}", normalize_code(BODY_A));
        records.push(GraphRecord::node(
            "codegraph:v6:go_compute".to_owned(),
            NodeKind::Symbol,
            Some("src/g.go".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
                start_column: None,
                end_column: None,
            }),
            Some("compute".to_owned()),
            go_summary,
        ));
        // Tag the forged record as Go: node() leaves language None, so set it
        // via a rebuilt record. (node() has no language param; emulate with a
        // JSON round-trip.)
        let mut value = serde_json::to_value(records.pop().unwrap()).unwrap();
        value["language"] = serde_json::json!("go");
        records.push(serde_json::from_value(value).unwrap());
        records.extend(extract("src/b.rs", BODY_A));

        let report = clone_classes(&records, None, 2, 50);
        // The Go impostor must not merge with the two Rust copies: no class
        // of size 3, and the Rust class has exactly the two Rust members.
        assert_eq!(report.total_class_count, 1);
        assert_eq!(report.classes[0].size, 2);
        assert!(
            report.classes[0]
                .members
                .iter()
                .all(|m| m.record_id != "codegraph:v6:go_compute")
        );
    }

    /// Symbols without a recoverable normalized body are skipped, not guessed.
    #[test]
    fn symbols_without_body_marker_are_skipped() {
        let mut records = extract("src/a.rs", BODY_A);
        records.extend(extract("src/b.rs", BODY_A));
        records.push(GraphRecord::node(
            "codegraph:v6:markerless".to_owned(),
            NodeKind::Symbol,
            Some("src/c.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
                start_column: None,
                end_column: None,
            }),
            Some("compute".to_owned()),
            "a summary with no body marker".to_owned(),
        ));

        let report = clone_classes(&records, None, 2, 50);
        assert_eq!(report.total_class_count, 1);
        assert_eq!(report.classes[0].size, 2);
        assert!(
            report.classes[0]
                .members
                .iter()
                .all(|m| m.record_id != "codegraph:v6:markerless")
        );
    }

    /// A tombstoned symbol leaves its class (latest-write-wins liveness).
    #[test]
    fn tombstoned_symbol_leaves_its_class() {
        let mut records = extract("src/a.rs", BODY_A);
        records.extend(extract("src/b.rs", BODY_A));
        let victim = records
            .iter()
            .find(|r| {
                matches!(
                    r,
                    GraphRecord::Node {
                        kind: NodeKind::Symbol,
                        repo_relative_path: Some(path),
                        ..
                    } if path == "src/a.rs"
                )
            })
            .map(|r| r.id().to_owned())
            .expect("a.rs symbol present");
        records.push(tombstone(&victim));

        let report = clone_classes(&records, None, 2, 50);
        assert!(report.classes.is_empty());
        assert_eq!(report.empty_reason.as_deref(), Some("no_clone_classes"));
    }

    /// Re-scanning a symbol with an edited body moves it out of the class:
    /// only the newest write counts.
    #[test]
    fn rescanned_symbol_contributes_only_its_newest_body() {
        // BODY_A_EDITED still defines `compute` (same symbol ID) but with a
        // different body, simulating an edit + re-scan of src/a.rs.
        const BODY_A_EDITED: &str = "pub fn compute(x: u32) -> u32 {\n    x * 2 + 2\n}\n";
        let first = extract("src/a.rs", BODY_A);
        let edited = extract("src/a.rs", BODY_A_EDITED);
        let mut records = first;
        records.extend(extract("src/b.rs", BODY_A));
        // The a.rs `compute` record ID is stable across re-scans; the later
        // BODY_A_EDITED write wins, so no BODY_A pair remains.
        records.extend(edited);

        let report = clone_classes(&records, None, 2, 50);
        assert!(
            report.classes.is_empty(),
            "a.rs now holds the edited body, so no BODY_A pair remains"
        );
    }

    /// `symbol_body_hash` returns None without the marker.
    #[test]
    fn body_hash_none_without_marker() {
        assert!(symbol_body_hash("no marker here").is_none());
    }

    /// Report limits echo the request.
    #[test]
    fn report_echoes_min_size_and_limit() {
        let report = clone_classes(&[], None, 2, 25);
        assert_eq!(report.min_size, 2);
        assert_eq!(report.limit, 25);
    }
}
