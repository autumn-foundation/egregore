//! Find usage sites of an external dependency symbol before upgrade (issue #258).
//!
//! Answers "where is this dependency symbol actually called?" — the
//! pre-upgrade question a dependency bump requires: every usage site of
//! `tokio::spawn`, alias-aware, with no comment/string false positives.
//!
//! The lane reads two AST-derived relations only:
//!
//! * `CALLS` edges whose `resolution` is `unresolved`: a call site the
//!   repo-wide resolver could not bind to an in-repo definition — i.e. a call
//!   into an external dependency, or a missing internal path (marked
//!   `external: false` and pointed at `transitive-callers`).
//! * `IMPORTS` edges from a file to its `Import` nodes: the import
//!   declarations that let the lane substitute a leading alias
//!   (`use a::b::c as d; d(...)` resolves against `a::b::c`).
//!
//! Deliberately out of scope:
//!
//! * Resolved internal `CALLS` edges: a symbol the resolver bound inside the
//!   repo has a definition, and its callers are the `transitive-callers`
//!   lane's job.
//! * `UnresolvedDispatch` markers: they name trait-dispatch sites, not
//!   dependency symbols.
//! * `MENTIONS` edges: no general extractor relation exists under that label
//!   (reserved per issue #442), so the lane claims no string/comment matching
//!   at all — by construction every row is backed by a parsed call site, and
//!   comments and string literals can never produce rows.
//!
//! Crate-relative paths (`crate::…`, `self::…`, `super::…`) are reported in
//! the crate-relative vocabulary the code uses: a `crate::`-leading query
//! matches `crate::`-written sites. The owning-crate comparison additionally
//! marks `use` paths rooted at the repository's own crate `internal` (e.g.
//! `use mycrate::utils; utils::helper()` where the call is unresolved).

use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{CallResolution, EdgeLabel, GraphRecord, NodeKind, SourceSpan};

use super::RepositoryIndex;
use super::liveness::Liveness;
use super::who_imports::{is_segment_prefix, owning_crate_name, parse_query_path};
use crate::languages::cross_file::{parse_import_segments, strip_use_prefix};

/// Why [`dep_usage`] refused to run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DepUsageError {
    /// The query path has no usable segments.
    #[error("malformed path '{path}': expected segments separated by '::', e.g. tokio::spawn")]
    MalformedPath {
        /// The rejected query path, echoed for diagnostics.
        path: String,
    },
}

/// One usage site of the queried dependency symbol.
///
/// The backing record is the unresolved-call `Diagnostic` node: one row per
/// (file, written callee) — repeated callers of the same written callee in one
/// file share the diagnostic and collapse to a single row whose span is the
/// first site.
#[derive(Debug, Clone)]
pub struct DepUsageRow<'a> {
    /// The unresolved-call diagnostic record backing this row.
    pub record_id: &'a str,
    /// Schema version of the backing record.
    pub schema_version: u32,
    /// Repository-relative path of the file containing the call site.
    pub repo_relative_path: &'a str,
    /// Source span of the (first) call site with this written callee.
    pub span: Option<SourceSpan>,
    /// The callee exactly as written at the call site (`spawn`, `tokio::spawn`).
    pub callee: &'a str,
    /// The resolving import path: the written callee with its leading alias
    /// substituted for the imported path (`tokio::spawn`,
    /// `aletheiadb::Store::open`).
    pub import_path: String,
    /// `true` when the resolving path names an external dependency rather
    /// than the repository's own crate namespace.
    pub external: bool,
}

/// Per-file alias table: alias → the imported path segments, or `None` when
/// the alias is bound by two different imports in the same file. Ambiguity is
/// a resolution refusal, never a guess: both bindings are dropped.
type AliasTable<'a> = BTreeMap<&'a str, BTreeMap<String, Option<Vec<String>>>>;

