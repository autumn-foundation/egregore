use std::collections::{BTreeMap, BTreeSet};

use super::RepositoryIndex;
use super::liveness::Liveness;
use crate::ir::{CallResolution, EdgeLabel, GraphRecord, NodeKind, SourceSpan};

// ---------------------------------------------------------------------------
// dead-code triage candidates (issue #240)
// ---------------------------------------------------------------------------

/// Maximum `--limit` for the dead-code lane, matching the sibling
/// bounded-list lanes.
pub const DEAD_CODE_MAX_LIMIT: usize = 500;

/// Default `--limit` for the dead-code lane.
pub const DEAD_CODE_DEFAULT_LIMIT: usize = 100;

/// Soundness boundary (issue #240 AC5).
///
/// Every row is a *candidate*, never proof. The graph cannot observe dynamic
/// dispatch through trait objects, macro-generated callers, reflection-like
/// usage, FFI consumers, or cross-crate consumers — a candidate may be live
/// through any of those.
pub const DEAD_CODE_SOUNDNESS: &str = "Candidates are suspected dead code, not proven-dead. \
     The graph cannot observe dynamic dispatch via trait objects, macro-generated \
     callers, reflection-like usage, FFI consumers, or cross-crate consumers of \
     this crate's items; any of those can keep a listed symbol live without a \
     recorded in-graph CALLS edge. Deletion stays a human/agent decision.";

/// One dead-code triage candidate row.
#[derive(Debug, Clone)]
pub struct DeadCodeCandidate<'a> {
    /// Stable record ID of the `Symbol` node — the citable handle.
    pub record_id: &'a str,
    /// Record schema version.
    pub schema_version: u32,
    /// Symbol name (qualified where the extractor qualifies it).
    pub name: &'a str,
    /// Language-specific symbol kind (`function`, `struct`, …).
    pub kind: &'a str,
    /// Repo-relative file of the declaration.
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the declaration.
    pub span: Option<SourceSpan>,
    /// Introducing commit for temporal (history-backed) records.
    pub git_commit: Option<&'a str>,
    /// Recorded direct incoming `CALLS` edges (`resolved`/`ambiguous`).
    pub incoming_calls: usize,
    /// `true` when `incoming_calls > 0` and every recorded direct caller is
    /// itself a reported candidate (the one-hop dead-cluster rule, AC6).
    pub callers_all_candidates: bool,
    /// Always `false` on a candidate row: the asserted exclusion from the
    /// issue #213 public surface.
    pub on_public_surface: bool,
    /// Row verdict — always `candidate`, never a dead-code proof.
    pub classification: &'static str,
}

/// Deterministic tallies for the dead-code result.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct DeadCodeCounts {
    /// Live, in-scope `Symbol` records considered (impl blocks excluded).
    pub symbols_considered: usize,
    /// Considered symbols with at least one recorded inbound `CALLS` edge.
    pub referenced: usize,
    /// Candidate rows before `--limit` truncation.
    pub candidates: usize,
    /// Candidate rows returned after `--limit` truncation.
    pub candidates_returned: usize,
    /// Zero-incoming symbols excluded as recognized non-call entry points.
    pub entry_points_excluded: usize,
    /// Zero-incoming, non-entry-point symbols excluded as externally
    /// reachable (issue #213 public surface).
    pub public_surface_excluded: usize,
}

/// A stable machine-readable condition attached to the result.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeadCodeDiagnostic {
    /// Stable diagnostic code (`no_candidates`, `no_symbols`,
    /// `unresolved_call_edges_present`).
    pub code: &'static str,
    /// Record the diagnostic is about, when one exists.
    pub record_id: Option<String>,
    /// Bounded human-readable detail (counts only — never payload).
    pub detail: String,
}

/// The dead-code candidate set plus tallies and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct DeadCodeResult<'a> {
    /// Candidates sorted by (`repo_relative_path`, `span.start_line`,
    /// `record_id`) — the documented deterministic ordering — truncated to
    /// `--limit`.
    pub candidates: Vec<DeadCodeCandidate<'a>>,
    /// Deterministic tallies (totals, unaffected by `--limit`).
    pub counts: DeadCodeCounts,
    /// Stable diagnostics, sorted and de-duplicated.
    pub diagnostics: Vec<DeadCodeDiagnostic>,
}

