//! `eg query who-imports <module-path>` — importer lookup over Import nodes
//! (issue #444).
//!
//! # SPEC
//!
//! Answer the question "which files import this module path?" — a precise,
//! read-only lookup over the [`NodeKind::Import`] nodes the language extractors
//! already mint. Each Import node's `name` field carries the raw import PATH
//! TEXT (`crate::a::T`, `std::collections::BTreeMap`, `serde::Serialize as S`,
//! `crate::a::{B, C}`) and its `repo_relative_path`/`span` point at the
//! IMPORTING file. This lane never re-parses source and never grep-matches
//! comments or strings: a `///` doc-comment mention of `foo::bar` produces no
//! Import node, so it is invisible here — the precision win over `grep`.
//!
//! ## Matching (segment-aware prefix)
//!
//! The query `<module-path>` is a `::`-separated module path (`serde`,
//! `foo::bar`, `crate::query::liveness`). Each live Import node's `name` is
//! parsed into a module-path segment list (trailing ` as <alias>` stripped, a
//! group import `a::b::{C, D}` reduced to its common module prefix `a::b`, a
//! glob `a::b::*` reduced to `a::b`, then split on `::`). An Import MATCHES iff
//! the query segment sequence is a SEGMENT-AWARE PREFIX of the import's segment
//! sequence — the query segments equal the import's first N segments. This
//! makes `foo::bar` match `foo::bar::Baz` and `foo::bar`, but NEVER
//! `foo::barbell` (segment boundaries are respected, so no sibling-path bleed).
//!
//! ## `crate::` unification boundary (honest, documented)
//!
//! The graph carries no per-source-file owning-crate name — a `File` node
//! records a repo-relative path, and a `Repository` node records a repo
//! identity, but neither maps a source file to the Cargo crate that compiles
//! it. Crate names therefore cannot be reliably derived at query time, so this
//! lane does NOT fabricate `crate::` ↔ `<crate_name>::` unification. Two honest
//! behaviours ship instead:
//!
//! * By default, segments are matched literally: a `crate`-relative import
//!   (`crate::foo::Bar`) and an absolute external import (`mycrate::foo::Bar`)
//!   are DISTINCT, and a leading `self`/`super` is matched literally (the graph
//!   has no module anchor to resolve a relative prefix soundly).
//! * With an explicit `--crate <name>`, a leading `crate` segment in EITHER the
//!   query OR an import is rewritten to `<name>` before matching, so the two
//!   forms unify: `mycrate::foo` then matches both `crate::foo::Bar` and
//!   `mycrate::foo::Bar`. This is caller-supplied ground truth, never guessed.
//!
//! Liveness follows the shared latest-write-wins [`Liveness`] gate so `--graph`
//! and `--data-dir` agree on tombstoned / revived Import records.

use super::liveness::Liveness;
use crate::ir::{GraphRecord, NodeKind, SourceSpan};

use super::RepositoryIndex;

/// Why a `who-imports` query path was rejected.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum WhoImportsError {
    /// The module path is empty, has a leading/trailing `::`, an empty interior
    /// segment (`a::::b`), or a whitespace-bearing segment.
    MalformedModulePath {
        /// The path as supplied by the caller.
        path: String,
    },
}

impl WhoImportsError {
    /// Stable machine-readable diagnostic code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MalformedModulePath { .. } => "malformed_module_path",
        }
    }
}

/// One importing Import node returned by [`who_imports`].
///
/// Every row carries the citation handles the code trust class requires: a
/// stable `record_id` plus the repo-relative file path (the importing file
/// handle) and, when recorded, the source span of the `use` declaration.
#[derive(Debug, Clone)]
pub struct WhoImportsRow<'a> {
    /// Stable record ID of the `Import` node.
    pub record_id: &'a str,
    /// Record schema version.
    pub schema_version: u32,
    /// Repo-relative path of the importing file (the file handle).
    pub repo_relative_path: &'a str,
    /// Source span of the `use` declaration, when recorded.
    pub span: Option<SourceSpan>,
    /// The raw import path text carried by the Import node's `name`.
    pub import_path: &'a str,
}

/// The importer set for one module-path query.
#[derive(Debug, Clone, Default)]
pub struct WhoImportsResult<'a> {
    /// The normalized query module path (segments re-joined with `::`).
    pub query_path: String,
    /// The `--crate` unification name in effect, when supplied.
    pub crate_name: Option<String>,
    /// Matching importer rows, sorted by
    /// (`repo_relative_path`, `span.start_line`, `record_id`).
    pub rows: Vec<WhoImportsRow<'a>>,
}

