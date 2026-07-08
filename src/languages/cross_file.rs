//! Deterministic repo-wide cross-file call resolution (issue #152).
//!
//! Per-file extraction only sees definitions in the file it is walking, so a
//! call from `src/b.rs` to a function defined in `src/a.rs` never produced a
//! `CALLS` edge. This module closes that recall gap with a resolution pass
//! that runs after every file has been extracted:
//!
//! 1. Each Rust file exports [`FileFacts`]: the callable definitions it
//!    contains and the syntactic call sites found inside recorded symbol
//!    bodies. Both come from the Tree-sitter AST — comments, string literals,
//!    and macro token trees can never produce a call site, so the pass adds
//!    zero comment/string/substring false positives (precision contract
//!    shared with issue #134).
//! 2. The pass builds a repo-wide index of callable definitions and resolves
//!    every call site against it, labeling each emitted `CALLS` edge with a
//!    [`CallResolution`] status:
//!    - `resolved` — exactly one in-repo candidate matched.
//!    - `ambiguous` — two or more candidates matched; an edge is emitted to
//!      every candidate.
//!    - `unresolved` — no in-repo candidate; the call is recorded against a
//!      deterministic `Diagnostic` node instead of being dropped or bound to
//!      an invented symbol.
//!
//! Resolution is purely syntactic and filesystem-local: simple-name matching
//! plus path-segment narrowing (`crate::`/`self::`/`super::` stripped,
//! `Self::` rewritten to the impl owner) and receiver kinds (`self.method()`
//! prefers the surrounding impl's methods). The documented boundary: in-repo
//! cross-file resolution yes; cross-crate targets, trait dynamic dispatch,
//! macro-expanded call sites, and generic monomorphization no. Method calls
//! with no in-repo candidate and constructor-style calls (leading-uppercase
//! final segment, e.g. `Some(..)`, `Vec::new()` receivers aside) are external
//! by construction and are not recorded as unresolved diagnostics to keep the
//! graph bounded; see `docs/prd/0001-codebase-knowledge-graph.md`.
//!
//! Output is deterministic: files iterate in sorted path order, call sites in
//! source order, candidates in sorted (path, qualified name, ID) order, and
//! duplicate (caller, target) pairs collapse to one edge preferring the
//! strongest status.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ir::{CallResolution, EdgeLabel, GraphRecord, NodeKind, SourceSpan, stable_id};

/// A callable definition exported by a per-file extractor for repo-wide
/// resolution.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DefinitionFact {
    /// Stable record ID of the Symbol node.
    pub id: String,
    /// Qualified display name (e.g. `alpha::Widget::render`).
    pub qualified_name: String,
    /// Unqualified name (last path segment).
    pub simple_name: String,
    /// Normalized segments used for path narrowing (module path, then the
    /// normalized impl owner for methods, then the simple name).
    pub match_segments: Vec<String>,
    /// Symbol kind: `function`, `method`, or `test`.
    pub symbol_kind: String,
    /// Repo-relative path of the defining file.
    pub repo_relative_path: String,
}

/// How a call site names its callee.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    /// Bare-identifier call: `helper()`.
    Direct,
    /// Path-qualified call: `alpha::helper()`, `Widget::render()`.
    Path,
    /// Method call with a non-`self` receiver: `w.render()`.
    Method,
    /// Method call on `self`: `self.render()`.
    SelfMethod,
}

/// A syntactic call site found inside a recorded symbol body.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CallSiteFact {
    /// Stable record ID of the calling Symbol node.
    pub caller_id: String,
    /// Qualified name of the calling symbol.
    pub caller_name: String,
    /// Callee as written in source (e.g. `external_dep::render_widget`).
    pub callee_display: String,
    /// Normalized callee path segments (`crate`/`self`/`super` stripped,
    /// `Self` rewritten to the impl owner). The last segment is the simple
    /// name.
    pub callee_segments: Vec<String>,
    /// Syntactic call form.
    pub call_kind: CallKind,
    /// Normalized impl owner for `self`-receiver calls; `None` elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_owner: Option<String>,
    /// Source span of the call expression.
    pub span: SourceSpan,
}

/// Cross-file resolution facts exported by one file's extraction.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileFacts {
    /// Callable definitions in the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub definitions: Vec<DefinitionFact>,
    /// Call sites found inside recorded symbol bodies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_sites: Vec<CallSiteFact>,
}

impl FileFacts {
    /// Returns `true` when the file exported no definitions and no call sites.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.definitions.is_empty() && self.call_sites.is_empty()
    }
}