/// Selects dead-code triage candidates: indexed symbols with no live
/// in-graph callers (issue #240).
///
/// A live `Symbol` record is a candidate when either
/// (a) it has **zero recorded direct incoming `CALLS` edges**, or
/// (b) every recorded direct caller is itself a candidate by rule (a) —
/// the one-hop dead-cluster rule (AC6: `a` called only by `b`, `b`
/// unreferenced ⇒ both reported). Rule (b) is deliberately NOT recursive:
/// a three-deep chain `c → b → a` reports `c` and `b` but not `a`. The lane
/// keys on direct incoming edges; transitive / whole-program reachability
/// pruning from public roots (mark-and-sweep) is out of scope.
///
/// Excluded from candidacy (counted, never silently dropped):
/// - recognized non-call entry points: the extractor's `entry_point` mark
///   (`#[test]`/`#[bench]`, `#[no_mangle]`/`#[export_name]`, binary-crate
///   `fn main`), plus `symbol_kind == "test"` for pre-#240 graphs;
/// - externally reachable symbols under the issue #213 public-surface rule
///   (top-level `pub`, visibility-widening `pub use` re-exports), consumed
///   via [`super::public_api_surface`] — never redefined here.
///
/// `impl`-block symbols are excluded from the population (unnameable
/// declaration details, as in the sibling unreferenced lane). Ambiguous
/// call edges count as references — a symbol that *might* be called is
/// never reported. Unresolved call edges cannot name a target, so they
/// never mark a symbol referenced; they are tallied honestly in
/// `unresolved_call_edges_present`.
///
/// Entry-point and public-surface exclusions also gate rule (b): a symbol
/// called only by `fn main`, a `#[test]`, or a `pub` entry point is live,
/// because its caller is a real (if unrecorded-caller) entry into the code.
///
/// Current-state view (tombstones, latest-write-wins, HEAD anchoring) and
/// deterministic ordering mirror the sibling unreferenced lane. Strictly
/// read-only. `limit == 0` returns no rows; totals in `counts` are
/// unaffected by `limit`.
#[must_use]
pub fn dead_code_candidates<'a>(
    records: &'a [GraphRecord],
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
    limit: usize,
) -> DeadCodeResult<'a> {
    // Latest-write-wins liveness (issues #421/#432) and the shared
    // head-anchor gate (issues #82/#427): same current-state view as the
    // sibling unreferenced lane.
    let liveness = Liveness::new(records);
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .filter(|&id| liveness.deleted(id))
        .collect();
    let is_owned =
        |id: &str| -> bool { repo_scope.is_none_or(|scope| index.owner_of(id) == Some(scope)) };
    let non_head_current = super::non_head_current_record_ids(records, index);

    // Externally reachable symbols under the issue #213 rule (top-level
    // `pub`, visibility-widening `pub use` re-exports). Both the declaring
    // row and the resolved re-export target exclude the symbol.
    let surface_ids: BTreeSet<&str> = {
        let surface = super::public_api_surface(records, index, repo_scope);
        let mut ids = BTreeSet::new();
        for item in &surface.items {
            ids.insert(item.record_id);
            if let Some(target) = item.target_record_id {
                ids.insert(target);
            }
        }
        ids
    };

    // Candidate population: live, in-scope Symbol nodes, keep-last dedupe by
    // stable ID so history graphs resolve to their newest version.
    let mut symbols: BTreeMap<&str, &'a GraphRecord> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            symbol_kind,
            ..
        } = record
        else {
            continue;
        };
        if !matches!(kind, NodeKind::Symbol) {
            continue;
        }
        if tombstoned.contains(id.as_str()) {
            continue;
        }
        if !is_owned(id) || non_head_current.contains(id.as_str()) {
            continue;
        }
        // impl blocks are unnameable declaration details, never triage
        // candidates; their methods are considered individually.
        if symbol_kind.as_deref() == Some("impl") {
            continue;
        }
        let replace = symbols.get(id.as_str()).is_none_or(|existing| {
            super::file_at_point::version_recency_key(record)
                >= super::file_at_point::version_recency_key(existing)
        });
        if replace {
            symbols.insert(id.as_str(), record);
        }
    }

    // Recognized non-call entry points: the extractor's `entry_point` mark,
    // plus `symbol_kind == "test"` so pre-#240 graphs (which record #[test]
    // fns by kind only) get the same exclusion.
    let is_entry_point = |record: &GraphRecord| -> bool {
        if record.entry_point().is_some() {
            return true;
        }
        matches!(
            record,
            GraphRecord::Node {
                symbol_kind: Some(kind),
                ..
            } if kind == "test"
        )
    };

    // Inbound CALLS counting over live, current-state edges. Only
    // `resolved`/`ambiguous` edges can name a target: unresolved edges (and
    // unresolved-dispatch boundary edges, issue #267) target a Diagnostic
    // marker, so they tally a diagnostic instead of marking references.
    let mut callers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut unresolved_call_edges = 0usize;
    for (edge_index, record) in records.iter().enumerate() {
        let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            resolution,
            ..
        } = record
        else {
            continue;
        };
        if !liveness.is_latest_edge_version(id.as_str(), edge_index) {
            continue;
        }
        if tombstoned.contains(id.as_str()) {
            continue;
        }
        if *label != EdgeLabel::Calls {
            continue;
        }
        if matches!(
            *resolution,
            Some(CallResolution::Unresolved | CallResolution::UnresolvedDispatch)
        ) {
            // Attributed through the SOURCE symbol (a caller in the
            // containment topology), so a repo-scoped run tallies only its
            // own repository's unresolved calls.
            if is_owned(source.as_str()) && !non_head_current.contains(id.as_str()) {
                unresolved_call_edges += 1;
            }
            continue;
        }
        if non_head_current.contains(id.as_str()) {
            continue;
        }
        callers
            .entry(target.as_str())
            .or_default()
            .insert(source.as_str());
    }

    let mut result = DeadCodeResult::default();
    result.counts.symbols_considered = symbols.len();

    // Rule (a): zero recorded direct incoming CALLS edges.
    let zero_incoming: BTreeSet<&str> = symbols
        .keys()
        .copied()
        .filter(|id| !callers.contains_key(id))
        .collect();
    result.counts.referenced = symbols.len() - zero_incoming.len();

    // Exclusions, counted in a fixed order: entry points first, then the
    // public surface over the remainder.
    let mut base: BTreeSet<&str> = BTreeSet::new();
    for &id in &zero_incoming {
        let record = symbols[id];
        if is_entry_point(record) {
            result.counts.entry_points_excluded += 1;
        } else if surface_ids.contains(id) {
            result.counts.public_surface_excluded += 1;
        } else {
            base.insert(id);
        }
    }

    // Rule (b), one hop only: a symbol with at least one recorded direct
    // caller, every caller in the rule-(a) candidate set, and itself
    // neither an entry point nor on the public surface. NOT recursive —
    // callers must be rule-(a) candidates, so deeper dead clusters are left
    // for the transitive follow-on lane.
    let mut one_hop: BTreeSet<&str> = BTreeSet::new();
    for (&id, &record) in &symbols {
        if base.contains(id) || zero_incoming.contains(id) {
            continue;
        }
        if is_entry_point(record) || surface_ids.contains(id) {
            continue;
        }
        let Some(sources) = callers.get(id) else {
            continue;
        };
        if !sources.is_empty() && sources.iter().all(|s| base.contains(s)) {
            one_hop.insert(id);
        }
    }

    let mut rows: Vec<DeadCodeCandidate<'_>> = Vec::new();
    for &id in base.iter().chain(one_hop.iter()) {
        let record = symbols[id];
        let GraphRecord::Node {
            schema_version,
            repo_relative_path,
            span,
            name,
            symbol_kind,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        let Some(name) = name.as_deref() else {
            continue;
        };
        let incoming = callers.get(id).map_or(0, BTreeSet::len);
        rows.push(DeadCodeCandidate {
            record_id: id,
            schema_version: *schema_version,
            name,
            kind: symbol_kind.as_deref().unwrap_or("symbol"),
            repo_relative_path: repo_relative_path.as_deref(),
            span: *span,
            git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
            incoming_calls: incoming,
            callers_all_candidates: incoming > 0,
            on_public_surface: false,
            classification: "candidate",
        });
    }
    result.counts.candidates = rows.len();

    // Documented deterministic ordering: (path, start_line, record_id).
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
    result.candidates = rows.into_iter().take(limit).collect();
    result.counts.candidates_returned = result.candidates.len();

    if result.counts.symbols_considered == 0 {
        result.diagnostics.push(DeadCodeDiagnostic {
            code: "no_symbols",
            record_id: None,
            detail: "graph contains no live code Symbol records in scope; there is \
                     nothing to triage (the store may predate code extraction or the \
                     repository scope excludes every symbol)"
                .to_owned(),
        });
    } else if result.counts.candidates == 0 {
        result.diagnostics.push(DeadCodeDiagnostic {
            code: "no_candidates",
            record_id: None,
            detail: "every considered symbol has a recorded caller, is a recognized \
                     non-call entry point, or is externally reachable; no dead-code \
                     triage candidates"
                .to_owned(),
        });
    }
    if unresolved_call_edges > 0 {
        result.diagnostics.push(DeadCodeDiagnostic {
            code: "unresolved_call_edges_present",
            record_id: None,
            detail: format!(
                "{unresolved_call_edges} call edge(s) in this graph have no resolved \
                 in-repo target; an unrecorded reference to a listed candidate may \
                 exist, so treat candidates as leads only"
            ),
        });
    }
    result.diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.record_id.cmp(&b.record_id))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    result.diagnostics.dedup();
    result
}

