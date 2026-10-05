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
//! ## `crate::` auto-unification (issue #450)
//!
//! A leading `crate::` in an import denotes the importing file's OWN crate, so
//! it can be resolved to that crate's absolute `<crate_name>::…` form without
//! guessing — from facts the graph already carries (issue #440's crate-root
//! partitioning and issue #117's manifest-stamped package attribution):
//!
//! 1. Auxiliary-target files (`src/bin/<t>.rs`, `examples/<t>.rs`,
//!    `tests/<t>.rs`, `benches/<t>.rs`) compile as their own crate named for
//!    the TARGET — the #440 crate-root id's `bin:`/`example:`/`test:`/`bench:`
//!    base carries it, and the owning package's name would be the wrong crate.
//! 2. Otherwise the record's manifest-stamped `crate_attribution` package
//!    name, validated per the #104 doctrine
//!    (`CrateAttribution::owning_package_for`): only a resolver-producible
//!    value whose cited manifest encloses the record's path counts, so a
//!    crafted record cannot forge another crate's identity.
//! 3. Otherwise the #440 workspace-prefix-derived name
//!    (`crate_name_of`): the last component of the workspace-crate directory
//!    prefix, cargo-normalized (`-` → `_`). This covers graphs whose records
//!    predate attribution.
//!
//! A query's leading `crate` resolves per import to that import's owning crate
//! ("the importer's own crate"), so `crate::foo` finds every crate's own
//! `foo`, and `mycrate::foo` matches both `crate::foo::Bar` (in `mycrate`) and
//! `mycrate::foo::Baz` — with no flag. When nothing resolves a name (e.g. a
//! hand-built graph with no attribution), segments match literally, preserving
//! the old distinct-forms behavior.
//!
//! An explicit `--crate <name>` remains as an override: caller-supplied ground
//! truth that wins over the facts for both the query and every import — for
//! graphs the facts cannot cover, or when the caller wants to force a name.
//!
//! Residual bounds (documented, not hidden): a `[lib] name` override makes the
//! true crate name differ from the package name; a helper module under an
//! auxiliary target's directory (`tests/common/mod.rs`) inherits the target
//! name although its `crate::` resolves to whichever target includes it; and a
//! leading `self`/`super` is still matched literally (the graph has no module
//! anchor to resolve a relative prefix soundly).
//!
//! Liveness follows the shared latest-write-wins [`Liveness`] gate so `--graph`
//! and `--data-dir` agree on tombstoned / revived Import records.

use super::RepositoryIndex;
use super::liveness::Liveness;
use crate::ir::{GraphRecord, NodeKind, SourceSpan};
use crate::languages::cross_file::parse_import_segments;

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

/// Applies the `crate::` unification: rewrites a leading `crate` segment to the
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

/// The crate TARGET name for an auxiliary-target file (`src/bin/<t>.rs`,
/// `examples/<t>.rs`, `tests/<t>.rs`, `benches/<t>.rs`, and their module
/// files), `None` for the primary crate root (`lib`) and `build.rs`.
///
/// These files compile as their OWN crate named for the target — `crate::`
/// inside `examples/demo.rs` denotes the `demo` crate, not the owning package —
/// so the #440 crate-root id's `bin:`/`example:`/`test:`/`bench:` base is the
/// honest resolution and the package name would be the wrong crate. `build.rs`
/// falls through to the package name as the best available answer.
fn aux_target_name(repo_relative_path: &str) -> Option<String> {
    use crate::languages::cross_file::crate_root_id;
    // `crate_root_id` is `{workspace_prefix}::{base}` or just `{base}`; the
    // prefix is `/`-joined so the last `::`-separated component is the base.
    let root_id = crate_root_id(repo_relative_path);
    let base = root_id.rsplit("::").next().unwrap_or(&root_id);
    let target = base
        .strip_prefix("bin:")
        .or_else(|| base.strip_prefix("example:"))
        .or_else(|| base.strip_prefix("test:"))
        .or_else(|| base.strip_prefix("bench:"))?;
    // Cargo normalizes `-` to `_` in target-derived crate names, matching the
    // #440 `crate_name_of` convention.
    Some(target.replace('-', "_"))
}

