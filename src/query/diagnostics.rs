//! Extractor-coverage-gap diagnostics inventory (issue #246).
//!
//! Pure helpers behind `eg query diagnostics`: the lane lists persisted
//! `NodeKind::Diagnostic` graph nodes — the extractor's self-declared blind
//! spots — with no scan or recompute.

use crate::ir::{GraphRecord, NodeKind};

/// Returns every persisted extractor-coverage-gap `Diagnostic` node in
/// `records`, sorted for deterministic, byte-stable output.
///
/// The lane's closed set is `NodeKind::Diagnostic` nodes carrying BOTH a
/// `repo_relative_path` AND a `span`: the unsupported-macro-invocation,
/// unresolved-call, and unresolved-dispatch gap markers the extractor mints
/// when it cannot parse or resolve a construct. Every returned node is
/// resolvable to a file + span that lands on the flagged source region.
///
/// Excluded by construction:
///
/// - `Diagnostic` nodes WITHOUT a span (skipped-manifest dependency-coverage
///   holes), which belong to the manifest-deps lane's `skipped_manifest`
///   surface;
/// - `Diagnostic` nodes WITHOUT a repo-relative path (GitHub-import review
///   diagnostics), which are importer artifacts, not extractor gaps;
/// - source-authored markers (TODO/FIXME `DebtMarker`s, `.unwrap()`/`.expect()`
///   `PanicRiskSite`s): different node kinds, so they can never match — the
///   lane does not reclassify them.
///
/// Sort key: `(repo_relative_path, span.start_byte, git_commit, record_id)`.
/// Every component is deterministic, so repeated runs over an unchanged store
/// are byte-identical. The `git_commit` tiebreak keeps `--all-history` rows
/// (same stable id at several commits) in a canonical order.
///
/// Tombstone liveness and corpus narrowing are the CLI dispatch's job
/// (mirroring the sibling lanes); this fn is the pure inventory.
#[must_use]
pub fn diagnostic_gap_records(records: &[GraphRecord]) -> Vec<&GraphRecord> {
    let mut gaps: Vec<(&GraphRecord, &str, usize, Option<&str>, &str)> = records
        .iter()
        .filter_map(|record| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Diagnostic,
                repo_relative_path: Some(path),
                span: Some(span),
                temporal,
                ..
            } = record
            else {
                return None;
            };
            Some((
                record,
                path.as_str(),
                span.start_byte,
                temporal.as_ref().map(|t| t.git_commit.as_str()),
                id.as_str(),
            ))
        })
        .collect();
    gaps.sort_by_key(|gap| (gap.1, gap.2, gap.3, gap.4));
    gaps.into_iter()
        .map(|(record, _, _, _, _)| record)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::SourceSpan;

    fn span(start_byte: usize, end_byte: usize, start_line: usize) -> SourceSpan {
        SourceSpan {
            start_byte,
            end_byte,
            start_line,
            end_line: start_line,
            start_column: None,
            end_column: None,
        }
    }

    fn gap(id: &str, path: &str, name: &str, summary: &str, span: SourceSpan) -> GraphRecord {
        GraphRecord::syntax_node(
            id.to_owned(),
            NodeKind::Diagnostic,
            path.to_owned(),
            span,
            name.to_owned(),
            "rust",
            summary.to_owned(),
        )
    }

    fn spanless_diagnostic(id: &str, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Diagnostic,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            "Unparseable Cargo manifest src/manifest: dependency declarations skipped".to_owned(),
        )
    }

    fn pathless_diagnostic(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Diagnostic,
            None,
            None,
            None,
            "[github_review_unanchored] review without commit".to_owned(),
        )
    }

    fn debt_marker(id: &str, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::DebtMarker,
            Some(path.to_owned()),
            Some(span(0, 10, 1)),
            Some("todo".to_owned()),
            "TODO: something".to_owned(),
        )
    }

    fn panic_risk(id: &str, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::PanicRiskSite,
            Some(path.to_owned()),
            Some(span(0, 10, 1)),
            Some("unwrap".to_owned()),
            ".unwrap() call".to_owned(),
        )
    }

    #[test]
    fn gap_inventory_keeps_only_path_and_span_diagnostics() {
        let records = vec![
            gap(
                "gap-a",
                "src/a.rs",
                "println!",
                "unsupported macro invocation println!",
                span(10, 30, 2),
            ),
            spanless_diagnostic("manifest-hole", "Cargo.toml"),
            pathless_diagnostic("review-artifact"),
            debt_marker("todo-1", "src/a.rs"),
            panic_risk("unwrap-1", "src/a.rs"),
            gap(
                "gap-b",
                "src/b.rs",
                "unresolved_dispatch: Trait::method",
                "unresolved trait-dispatch target Trait::method (no in-crate implementor method)",
                span(0, 20, 1),
            ),
        ];
        let gaps = diagnostic_gap_records(&records);
        let ids: Vec<&str> = gaps.iter().map(|r| r.id()).collect();
        assert_eq!(ids, vec!["gap-a", "gap-b"]);
    }

    #[test]
    fn gap_inventory_sorts_canonically() {
        let records = vec![
            gap(
                "gap-z",
                "src/z.rs",
                "println!",
                "unsupported macro invocation println!",
                span(50, 70, 5),
            ),
            gap(
                "gap-a2",
                "src/a.rs",
                "eprintln!",
                "unsupported macro invocation eprintln!",
                span(90, 110, 9),
            ),
            gap(
                "gap-a1",
                "src/a.rs",
                "println!",
                "unsupported macro invocation println!",
                span(10, 30, 2),
            ),
        ];
        let gaps = diagnostic_gap_records(&records);
        let ids: Vec<&str> = gaps.iter().map(|r| r.id()).collect();
        assert_eq!(ids, vec!["gap-a1", "gap-a2", "gap-z"]);
    }

    #[test]
    fn gap_inventory_is_stable_across_runs() {
        let records = vec![
            gap(
                "gap-a",
                "src/a.rs",
                "println!",
                "unsupported macro invocation println!",
                span(10, 30, 2),
            ),
            gap(
                "gap-b",
                "src/b.rs",
                "println!",
                "unsupported macro invocation println!",
                span(10, 30, 2),
            ),
        ];
        let first: Vec<String> = diagnostic_gap_records(&records)
            .iter()
            .map(|r| r.id().to_owned())
            .collect();
        let second: Vec<String> = diagnostic_gap_records(&records)
            .iter()
            .map(|r| r.id().to_owned())
            .collect();
        assert_eq!(first, second);
    }

    #[test]
    fn gap_inventory_empty_store_yields_empty() {
        assert!(diagnostic_gap_records(&[]).is_empty());
    }
}