// Placeholder for the issue #240 dead-code triage lane. The test module
// below is the executable spec (RED); the implementation follows.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        CallResolution, EdgeLabel, EntryPointKind, EntryPointMark, GraphRecord, NodeKind,
        SourceSpan,
    };

    fn span(line: u32) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line: line as usize,
            end_line: line as usize + 1,
            start_column: None,
            end_column: None,
        }
    }

    /// A private Rust function symbol with no visibility recorded.
    fn sym(id: &str, name: &str, path: &str, line: u32) -> GraphRecord {
        let mut rec = GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(path.to_owned()),
            Some(span(line)),
            Some(name.to_owned()),
            format!("symbol {name}"),
        );
        if let GraphRecord::Node {
            symbol_kind,
            language,
            ..
        } = &mut rec
        {
            *symbol_kind = Some("function".to_owned());
            *language = Some("rust".to_owned());
        }
        rec
    }

    /// A `pub` Rust function symbol (externally reachable when at a
    /// reachable site, per the issue #213 rule).
    fn pub_sym(id: &str, name: &str, path: &str, line: u32) -> GraphRecord {
        let mut rec = sym(id, name, path, line);
        if let GraphRecord::Node { visibility, .. } = &mut rec {
            *visibility = Some("public".to_owned());
        }
        rec
    }

    /// A symbol carrying a non-call entry-point mark (issue #240).
    fn entry_sym(id: &str, name: &str, path: &str, line: u32, kind: EntryPointKind) -> GraphRecord {
        sym(id, name, path, line).with_entry_point(EntryPointMark { kind })
    }

    /// A `pub use` re-export `Import` node plus its owning `File` node and
    /// the `File —IMPORTS→ Import` containment edge the surface rule reads.
    fn reexport(path: &str, use_text: &str, line: u32) -> Vec<GraphRecord> {
        let file_id = format!("codegraph:v6:file:{path}");
        let import_id = format!("codegraph:v6:import:{path}:{line}");
        let mut file = GraphRecord::node(
            file_id.clone(),
            NodeKind::File,
            Some(path.to_owned()),
            Some(span(1)),
            Some(path.rsplit('/').next().unwrap_or(path).to_owned()),
            format!("file {path}"),
        );
        if let GraphRecord::Node { language, .. } = &mut file {
            *language = Some("rust".to_owned());
        }
        let mut import = GraphRecord::node(
            import_id.clone(),
            NodeKind::Import,
            Some(path.to_owned()),
            Some(span(line)),
            Some(use_text.to_owned()),
            format!("import {use_text}"),
        );
        if let GraphRecord::Node { language, .. } = &mut import {
            *language = Some("rust".to_owned());
        }
        let edge = GraphRecord::edge(
            EdgeLabel::Imports,
            file_id,
            import_id,
            Some("1.0".to_owned()),
            "contains import".to_owned(),
        );
        vec![file, import, edge]
    }

    fn calls(source: &str, target: &str, resolution: CallResolution) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Calls,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            "calls".to_owned(),
        )
        .with_resolution(resolution)
    }

    fn candidate_names<'a>(result: &'a DeadCodeResult<'a>) -> Vec<&'a str> {
        result.candidates.iter().map(|c| c.name).collect()
    }

    #[test]
    fn zero_incoming_private_fn_is_candidate_with_evidence() {
        let lonely = "codegraph:v6:lonely";
        let records = vec![sym(lonely, "lonely", "src/lib.rs", 10)];
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert_eq!(candidate_names(&result), vec!["lonely"]);
        let row = &result.candidates[0];
        assert_eq!(row.record_id, lonely);
        assert_eq!(row.kind, "function");
        assert_eq!(row.repo_relative_path, Some("src/lib.rs"));
        assert!(row.span.is_some(), "every row must cite a file/span handle");
        assert_eq!(row.incoming_calls, 0);
        assert!(!row.callers_all_candidates);
        assert!(!row.on_public_surface);
        assert_eq!(result.counts.symbols_considered, 1);
        assert_eq!(result.counts.candidates, 1);
        assert_eq!(result.counts.candidates_returned, 1);
    }

    #[test]
    fn called_fn_is_not_a_candidate() {
        // `callee` has an incoming `Resolved` call, so rule (a) never fires
        // for it; its caller `caller` is itself called by a live binary entry
        // point, so the one-hop dead-cluster rule (issue #240 AC6) does not
        // promote it either. (With `caller` unreferenced this shape IS the
        // AC6 dead cluster covered by `one_hop_dead_cluster_reports_both_a_and_b`.)
        #![allow(clippy::similar_names)]
        let entry_id = "codegraph:v6:entry";
        let caller_id = "codegraph:v6:caller";
        let callee_id = "codegraph:v6:callee";
        let records = vec![
            entry_sym(
                entry_id,
                "entry",
                "src/main.rs",
                5,
                EntryPointKind::BinaryEntry,
            ),
            sym(caller_id, "caller", "src/main.rs", 10),
            sym(callee_id, "callee", "src/main.rs", 20),
            calls(entry_id, caller_id, CallResolution::Resolved),
            calls(caller_id, callee_id, CallResolution::Resolved),
        ];
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert!(result.candidates.is_empty());
        assert_eq!(result.counts.referenced, 2);
    }

    #[test]
    fn one_hop_dead_cluster_reports_both_a_and_b() {
        // Issue #240 AC6: `a` is called only by `b`, and `b` itself is
        // unreferenced. Both are reported: the lane keys on direct incoming
        // edges (one hop), not on transitive reachability pruning.
        let a = "codegraph:v6:a";
        let b = "codegraph:v6:b";
        let records = vec![
            sym(a, "a", "src/lib.rs", 10),
            sym(b, "b", "src/lib.rs", 20),
            calls(b, a, CallResolution::Resolved),
        ];
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert_eq!(candidate_names(&result), vec!["a", "b"]);
        let row_a = result.candidates.iter().find(|c| c.name == "a").unwrap();
        assert_eq!(row_a.incoming_calls, 1);
        assert!(
            row_a.callers_all_candidates,
            "a's only recorded caller is itself a candidate"
        );
        let row_b = result.candidates.iter().find(|c| c.name == "b").unwrap();
        assert_eq!(row_b.incoming_calls, 0);
        assert!(!row_b.callers_all_candidates);
    }

    #[test]
    fn one_hop_rule_does_not_recurse_transitively() {
        // c -> b -> a, c unreferenced. b's only caller (c) is a candidate,
        // so b is reported; a's only caller (b) is NOT itself a zero-incoming
        // candidate, so a is not reported — no transitive pruning.
        let (a, b, c) = ("codegraph:v6:a", "codegraph:v6:b", "codegraph:v6:c");
        let records = vec![
            sym(a, "a", "src/lib.rs", 10),
            sym(b, "b", "src/lib.rs", 20),
            sym(c, "c", "src/lib.rs", 30),
            calls(c, b, CallResolution::Resolved),
            calls(b, a, CallResolution::Resolved),
        ];
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert_eq!(candidate_names(&result), vec!["b", "c"]);
    }

    #[test]
    fn top_level_pub_fn_is_excluded_as_externally_reachable() {
        let entry = "codegraph:v6:entry";
        let records = vec![pub_sym(entry, "entry", "src/lib.rs", 10)];
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert!(
            result.candidates.is_empty(),
            "a top-level pub fn may be an external entry point"
        );
        assert_eq!(result.counts.public_surface_excluded, 1);
        assert!(result.diagnostics.iter().any(|d| d.code == "no_candidates"));
    }

    #[test]
    fn pub_use_reexport_target_is_excluded() {
        let helper = "codegraph:v6:helper";
        let mut records = vec![sym(helper, "helper", "src/lib.rs", 10)];
        records.extend(reexport("src/lib.rs", "pub use helper;", 3));
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert!(
            result.candidates.is_empty(),
            "a pub use re-export widens visibility to the outside world"
        );
        assert_eq!(result.counts.public_surface_excluded, 1);
    }

    #[test]
    fn recognized_entry_points_are_excluded() {
        let records = vec![
            entry_sym(
                "codegraph:v6:main",
                "main",
                "src/main.rs",
                10,
                EntryPointKind::BinaryEntry,
            ),
            entry_sym(
                "codegraph:v6:test_fn",
                "test_fn",
                "src/lib.rs",
                20,
                EntryPointKind::Test,
            ),
            entry_sym(
                "codegraph:v6:ffi_fn",
                "ffi_fn",
                "src/lib.rs",
                30,
                EntryPointKind::FfiExport,
            ),
            entry_sym(
                "codegraph:v6:export_named",
                "export_named",
                "src/lib.rs",
                40,
                EntryPointKind::FfiExport,
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert!(
            result.candidates.is_empty(),
            "fn main, #[test]/#[bench], #[no_mangle]/#[export_name] are non-call entry points"
        );
        assert_eq!(result.counts.entry_points_excluded, 4);
    }

    #[test]
    fn legacy_test_symbol_kind_is_excluded_without_a_mark() {
        // Pre-#240 graphs record #[test] fns as symbol_kind "test" with no
        // entry-point mark; the lane must still exclude them.
        let mut rec = sym("codegraph:v6:old_test", "old_test", "src/lib.rs", 10);
        if let GraphRecord::Node { symbol_kind, .. } = &mut rec {
            *symbol_kind = Some("test".to_owned());
        }
        let records = vec![rec];
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert!(result.candidates.is_empty());
        assert_eq!(result.counts.entry_points_excluded, 1);
    }

    #[test]
    fn entry_point_with_callers_does_not_whiten_its_callees() {
        // `main` calls `helper`; helper has a real (if entry-point) caller,
        // so helper is NOT a candidate even though main is excluded.
        let main = "codegraph:v6:main";
        let helper = "codegraph:v6:helper";
        let records = vec![
            entry_sym(main, "main", "src/main.rs", 10, EntryPointKind::BinaryEntry),
            sym(helper, "helper", "src/main.rs", 20),
            calls(main, helper, CallResolution::Resolved),
        ];
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert!(
            result.candidates.is_empty(),
            "a symbol called by an entry point is live"
        );
    }

    #[test]
    fn limit_truncates_rows_but_counts_stay_total() {
        let records: Vec<GraphRecord> = (0..3u32)
            .map(|i| {
                sym(
                    &format!("codegraph:v6:lonely{i}"),
                    &format!("lonely{i}"),
                    "src/lib.rs",
                    10 + i * 10,
                )
            })
            .collect();
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 2);
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.counts.candidates, 3);
        assert_eq!(result.counts.candidates_returned, 2);
    }

    #[test]
    fn output_ordering_is_deterministic() {
        let mk = || {
            vec![
                sym("codegraph:v6:z", "z", "src/b.rs", 30),
                sym("codegraph:v6:a", "a", "src/a.rs", 10),
                sym("codegraph:v6:m", "m", "src/a.rs", 5),
            ]
        };
        let order = |records: &[GraphRecord]| {
            let index = RepositoryIndex::build(records);
            let result = dead_code_candidates(records, &index, None, 100);
            candidate_names(&result).join(",")
        };
        let records = mk();
        let mut reversed = mk();
        reversed.reverse();
        assert_eq!(order(&records), order(&reversed));
        assert_eq!(order(&records), "m,a,z");
    }

    #[test]
    fn unresolved_call_edges_raise_a_diagnostic_not_a_verdict() {
        let caller = "codegraph:v6:caller";
        let lonely = "codegraph:v6:lonely";
        let records = vec![
            sym(caller, "caller", "src/lib.rs", 10),
            sym(lonely, "lonely", "src/lib.rs", 20),
            calls(
                caller,
                "codegraph:v6:diagnostic:unresolved",
                CallResolution::Unresolved,
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "unresolved_call_edges_present")
        );
        // Both symbols are still classified on recorded edges.
        assert_eq!(candidate_names(&result), vec!["caller", "lonely"]);
    }

    #[test]
    fn ambiguous_calls_count_as_references() {
        // An `Ambiguous` call edge still counts as a reference: `callee` is
        // called by the live `caller`, so neither is a candidate, and no
        // `unresolved_call` diagnostic is raised for the ambiguous edge.
        // (`caller` must be live here — with `caller` unreferenced this shape
        // is the AC6 dead cluster, which reports both.)
        #![allow(clippy::similar_names)]
        let entry_id = "codegraph:v6:entry";
        let caller_id = "codegraph:v6:caller";
        let callee_id = "codegraph:v6:callee";
        let records = vec![
            entry_sym(
                entry_id,
                "entry",
                "src/main.rs",
                5,
                EntryPointKind::BinaryEntry,
            ),
            sym(caller_id, "caller", "src/main.rs", 10),
            sym(callee_id, "callee", "src/main.rs", 20),
            calls(entry_id, caller_id, CallResolution::Resolved),
            calls(caller_id, callee_id, CallResolution::Ambiguous),
        ];
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);
        assert!(result.candidates.is_empty());
        assert_eq!(result.counts.referenced, 2);
        assert!(
            result
                .diagnostics
                .iter()
                .all(|d| d.code != "unresolved_call")
        );
    }

    #[test]
    fn success_metric_hand_labeled_fixture() {
        // The issue's success metric: top-level pub entry point, pub use
        // re-export, fn main, a #[test], a genuinely-unreferenced private fn,
        // and a private fn called only by another unreferenced fn — 100%
        // correct classification.
        let pub_entry = "codegraph:v6:pub_entry";
        let reexported = "codegraph:v6:reexported";
        let main = "codegraph:v6:main";
        let test_fn = "codegraph:v6:test_fn";
        let lonely = "codegraph:v6:lonely";
        let a = "codegraph:v6:a";
        let b = "codegraph:v6:b";
        let mut records = vec![
            pub_sym(pub_entry, "pub_entry", "src/lib.rs", 10),
            sym(reexported, "reexported", "src/lib.rs", 20),
            entry_sym(main, "main", "src/main.rs", 5, EntryPointKind::BinaryEntry),
            entry_sym(test_fn, "test_fn", "src/lib.rs", 30, EntryPointKind::Test),
            sym(lonely, "lonely", "src/lib.rs", 40),
            sym(a, "a", "src/lib.rs", 50),
            sym(b, "b", "src/lib.rs", 60),
            calls(b, a, CallResolution::Resolved),
        ];
        records.extend(reexport("src/lib.rs", "pub use reexported;", 3));
        let index = RepositoryIndex::build(&records);
        let result = dead_code_candidates(&records, &index, None, 100);

        let names = candidate_names(&result);
        // Expected candidates present …
        for expected in ["lonely", "a", "b"] {
            assert!(
                names.contains(&expected),
                "expected candidate {expected} missing from {names:?}"
            );
        }
        // … and every excluded item absent.
        for excluded in ["pub_entry", "reexported", "main", "test_fn"] {
            assert!(
                !names.contains(&excluded),
                "excluded item {excluded} present in {names:?}"
            );
        }
        assert_eq!(names.len(), 3, "no more and no fewer than the 3 candidates");
        // Every returned row cites a repo-relative file/span handle.
        for row in &result.candidates {
            assert!(
                row.repo_relative_path.is_some(),
                "row {} cites a path",
                row.name
            );
            assert!(row.span.is_some(), "row {} cites a span", row.name);
        }
    }
}