impl WhoImportsResult<'_> {
    /// Returns `true` when a well-formed query matched zero importers.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Parses and validates a query module path into its segment list.
///
/// # Errors
///
/// Returns [`WhoImportsError::MalformedModulePath`] when the path is empty, has
/// a leading/trailing `::`, an empty interior segment, or a segment bearing
/// whitespace.
pub fn parse_query_path(raw: &str) -> Result<Vec<String>, WhoImportsError> {
    let malformed = || WhoImportsError::MalformedModulePath {
        path: raw.to_owned(),
    };
    if raw.is_empty() {
        return Err(malformed());
    }
    let segments: Vec<&str> = raw.split("::").collect();
    for seg in &segments {
        // An empty segment covers the leading-`::`, trailing-`::`, and interior
        // `a::::b` cases in one check.
        if seg.is_empty() {
            return Err(malformed());
        }
        // A whitespace-bearing segment (including a leading/trailing space) is
        // never a valid module path segment.
        if seg.chars().any(char::is_whitespace) {
            return Err(malformed());
        }
    }
    Ok(segments.into_iter().map(str::to_owned).collect())
}

/// Reduces an Import node's raw `name` path text to its module-path segment
/// list.
///
/// A group import `a::b::{C, D}` reduces to the common module prefix `a::b`; a
/// glob `a::b::*` reduces to `a::b`; a trailing ` as <alias>` rename is
/// stripped. Empty segments (from a trailing `::`) and `*` are dropped.
#[must_use]
pub fn parse_import_segments(name: &str) -> Vec<String> {
    // Group import: everything before the first `{` is the common module
    // prefix; the braced leaves (and any leaf renames inside them) are dropped.
    let head = name.find('{').map_or(name, |idx| &name[..idx]);
    // Non-group rename: strip a trailing ` as <alias>` (a group's leaf renames
    // already went with the braces above).
    let head = head.find(" as ").map_or(head, |idx| &head[..idx]);
    head.split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*")
        .map(str::to_owned)
        .collect()
}

/// Applies the `--crate` unification: rewrites a leading `crate` segment to the
/// supplied crate name so `crate`-relative and absolute forms unify. A no-op
/// when `crate_name` is `None` or the first segment is not `crate`.
fn normalize<'seg>(segments: &'seg [String], crate_name: Option<&'seg str>) -> Vec<&'seg str> {
    let mut out: Vec<&str> = segments.iter().map(String::as_str).collect();
    if let (Some(name), Some(first)) = (crate_name, out.first_mut()) {
        if *first == "crate" {
            *first = name;
        }
    }
    out
}

/// Returns `true` when `query` is a segment-aware prefix of `import`.
fn is_segment_prefix(query: &[&str], import: &[&str]) -> bool {
    query.len() <= import.len() && import[..query.len()] == *query
}