/// Resolves the owning crate name for one Import record's leading `crate::`
/// (issue #450), or `None` when no fact covers it.
///
/// Resolution order: auxiliary-target name (the file's own crate, which the
/// package name would misname) → manifest-stamped package attribution
/// (validated per the #104 doctrine, so only a resolver-producible value whose
/// cited manifest encloses the record's path counts) → the #440
/// workspace-prefix-derived name (for graphs whose records predate
/// attribution). Never guessed beyond these facts.
fn owning_crate_name(record: &GraphRecord, repo_relative_path: &str) -> Option<String> {
    if let Some(target) = aux_target_name(repo_relative_path) {
        return Some(target);
    }
    if let Some((name, _manifest)) = record
        .crate_attribution()
        .and_then(|attribution| attribution.owning_package_for(repo_relative_path))
    {
        return Some(name.to_owned());
    }
    crate::languages::cross_file::crate_name_of(repo_relative_path)
}

/// Returns `true` when `query` is a segment-aware prefix of `import`.
fn is_segment_prefix(query: &[&str], import: &[&str]) -> bool {
    query.len() <= import.len() && import[..query.len()] == *query
}

/// Returns every file that imports a module path (issue #444).
///
/// `query_path` is a `::`-separated module path; `crate_name` is the explicit
/// `--crate` override that wins over the auto-resolved owning-crate facts for
/// both the query and every import (see the module docs). `repo_scope`, when
/// set, restricts results to Import nodes owned by that repository (resolved
/// via [`RepositoryIndex::owner_of`]).
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
        // Auto-unification (issue #450): a leading `crate` in EITHER the query
        // or the import resolves to this import's owning crate name — the
        // explicit `--crate` override first, then the owning-crate facts. A
        // `crate::`-leading query therefore means "the importer's own crate"
        // per import, and `mycrate::foo` matches `crate::foo::Bar` written in
        // `mycrate` with no flag. When nothing resolves a name, both stay
        // literal and segments match as before. The facts are consulted only
        // when no override is in effect.
        let auto_name = if crate_name.is_none() {
            owning_crate_name(record, path)
        } else {
            None
        };
        let effective: Option<&str> = crate_name.or(auto_name.as_deref());
        let query_norm = normalize(&query_segments, effective);
        let import_norm = normalize(&import_segments, effective);
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
    use crate::ir::{CrateAttribution, CrateAttributionStatus, Graph, stable_id};

    fn span(start_line: usize) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line,
            end_line: start_line,
            start_column: None,
            end_column: None,
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

    /// Pushes an Import node stamped with owning-package attribution (issue
    /// #117), as `eg scan` produces via `apply_crate_attribution`, so the
    /// auto-unification tests exercise the real facts rather than a mock.
    fn import_attributed(
        graph: &mut Graph,
        path: &str,
        name: &str,
        line: usize,
        package_name: &str,
        manifest: &str,
    ) -> String {
        let id = import_id(path, name);
        graph.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::Import,
                path.to_owned(),
                span(line),
                name.to_owned(),
                "rust",
                format!("Rust import {name}"),
            )
            .with_crate_attribution(CrateAttribution {
                status: CrateAttributionStatus::Attributed,
                package_name: Some(package_name.to_owned()),
                manifest_repo_relative_path: Some(manifest.to_owned()),
                unattributed_reason: None,
            }),
        );
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

    #[test]
    fn import_segments_strip_pub_use_re_export_prefix() {
        // The Rust extractor's `import_name` only trims a leading bare `use`, so
        // a public re-export keeps its visibility on the Import node `name`:
        // `pub use crate::internal::Widget;` mints the literal name
        // `pub use crate::internal::Widget`. The segment parser must drop that
        // keyword prefix or the first segment becomes `pub use crate` (#449).
        assert_eq!(
            parse_import_segments("pub use crate::internal::Widget"),
            vec!["crate", "internal", "Widget"]
        );
        assert_eq!(
            parse_import_segments("pub(crate) use crate::a::T"),
            vec!["crate", "a", "T"]
        );
        assert_eq!(
            parse_import_segments("pub(super) use super::a::T"),
            vec!["super", "a", "T"]
        );
        assert_eq!(
            parse_import_segments("pub(in crate::foo) use crate::a::T"),
            vec!["crate", "a", "T"]
        );
        // A bare leading `use ` keyword (robustness) is also stripped.
        assert_eq!(
            parse_import_segments("use crate::a::T"),
            vec!["crate", "a", "T"]
        );
        // Plain already-stripped paths are unaffected — `pub`/`use` are reserved
        // words, so a real segment can never begin with them, and a segment that
        // merely starts with the substring `use` (e.g. `used`) is not stripped.
        assert_eq!(
            parse_import_segments("crate::a::T"),
            vec!["crate", "a", "T"]
        );
        assert_eq!(
            parse_import_segments("used::helpers::X"),
            vec!["used", "helpers", "X"]
        );
    }

    #[test]
    fn pub_use_re_export_site_matches_prefix_query() {
        // A public re-export whose extractor name retains `pub use` must still be
        // found by a `crate::internal` query (#449, finding 1).
        let mut g = Graph::new();
        let id = import(&mut g, "src/lib.rs", "pub use crate::internal::Widget", 1);
        let recs = g.into_records();
        let result = run(&recs, "crate::internal");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].record_id, id);
    }

    #[test]
    fn crate_flag_unifies_pub_use_re_export() {
        // The `--crate` unification must also reach a `pub use crate::…`
        // re-export site once the keyword prefix is stripped (#449, finding 1).
        let mut g = Graph::new();
        import(&mut g, "src/lib.rs", "pub use crate::widget::Thing", 1);
        let recs = g.into_records();
        let index = RepositoryIndex::build(&recs);
        let unified = who_imports(&recs, "mycrate::widget", Some("mycrate"), &index, None).unwrap();
        assert_eq!(unified.rows.len(), 1);
        assert_eq!(unified.rows[0].import_path, "pub use crate::widget::Thing");
    }

    // ── scan-history union semantics ─────────────────────────────────────
    //
    // A `scan-history` graph is the UNION of every commit snapshot: history
    // replay stamps per-commit Import records but does NOT tombstone an import
    // removed in a later commit. This lane has no `--at`/`--as-of` and no
    // HEAD-only filter, so an import present only in an early commit is STILL
    // returned by an unpinned query — mirroring `deps`/`path` (#449, finding 2).
    #[test]
    fn history_union_returns_import_removed_in_a_later_commit() {
        let mut g = Graph::new();
        // Import present in an early commit; a later commit dropped it WITHOUT a
        // tombstone (history replay never tombstones removed imports). The
        // un-tombstoned early-commit record persists in the union JSONL.
        let id = import(&mut g, "src/old.rs", "foo::bar::Legacy", 1);
        let recs = g.into_records();
        let result = run(&recs, "foo::bar");
        assert_eq!(
            result.rows.len(),
            1,
            "unpinned who-imports over a history union still returns a later-removed import"
        );
        assert_eq!(result.rows[0].record_id, id);
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

    // ── `crate::` auto-unification (issue #450) ──────────────────────────
    //
    // A leading `crate::` resolves to the importing file's owning crate from
    // the facts the graph already carries — no `--crate` flag needed.

    #[test]
    fn auto_unify_attribution_resolves_crate_without_flag() {
        // Workspace member `crates/foo`: attribution (from `eg scan`'s nearest
        // enclosing `Cargo.toml` walk) names the true crate.
        let mut g = Graph::new();
        let id = import_attributed(
            &mut g,
            "crates/foo/src/x.rs",
            "crate::bar::Baz",
            1,
            "foo",
            "crates/foo/Cargo.toml",
        );
        import_attributed(
            &mut g,
            "crates/foo/src/y.rs",
            "foo::bar::Qux",
            2,
            "foo",
            "crates/foo/Cargo.toml",
        );
        let recs = g.into_records();

        // An absolute query unifies with the `crate::` import — no flag.
        let result = run(&recs, "foo::bar");
        assert_eq!(result.rows.len(), 2);
        assert!(result.rows.iter().any(|r| r.record_id == id));

        // A `crate::`-leading query means "the importer's own crate", resolved
        // per import.
        let result = run(&recs, "crate::bar");
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn auto_unify_prefers_attribution_over_directory_name() {
        // The package was renamed relative to its directory: the manifest's
        // `[package] name` is the true crate name, the directory is not.
        let mut g = Graph::new();
        import_attributed(
            &mut g,
            "crates/foo-bar/src/x.rs",
            "crate::q::W",
            1,
            "renamed",
            "crates/foo-bar/Cargo.toml",
        );
        let recs = g.into_records();

        assert_eq!(run(&recs, "renamed::q").rows.len(), 1);
        // The directory-derived `foo_bar` is NOT the crate — no false match.
        assert!(run(&recs, "foo_bar::q").is_empty());
    }

    #[test]
    fn auto_unify_path_derived_name_without_attribution() {
        // Graphs whose records predate attribution still unify via the #440
        // workspace-prefix-derived name.
        let mut g = Graph::new();
        import(&mut g, "crates/other/src/y.rs", "crate::q::W", 1);
        // The reverse direction unifies too: an absolute import of the member
        // matches a `crate::` query resolved to that member.
        import(&mut g, "crates/other/src/z.rs", "other::q::V", 2);
        let recs = g.into_records();

        // Both directions unify: the absolute query finds the `crate::`-written
        // import (resolved to `other::q::W`), and the `crate::` query finds
        // both rows.
        assert_eq!(run(&recs, "other::q").rows.len(), 2);
        assert_eq!(run(&recs, "crate::q").rows.len(), 2);
    }

    #[test]
    fn auto_unify_aux_target_uses_target_name_not_package() {
        // `examples/demo.rs` compiles as its OWN crate named `demo` — `crate::`
        // there denotes the `demo` crate, so the owning package name `mypkg`
        // would be the wrong resolution.
        let mut g = Graph::new();
        import_attributed(
            &mut g,
            "examples/demo.rs",
            "crate::util::H",
            1,
            "mypkg",
            "Cargo.toml",
        );
        import_attributed(
            &mut g,
            "src/bin/tool.rs",
            "crate::cli::X",
            2,
            "mypkg",
            "Cargo.toml",
        );
        let recs = g.into_records();

        assert_eq!(run(&recs, "demo::util").rows.len(), 1);
        assert_eq!(run(&recs, "tool::cli").rows.len(), 1);
        // The package name must NOT unify with these aux-target `crate::`s.
        assert!(run(&recs, "mypkg::util").is_empty());
        assert!(run(&recs, "mypkg::cli").is_empty());
    }

    #[test]
    fn auto_unify_single_crate_from_package_attribution() {
        // A single-crate `src/…` layout has no workspace prefix for the #440
        // name inference, but the root manifest's package name still resolves
        // `crate::`.
        let mut g = Graph::new();
        import_attributed(&mut g, "src/lib.rs", "crate::m::N", 1, "solo", "Cargo.toml");
        let recs = g.into_records();

        assert_eq!(run(&recs, "solo::m").rows.len(), 1);
        assert_eq!(run(&recs, "crate::m").rows.len(), 1);
    }

    #[test]
    fn auto_unify_keeps_crates_apart() {
        // Per-import resolution must not pool same-named modules across crates.
        let mut g = Graph::new();
        let a_id = import_attributed(
            &mut g,
            "crates/a/src/x.rs",
            "crate::shared::A",
            1,
            "a",
            "crates/a/Cargo.toml",
        );
        import_attributed(
            &mut g,
            "crates/b/src/y.rs",
            "crate::shared::B",
            2,
            "b",
            "crates/b/Cargo.toml",
        );
        let recs = g.into_records();

        let result = run(&recs, "a::shared");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].record_id, a_id);

        // `crate::shared` means each importer's own crate: both match.
        assert_eq!(run(&recs, "crate::shared").rows.len(), 2);
    }

    #[test]
    fn auto_unify_rejects_forged_attribution_fail_closed() {
        // A record citing a manifest that does not enclose its path is a shape
        // the resolver never produces (#104 doctrine): the attribution is
        // ignored and resolution falls back to the #440 path-derived name.
        let mut g = Graph::new();
        import_attributed(
            &mut g,
            "crates/a/src/x.rs",
            "crate::q::W",
            1,
            "evil",
            "crates/b/Cargo.toml",
        );
        let recs = g.into_records();

        assert!(run(&recs, "evil::q").is_empty());
        assert_eq!(run(&recs, "a::q").rows.len(), 1);
    }

    #[test]
    fn crate_override_wins_over_auto_facts() {
        // `--crate` stays caller-supplied ground truth: it overrides the facts
        // for both the query and every import.
        let mut g = Graph::new();
        import_attributed(
            &mut g,
            "crates/foo/src/x.rs",
            "crate::bar::Baz",
            1,
            "foo",
            "crates/foo/Cargo.toml",
        );
        let recs = g.into_records();
        let index = RepositoryIndex::build(&recs);

        let forced = who_imports(&recs, "forced::bar", Some("forced"), &index, None).unwrap();
        assert_eq!(forced.rows.len(), 1);
        assert_eq!(forced.crate_name.as_deref(), Some("forced"));

        let crate_query = who_imports(&recs, "crate::bar", Some("forced"), &index, None).unwrap();
        assert_eq!(crate_query.rows.len(), 1);
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

    // ── --repo scoping ───────────────────────────────────────────────────

    /// Two repositories in one store, each importing `foo::bar`. `--repo`
    /// must restrict the importer set to exactly the selected repository
    /// (issue #444: "honoring --repo").
    #[test]
    fn repo_scope_restricts_to_one_repository() {
        use crate::ir::EdgeLabel;

        let mut g = Graph::new();
        let repo_a = "codegraph:v1:repo-a";
        let repo_b = "codegraph:v1:repo-b";
        let mut import_ids = Vec::new();
        for (repo_id, basename, file_path, import_name) in [
            (repo_a, "alpha", "src/a.rs", "foo::bar::A"),
            (repo_b, "beta", "src/b.rs", "foo::bar::B"),
        ] {
            g.push(GraphRecord::node(
                repo_id.to_owned(),
                NodeKind::Repository,
                None,
                None,
                Some(basename.to_owned()),
                format!("repository {basename}"),
            ));
            let file_id = stable_id(&["node", "file", repo_id, file_path]);
            g.push(GraphRecord::node(
                file_id.clone(),
                NodeKind::File,
                Some(file_path.to_owned()),
                None,
                Some(file_path.to_owned()),
                format!("file {file_path}"),
            ));
            g.push(GraphRecord::edge(
                EdgeLabel::Contains,
                repo_id.to_owned(),
                file_id.clone(),
                None,
                format!("{repo_id} contains {file_path}"),
            ));
            let import_id = stable_id(&["node", "import", repo_id, file_path, import_name]);
            g.push(GraphRecord::syntax_node(
                import_id.clone(),
                NodeKind::Import,
                file_path.to_owned(),
                span(1),
                import_name.to_owned(),
                "rust",
                format!("Rust import {import_name}"),
            ));
            // The extractor's containment-shaped edge attributes the import
            // declaration to its file (and hence its repository).
            g.push(GraphRecord::edge(
                EdgeLabel::Imports,
                file_id,
                import_id.clone(),
                None,
                format!("{file_path} imports {import_name}"),
            ));
            import_ids.push(import_id);
        }
        let recs = g.into_records();
        let index = RepositoryIndex::build(&recs);

        // Sanity: each Import node is owned by its own repository.
        assert_eq!(index.owner_of(&import_ids[0]), Some(repo_a));
        assert_eq!(index.owner_of(&import_ids[1]), Some(repo_b));

        let scoped =
            who_imports(&recs, "foo::bar", None, &index, Some(repo_a)).expect("valid query");
        assert_eq!(
            scoped.rows.len(),
            1,
            "--repo must restrict importers to the selected repository"
        );
        assert_eq!(scoped.rows[0].record_id, import_ids[0]);
        assert_eq!(scoped.rows[0].repo_relative_path, "src/a.rs");

        let unscoped = who_imports(&recs, "foo::bar", None, &index, None).expect("valid query");
        assert_eq!(unscoped.rows.len(), 2, "no --repo keeps both repositories");
    }
}