/// Computes the cross-file call records for one scanned tree.
///
/// Returned records are `Diagnostic` nodes for unresolved calls followed by
/// `CALLS` edges, in deterministic order.
#[must_use]
pub fn cross_file_call_records(
    repository_id: &str,
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> Vec<GraphRecord> {
    let index = DefinitionIndex::build(facts_by_file);

    // (source, target) -> strongest resolution + summary, deduplicating
    // repeated call sites between the same pair.
    let mut edges: BTreeMap<(String, String), (CallResolution, String)> = BTreeMap::new();
    // (file, callee display) -> first-seen span, for diagnostic nodes.
    let mut diagnostics: BTreeMap<(String, String), SourceSpan> = BTreeMap::new();
    // (file, callee display, caller ID) -> caller name, for unresolved edges.
    let mut diagnostic_edges: BTreeMap<(String, String, String), String> = BTreeMap::new();

    for (path, facts) in facts_by_file {
        for call in &facts.call_sites {
            let Some(simple_name) = call.callee_segments.last() else {
                continue;
            };
            let candidates = index.candidates(call, simple_name);
            match candidates.len() {
                0 => {
                    record_unresolved(
                        path,
                        call,
                        simple_name,
                        &mut diagnostics,
                        &mut diagnostic_edges,
                    );
                }
                1 => {
                    record_candidate_edge(
                        path,
                        call,
                        candidates[0],
                        CallResolution::Resolved,
                        1,
                        &mut edges,
                    );
                }
                n => {
                    for candidate in &candidates {
                        record_candidate_edge(
                            path,
                            call,
                            candidate,
                            CallResolution::Ambiguous,
                            n,
                            &mut edges,
                        );
                    }
                }
            }
        }
    }

    let mut records = Vec::new();
    for ((path, display), span) in &diagnostics {
        records.push(unresolved_call_diagnostic(
            repository_id,
            path,
            display,
            *span,
        ));
    }
    for ((source, target), (resolution, summary)) in edges {
        let confidence = match resolution {
            CallResolution::Resolved => Some("1.0".to_owned()),
            CallResolution::Ambiguous | CallResolution::Unresolved => None,
        };
        records.push(
            GraphRecord::edge(EdgeLabel::Calls, source, target, confidence, summary)
                .with_resolution(resolution),
        );
    }
    for ((path, display, caller_id), caller_name) in diagnostic_edges {
        let target = unresolved_call_diagnostic_id(repository_id, &path, &display);
        records.push(
            GraphRecord::edge(
                EdgeLabel::Calls,
                caller_id,
                target,
                None,
                format!("{caller_name} calls {display} (cross-file, unresolved)"),
            )
            .with_resolution(CallResolution::Unresolved),
        );
    }
    records
}

fn record_candidate_edge(
    caller_path: &str,
    call: &CallSiteFact,
    candidate: &DefinitionFact,
    resolution: CallResolution,
    candidate_count: usize,
    edges: &mut BTreeMap<(String, String), (CallResolution, String)>,
) {
    // Same-file targets are already covered by the per-file reference pass;
    // emitting them again would duplicate stable edge IDs.
    if candidate.repo_relative_path == caller_path || candidate.id == call.caller_id {
        return;
    }
    let summary = match resolution {
        CallResolution::Resolved => format!(
            "{} calls {} (cross-file, resolved)",
            call.caller_name, candidate.qualified_name
        ),
        CallResolution::Ambiguous => format!(
            "{} calls {} (cross-file, ambiguous: {candidate_count} in-repo candidates)",
            call.caller_name, candidate.qualified_name
        ),
        CallResolution::Unresolved => unreachable!("unresolved calls never bind a candidate"),
    };
    let key = (call.caller_id.clone(), candidate.id.clone());
    let entry = edges
        .entry(key)
        .or_insert_with(|| (resolution, summary.clone()));
    // Prefer the strongest status when several call sites hit one pair.
    if resolution < entry.0 {
        *entry = (resolution, summary);
    }
}

fn record_unresolved(
    caller_path: &str,
    call: &CallSiteFact,
    simple_name: &str,
    diagnostics: &mut BTreeMap<(String, String), SourceSpan>,
    diagnostic_edges: &mut BTreeMap<(String, String, String), String>,
) {
    // Receiver-typed method calls with no in-repo candidate are external by
    // construction (resolving them needs type information), and
    // constructor-style calls (leading-uppercase final segment: `Some(..)`,
    // tuple-struct constructors) are value constructions, not function calls
    // the graph tracks. Both are documented out of the unresolved contract to
    // keep the graph bounded.
    if matches!(call.call_kind, CallKind::Method | CallKind::SelfMethod) {
        return;
    }
    if simple_name.chars().next().is_some_and(char::is_uppercase) {
        return;
    }
    diagnostics
        .entry((caller_path.to_owned(), call.callee_display.clone()))
        .or_insert(call.span);
    diagnostic_edges
        .entry((
            caller_path.to_owned(),
            call.callee_display.clone(),
            call.caller_id.clone(),
        ))
        .or_insert_with(|| call.caller_name.clone());
}

fn unresolved_call_diagnostic_id(repository_id: &str, path: &str, display: &str) -> String {
    stable_id(&[
        "node",
        "diagnostic",
        repository_id,
        path,
        "unresolved-call",
        display,
    ])
}

fn unresolved_call_diagnostic(
    repository_id: &str,
    path: &str,
    display: &str,
    span: SourceSpan,
) -> GraphRecord {
    GraphRecord::syntax_node(
        unresolved_call_diagnostic_id(repository_id, path, display),
        NodeKind::Diagnostic,
        path.to_owned(),
        span,
        display.to_owned(),
        "rust",
        format!("unresolved call target {display} (no in-repo definition)"),
    )
}

struct DefinitionIndex<'facts> {
    by_simple_name: BTreeMap<&'facts str, Vec<&'facts DefinitionFact>>,
}