/// Splits `path as alias` into `(path, alias)`; without ` as ` the alias is
/// the final path segment.
fn split_alias(name: &str) -> (&str, &str) {
    name.rfind(" as ").map_or_else(
        || {
            let alias = name.rsplit("::").next().unwrap_or(name).trim();
            (name, alias)
        },
        |idx| (name[..idx].trim(), name[idx + 4..].trim()),
    )
}

/// Expands one import declaration's `name` into `(alias, path-segments)` pairs:
///
/// * `tokio::spawn` → `[("spawn", ["tokio", "spawn"])]`
/// * `aletheiadb::Store as Backend` → `[("Backend", ["aletheiadb", "Store"])]`
/// * `a::b::{C, D}` → `[("C", ["a","b","C"]), ("D", ["a","b","D"])]`
/// * `a::b::{C as D}` → `[("D", ["a","b","C"])]`
/// * `a::b::*` → `[]` (a glob binds no single alias)
fn import_alias_bindings(name: &str) -> Vec<(String, Vec<String>)> {
    let name = strip_use_prefix(name).trim().trim_end_matches(';').trim();
    if let Some(brace) = name.find('{') {
        let prefix: Vec<String> = name[..brace]
            .split("::")
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect();
        let leaves = name[brace..].trim_matches(|c| c == '{' || c == '}');
        return leaves
            .split(',')
            .filter_map(|leaf| {
                let leaf = leaf.trim().trim_end_matches(';').trim();
                if leaf.is_empty() || leaf == "*" {
                    return None;
                }
                let (path_leaf, alias) = split_alias(leaf);
                if alias.is_empty() || alias == "*" {
                    return None;
                }
                let mut segments = prefix.clone();
                segments.push(path_leaf.to_owned());
                Some((alias.to_owned(), segments))
            })
            .collect();
    }
    if name.ends_with("::*") {
        return Vec::new();
    }
    let (path_part, alias) = split_alias(name);
    if alias.is_empty() || alias == "*" {
        return Vec::new();
    }
    let segments = parse_import_segments(path_part);
    if segments.is_empty() {
        return Vec::new();
    }
    vec![(alias.to_owned(), segments)]
}

/// Builds the per-file alias tables from every live `Import` node.
fn alias_bindings(records: &[GraphRecord]) -> AliasTable<'_> {
    let mut tables: AliasTable<'_> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Import,
            name: Some(import_name),
            repo_relative_path: Some(path),
            ..
        } = record
        else {
            continue;
        };
        let table = tables.entry(path.as_str()).or_default();
        for (alias, segments) in import_alias_bindings(import_name) {
            table
                .entry(alias)
                .and_modify(|slot| {
                    if slot.as_ref() != Some(&segments) {
                        *slot = None;
                    }
                })
                .or_insert(Some(segments));
        }
    }
    tables
}

/// Resolves a written callee (`spawn`, `tokio::spawn`, `Backend::open`,
/// `crate::missing::thing`) against one file's alias table.
///
/// Returns `None` when the leading alias is ambiguously bound — resolution
/// stays conservative rather than guessing. A leading `crate`/`self`/`super`
/// resolves crate-relatively; any other unaliased head stands as written.
fn resolve_written_path(
    written: &[&str],
    aliases: Option<&BTreeMap<String, Option<Vec<String>>>>,
) -> Option<Vec<String>> {
    let (head, tail) = written.split_first()?;
    if matches!(*head, "crate" | "self" | "super") {
        return Some(written.iter().map(ToString::to_string).collect());
    }
    Some(
        aliases
            .and_then(|table| table.get(*head))
            .and_then(Option::as_ref)
            .map_or_else(
                || {
                    // No binding, or an ambiguous binding: the written path
                    // stands as written. (An ambiguous alias can only *miss*
                    // here — the bare written path never prefix-matches a
                    // longer qualified query — so this never manufactures a
                    // false positive.)
                    written.iter().map(ToString::to_string).collect()
                },
                |imported| {
                    let mut resolved = imported.clone();
                    resolved.extend(tail.iter().map(ToString::to_string));
                    resolved
                },
            ),
    )
}