/// Returns every file that imports a module path (issue #444).
///
/// `query_path` is a `::`-separated module path; `crate_name` optionally
/// unifies `crate::` with an absolute crate name (see the module docs).
/// `repo_scope`, when set, restricts results to Import nodes owned by that
/// repository (resolved via [`RepositoryIndex::owner_of`]).
///
/// # Errors
///
/// Returns [`WhoImportsError::MalformedModulePath`] when `query_path` is
/// malformed (see [`parse_query_path`]).
pub fn who_imports<'a>(
    records: &'a [GraphRecord],
    query_path: &str,
    crate_name: Option<&str>,
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> Result<WhoImportsResult<'a>, WhoImportsError> {
    let query_segments = parse_query_path(query_path)?;
    let query_norm = normalize(&query_segments, crate_name);

    let liveness = Liveness::new(records);
    let is_owned =
        |id: &str| -> bool { repo_scope.is_none_or(|scope| index.owner_of(id) == Some(scope)) };

    // Collapse the append-only slice to latest-write-wins per Import id: an
    // Import re-added after its own tombstone (or simply re-written) recurs by
    // stable id, so keep only its last occurrence — matching the embedded
    // current-state read so `--graph` and `--data-dir` agree. A later
    // non-matching version correctly supersedes an earlier matching one.
    let mut latest: std::collections::BTreeMap<&'a str, &'a GraphRecord> =
        std::collections::BTreeMap::new();
    for record in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Import,
            ..
        } = record
        {
            latest.insert(id.as_str(), record);
        }
    }

    let mut rows: Vec<WhoImportsRow<'a>> = Vec::new();
    for record in latest.into_values() {
        let GraphRecord::Node {
            id,
            schema_version,
            name: Some(import_path),
            repo_relative_path: Some(path),
            span,
            ..
        } = record
        else {
            continue;
        };
        if liveness.deleted(id) {
            continue;
        }
        if !is_owned(id) {
            continue;
        }
        let import_segments = parse_import_segments(import_path);
        let import_norm = normalize(&import_segments, crate_name);
        if !is_segment_prefix(&query_norm, &import_norm) {
            continue;
        }
        rows.push(WhoImportsRow {
            record_id: id,
            schema_version: *schema_version,
            repo_relative_path: path,
            span: *span,
            import_path,
        });
    }

    rows.sort_by(|a, b| {
        a.repo_relative_path
            .cmp(b.repo_relative_path)
            .then_with(|| {
                a.span
                    .map(|s| s.start_line)
                    .cmp(&b.span.map(|s| s.start_line))
            })
            .then_with(|| a.record_id.cmp(b.record_id))
    });

    Ok(WhoImportsResult {
        query_path: query_segments.join("::"),
        crate_name: crate_name.map(str::to_owned),
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Graph, stable_id};

    fn span(start_line: usize) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line,
            end_line: start_line,
        }
    }

    fn import_id(path: &str, name: &str) -> String {
        stable_id(&["node", "import", "repo", path, name])
    }

    /// Pushes an Import node (and its owning-file IMPORTS edge) whose `name`
    /// carries the raw import path text, mirroring the extractor.
    fn import(graph: &mut Graph, path: &str, name: &str, line: usize) -> String {
        let id = import_id(path, name);
        graph.push(GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Import,
            path.to_owned(),
            span(line),
            name.to_owned(),
            "rust",
            format!("Rust import {name}"),
        ));
        id
    }

    fn tombstone(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v6:tomb_{deleted_id}"),
            schema_version: 6,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    fn run<'a>(records: &'a [GraphRecord], query: &str) -> WhoImportsResult<'a> {
        let index = RepositoryIndex::build(records);
        who_imports(records, query, None, &index, None).expect("valid query")
    }

    // ── query-path validation ────────────────────────────────────────────

    #[test]
    fn valid_query_paths_parse() {
        assert_eq!(parse_query_path("serde").unwrap(), vec!["serde"]);
        assert_eq!(parse_query_path("foo::bar").unwrap(), vec!["foo", "bar"]);
        assert_eq!(
            parse_query_path("crate::query::liveness").unwrap(),
            vec!["crate", "query", "liveness"]
        );
    }

    #[test]
    fn malformed_query_paths_are_rejected() {
        for bad in [
            "",
            "::foo",
            "foo::",
            "a::::b",
            "foo bar",
            "foo:: bar",
            " foo",
        ] {
            assert!(
                parse_query_path(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    // ── import-name parsing ──────────────────────────────────────────────

    #[test]
    fn import_segments_cover_alias_group_and_glob() {
        assert_eq!(
            parse_import_segments("std::collections::BTreeMap"),
            vec!["std", "collections", "BTreeMap"]
        );
        assert_eq!(
            parse_import_segments("serde::Serialize as S"),
            vec!["serde", "Serialize"]
        );
        assert_eq!(
            parse_import_segments("crate::a::{B, C}"),
            vec!["crate", "a"]
        );
        assert_eq!(
            parse_import_segments("crate::a::{B, C as D}"),
            vec!["crate", "a"]
        );
        assert_eq!(parse_import_segments("crate::a::*"), vec!["crate", "a"]);
        assert_eq!(parse_import_segments("crate::{a, b}"), vec!["crate"]);
    }

    // ── matching ─────────────────────────────────────────────────────────

    #[test]
    fn prefix_matches_deeper_import() {
        let mut g = Graph::new();
        let id = import(&mut g, "src/a.rs", "foo::bar::Baz", 1);
        let recs = g.into_records();
        let result = run(&recs, "foo::bar");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].record_id, id);
        assert_eq!(result.rows[0].import_path, "foo::bar::Baz");
    }

    #[test]
    fn exact_module_import_matches() {
        let mut g = Graph::new();
        import(&mut g, "src/a.rs", "foo::bar", 1);
        let recs = g.into_records();
        assert_eq!(run(&recs, "foo::bar").rows.len(), 1);
    }

    #[test]
    fn segment_boundary_prevents_sibling_bleed() {
        // `foo::bar` must never match `foo::barbell`.
        let mut g = Graph::new();
        import(&mut g, "src/a.rs", "foo::barbell::X", 1);
        let recs = g.into_records();
        assert!(run(&recs, "foo::bar").is_empty());
    }

    #[test]
    fn alias_import_matches_on_path_not_alias() {
        let mut g = Graph::new();
        import(&mut g, "src/a.rs", "serde::Serialize as S", 1);
        let recs = g.into_records();
        assert_eq!(run(&recs, "serde").rows.len(), 1);
        assert_eq!(run(&recs, "serde::Serialize").rows.len(), 1);
        // The alias itself is not part of the module path.
        assert!(run(&recs, "serde::Serialize::S").is_empty());
    }

    #[test]
    fn group_import_matches_common_prefix() {
        let mut g = Graph::new();
        import(&mut g, "src/a.rs", "crate::a::{B, C}", 1);
        let recs = g.into_records();
        assert_eq!(run(&recs, "crate::a").rows.len(), 1);
        // The braced leaves are not reachable as module segments.
        assert!(run(&recs, "crate::a::B").is_empty());
    }

    #[test]
    fn rustdoc_decoy_produces_no_match() {
        // The precision win over grep: a `///` doc comment mentioning `foo::bar`
        // mints no Import node, so nothing matches — modeled by simply not
        // adding an Import node for the decoy.
        let mut g = Graph::new();
        g.push(GraphRecord::syntax_node(
            stable_id(&["node", "Symbol", "src/a.rs", "thing"]),
            NodeKind::Symbol,
            "src/a.rs".to_owned(),
            span(1),
            "thing".to_owned(),
            "rust",
            "/// see foo::bar for details".to_owned(),
        ));
        let recs = g.into_records();
        assert!(run(&recs, "foo::bar").is_empty());
    }

    // ── liveness ─────────────────────────────────────────────────────────

    #[test]
    fn tombstoned_import_is_excluded() {
        let mut g = Graph::new();
        let id = import(&mut g, "src/a.rs", "foo::bar::Baz", 1);
        g.push(tombstone(&id));
        let recs = g.into_records();
        assert!(run(&recs, "foo::bar").is_empty());
    }

    #[test]
    fn tombstoned_then_revived_import_is_included() {
        // Latest-write-wins: node, tombstone, node again → live.
        let mut g = Graph::new();
        let id = import(&mut g, "src/a.rs", "foo::bar::Baz", 1);
        g.push(tombstone(&id));
        import(&mut g, "src/a.rs", "foo::bar::Baz", 1);
        let recs = g.into_records();
        let result = run(&recs, "foo::bar");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].record_id, id);
    }

    // ── crate unification ────────────────────────────────────────────────

    #[test]
    fn crate_flag_unifies_internal_and_external_forms() {
        let mut g = Graph::new();
        import(&mut g, "src/a.rs", "crate::foo::Bar", 1);
        import(&mut g, "src/b.rs", "mycrate::foo::Baz", 1);
        let recs = g.into_records();
        let index = RepositoryIndex::build(&recs);

        // Without --crate: the two forms are distinct.
        let plain = who_imports(&recs, "mycrate::foo", None, &index, None).unwrap();
        assert_eq!(plain.rows.len(), 1);
        assert_eq!(plain.rows[0].import_path, "mycrate::foo::Baz");

        // With --crate mycrate: both forms unify against the query.
        let unified = who_imports(&recs, "mycrate::foo", Some("mycrate"), &index, None).unwrap();
        assert_eq!(unified.rows.len(), 2);

        // A `crate::` query also unifies when --crate is given.
        let crate_query = who_imports(&recs, "crate::foo", Some("mycrate"), &index, None).unwrap();
        assert_eq!(crate_query.rows.len(), 2);
    }

    // ── determinism ──────────────────────────────────────────────────────

    #[test]
    fn rows_sorted_by_path_then_line_then_id() {
        let mut g = Graph::new();
        import(&mut g, "src/z.rs", "foo::bar::A", 3);
        import(&mut g, "src/a.rs", "foo::bar::B", 9);
        import(&mut g, "src/a.rs", "foo::bar::C", 2);
        let recs = g.into_records();
        let result = run(&recs, "foo::bar");
        let paths: Vec<(&str, usize)> = result
            .rows
            .iter()
            .map(|r| (r.repo_relative_path, r.span.map_or(0, |s| s.start_line)))
            .collect();
        assert_eq!(
            paths,
            vec![("src/a.rs", 2), ("src/a.rs", 9), ("src/z.rs", 3)]
        );
    }

    #[test]
    fn malformed_query_returns_error() {
        let recs: Vec<GraphRecord> = Vec::new();
        let index = RepositoryIndex::build(&recs);
        let err = who_imports(&recs, "a::::b", None, &index, None).unwrap_err();
        assert_eq!(err.code(), "malformed_module_path");
    }
}
