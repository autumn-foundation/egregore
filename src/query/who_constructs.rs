//! `eg query who-constructs <Type>` — struct-literal construction-site lookup
//! over the `CONSTRUCTS` edges the Rust extractor mints (issue #471).
//!
//! # SPEC
//!
//! Answer the question "who builds this type?" — a precise, read-only lookup
//! over the [`EdgeLabel::Constructs`] edges PR #467 (issue #443) landed. Each
//! `CONSTRUCTS` edge runs from a CONSTRUCTING symbol (the function/method whose
//! body writes a `Type { … }` literal) to the struct/enum DEFINITION symbol, and
//! carries an `is_exhaustive` marker (OR-aggregated across the sites that
//! collapse to one edge): `true` when at least one collapsed literal uses the
//! exhaustive, non-`..base` form (which fails to compile — rustc E0063 — when a
//! required field is added), `false` when every site used struct-update `..base`
//! (functional-update, FRU) which stays valid across a field addition.
//!
//! This lane is the inbound, type-anchored mirror of the outbound
//! [`super::deps`] lane and the symmetric partner to [`super::who_imports`]. It
//! is the first-class, directly addressable form of the `construction_sites`
//! group that `eg query change-impact <type>` already surfaces (the interim
//! answer shipped in #467): both read the same `CONSTRUCTS` edges and both
//! expose the same `e0063_risk` signal, but this lane makes "who constructs
//! this type?" a dedicated question rather than a slice of a broader
//! blast-radius envelope.
//!
//! ## What a row is
//!
//! For the resolved anchor type, every live inbound `CONSTRUCTS` edge yields one
//! row citing the CONSTRUCTING symbol's stable `record_id`, name, node kind, and
//! repo-relative file/span handle, plus:
//!
//! * `e0063_risk` — the actionable flag: `true` when a new required field on the
//!   type would break this construction site (the exhaustive form), `false` when
//!   the site's `..base` keeps it valid. Derived as `is_exhaustive.unwrap_or(true)`
//!   — a missing marker (legacy edge) is conservatively treated as risky.
//! * `is_exhaustive` — the raw edge marker as recorded, `None` on a legacy edge.
//!
//! Construction is a caller-granularity relation: per-site spans collapse to the
//! constructing symbol, exactly as `CALLS` does. A row is a construction-site
//! LEAD, never proof that a specific field addition breaks — the `e0063_risk`
//! flag is the actionable signal.
//!
//! ## Liveness
//!
//! Liveness follows the shared latest-write-wins [`Liveness`] gate: a
//! constructing symbol re-added after its own tombstone is live again, a
//! tombstone with no later re-add excludes it, and only the latest write of a
//! stable `CONSTRUCTS` edge id supplies the row — so `--graph` and `--data-dir`
//! agree on tombstoned / revived construction sites (issue #421).

use std::collections::BTreeMap;

use super::liveness::Liveness;
use crate::ir::{EdgeLabel, GraphRecord, SourceSpan};

/// One constructing-symbol row returned by [`who_constructs`].
///
/// Every row carries the citation handles the code trust class requires: a
/// stable `record_id` plus the repo-relative file path and, when recorded, the
/// source span of the constructing symbol.
#[derive(Debug, Clone, Copy)]
pub struct WhoConstructsRow<'a> {
    /// Stable record ID of the constructing symbol (the `CONSTRUCTS` source).
    pub record_id: &'a str,
    /// Record schema version of the constructing symbol.
    pub schema_version: u32,
    /// Name of the constructing symbol, when recorded.
    pub name: Option<&'a str>,
    /// Node kind of the constructing symbol (wire string, e.g. `Symbol`).
    pub kind: &'static str,
    /// Repo-relative path of the constructing symbol's file.
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the constructing symbol, when recorded.
    pub span: Option<SourceSpan>,
    /// Stable record ID of the `CONSTRUCTS` edge.
    pub edge_id: &'a str,
    /// Raw exhaustiveness marker carried by the edge (issue #443), `None` on a
    /// legacy edge that predates the marker.
    pub is_exhaustive: Option<bool>,
    /// E0063 blast-radius flag: `true` when adding a required field to the
    /// anchor type would break this site (the exhaustive, non-`..base` form).
    /// `is_exhaustive.unwrap_or(true)` — a missing marker is conservatively risky.
    pub e0063_risk: bool,
}

/// The construction-site set for one resolved anchor type.
#[derive(Debug)]
pub struct WhoConstructsResult<'a> {
    /// The resolved anchor type's node record (the `CONSTRUCTS` target).
    pub anchor: &'a GraphRecord,
    /// Constructing-symbol rows, sorted by
    /// (`repo_relative_path`, `span.start_line`, `record_id`).
    pub rows: Vec<WhoConstructsRow<'a>>,
}