/// Classifies a resolved path as external vs. the repository's own crate
/// namespace. Crate-relative heads are always internal; otherwise the head is
/// compared against the file's owning crate name (issue #450 facts), and a
/// path whose crate cannot be established is treated as external.
fn is_external_path(resolved: &[String], owning_crate: Option<&str>) -> bool {
    let [head, ..] = resolved else {
        return true;
    };
    if matches!(head.as_str(), "crate" | "self" | "super") {
        return false;
    }
    owning_crate.is_none_or(|owning| head != owning)
}

/// Finds every usage site of the dependency symbol named by `query_path`.
///
/// `query_path` is a fully or partially qualified `::`-separated path
/// (`tokio::spawn`, `aletheiadb`); matching is segment-prefix on the
/// alias-resolved path, so `tokio::spaw` never matches and `aletheiadb`
/// matches `aletheiadb::Store::open`. `repo_scope`, when set, restricts rows
/// to diagnostics owned by that repository. Rows are deterministic:
/// `(repo_relative_path, span start, callee, import_path)`.
///
/// # Errors
///
/// Returns [`DepUsageError::MalformedPath`] when `query_path` is empty, has a
/// leading/trailing `::`, an empty interior segment, or a whitespace-bearing
/// segment.
pub fn dep_usage<'a>(
    records: &'a [GraphRecord],
    query_path: &str,
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> Result<Vec<DepUsageRow<'a>>, DepUsageError> {
    let query_segments =
        parse_query_path(query_path).map_err(|_| DepUsageError::MalformedPath {
            path: query_path.to_owned(),
        })?;
    let query_segments: Vec<&str> = query_segments.iter().map(String::as_str).collect();
    let liveness = Liveness::new(records);
    let aliases = alias_bindings(records);

    let diagnostics: BTreeMap<&str, &GraphRecord> = records
        .iter()
        .filter_map(|record| match record {
            GraphRecord::Node {
                id,
                kind: NodeKind::Diagnostic,
                ..
            } => Some((id.as_str(), record)),
            _ => None,
        })
        .collect();

    // Owning crate name per file, for the external/internal classification.
    let mut owning_crates: BTreeMap<&str, String> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Import,
            repo_relative_path: Some(path),
            ..
        } = record
        {
            owning_crates
                .entry(path.as_str())
                .or_insert_with(|| owning_crate_name(record, path).unwrap_or_default());
        }
    }

    let mut rows: Vec<DepUsageRow<'a>> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        let GraphRecord::Edge {
            label: EdgeLabel::Calls,
            source,
            target,
            resolution: Some(CallResolution::Unresolved),
            ..
        } = record
        else {
            continue;
        };
        if liveness.deleted(target) {
            continue;
        }
        let Some(diagnostic) = diagnostics.get(target.as_str()) else {
            continue;
        };
        let GraphRecord::Node {
            id: diagnostic_id,
            kind: NodeKind::Diagnostic,
            name: Some(callee),
            repo_relative_path: Some(path),
            span,
            schema_version,
            ..
        } = *diagnostic
        else {
            continue;
        };
        if liveness.deleted(diagnostic_id) {
            continue;
        }
        if let Some(scope) = repo_scope {
            // Repo scoping is attributed through the CALLER, not the
            // diagnostic: unresolved-call diagnostic nodes hang off the call
            // graph (CALLS edges), outside the containment topology the
            // repository index walks, so the index cannot attribute them
            // directly. The caller symbol is containment-attributed, and
            // every edge targeting one diagnostic shares the scan that
            // minted it, so filtering edges is equivalent to filtering
            // diagnostics. Unattributed callers are dropped under a scope.
            if index.owner_of(source.as_str()) != Some(scope) {
                continue;
            }
        }
        // One row per diagnostic: repeated callers of the same written callee
        // in one file share the node and collapse to its first-site span.
        if !seen.insert(diagnostic_id.as_str()) {
            continue;
        }
        let written: Vec<&str> = callee.split("::").collect();
        let Some(resolved) = resolve_written_path(&written, aliases.get(path.as_str())) else {
            continue;
        };
        let resolved_refs: Vec<&str> = resolved.iter().map(String::as_str).collect();
        if !is_segment_prefix(&query_segments, &resolved_refs) {
            continue;
        }
        let owning = owning_crates
            .get(path.as_str())
            .map(String::as_str)
            .filter(|name| !name.is_empty());
        rows.push(DepUsageRow {
            record_id: diagnostic_id,
            schema_version: *schema_version,
            repo_relative_path: path,
            span: *span,
            callee,
            import_path: resolved.join("::"),
            external: is_external_path(&resolved, owning),
        });
    }

    rows.sort_by(|a, b| {
        (
            a.repo_relative_path,
            span_sort_key(a.span),
            a.callee,
            a.import_path.as_str(),
        )
            .cmp(&(
                b.repo_relative_path,
                span_sort_key(b.span),
                b.callee,
                b.import_path.as_str(),
            ))
    });
    Ok(rows)
}