impl<'facts> DefinitionIndex<'facts> {
    fn build(facts_by_file: &'facts BTreeMap<String, FileFacts>) -> Self {
        let mut by_simple_name: BTreeMap<&str, Vec<&DefinitionFact>> = BTreeMap::new();
        for facts in facts_by_file.values() {
            for definition in &facts.definitions {
                by_simple_name
                    .entry(definition.simple_name.as_str())
                    .or_default()
                    .push(definition);
            }
        }
        for candidates in by_simple_name.values_mut() {
            candidates.sort_by(|a, b| {
                (&a.repo_relative_path, &a.qualified_name, &a.id).cmp(&(
                    &b.repo_relative_path,
                    &b.qualified_name,
                    &b.id,
                ))
            });
            candidates.dedup_by(|a, b| a.id == b.id);
        }
        Self { by_simple_name }
    }

    /// Returns the in-repo candidates for a call site, deterministically
    /// ordered. Pool selection is syntactic:
    ///
    /// - `Direct` calls can only bind free functions (a bare Rust call can
    ///   never invoke a method).
    /// - `Method`/`SelfMethod` calls can only bind methods; `self.x()`
    ///   prefers the surrounding impl's methods when any match.
    /// - `Path` calls bind any callable whose match segments end with the
    ///   normalized call path; a single-segment path degrades to the
    ///   free-function pool.
    fn candidates(&self, call: &CallSiteFact, simple_name: &str) -> Vec<&'facts DefinitionFact> {
        let Some(pool) = self.by_simple_name.get(simple_name) else {
            return Vec::new();
        };
        match call.call_kind {
            CallKind::Direct => pool
                .iter()
                .copied()
                .filter(|definition| is_free_function(definition))
                .collect(),
            CallKind::Method => pool
                .iter()
                .copied()
                .filter(|definition| definition.symbol_kind == "method")
                .collect(),
            CallKind::SelfMethod => {
                let methods: Vec<&DefinitionFact> = pool
                    .iter()
                    .copied()
                    .filter(|definition| definition.symbol_kind == "method")
                    .collect();
                if let Some(owner) = &call.receiver_owner {
                    let narrowing = [owner.clone(), simple_name.to_owned()];
                    let narrowed: Vec<&DefinitionFact> = methods
                        .iter()
                        .copied()
                        .filter(|definition| {
                            segments_end_with(&definition.match_segments, &narrowing)
                        })
                        .collect();
                    if !narrowed.is_empty() {
                        return narrowed;
                    }
                }
                methods
            }
            CallKind::Path => {
                if call.callee_segments.len() <= 1 {
                    return pool
                        .iter()
                        .copied()
                        .filter(|definition| is_free_function(definition))
                        .collect();
                }
                pool.iter()
                    .copied()
                    .filter(|definition| {
                        segments_end_with(&definition.match_segments, &call.callee_segments)
                    })
                    .collect()
            }
        }
    }
}

fn is_free_function(definition: &DefinitionFact) -> bool {
    definition.symbol_kind == "function" || definition.symbol_kind == "test"
}