impl WhoConstructsResult<'_> {
    /// Returns `true` when the resolved type has zero live constructors.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Collects every constructing symbol that builds a `Type { … }` literal of the
/// resolved anchor type (issue #471) — the live inbound `CONSTRUCTS` edges.
///
/// `anchor_id` names the struct/enum DEFINITION symbol (a `CONSTRUCTS` target).
/// Returns `None` when `anchor_id` names no live node in `records`. Duplicate
/// discoveries from history views (the same stable edge id recurring across
/// commit snapshots) are collapsed, and rows are canonically ordered, so output
/// over an unchanged store is byte-identical across runs.
///
/// Liveness is latest-write-wins: a constructing symbol re-added after its own
/// tombstone is live, and only the latest write of a stable `CONSTRUCTS` edge id
/// supplies a row — matching the embedded current-state read so `--graph` and
/// `--data-dir` agree (issue #421).
#[must_use]
pub fn who_constructs<'a>(
    records: &'a [GraphRecord],
    anchor_id: &str,
) -> Option<WhoConstructsResult<'a>> {
    let liveness = Liveness::new(records);
    let deleted = |id: &str| liveness.deleted(id);
    let by_id: BTreeMap<&str, &GraphRecord> = records
        .iter()
        .filter_map(|r| {
            let id = r.id();
            if deleted(id) { None } else { Some((id, r)) }
        })
        .collect();

    let anchor = by_id.get(anchor_id).copied()?;
    let anchor_id: &str = anchor.id();

    // Keyed map collapses duplicate (source, edge) discoveries from history
    // views where the same stable edge id recurs across commit snapshots, and
    // gives a stable intermediate order for free.
    let mut sites: BTreeMap<(&str, &str), WhoConstructsRow<'a>> = BTreeMap::new();

    for (index, r) in records.iter().enumerate() {
        let GraphRecord::Edge {
            id: edge_id,
            label: EdgeLabel::Constructs,
            source,
            target,
            is_exhaustive,
            ..
        } = r
        else {
            continue;
        };
        // Latest-write-wins for edge metadata: over an append-only `--graph` a
        // stable edge id may be re-ingested with a changed `is_exhaustive`
        // marker. Only the latest EDGE write for the id is live, mirroring
        // embedded `latest_edge_versions`.
        if !liveness.is_latest_edge_version(edge_id.as_str(), index) {
            continue;
        }
        if target != anchor_id || deleted(edge_id.as_str()) {
            continue;
        }
        // The constructing symbol must resolve to a live node; a dangling source
        // (tombstoned or absent) mints no row — there is nothing citable.
        let Some(node) = by_id.get(source.as_str()).copied() else {
            continue;
        };
        let GraphRecord::Node {
            id,
            kind,
            schema_version,
            name,
            repo_relative_path,
            span,
            ..
        } = node
        else {
            continue;
        };
        sites
            .entry((id.as_str(), edge_id.as_str()))
            .or_insert_with(|| WhoConstructsRow {
                record_id: id,
                schema_version: *schema_version,
                name: name.as_deref(),
                kind: kind.as_str(),
                repo_relative_path: repo_relative_path.as_deref(),
                span: *span,
                edge_id: edge_id.as_str(),
                is_exhaustive: *is_exhaustive,
                e0063_risk: is_exhaustive.unwrap_or(true),
            });
    }

    let mut rows: Vec<WhoConstructsRow<'a>> = sites.into_values().collect();
    rows.sort_by(|a, b| {
        a.repo_relative_path
            .cmp(&b.repo_relative_path)
            .then_with(|| {
                a.span
                    .map(|s| s.start_line)
                    .cmp(&b.span.map(|s| s.start_line))
            })
            .then_with(|| a.record_id.cmp(b.record_id))
    });

    Some(WhoConstructsResult { anchor, rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Graph, NodeKind, stable_id};

    fn span(start_line: usize) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line,
            end_line: start_line,
        }
    }

    fn sym_id(path: &str, name: &str) -> String {
        stable_id(&["node", "Symbol", path, name])
    }

    /// Pushes a Symbol node and returns its stable id.
    fn symbol(graph: &mut Graph, path: &str, name: &str, line: usize) -> String {
        let id = sym_id(path, name);
        graph.push(GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Symbol,
            path.to_owned(),
            span(line),
            name.to_owned(),
            "rust",
            format!("fn {name}"),
        ));
        id
    }

    /// Pushes a `CONSTRUCTS` edge (constructor → type) carrying the given
    /// exhaustiveness marker, returning its stable edge id.
    fn constructs(
        graph: &mut Graph,
        constructor: &str,
        type_def: &str,
        exhaustive: bool,
    ) -> String {
        let edge = GraphRecord::edge(
            EdgeLabel::Constructs,
            constructor.to_owned(),
            type_def.to_owned(),
            None,
            "constructs".to_owned(),
        )
        .with_construct_exhaustive(exhaustive);
        let id = edge.id().to_owned();
        graph.push(edge);
        id
    }

    fn tombstone(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v7:tomb_{deleted_id}"),
            schema_version: 7,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    fn run<'a>(records: &'a [GraphRecord], anchor: &str) -> WhoConstructsResult<'a> {
        who_constructs(records, anchor).expect("anchor resolves")
    }

    // ── basic resolution ─────────────────────────────────────────────────

    #[test]
    fn unknown_anchor_returns_none() {
        let records: Vec<GraphRecord> = Vec::new();
        assert!(who_constructs(&records, "codegraph:v7:missing").is_none());
    }

    #[test]
    fn resolved_type_with_zero_constructors_is_empty() {
        // The type exists but nothing constructs it → empty (the CLI maps this
        // to no_match, exit 2).
        let mut g = Graph::new();
        let ty = symbol(&mut g, "src/model.rs", "Deal", 1);
        let recs = g.into_records();
        let result = run(&recs, &ty);
        assert!(result.is_empty());
    }

    // ── cross-file & cross-crate construction ────────────────────────────

    #[test]
    fn cross_file_constructor_is_reported() {
        // A constructor in a different file from the type definition still
        // yields a row carrying the constructor's own file/span handle.
        let mut g = Graph::new();
        let ty = symbol(&mut g, "src/model.rs", "Deal", 1);
        let ctor = symbol(&mut g, "src/handlers.rs", "make_deal", 10);
        constructs(&mut g, &ctor, &ty, true);
        let recs = g.into_records();
        let result = run(&recs, &ty);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].record_id, ctor);
        assert_eq!(result.rows[0].repo_relative_path, Some("src/handlers.rs"));
        assert_eq!(result.rows[0].span.map(|s| s.start_line), Some(10));
    }

    #[test]
    fn cross_crate_constructors_each_get_a_row() {
        // Two constructors in different crates (different path prefixes) build
        // the same type; both are reported.
        let mut g = Graph::new();
        let ty = symbol(&mut g, "crates/core/src/model.rs", "Deal", 1);
        let ctor_a = symbol(&mut g, "crates/api/src/build.rs", "build_a", 5);
        let ctor_b = symbol(&mut g, "crates/cli/src/build.rs", "build_b", 7);
        constructs(&mut g, &ctor_a, &ty, true);
        constructs(&mut g, &ctor_b, &ty, false);
        let recs = g.into_records();
        let result = run(&recs, &ty);
        assert_eq!(result.rows.len(), 2);
        let ids: Vec<&str> = result.rows.iter().map(|r| r.record_id).collect();
        assert!(ids.contains(&ctor_a.as_str()));
        assert!(ids.contains(&ctor_b.as_str()));
    }

    // ── e0063_risk / is_exhaustive ───────────────────────────────────────

    #[test]
    fn exhaustive_site_is_e0063_risky_fru_site_is_not() {
        let mut g = Graph::new();
        let ty = symbol(&mut g, "src/model.rs", "Deal", 1);
        let exhaustive_ctor = symbol(&mut g, "src/a.rs", "exhaustive_build", 3);
        let fru_ctor = symbol(&mut g, "src/b.rs", "fru_build", 4);
        constructs(&mut g, &exhaustive_ctor, &ty, true);
        constructs(&mut g, &fru_ctor, &ty, false);
        let recs = g.into_records();
        let result = run(&recs, &ty);
        let exhaustive = result
            .rows
            .iter()
            .find(|r| r.record_id == exhaustive_ctor)
            .expect("exhaustive row");
        assert_eq!(exhaustive.is_exhaustive, Some(true));
        assert!(exhaustive.e0063_risk, "exhaustive literal breaks E0063");
        let fru = result
            .rows
            .iter()
            .find(|r| r.record_id == fru_ctor)
            .expect("fru row");
        assert_eq!(fru.is_exhaustive, Some(false));
        assert!(!fru.e0063_risk, "..base FRU site stays valid");
    }

    #[test]
    fn missing_marker_defaults_to_risky() {
        // A legacy CONSTRUCTS edge with no exhaustiveness marker is
        // conservatively treated as E0063-risky.
        let mut g = Graph::new();
        let ty = symbol(&mut g, "src/model.rs", "Deal", 1);
        let ctor = symbol(&mut g, "src/a.rs", "legacy_build", 3);
        // Edge WITHOUT `with_construct_exhaustive`.
        g.push(GraphRecord::edge(
            EdgeLabel::Constructs,
            ctor,
            ty.clone(),
            None,
            "constructs".to_owned(),
        ));
        let recs = g.into_records();
        let result = run(&recs, &ty);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].is_exhaustive, None);
        assert!(
            result.rows[0].e0063_risk,
            "absent marker is conservatively risky"
        );
    }

    // ── liveness ─────────────────────────────────────────────────────────

    #[test]
    fn tombstoned_constructor_is_excluded() {
        let mut g = Graph::new();
        let ty = symbol(&mut g, "src/model.rs", "Deal", 1);
        let ctor = symbol(&mut g, "src/a.rs", "make", 3);
        constructs(&mut g, &ctor, &ty, true);
        g.push(tombstone(&ctor));
        let recs = g.into_records();
        assert!(run(&recs, &ty).is_empty());
    }

    #[test]
    fn tombstoned_then_revived_constructor_is_included() {
        // Latest-write-wins: node, tombstone, node again → the constructor is
        // live and its row reappears.
        let mut g = Graph::new();
        let ty = symbol(&mut g, "src/model.rs", "Deal", 1);
        let ctor = symbol(&mut g, "src/a.rs", "make", 3);
        constructs(&mut g, &ctor, &ty, true);
        g.push(tombstone(&ctor));
        symbol(&mut g, "src/a.rs", "make", 3);
        let recs = g.into_records();
        let result = run(&recs, &ty);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].record_id, ctor);
    }

    #[test]
    fn tombstoned_edge_without_reingest_drops_the_site() {
        let mut g = Graph::new();
        let ty = symbol(&mut g, "src/model.rs", "Deal", 1);
        let ctor = symbol(&mut g, "src/a.rs", "make", 3);
        let edge_id = constructs(&mut g, &ctor, &ty, true);
        g.push(tombstone(&edge_id));
        let recs = g.into_records();
        assert!(run(&recs, &ty).is_empty());
    }

    #[test]
    fn latest_edge_version_supplies_exhaustiveness() {
        // Two versions of one stable CONSTRUCTS edge id: v1 non-exhaustive,
        // v2 exhaustive. Only the latest EDGE write supplies the row.
        let mut g = Graph::new();
        let ty = symbol(&mut g, "src/model.rs", "Deal", 1);
        let ctor = symbol(&mut g, "src/a.rs", "make", 3);
        constructs(&mut g, &ctor, &ty, false);
        constructs(&mut g, &ctor, &ty, true);
        let recs = g.into_records();
        let result = run(&recs, &ty);
        assert_eq!(result.rows.len(), 1, "one edge id, not two rows");
        assert_eq!(result.rows[0].is_exhaustive, Some(true));
        assert!(result.rows[0].e0063_risk);
    }

    // ── determinism ──────────────────────────────────────────────────────

    #[test]
    fn rows_sorted_by_path_then_line_then_id() {
        let mut g = Graph::new();
        let ty = symbol(&mut g, "src/model.rs", "Deal", 1);
        let c_z = symbol(&mut g, "src/z.rs", "z_make", 3);
        let c_a2 = symbol(&mut g, "src/a.rs", "a_make_hi", 9);
        let c_a1 = symbol(&mut g, "src/a.rs", "a_make_lo", 2);
        constructs(&mut g, &c_z, &ty, true);
        constructs(&mut g, &c_a2, &ty, true);
        constructs(&mut g, &c_a1, &ty, true);
        let recs = g.into_records();
        let result = run(&recs, &ty);
        let ordered: Vec<(&str, usize)> = result
            .rows
            .iter()
            .map(|r| {
                (
                    r.repo_relative_path.unwrap_or(""),
                    r.span.map_or(0, |s| s.start_line),
                )
            })
            .collect();
        assert_eq!(
            ordered,
            vec![("src/a.rs", 2), ("src/a.rs", 9), ("src/z.rs", 3)]
        );
    }

    #[test]
    fn output_is_byte_identical_across_runs() {
        let mut g = Graph::new();
        let ty = symbol(&mut g, "src/model.rs", "Deal", 1);
        let c1 = symbol(&mut g, "src/a.rs", "one", 4);
        let c2 = symbol(&mut g, "src/b.rs", "two", 8);
        constructs(&mut g, &c1, &ty, true);
        constructs(&mut g, &c2, &ty, false);
        let recs = g.into_records();
        let first: Vec<(String, bool)> = run(&recs, &ty)
            .rows
            .iter()
            .map(|r| (r.record_id.to_owned(), r.e0063_risk))
            .collect();
        let second: Vec<(String, bool)> = run(&recs, &ty)
            .rows
            .iter()
            .map(|r| (r.record_id.to_owned(), r.e0063_risk))
            .collect();
        assert_eq!(first, second);
    }
}