/// Sort key for an optional span: spanless rows sort last.
fn span_sort_key(span: Option<SourceSpan>) -> (usize, usize, usize, usize) {
    span.map_or((usize::MAX, usize::MAX, usize::MAX, usize::MAX), |s| {
        (s.start_line, s.end_line, s.start_byte, s.end_byte)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{GraphRecord, NodeKind};

    fn import_node(id: &str, name: &str, path: &str) -> GraphRecord {
        GraphRecord::syntax_node(
            id.to_owned(),
            NodeKind::Import,
            path.to_owned(),
            SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 1,
                start_column: Some(0),
                end_column: Some(10),
            },
            name.to_owned(),
            "rust",
            String::new(),
        )
    }

    fn diagnostic_node(id: &str, display: &str, path: &str, line: usize) -> GraphRecord {
        GraphRecord::syntax_node(
            id.to_owned(),
            NodeKind::Diagnostic,
            path.to_owned(),
            SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: line,
                end_line: line,
                start_column: Some(0),
                end_column: Some(10),
            },
            display.to_owned(),
            "rust",
            format!("unresolved call target {display} (no in-repo definition)"),
        )
    }

    fn unresolved_edge(id: &str, caller: &str, diagnostic: &str) -> GraphRecord {
        GraphRecord::Edge {
            id: id.to_owned(),
            schema_version: 1,
            label: EdgeLabel::Calls,
            source: caller.to_owned(),
            target: diagnostic.to_owned(),
            confidence: None,
            resolution: Some(CallResolution::Unresolved),
            frame_resolution: None,
            frame_index: None,
            basis: None,
            call_site_spans: None,
            is_exhaustive: None,
            temporal: None,
            summary: String::new(),
            producer: None,
        }
    }

    fn fixture() -> Vec<GraphRecord> {
        vec![
            import_node("imp1", "tokio::spawn", "src/main.rs"),
            import_node("imp2", "aletheiadb::Store as Backend", "src/main.rs"),
            diagnostic_node("diag1", "spawn", "src/main.rs", 7),
            diagnostic_node("diag2", "tokio::spawn", "src/main.rs", 9),
            diagnostic_node("diag3", "Backend::open", "src/main.rs", 11),
            diagnostic_node("diag4", "crate::missing::thing", "src/main.rs", 13),
            unresolved_edge("e1", "main", "diag1"),
            unresolved_edge("e2", "main", "diag2"),
            unresolved_edge("e3", "main", "diag3"),
            unresolved_edge("e4", "main", "diag4"),
        ]
    }

    fn index_for(records: &[GraphRecord]) -> RepositoryIndex {
        RepositoryIndex::build(records)
    }

    #[test]
    fn alias_and_qualified_sites_resolve() {
        let records = fixture();
        let index = index_for(&records);
        let rows = dep_usage(&records, "tokio::spawn", &index, None).expect("query runs");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].callee, "spawn");
        assert_eq!(rows[0].import_path, "tokio::spawn");
        assert!(rows[0].external);
        assert_eq!(rows[1].callee, "tokio::spawn");
        assert_eq!(rows[1].import_path, "tokio::spawn");
    }

    #[test]
    fn partial_prefix_resolves_renamed_alias() {
        let records = fixture();
        let index = index_for(&records);
        let rows = dep_usage(&records, "aletheiadb", &index, None).expect("query runs");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].callee, "Backend::open");
        assert_eq!(rows[0].import_path, "aletheiadb::Store::open");
        assert!(rows[0].external);
    }

    #[test]
    fn segment_boundary_is_exact() {
        let records = fixture();
        let index = index_for(&records);
        let rows = dep_usage(&records, "tokio::spaw", &index, None).expect("query runs");
        assert!(rows.is_empty());
    }

    #[test]
    fn internal_paths_are_marked_not_external() {
        let records = fixture();
        let index = index_for(&records);
        let rows = dep_usage(&records, "crate::missing", &index, None).expect("query runs");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].external);
        assert_eq!(rows[0].import_path, "crate::missing::thing");
    }

    #[test]
    fn malformed_paths_are_rejected() {
        let records = fixture();
        let index = index_for(&records);
        for bad in ["", "tokio::", "::tokio", "tokio::::spawn", "tokio:: spawn"] {
            let err = dep_usage(&records, bad, &index, None).expect_err("malformed");
            assert_eq!(
                err,
                DepUsageError::MalformedPath {
                    path: bad.to_owned()
                }
            );
        }
    }

    #[test]
    fn group_import_leaves_bind() {
        let records = vec![
            import_node("imp1", "a::b::{C, D}", "src/main.rs"),
            diagnostic_node("diag1", "D::new", "src/main.rs", 3),
            unresolved_edge("e1", "main", "diag1"),
        ];
        let index = index_for(&records);
        let rows = dep_usage(&records, "a::b", &index, None).expect("query runs");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].import_path, "a::b::D::new");
    }

    #[test]
    fn ambiguous_alias_is_dropped_not_guessed() {
        let records = vec![
            import_node("imp1", "tokio::spawn", "src/main.rs"),
            import_node("imp2", "other::spawn", "src/main.rs"),
            diagnostic_node("diag1", "spawn", "src/main.rs", 7),
            unresolved_edge("e1", "main", "diag1"),
        ];
        let index = index_for(&records);
        // The bare alias cannot resolve to `tokio::spawn` — no row, no guess.
        let rows = dep_usage(&records, "tokio::spawn", &index, None).expect("query runs");
        assert!(rows.is_empty());
    }

    #[test]
    fn repeated_callers_collapse_to_one_row() {
        let records = vec![
            import_node("imp1", "tokio::spawn", "src/main.rs"),
            diagnostic_node("diag1", "spawn", "src/main.rs", 7),
            unresolved_edge("e1", "main", "diag1"),
            unresolved_edge("e2", "other", "diag1"),
        ];
        let index = index_for(&records);
        let rows = dep_usage(&records, "tokio", &index, None).expect("query runs");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].span.map(|s| s.start_line), Some(7));
    }

    #[test]
    fn rows_sort_deterministically() {
        let records = vec![
            import_node("imp1", "tokio::spawn", "src/b.rs"),
            import_node("imp2", "tokio::spawn", "src/a.rs"),
            diagnostic_node("diag1", "spawn", "src/b.rs", 7),
            diagnostic_node("diag2", "spawn", "src/a.rs", 9),
            diagnostic_node("diag3", "tokio::spawn", "src/a.rs", 3),
            unresolved_edge("e1", "main", "diag1"),
            unresolved_edge("e2", "main", "diag2"),
            unresolved_edge("e3", "main", "diag3"),
        ];
        let index = index_for(&records);
        let rows = dep_usage(&records, "tokio", &index, None).expect("query runs");
        let order: Vec<(&str, usize)> = rows
            .iter()
            .map(|r| (r.repo_relative_path, r.span.map_or(0, |s| s.start_line)))
            .collect();
        assert_eq!(
            order,
            vec![("src/a.rs", 3), ("src/a.rs", 9), ("src/b.rs", 7)]
        );
    }
}