fn segments_end_with(segments: &[String], suffix: &[impl AsRef<str>]) -> bool {
    if suffix.len() > segments.len() {
        return false;
    }
    segments[segments.len() - suffix.len()..]
        .iter()
        .zip(suffix)
        .all(|(segment, expected)| segment == expected.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 1,
            start_line: 1,
            end_line: 1,
        }
    }

    fn definition(id: &str, kind: &str, path: &str, segments: &[&str]) -> DefinitionFact {
        DefinitionFact {
            id: id.to_owned(),
            qualified_name: segments.join("::"),
            simple_name: segments.last().expect("segments").to_owned().to_owned(),
            match_segments: segments
                .iter()
                .map(ToOwned::to_owned)
                .map(String::from)
                .collect(),
            symbol_kind: kind.to_owned(),
            repo_relative_path: path.to_owned(),
        }
    }

    fn call(
        caller_id: &str,
        display: &str,
        segments: &[&str],
        kind: CallKind,
        owner: Option<&str>,
    ) -> CallSiteFact {
        CallSiteFact {
            caller_id: caller_id.to_owned(),
            caller_name: caller_id.to_owned(),
            callee_display: display.to_owned(),
            callee_segments: segments.iter().map(|s| (*s).to_owned()).collect(),
            call_kind: kind,
            receiver_owner: owner.map(ToOwned::to_owned),
            span: span(),
        }
    }

    fn facts(
        entries: &[(&str, Vec<DefinitionFact>, Vec<CallSiteFact>)],
    ) -> BTreeMap<String, FileFacts> {
        entries
            .iter()
            .map(|(path, definitions, call_sites)| {
                (
                    (*path).to_owned(),
                    FileFacts {
                        definitions: definitions.clone(),
                        call_sites: call_sites.clone(),
                    },
                )
            })
            .collect()
    }

    fn edge_targets(records: &[GraphRecord], resolution: CallResolution) -> Vec<String> {
        records
            .iter()
            .filter(|record| record.resolution() == Some(resolution))
            .filter_map(|record| match record {
                GraphRecord::Edge { target, .. } => Some(target.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn direct_calls_never_bind_methods() {
        let facts = facts(&[
            (
                "src/a.rs",
                vec![definition(
                    "m1",
                    "method",
                    "src/a.rs",
                    &["a", "Widget", "run"],
                )],
                vec![],
            ),
            (
                "src/b.rs",
                vec![],
                vec![call("caller", "run", &["run"], CallKind::Direct, None)],
            ),
        ]);
        let records = cross_file_call_records("repo", &facts);
        assert!(
            edge_targets(&records, CallResolution::Resolved).is_empty(),
            "a bare call must not bind a method definition"
        );
        // The call is honestly unresolved instead.
        assert_eq!(edge_targets(&records, CallResolution::Unresolved).len(), 1);
    }

    #[test]
    fn self_method_calls_prefer_the_impl_owner() {
        let facts = facts(&[
            (
                "src/a.rs",
                vec![definition(
                    "a-run",
                    "method",
                    "src/a.rs",
                    &["a", "Widget", "run"],
                )],
                vec![],
            ),
            (
                "src/b.rs",
                vec![definition(
                    "b-run",
                    "method",
                    "src/b.rs",
                    &["b", "Gadget", "run"],
                )],
                vec![call(
                    "caller",
                    "run",
                    &["run"],
                    CallKind::SelfMethod,
                    Some("Widget"),
                )],
            ),
        ]);
        let records = cross_file_call_records("repo", &facts);
        assert_eq!(
            edge_targets(&records, CallResolution::Resolved),
            vec!["a-run".to_owned()],
            "self.run() inside impl Widget must bind Widget::run only"
        );
        assert!(edge_targets(&records, CallResolution::Ambiguous).is_empty());
    }

    #[test]
    fn constructor_style_and_external_method_calls_are_not_diagnosed() {
        let facts = facts(&[(
            "src/b.rs",
            vec![],
            vec![
                call("caller", "Some", &["Some"], CallKind::Direct, None),
                call("caller", "clone", &["clone"], CallKind::Method, None),
            ],
        )]);
        let records = cross_file_call_records("repo", &facts);
        assert!(
            records.is_empty(),
            "constructor-style and external method calls stay out of the graph: {records:?}"
        );
    }

    #[test]
    fn duplicate_call_sites_collapse_to_one_edge() {
        let helper = definition("helper", "function", "src/a.rs", &["a", "helper"]);
        let facts = facts(&[
            ("src/a.rs", vec![helper], vec![]),
            (
                "src/b.rs",
                vec![],
                vec![
                    call("caller", "helper", &["helper"], CallKind::Direct, None),
                    call(
                        "caller",
                        "a::helper",
                        &["a", "helper"],
                        CallKind::Path,
                        None,
                    ),
                ],
            ),
        ]);
        let records = cross_file_call_records("repo", &facts);
        assert_eq!(
            edge_targets(&records, CallResolution::Resolved),
            vec!["helper".to_owned()],
            "two call sites for one pair must collapse to a single edge"
        );
        assert_eq!(records.len(), 1);
    }
}
