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
use crate::languages::rust::is_impl_target_kind;

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

/// One out-of-line module declaration (`mod name;`) exported for the
/// repo-wide out-of-line test-scope pass (issue #223).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutOfLineModFact {
    /// Declared module name.
    pub name: String,
    /// Inline-module segments enclosing the declaration within the file
    /// (`mod a { mod b; }` records `["a"]` for `b`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inline_module_path: Vec<String>,
    /// `true` when the declaration is test-gated: annotated `#[cfg(test)]`
    /// or declared inside an already-test scope.
    pub test_gated: bool,
    /// Trivial `#[path = "literal"]` override. Per the Rust reference it
    /// resolves relative to the declaring file's directory for top-level
    /// declarations, and relative to the module directory plus the inline
    /// components for declarations inside inline module blocks. Non-literal
    /// path attributes are not resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_override: Option<String>,
    /// `true` when any enclosing inline module itself carries a `#[path]`
    /// attribute, which changes the resolution base for everything nested in
    /// it. Such declarations are not resolved (documented gap) rather than
    /// resolved against the wrong directory.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub under_inline_path_override: bool,
}

/// An IMPLEMENTS-eligible trait/type definition exported for the repo-wide
/// cross-file `IMPLEMENTS` resolution pass (issue #344).
///
/// Only symbols whose kind can be an `IMPLEMENTS` target are exported here —
/// value-namespace items and callables never enter this index, so an out-of-line
/// impl can never bind its trait to a same-named function.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImplTargetFact {
    /// Stable record ID of the trait/type Symbol node.
    pub id: String,
    /// Crate-root-relative qualified name (`m::T`; root items bare `T`), the
    /// key the repo-wide index resolves an impl's trait path against.
    pub qualified_name: String,
    /// The crate root this definition belongs to (issue #394). In a
    /// multi-crate-root package (`src/lib.rs` + `src/bin/tool.rs`, or
    /// `src/bin/*.rs`), a root `trait T` in each crate shares the same
    /// crate-root-relative `qualified_name` `T`; keying the repo-wide index on
    /// `(crate_root, qualified_name)` keeps the two from pooling. Derived from
    /// the declaring file's repo-relative path by [`crate_root_id`]. Serde
    /// default (empty) so a pre-#394 cache reads back as a single default root.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub crate_root: String,
    /// The declaring module path (crate-root-relative), for provenance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_path: Vec<String>,
    /// Symbol kind, restricted to the `IMPLEMENTS`-target set
    /// (`trait`/`struct`/`enum`/`type_alias`).
    pub symbol_kind: String,
}

/// A trait impl that failed to resolve its trait LOCALLY, deferred to the
/// repo-wide cross-file `IMPLEMENTS` pass (issue #344).
///
/// Carries the parsed+normalized trait path exactly as the local resolver saw
/// it (`crate::T`, `T`, `super::T`, `sibling::T`; generic binders and trait
/// generic args already stripped) plus the declaring module scope, so the
/// repo-wide pass can replay the same crate/self/super + scope-walk resolution
/// against every file's [`ImplTargetFact`]s.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingImplFact {
    /// Stable record ID of the `impl` Symbol node (the edge source).
    pub source_id: String,
    /// The normalized trait path as written on the impl header.
    pub trait_path: String,
    /// The crate root the impl belongs to (issue #394), derived from the impl
    /// file's repo-relative path by [`crate_root_id`]. The repo-wide resolver
    /// restricts this pending impl's candidate targets to its OWN crate root so
    /// a lib impl of a root `T` never binds a same-named binary crate's `T`.
    /// Serde default (empty) reads back as a single default root.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub crate_root: String,
    /// The impl's enclosing module path (crate-root-relative).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_names: Vec<String>,
    /// `true` when a `use` import visible in the impl's module scope binds the
    /// same bare final segment as this (bare) trait/type name — the name refers
    /// to the IMPORT, not any repo same-name definition, so the cross-file
    /// resolver mints NO edge (AST-derived import-shadow veto, issues
    /// #343/#344 round 9). Set at extraction time from the same predicate the
    /// local per-file resolver uses, so the two IMPLEMENTS paths never diverge.
    /// Only ever `true` for a bare `trait_path`; a qualified path is never
    /// shadowed. As of issue #393 the cross-file resolver no longer treats this
    /// as an unconditional veto: when a `use` in the impl's module scope binds
    /// the bare name, the resolver resolves the aliased import PATH against the
    /// crate-root-partitioned index (see [`FileFacts::use_trait_imports`]) and
    /// mints the correct edge (or leaves it unresolved), falling back to the
    /// conservative ambiguity bound only for genuinely un-imported bare names.
    #[serde(default)]
    pub shadowed_by_use: bool,
}

/// One `use`-import binding exported for import-aware cross-file `IMPLEMENTS`
/// resolution (issue #393).
///
/// PR #389 conservatively left `impl T for Foo` (with `use crate::a::T;` in the
/// same module) UNRESOLVED via the [`PendingImplFact::shadowed_by_use`] veto,
/// because the module-scope outward walk cannot see the import and would reach a
/// root same-named `T`. This fact captures the RESOLVED FULL PATH each bare name
/// binds so the resolver can look up the aliased target directly instead of
/// vetoing. Only MODULE-ITEM `use` declarations in the impl's own module scope
/// are exported (the same Rust-visibility-correct scoping the veto uses: block
/// -local and ancestor imports are excluded), so recall recovery never
/// reintroduces a wrong edge. Glob imports (`use a::*;`) bind no simple name and
/// contribute nothing, so they stay bounded out (unresolved).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct UseImportFact {
    /// The module scope the `use` appears in (crate-root-relative), the same
    /// chain a co-located impl records, keyed identically to the veto index so
    /// visibility follows module nesting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_names: Vec<String>,
    /// The simple name the import binds into scope (`T`, or the alias in
    /// `use ... as U;`).
    pub simple_name: String,
    /// The import path as written (`crate::a::T`, `super::a::T`,
    /// `std::fmt::Debug`), resolved against the crate-root-partitioned index by
    /// the same `crate::`/`self::`/`super::` normalization the resolver applies
    /// to a qualified impl trait path — so an external/std import resolves to
    /// nothing (no wrong edge) and an in-repo alias resolves to its true target.
    pub resolved_path: String,
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
    /// Out-of-line module declarations in the file (issue #223).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_line_mods: Vec<OutOfLineModFact>,
    /// IMPLEMENTS-eligible trait/type definitions in the file (issue #344).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impl_targets: Vec<ImplTargetFact>,
    /// Trait impls that failed local resolution, deferred to the repo-wide
    /// cross-file `IMPLEMENTS` pass (issue #344).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_impls: Vec<PendingImplFact>,
    /// Module-item `use`-import bindings, for import-aware cross-file
    /// `IMPLEMENTS` resolution (issue #393).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub use_trait_imports: Vec<UseImportFact>,
}

impl FileFacts {
    /// Returns `true` when the file exported no cross-file resolution facts of
    /// any kind.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.definitions.is_empty()
            && self.call_sites.is_empty()
            && self.out_of_line_mods.is_empty()
            && self.impl_targets.is_empty()
            && self.pending_impls.is_empty()
            && self.use_trait_imports.is_empty()
    }
}

/// Marks panic-risk call sites inside out-of-line `#[cfg(test)]` modules as
/// test context (issue #223).
///
/// A `#[cfg(test)] mod tests;` declaration places the module body in its own
/// file (`tests.rs` / `tests/mod.rs`), which the per-file extractor scans with
/// no knowledge of the gating attribute. This deterministic repo-wide pass
/// resolves test-gated out-of-line declarations to their module files, expands
/// through those files' transitive out-of-line submodules (any gating), and
/// rewrites the affected `PanicRiskSite` records' `call_context` to `test`.
/// `call_context` is never an identity input, so record IDs are unchanged.
pub fn apply_out_of_line_test_scope(
    records: &mut [GraphRecord],
    facts_by_file: &BTreeMap<String, FileFacts>,
) {
    // Known repo-relative file paths: fact keys plus every file-backed record
    // path, so a module file that exported no facts still resolves.
    let mut known_paths: std::collections::BTreeSet<String> =
        facts_by_file.keys().cloned().collect();
    for record in records.iter() {
        if let GraphRecord::Node {
            repo_relative_path: Some(path),
            ..
        } = record
        {
            known_paths.insert(path.clone());
        }
    }

    // Resolve every out-of-line declaration once into (from, to, gated)
    // module-load edges.
    let mut edges: Vec<(String, String, bool)> = Vec::new();
    for (file, facts) in facts_by_file {
        for fact in &facts.out_of_line_mods {
            if let Some(target) = resolve_out_of_line_target(file, fact, &known_paths) {
                edges.push((file.clone(), target, fact.test_gated));
            }
        }
    }
    if edges.is_empty() {
        return;
    }

    // Production takes precedence for dual-use files: a module file that a
    // non-test declaration also loads still compiles into the production
    // build, and hiding its panic-risk sites behind a `test` label would
    // hide production risk. Compute the production-reachable set first —
    // declaring files that are never themselves loaded as out-of-line
    // modules (e.g. crate roots) seed it, and it propagates through ungated
    // declarations to a fixpoint — then never rewrite (or expand through)
    // anything production-reachable.
    let targets: std::collections::BTreeSet<&str> =
        edges.iter().map(|(_, to, _)| to.as_str()).collect();
    let mut production: std::collections::BTreeSet<String> = edges
        .iter()
        .filter(|(from, _, _)| !targets.contains(from.as_str()))
        .map(|(from, _, _)| from.clone())
        .collect();
    // Conventional crate roots always compile into a production build, so
    // they keep their production seed even when a test-gated `#[path]`
    // declaration also targets them (a binary root loaded as a test module
    // is still a production binary).
    production.extend(
        known_paths
            .iter()
            .filter(|path| is_conventional_crate_root(path))
            .cloned(),
    );
    loop {
        let mut changed = false;
        for (from, to, gated) in &edges {
            if !gated && production.contains(from) && !production.contains(to) {
                production.insert(to.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Test-reachable fixpoint: seed with every test-gated declaration's
    // target, expand through all declarations of test-only files — but a
    // production-reachable file is never rewritten and never expanded
    // through (its children compile in the production instantiation too).
    let mut test_files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut worklist: Vec<String> = edges
        .iter()
        .filter(|(_, _, gated)| *gated)
        .map(|(_, to, _)| to.clone())
        .collect();
    while let Some(file) = worklist.pop() {
        if production.contains(&file) || !test_files.insert(file.clone()) {
            continue;
        }
        for (from, to, _) in &edges {
            if *from == file {
                worklist.push(to.clone());
            }
        }
    }
    if test_files.is_empty() {
        return;
    }

    for record in records.iter_mut() {
        if let GraphRecord::Node {
            kind: NodeKind::PanicRiskSite,
            repo_relative_path: Some(path),
            call_context,
            ..
        } = record
            && test_files.contains(path.as_str())
        {
            *call_context = Some("test".to_owned());
        }
    }
}

/// Resolves one out-of-line module declaration to a scanned repo-relative
/// file path, or `None` when it is unresolvable.
///
/// An enclosing inline module with its own `#[path]` attribute rebases
/// everything nested in it; that combination is not resolved (documented gap)
/// rather than probed against the wrong directory. `#[path = "literal"]`
/// overrides follow the Rust reference: relative to the declaring file's
/// directory for top-level declarations, and relative to the module directory
/// plus the inline components for declarations inside inline module blocks.
fn resolve_out_of_line_target(
    declaring_file: &str,
    fact: &OutOfLineModFact,
    known_paths: &std::collections::BTreeSet<String>,
) -> Option<String> {
    if fact.under_inline_path_override {
        return None;
    }
    if let Some(override_path) = &fact.path_override {
        let base = if fact.inline_module_path.is_empty() {
            parent_dir_segments(declaring_file)
        } else {
            let mut base = module_dir_segments(declaring_file);
            base.extend(fact.inline_module_path.iter().cloned());
            base
        };
        let candidate = join_segments(&base, override_path)?;
        return known_paths.contains(&candidate).then_some(candidate);
    }
    let mut base = module_dir_segments(declaring_file);
    base.extend(fact.inline_module_path.iter().cloned());
    let file_candidate = join_segments(&base, &format!("{}.rs", fact.name))?;
    if known_paths.contains(&file_candidate) {
        return Some(file_candidate);
    }
    let dir_candidate = join_segments(&base, &format!("{}/mod.rs", fact.name))?;
    known_paths
        .contains(&dir_candidate)
        .then_some(dir_candidate)
}

/// `true` for files that are crate roots under the standard Cargo layout
/// conventions: `src/lib.rs`, `src/main.rs`, `src/bin/<name>.rs`,
/// `src/bin/<name>/main.rs`, and `build.rs`. Crate roots always compile into
/// a production build; files under a top-level `tests/` directory are their
/// own test crates and are intentionally not in this set (they classify as
/// test at extraction time).
fn is_conventional_crate_root(path: &str) -> bool {
    if matches!(path, "build.rs" | "src/lib.rs" | "src/main.rs") {
        return true;
    }
    let segments: Vec<&str> = path.split('/').collect();
    match segments.as_slice() {
        ["src", "bin", file] => std::path::Path::new(file)
            .extension()
            .is_some_and(|ext| ext == "rs"),
        ["src", "bin", _, "main.rs"] => true,
        _ => false,
    }
}

/// Derives a stable crate-root identifier for a repo-relative Rust file path.
///
/// This lets the repo-wide `IMPLEMENTS` index (issue #394) partition same-named
/// root definitions that belong to different crate roots in one package.
///
/// Under the standard Cargo layout each buildable target is its own crate root:
/// `src/bin/<name>.rs` and `src/bin/<name>/**` form `bin:<name>`,
/// `examples/<name>.rs` / `examples/<name>/**` form `example:<name>`,
/// `tests/<name>.rs` / `tests/<name>/**` form `test:<name>`,
/// `benches/<name>.rs` / `benches/<name>/**` form `bench:<name>`, and `build.rs`
/// is `build`. Every other file under `src/` — `src/lib.rs`, `src/main.rs`, and
/// their module files (`src/a.rs`, `src/foo/bar.rs`) — shares the primary
/// crate root `lib`: it is the library crate when `src/lib.rs` exists and the
/// default binary crate otherwise, which cannot be distinguished from a single
/// file's path but resolves to the same partition either way. Any path outside
/// these conventions also falls into `lib`, so resolution is never worse than
/// the pre-#394 single-index behavior.
///
/// Documented residual bound: a package carrying BOTH `src/lib.rs` and
/// `src/main.rs`, each defining a same-named ROOT trait, still pools those two
/// roots in `lib`. The common multi-crate-root case (auxiliary `bin`/`example`/
/// `test`/`bench` targets) is partitioned; the lib-plus-default-binary same-name
/// root-trait collision is rare and left as a known bound.
#[must_use]
pub fn crate_root_id(repo_relative_path: &str) -> String {
    let segments: Vec<&str> = repo_relative_path.split('/').collect();
    let is_rs = |name: &str| {
        std::path::Path::new(name)
            .extension()
            .is_some_and(|e| e == "rs")
    };
    // A single-file auxiliary target (`examples/foo.rs`) uses the file stem;
    // a directory target (`examples/foo/main.rs`, or any submodule under it)
    // uses the directory name.
    let aux = |kind: &str, rest: &[&str]| -> Option<String> {
        match rest {
            [file] if is_rs(file) => Some(format!("{kind}:{}", file.trim_end_matches(".rs"))),
            [dir, ..] => Some(format!("{kind}:{dir}")),
            _ => None,
        }
    };
    match segments.as_slice() {
        ["build.rs"] => "build".to_owned(),
        ["src", "bin", rest @ ..] => aux("bin", rest).unwrap_or_else(|| "lib".to_owned()),
        ["examples", rest @ ..] => aux("example", rest).unwrap_or_else(|| "lib".to_owned()),
        ["tests", rest @ ..] => aux("test", rest).unwrap_or_else(|| "lib".to_owned()),
        ["benches", rest @ ..] => aux("bench", rest).unwrap_or_else(|| "lib".to_owned()),
        _ => "lib".to_owned(),
    }
}

/// The directory whose files are the declaring file's child modules:
/// `src/lib.rs` / `src/main.rs` / `x/mod.rs` own their containing directory;
/// `src/foo.rs` owns `src/foo/`.
fn module_dir_segments(file_path: &str) -> Vec<String> {
    let mut parts: Vec<String> = file_path.split('/').map(str::to_owned).collect();
    let Some(last) = parts.pop() else {
        return parts;
    };
    match last.as_str() {
        "lib.rs" | "main.rs" | "mod.rs" => {}
        other => {
            if let Some(stem) = other.strip_suffix(".rs") {
                parts.push(stem.to_owned());
            }
        }
    }
    parts
}

/// The declaring file's own directory (for trivial `#[path]` overrides).
fn parent_dir_segments(file_path: &str) -> Vec<String> {
    let mut parts: Vec<String> = file_path.split('/').map(str::to_owned).collect();
    parts.pop();
    parts
}

/// Joins directory segments with a relative suffix into one lexically
/// normalized repo-relative path: `.` segments are dropped and `..` segments
/// pop the preceding component, so `src/tests` + `../support.rs` yields
/// `src/support.rs`. A `..` chain that would escape the repository root (or a
/// suffix that normalizes to nothing) yields `None` — unresolvable, never a
/// panic or a wrong probe.
fn join_segments(dir: &[String], suffix: &str) -> Option<String> {
    let mut segments: Vec<&str> = dir.iter().map(String::as_str).collect();
    for part in suffix.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("/"))
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

/// Computes the cross-file `IMPLEMENTS` records for one scanned tree
/// (issue #344).
///
/// Per-file extraction resolves an impl's trait only against definitions in the
/// file it is walking, so the common out-of-line module layout
/// (`trait T` in `src/lib.rs`, `impl crate::T for Foo` in `src/m.rs`) emitted
/// ZERO `IMPLEMENTS` edges. This pass closes that recall gap: it builds a
/// repo-wide index of every [`ImplTargetFact`] keyed by
/// `(crate_root, crate-root-relative qualified name)` (issue #394) and resolves
/// each [`PendingImplFact`] — an impl the per-file pass could not resolve
/// locally — with the SAME crate/self/super + module-scope-walk semantics the
/// local resolver uses, restricted to the impl's own crate root, minting one
/// `IMPLEMENTS` edge per unique resolution.
///
/// A bare (unqualified) trait/type name bound by a module-item `use` in the
/// impl's own module scope is resolved import-aware (issue #393): the aliased
/// import PATH is resolved against the crate-root-partitioned index, so
/// `use crate::a::T; impl T for Foo` edge-backs `Foo` to `a::T`, not a root `T`.
/// An external/std import resolves to no in-repo definition and therefore mints
/// no edge, so the recall recovery never reintroduces a wrong-target edge.
///
/// Only impls that missed local resolution are deferred here, so a locally
/// resolved edge is never duplicated. An unresolved or ambiguous trait path is
/// left edge-free rather than diagnosed: an out-of-line impl of an external/std
/// trait (`impl Debug for Foo`) is the overwhelming unresolved case and is
/// external by construction, matching the `implementors` completeness contract
/// (`local_traits_only`). The documented residual bound: cross-CRATE traits,
/// non-Rust languages, blanket impls, glob (`use a::*;`) imports, and a
/// same-name-ambiguous bare name with no captured `use` import stay out.
///
/// Output is deterministic: edges are keyed and emitted in sorted
/// `(source, target)` order, byte-identical across runs.
#[must_use]
pub fn cross_file_implements_records(
    _repository_id: &str,
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> Vec<GraphRecord> {
    let index = ImplTargetIndex::build(facts_by_file);
    // (source impl ID, trait target ID) -> summary, deduplicating so a source
    // never mints two edges to one target.
    let mut edges: BTreeMap<(String, String), String> = BTreeMap::new();
    for facts in facts_by_file.values() {
        for pending in &facts.pending_impls {
            // Import-aware resolution (issue #393): a bare trait/type name bound
            // by a `use` in the impl's module scope refers to the IMPORT, not a
            // repo same-name definition. Resolve the aliased import path against
            // the crate-root-partitioned index — the correct target, or nothing
            // (an external/std import resolves to no in-repo definition, so it
            // still mints no WRONG edge, preserving the #343/#344 round-9 veto
            // intent). A genuinely un-imported bare name falls through to the
            // conservative same-name ambiguity bound instead.
            if let Some(target) = index.resolve(pending, &facts.use_trait_imports) {
                let summary = format!(
                    "{} implementation relationship (cross-file)",
                    pending.trait_path
                );
                edges
                    .entry((pending.source_id.clone(), target.id.clone()))
                    .or_insert(summary);
            }
        }
    }
    edges
        .into_iter()
        .map(|((source, target), summary)| {
            GraphRecord::edge(
                EdgeLabel::Implements,
                source,
                target,
                Some("1.0".to_owned()),
                summary,
            )
        })
        .collect()
}

/// Repo-wide index of IMPLEMENTS-eligible trait/type definitions, keyed by
/// `(crate_root, crate-root-relative qualified name)` (issues #344/#394).
///
/// The crate-root component (issue #394) partitions same-named root definitions
/// that belong to different crate roots in one package (`src/lib.rs` +
/// `src/bin/tool.rs`), so a lib impl of a root `T` resolves to the LIBRARY `T`
/// and never pools with a binary crate's same-named `T`.
struct ImplTargetIndex<'facts> {
    by_qualified: BTreeMap<(&'facts str, &'facts str), Vec<&'facts ImplTargetFact>>,
}

impl<'facts> ImplTargetIndex<'facts> {
    fn build(facts_by_file: &'facts BTreeMap<String, FileFacts>) -> Self {
        let mut by_qualified: BTreeMap<(&str, &str), Vec<&ImplTargetFact>> = BTreeMap::new();
        for facts in facts_by_file.values() {
            for target in &facts.impl_targets {
                by_qualified
                    .entry((target.crate_root.as_str(), target.qualified_name.as_str()))
                    .or_default()
                    .push(target);
            }
        }
        for candidates in by_qualified.values_mut() {
            candidates.sort_by(|a, b| a.id.cmp(&b.id));
            candidates.dedup_by(|a, b| a.id == b.id);
        }
        Self { by_qualified }
    }

    /// Resolves a pending impl's trait path to a UNIQUE target, or `None` when
    /// nothing matches or the match is ambiguous (2+ same-qualified-name
    /// definitions) — ambiguity never silently picks one, mirroring the CALLS
    /// pass. Candidates are restricted to the pending impl's OWN crate root
    /// (issue #394).
    fn resolve(
        &self,
        pending: &PendingImplFact,
        use_imports: &'facts [UseImportFact],
    ) -> Option<&'facts ImplTargetFact> {
        // Import-aware resolution (issue #393): when the impl's trait path is a
        // BARE name that a module-item `use` in the impl's own module scope
        // binds, the name refers to that import. Resolve the aliased import PATH
        // (respecting the same crate-root partition) rather than walking the
        // module scope to a root same-named definition. If the aliased path is
        // in-repo it resolves to the true target; if it is external/std it
        // resolves to nothing (no wrong edge). This intentionally does NOT fall
        // through to the scope walk — the bare name is the import, full stop.
        if !pending.trait_path.contains("::")
            && let Some(resolved_path) =
                lookup_use_import(use_imports, &pending.module_names, &pending.trait_path)
        {
            // Import-aware in-repo resolution fires for any import path that
            // could name a local target: an explicitly in-repo-rooted path
            // (`crate::`/`self::`/`super::`) OR a bare path whose first segment is
            // NOT a known extern-prelude crate name. A bare first-segment import
            // splits two ways per Rust 2018+ path resolution:
            //   * `use std::fmt::Display;` — first segment `std` is an
            //     EXTERN-prelude crate, so the path names an external crate and
            //     resolves to NO in-repo target even when the repo coincidentally
            //     defines a same-path local module (`mod std::fmt`). Returning
            //     `None` directly (no edge) — rather than running the extern path
            //     through the scope walk, which would mis-bind that coincident
            //     local module — preserves the wrong-edge fix Codex P2 (PR #399)
            //     asked for.
            //   * `use a::T;` — first segment `a` is NOT an extern-prelude crate,
            //     so it names a LOCAL crate-root module (valid Rust 2018). It
            //     resolves against the crate-root-partitioned index below; an
            //     actual in-repo target must exist for an edge to mint, so an
            //     ordinary external dependency (`use serde::Serialize;`) still
            //     yields no edge without any dependency list. Dropping this case
            //     was the round-1 recall regression Codex "crate-root local
            //     imports" (PR #399) flagged.
            // Either way the bare name never falls through to the un-imported
            // scope walk; the name is the import, full stop.
            if !use_path_is_in_repo_rooted(resolved_path)
                && use_path_first_segment_is_extern_prelude(resolved_path)
            {
                return None;
            }
            return match self
                .candidates(&pending.crate_root, resolved_path, &pending.module_names)
                .as_slice()
            {
                [only] => Some(only),
                _ => None,
            };
        }
        match self
            .candidates(
                &pending.crate_root,
                &pending.trait_path,
                &pending.module_names,
            )
            .as_slice()
        {
            [only] => Some(only),
            _ => None,
        }
    }

    /// The candidate targets for a trait path within `crate_root`, applying the
    /// same resolution ladder as the local resolver: an absolute
    /// `crate::`/`self::`/`super::` path resolves to its exact crate-root-relative
    /// qualified name; every other path (relative-qualified or unqualified)
    /// walks the impl's module scope outward to the crate root.
    fn candidates(
        &self,
        crate_root: &str,
        trait_path: &str,
        module_names: &[String],
    ) -> Vec<&'facts ImplTargetFact> {
        if trait_path.contains("::") {
            if let Some(normalized) = normalize_absolute_trait_path(trait_path, module_names) {
                return self.lookup(crate_root, &normalized);
            }
            // A relative-qualified path (`sibling::T`): scope-walk as before.
            return self.scope_walk(crate_root, trait_path, module_names);
        }
        // A BARE (unqualified) impl-target name whose simple name is ambiguous
        // across this crate root's impl-target index (e.g. root `Foo` and
        // `a::Foo`) may be a `use`-alias of a NON-root definition the scope walk
        // cannot see (and, when it carries no captured `use` import, cannot
        // resolve — issue #393 recovers only imported bare names). This covers
        // BOTH pending-impl target kinds: a trait path from a trait impl AND the
        // TYPE name from a non-generic inherent impl (`impl Foo {}`, whose
        // pending `trait_path` is the type `Foo`). Rather than let the outward
        // walk mis-bind it to a root same-named definition (a WRONG-target edge,
        // worse than a missing one), leave it unresolved — matching the
        // documented `local_traits_only` use-alias bound. Only an unambiguous
        // single same-simple-name impl-target resolves outward.
        if self.bare_simple_name_is_ambiguous(crate_root, trait_path) {
            return Vec::new();
        }
        self.scope_walk(crate_root, trait_path, module_names)
    }

    /// Reports whether more than one distinct impl-target definition in this
    /// crate root's index shares the given bare simple name, counting ALL
    /// impl-target kinds ([`is_impl_target_kind`]: `trait` / `struct` / `enum` /
    /// `type_alias`), not only traits. The scope walk resolves a bare name
    /// against every one of those kinds, so a bare inherent-impl type name
    /// (`impl Foo {}`) collides with an unrelated same-named type exactly as a
    /// bare trait name collides with an unrelated same-named trait — the guard
    /// must count them all.
    ///
    /// Such a bare reference cannot be disambiguated without import-aware
    /// (`use`-decl) resolution; when the impl carries no captured `use` import
    /// for the name (issue #393 handles the imported case earlier), it is left
    /// unresolved. Trade-off accepted (the honest-bound direction): when a trait
    /// `T` and an unrelated type `T` coexist across files in one crate root, a
    /// bare `impl T for X` with no import is left UNRESOLVED — a rare potential
    /// WRONG-edge converted into a rare MISSED-edge, consistent with
    /// `local_traits_only`.
    fn bare_simple_name_is_ambiguous(&self, crate_root: &str, simple: &str) -> bool {
        // Share the local resolver's COUNTING predicate over exactly this crate
        // root's impl-target qualified names, so the local (per-file) and
        // cross-file ambiguity bounds can never drift a target-kind or a
        // counting rule apart. Only qualified names carrying at least one
        // impl-target-kind fact ([`is_impl_target_kind`]) are counted — a
        // value-namespace collision never triggers the guard.
        crate::languages::rust::bare_simple_name_is_ambiguous(
            simple,
            self.by_qualified
                .iter()
                .filter_map(|((root, qualified), facts)| {
                    (*root == crate_root
                        && facts
                            .iter()
                            .any(|fact| is_impl_target_kind(&fact.symbol_kind)))
                    .then_some(*qualified)
                }),
        )
    }

    /// Walks the module scope from the impl's own module outward to the crate
    /// root, returning the candidates at the first level that matches (within
    /// `crate_root`).
    fn scope_walk(
        &self,
        crate_root: &str,
        target: &str,
        module_names: &[String],
    ) -> Vec<&'facts ImplTargetFact> {
        for depth in (0..=module_names.len()).rev() {
            let candidate = if depth == 0 {
                target.to_owned()
            } else {
                format!("{}::{target}", module_names[..depth].join("::"))
            };
            let hits = self.lookup(crate_root, &candidate);
            if !hits.is_empty() {
                return hits;
            }
        }
        Vec::new()
    }

    fn lookup(&self, crate_root: &str, qualified: &str) -> Vec<&'facts ImplTargetFact> {
        self.by_qualified
            .get(&(crate_root, qualified))
            .cloned()
            .unwrap_or_default()
    }
}

/// Resolves the import path a module-item `use` binds for `simple_name` in the
/// impl's exact module scope (issue #393), or `None` when no such import exists
/// or the scope binds the name to two DISTINCT paths (defensive — a name
/// collision in real Rust; never silently pick one). Scoping mirrors the
/// import-shadow veto exactly (own module scope only; ancestor/block-local
/// imports are never captured here), so recall recovery cannot reintroduce a
/// wrong edge.
fn lookup_use_import<'a>(
    use_imports: &'a [UseImportFact],
    module_names: &[String],
    simple_name: &str,
) -> Option<&'a str> {
    let mut found: Option<&str> = None;
    for import in use_imports {
        if import.module_names == module_names && import.simple_name == simple_name {
            match found {
                None => found = Some(import.resolved_path.as_str()),
                Some(existing) if existing != import.resolved_path => return None,
                Some(_) => {}
            }
        }
    }
    found
}

/// Reports whether a captured `use`-import path is EXPLICITLY in-repo-rooted —
/// its first `::`-separated segment is `crate`, `self`, or `super` (Codex P2 on
/// PR #399). Per Rust 2018+ path resolution these roots unambiguously denote the
/// local crate inside a `use` path. Such a path always resolves in-repo; it is
/// the always-true half of the import-aware resolution gate, complementing
/// [`use_path_first_segment_is_extern_prelude`] (which vetoes bare extern-crate
/// imports). (`Self` is never valid as a `use`-path root, so it need not be
/// listed.)
fn use_path_is_in_repo_rooted(path: &str) -> bool {
    matches!(
        path.split("::").next().unwrap_or(path),
        "crate" | "self" | "super"
    )
}

/// Reports whether a captured `use`-import path's first `::`-separated segment is
/// a KNOWN extern-prelude crate name — `std`, `core`, `alloc`, `proc_macro`, or
/// `test` (the crates Rust injects into the extern prelude). Such a bare import
/// (`use std::fmt::Display;`, `use core::fmt::Debug;`) names an external crate per
/// Rust 2018+ path resolution, NEVER a local module that merely shares the name;
/// import-aware in-repo resolution (issue #393) must NOT fire for it, otherwise a
/// repo that coincidentally defines a same-path local module (`mod std::fmt`)
/// steals the extern import and mints a WRONG `IMPLEMENTS` edge, breaking PR
/// #389's no-wrong-edge invariant. Every OTHER bare first segment
/// (`a::T`, `serde::Serialize`) is a potential local crate-root module and is
/// resolved against the crate-root-partitioned index — a genuine external
/// dependency yields no edge simply because no local target matches, no
/// dependency list required. This closed exclusion set is what distinguishes an
/// extern-prelude crate import from a valid bare crate-root-local import (the
/// recall Codex "crate-root local imports" (PR #399) asked to recover).
///
/// Accepted rare bound: a LOCAL crate-root module whose name collides with one of
/// these extern-prelude crates (e.g. a hand-rolled `mod core`) is ambiguous /
/// invalid Rust and is treated as external here — out of scope.
fn use_path_first_segment_is_extern_prelude(path: &str) -> bool {
    matches!(
        path.split("::").next().unwrap_or(path),
        "std" | "core" | "alloc" | "proc_macro" | "test"
    )
}

/// Normalizes an absolute `crate::`/`self::`/`super::` trait path to the
/// crate-root-relative qualified name the index keys on (issue #344), mirroring
/// the local resolver's `normalize_local_trait_path`.
///
/// `crate::` is taken from the crate root; `self::`/`super::` resolve against
/// the impl's enclosing module path. Returns `None` for a relative-qualified
/// path (`sibling::T`, handled by the scope walk instead) and for a `super::`
/// chain that walks above the file's module scope.
fn normalize_absolute_trait_path(target: &str, module_names: &[String]) -> Option<String> {
    if let Some(rest) = target.strip_prefix("crate::") {
        return Some(rest.to_owned());
    }
    if let Some(rest) = target.strip_prefix("self::") {
        return Some(if module_names.is_empty() {
            rest.to_owned()
        } else {
            format!("{}::{rest}", module_names.join("::"))
        });
    }
    if !target.starts_with("super::") {
        return None;
    }
    let mut remaining = target;
    let mut modules: &[String] = module_names;
    while let Some(rest) = remaining.strip_prefix("super::") {
        let (_, init) = modules.split_last()?;
        modules = init;
        remaining = rest;
    }
    Some(if modules.is_empty() {
        remaining.to_owned()
    } else {
        format!("{}::{remaining}", modules.join("::"))
    })
}

/// Attaches a [`CallResolution`] status to same-file `CALLS` edges emitted by
/// the per-file reference pass (issue #134).
///
/// For every Tree-sitter call site whose candidate set includes a definition
/// in the caller's own file, the (caller, target) pair is labeled `resolved`
/// (exactly one in-repo candidate) or `ambiguous` (two or more in-repo
/// candidates) on the already-emitted per-file edge. Edge IDs, sources,
/// targets, and summaries are untouched, so stable-ID contracts hold; an
/// `ambiguous` label also clears the asserted `1.0` confidence, matching the
/// repo-wide pass. Per-file `CALLS` edges with no corresponding call site
/// (e.g. calls inside macro token trees, constructor-style matches) keep no
/// resolution field — absence means "outside the resolution contract", never
/// "resolved".
///
/// The pass is deterministic: pair statuses come from `BTreeMap` iteration
/// and repeated call sites for one pair keep the strongest status.
pub fn label_same_file_call_resolutions(
    records: &mut [GraphRecord],
    facts_by_file: &BTreeMap<String, FileFacts>,
) {
    let resolutions = same_file_call_resolutions(facts_by_file);
    if resolutions.is_empty() {
        return;
    }
    for record in records {
        let GraphRecord::Edge {
            label: EdgeLabel::Calls,
            source,
            target,
            confidence,
            resolution,
            ..
        } = record
        else {
            continue;
        };
        if resolution.is_some() {
            continue;
        }
        let Some(status) = resolutions.get(&(source.clone(), target.clone())) else {
            continue;
        };
        *resolution = Some(*status);
        if *status == CallResolution::Ambiguous {
            *confidence = None;
        }
    }
}

/// Computes the resolution status for every same-file (caller, target) call
/// pair backed by a Tree-sitter call site.
///
/// Candidate counting is repo-wide (a same-file call whose simple name also
/// matches definitions in other files is `ambiguous`), but only pairs whose
/// candidate lives in the caller's file are returned — cross-file pairs are
/// emitted with their status by [`cross_file_call_records`].
fn same_file_call_resolutions(
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> BTreeMap<(String, String), CallResolution> {
    let index = DefinitionIndex::build(facts_by_file);
    let mut resolutions = BTreeMap::new();
    for (path, facts) in facts_by_file {
        for call in &facts.call_sites {
            let Some(simple_name) = call.callee_segments.last() else {
                continue;
            };
            let candidates = index.candidates(call, simple_name);
            let status = match candidates.len() {
                0 => continue,
                1 => CallResolution::Resolved,
                _ => CallResolution::Ambiguous,
            };
            for candidate in candidates {
                if candidate.repo_relative_path != *path || candidate.id == call.caller_id {
                    continue;
                }
                let entry = resolutions
                    .entry((call.caller_id.clone(), candidate.id.clone()))
                    .or_insert(status);
                // Prefer the strongest status when several call sites hit one pair.
                if status < *entry {
                    *entry = status;
                }
            }
        }
    }
    resolutions
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
                        ..FileFacts::default()
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

    fn per_file_calls_edge(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Calls,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            format!("{source} calls {target}"),
        )
    }

    #[test]
    fn same_file_unique_call_pair_is_labeled_resolved() {
        let facts = facts(&[(
            "src/a.rs",
            vec![definition(
                "helper",
                "function",
                "src/a.rs",
                &["a", "helper"],
            )],
            vec![call(
                "caller",
                "helper",
                &["helper"],
                CallKind::Direct,
                None,
            )],
        )]);
        let mut records = vec![per_file_calls_edge("caller", "helper")];
        label_same_file_call_resolutions(&mut records, &facts);
        assert_eq!(records[0].resolution(), Some(CallResolution::Resolved));
        assert!(
            matches!(
                &records[0],
                GraphRecord::Edge {
                    confidence: Some(confidence),
                    ..
                } if confidence == "1.0"
            ),
            "a resolved edge keeps its asserted confidence"
        );
    }

    #[test]
    fn same_file_collision_pair_is_labeled_ambiguous_and_drops_confidence() {
        let facts = facts(&[
            (
                "src/a.rs",
                vec![definition("a-dupe", "function", "src/a.rs", &["a", "dupe"])],
                vec![call("caller", "dupe", &["dupe"], CallKind::Direct, None)],
            ),
            (
                "src/b.rs",
                vec![definition("b-dupe", "function", "src/b.rs", &["b", "dupe"])],
                vec![],
            ),
        ]);
        let mut records = vec![per_file_calls_edge("caller", "a-dupe")];
        label_same_file_call_resolutions(&mut records, &facts);
        assert_eq!(records[0].resolution(), Some(CallResolution::Ambiguous));
        assert!(
            matches!(
                &records[0],
                GraphRecord::Edge {
                    confidence: None,
                    ..
                }
            ),
            "an ambiguous edge must not assert full confidence: {:?}",
            records[0]
        );
    }

    #[test]
    fn edges_without_a_call_site_or_with_a_status_are_untouched() {
        let facts = facts(&[(
            "src/a.rs",
            vec![definition(
                "helper",
                "function",
                "src/a.rs",
                &["a", "helper"],
            )],
            vec![call(
                "caller",
                "helper",
                &["helper"],
                CallKind::Direct,
                None,
            )],
        )]);
        // A macro-arg style per-file edge (no Tree-sitter call site) and an
        // already-labeled cross-file edge must both stay as-is.
        let mut records = vec![
            per_file_calls_edge("other-caller", "helper"),
            per_file_calls_edge("caller", "helper").with_resolution(CallResolution::Unresolved),
        ];
        label_same_file_call_resolutions(&mut records, &facts);
        assert_eq!(
            records[0].resolution(),
            None,
            "a pair with no call-site backing stays outside the resolution contract"
        );
        assert_eq!(records[1].resolution(), Some(CallResolution::Unresolved));
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

    // --- Cross-file IMPLEMENTS resolution (issue #344) ---------------------

    fn impl_target(id: &str, qualified: &str, module: &[&str], kind: &str) -> ImplTargetFact {
        impl_target_in(id, qualified, module, kind, "lib")
    }

    fn impl_target_in(
        id: &str,
        qualified: &str,
        module: &[&str],
        kind: &str,
        crate_root: &str,
    ) -> ImplTargetFact {
        ImplTargetFact {
            id: id.to_owned(),
            qualified_name: qualified.to_owned(),
            crate_root: crate_root.to_owned(),
            module_path: module.iter().map(|s| (*s).to_owned()).collect(),
            symbol_kind: kind.to_owned(),
        }
    }

    fn pending_impl(source_id: &str, trait_path: &str, module: &[&str]) -> PendingImplFact {
        pending_impl_in(source_id, trait_path, module, "lib")
    }

    fn pending_impl_in(
        source_id: &str,
        trait_path: &str,
        module: &[&str],
        crate_root: &str,
    ) -> PendingImplFact {
        PendingImplFact {
            source_id: source_id.to_owned(),
            trait_path: trait_path.to_owned(),
            crate_root: crate_root.to_owned(),
            module_names: module.iter().map(|s| (*s).to_owned()).collect(),
            shadowed_by_use: false,
        }
    }

    fn impl_facts(
        entries: &[(&str, Vec<ImplTargetFact>, Vec<PendingImplFact>)],
    ) -> BTreeMap<String, FileFacts> {
        entries
            .iter()
            .map(|(path, impl_targets, pending_impls)| {
                (
                    (*path).to_owned(),
                    FileFacts {
                        impl_targets: impl_targets.clone(),
                        pending_impls: pending_impls.clone(),
                        ..FileFacts::default()
                    },
                )
            })
            .collect()
    }

    fn implements_pairs(records: &[GraphRecord]) -> Vec<(String, String)> {
        records
            .iter()
            .filter_map(|record| match record {
                GraphRecord::Edge {
                    label: EdgeLabel::Implements,
                    source,
                    target,
                    ..
                } => Some((source.clone(), target.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn cross_file_absolute_crate_path_edge_backs() {
        // `impl crate::T for Foo` in src/m.rs; trait T defined at the crate
        // root in src/lib.rs.
        let facts = impl_facts(&[
            (
                "src/lib.rs",
                vec![impl_target("trait-T", "T", &[], "trait")],
                vec![],
            ),
            (
                "src/m.rs",
                vec![impl_target("struct-Foo", "m::Foo", &["m"], "struct")],
                vec![pending_impl("impl-Foo", "crate::T", &["m"])],
            ),
        ]);
        let records = cross_file_implements_records("repo", &facts);
        assert_eq!(
            implements_pairs(&records),
            vec![("impl-Foo".to_owned(), "trait-T".to_owned())],
            "the out-of-line impl edge-backs to the crate-root trait: {records:?}"
        );
    }

    #[test]
    fn cross_file_unqualified_trait_walks_outward() {
        // `impl Draw for Button` in src/widgets.rs resolves outward through
        // the module scope to the crate-root trait `Draw`.
        let facts = impl_facts(&[
            (
                "src/lib.rs",
                vec![impl_target("trait-Draw", "Draw", &[], "trait")],
                vec![],
            ),
            (
                "src/widgets.rs",
                vec![impl_target(
                    "struct-Button",
                    "widgets::Button",
                    &["widgets"],
                    "struct",
                )],
                vec![pending_impl("impl-Button", "Draw", &["widgets"])],
            ),
        ]);
        let records = cross_file_implements_records("repo", &facts);
        assert_eq!(
            implements_pairs(&records),
            vec![("impl-Button".to_owned(), "trait-Draw".to_owned())]
        );
    }

    #[test]
    fn cross_file_ambiguous_trait_mints_no_edge() {
        // Two distinct definitions share the crate-root qualified name `T`:
        // the impl's trait path is ambiguous, so no edge is minted (ambiguity
        // never silently picks one).
        let facts = impl_facts(&[
            (
                "src/a.rs",
                vec![impl_target("trait-T-a", "T", &[], "trait")],
                vec![],
            ),
            (
                "src/b.rs",
                vec![impl_target("trait-T-b", "T", &[], "trait")],
                vec![],
            ),
            (
                "src/m.rs",
                vec![impl_target("struct-Foo", "m::Foo", &["m"], "struct")],
                vec![pending_impl("impl-Foo", "crate::T", &["m"])],
            ),
        ]);
        let records = cross_file_implements_records("repo", &facts);
        assert!(
            implements_pairs(&records).is_empty(),
            "an ambiguous trait path mints no edge: {records:?}"
        );
    }

    #[test]
    fn cross_file_bare_trait_with_ambiguous_simple_name_is_unresolved() {
        // `impl T for Foo` in src/m.rs is a bare (unqualified) trait name that,
        // via `use crate::a::T`, means `a::T` — but the module-scope outward
        // walk cannot see the import and would reach the root `T` at depth 0.
        // Because the simple name `T` is ambiguous across the repo trait index
        // (root `T` and `a::T`), the reference is left UNRESOLVED rather than
        // mis-bound to the root trait (a wrong-target edge). Matches the
        // documented `local_traits_only` use-alias bound.
        let facts = impl_facts(&[
            (
                "src/lib.rs",
                vec![impl_target("trait-T-root", "T", &[], "trait")],
                vec![],
            ),
            (
                "src/a.rs",
                vec![impl_target("trait-T-a", "a::T", &["a"], "trait")],
                vec![],
            ),
            (
                "src/m.rs",
                vec![impl_target("struct-Foo", "m::Foo", &["m"], "struct")],
                vec![pending_impl("impl-Foo", "T", &["m"])],
            ),
        ]);
        let records = cross_file_implements_records("repo", &facts);
        assert!(
            implements_pairs(&records).is_empty(),
            "an ambiguous bare trait name mints no edge: {records:?}"
        );
    }

    #[test]
    fn cross_file_bare_inherent_impl_with_ambiguous_type_name_is_unresolved() {
        // `impl Foo {}` in src/m.rs is a non-generic inherent impl whose pending
        // trait path is the TYPE name `Foo` (via `use crate::a::Foo`). The
        // module-scope outward walk cannot see the import and would reach the
        // root `Foo` struct at depth 0. Because the simple name `Foo` is
        // ambiguous across the repo impl-target index (root `Foo` and `a::Foo` —
        // both STRUCTS, no trait involved), the reference is left UNRESOLVED
        // rather than mis-bound to the root struct (a wrong-target edge). The
        // guard must count type-defining impl targets, not only traits.
        let facts = impl_facts(&[
            (
                "src/lib.rs",
                vec![impl_target("struct-Foo-root", "Foo", &[], "struct")],
                vec![],
            ),
            (
                "src/a.rs",
                vec![impl_target("struct-Foo-a", "a::Foo", &["a"], "struct")],
                vec![],
            ),
            (
                "src/m.rs",
                vec![],
                vec![pending_impl("impl-Foo", "Foo", &["m"])],
            ),
        ]);
        let records = cross_file_implements_records("repo", &facts);
        assert!(
            implements_pairs(&records).is_empty(),
            "an ambiguous bare inherent-impl type name mints no edge: {records:?}"
        );
    }

    #[test]
    fn cross_file_bare_name_ambiguity_counts_enum_and_type_alias() {
        // The ambiguity guard spans EVERY impl-target kind. An enum `Bar` and a
        // type alias `Bar` sharing the simple name across files is ambiguous, so
        // a bare inherent impl `impl Bar {}` mints no edge — the same guard that
        // covers traits and structs, proven for the remaining two kinds in one
        // sweep so the defect cannot return a target-kind at a time.
        let facts = impl_facts(&[
            (
                "src/lib.rs",
                vec![impl_target("enum-Bar-root", "Bar", &[], "enum")],
                vec![],
            ),
            (
                "src/a.rs",
                vec![impl_target("alias-Bar-a", "a::Bar", &["a"], "type_alias")],
                vec![],
            ),
            (
                "src/m.rs",
                vec![],
                vec![pending_impl("impl-Bar", "Bar", &["m"])],
            ),
        ]);
        let records = cross_file_implements_records("repo", &facts);
        assert!(
            implements_pairs(&records).is_empty(),
            "an enum + type-alias same-name collision is ambiguous, no edge: {records:?}"
        );
    }

    #[test]
    fn cross_file_external_trait_is_not_diagnosed() {
        // `impl std::fmt::Debug for Foo` resolves to nothing in-repo: no edge,
        // and — unlike the CALLS pass — no diagnostic (external traits are
        // external by construction, matching the implementors completeness
        // contract).
        let facts = impl_facts(&[(
            "src/m.rs",
            vec![impl_target("struct-Foo", "m::Foo", &["m"], "struct")],
            vec![pending_impl("impl-Foo", "std::fmt::Debug", &["m"])],
        )]);
        let records = cross_file_implements_records("repo", &facts);
        assert!(
            records.is_empty(),
            "an external trait impl mints no edge and no diagnostic: {records:?}"
        );
    }

    #[test]
    fn cross_file_implements_output_is_deterministic() {
        let facts = impl_facts(&[
            (
                "src/lib.rs",
                vec![
                    impl_target("trait-T", "T", &[], "trait"),
                    impl_target("trait-U", "U", &[], "trait"),
                ],
                vec![],
            ),
            (
                "src/m.rs",
                vec![impl_target("struct-Foo", "m::Foo", &["m"], "struct")],
                vec![
                    pending_impl("impl-Foo-T", "crate::T", &["m"]),
                    pending_impl("impl-Foo-U", "crate::U", &["m"]),
                ],
            ),
        ]);
        let first = cross_file_implements_records("repo", &facts);
        let second = cross_file_implements_records("repo", &facts);
        assert_eq!(first, second, "output must be byte-identical across runs");
        assert_eq!(implements_pairs(&first).len(), 2);
    }

    // --- crate-root partitioning (issue #394) ------------------------------

    #[test]
    fn crate_root_id_partitions_targets() {
        assert_eq!(crate_root_id("src/lib.rs"), "lib");
        assert_eq!(crate_root_id("src/main.rs"), "lib");
        assert_eq!(crate_root_id("src/a.rs"), "lib");
        assert_eq!(crate_root_id("src/foo/bar.rs"), "lib");
        assert_eq!(crate_root_id("src/bin/tool.rs"), "bin:tool");
        assert_eq!(crate_root_id("src/bin/tool/main.rs"), "bin:tool");
        assert_eq!(crate_root_id("src/bin/tool/helper.rs"), "bin:tool");
        assert_eq!(crate_root_id("examples/demo.rs"), "example:demo");
        assert_eq!(crate_root_id("examples/demo/main.rs"), "example:demo");
        assert_eq!(crate_root_id("tests/it.rs"), "test:it");
        assert_eq!(crate_root_id("benches/perf.rs"), "bench:perf");
        assert_eq!(crate_root_id("build.rs"), "build");
    }

    #[test]
    fn cross_file_multi_crate_root_same_name_trait_resolves_within_own_root() {
        // Two crate roots each define a root `trait T` (same qualified name `T`).
        // A lib impl `impl crate::T for Foo` must resolve to the LIBRARY `T`, and
        // a bin impl to the BIN `T`; the two never cross-pool (issue #394).
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("lib-T", "T", &[], "trait", "lib")],
                    ..FileFacts::default()
                },
            ),
            (
                "src/m.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in(
                        "lib-Foo",
                        "m::Foo",
                        &["m"],
                        "struct",
                        "lib",
                    )],
                    pending_impls: vec![pending_impl_in("impl-lib-Foo", "crate::T", &["m"], "lib")],
                    ..FileFacts::default()
                },
            ),
            (
                "src/bin/tool.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![
                        impl_target_in("bin-T", "T", &[], "trait", "bin:tool"),
                        impl_target_in("bin-Bar", "Bar", &[], "struct", "bin:tool"),
                    ],
                    pending_impls: vec![pending_impl_in(
                        "impl-bin-Bar",
                        "crate::T",
                        &[],
                        "bin:tool",
                    )],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let records = cross_file_implements_records("repo", &facts);
        let mut pairs = implements_pairs(&records);
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("impl-bin-Bar".to_owned(), "bin-T".to_owned()),
                ("impl-lib-Foo".to_owned(), "lib-T".to_owned()),
            ],
            "each crate root's impl binds its OWN root trait, never the other's: {records:?}"
        );
    }

    // --- import-aware bare-name resolution (issue #393) --------------------

    fn use_import(module: &[&str], simple: &str, path: &str) -> UseImportFact {
        UseImportFact {
            module_names: module.iter().map(|s| (*s).to_owned()).collect(),
            simple_name: simple.to_owned(),
            resolved_path: path.to_owned(),
        }
    }

    #[test]
    fn cross_file_bare_imported_trait_resolves_to_aliased_target() {
        // `use crate::a::T; impl T for Foo` in src/m.rs: the bare `T` names the
        // import `a::T`, NOT the root `T`. Import-aware resolution binds `a::T`.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("root-T", "T", &[], "trait", "lib")],
                    ..FileFacts::default()
                },
            ),
            (
                "src/a.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("a-T", "a::T", &["a"], "trait", "lib")],
                    ..FileFacts::default()
                },
            ),
            (
                "src/m.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("m-Foo", "m::Foo", &["m"], "struct", "lib")],
                    pending_impls: vec![PendingImplFact {
                        source_id: "impl-Foo".to_owned(),
                        trait_path: "T".to_owned(),
                        crate_root: "lib".to_owned(),
                        module_names: vec!["m".to_owned()],
                        shadowed_by_use: true,
                    }],
                    use_trait_imports: vec![use_import(&["m"], "T", "crate::a::T")],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let records = cross_file_implements_records("repo", &facts);
        assert_eq!(
            implements_pairs(&records),
            vec![("impl-Foo".to_owned(), "a-T".to_owned())],
            "the bare imported trait binds a::T, never the root T: {records:?}"
        );
    }

    #[test]
    fn cross_file_use_alias_rename_resolves() {
        // `use crate::a::T as U; impl U for Bar` binds a::T through the alias.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/a.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("a-T", "a::T", &["a"], "trait", "lib")],
                    ..FileFacts::default()
                },
            ),
            (
                "src/m.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("m-Bar", "m::Bar", &["m"], "struct", "lib")],
                    pending_impls: vec![PendingImplFact {
                        source_id: "impl-Bar".to_owned(),
                        trait_path: "U".to_owned(),
                        crate_root: "lib".to_owned(),
                        module_names: vec!["m".to_owned()],
                        shadowed_by_use: true,
                    }],
                    use_trait_imports: vec![use_import(&["m"], "U", "crate::a::T")],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let records = cross_file_implements_records("repo", &facts);
        assert_eq!(
            implements_pairs(&records),
            vec![("impl-Bar".to_owned(), "a-T".to_owned())],
            "the renamed import U binds a::T: {records:?}"
        );
    }

    #[test]
    fn cross_file_external_import_resolves_to_nothing() {
        // `use std::fmt::Display; impl Display for Foo` while a root `Display`
        // exists: the import path resolves to no in-repo target, so NO edge —
        // the root `Display` is never mis-bound (issue #393 preserves the veto).
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in(
                        "root-Display",
                        "Display",
                        &[],
                        "trait",
                        "lib",
                    )],
                    ..FileFacts::default()
                },
            ),
            (
                "src/m.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("m-Foo", "m::Foo", &["m"], "struct", "lib")],
                    pending_impls: vec![PendingImplFact {
                        source_id: "impl-Foo".to_owned(),
                        trait_path: "Display".to_owned(),
                        crate_root: "lib".to_owned(),
                        module_names: vec!["m".to_owned()],
                        shadowed_by_use: true,
                    }],
                    use_trait_imports: vec![use_import(&["m"], "Display", "std::fmt::Display")],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let records = cross_file_implements_records("repo", &facts);
        assert!(
            implements_pairs(&records).is_empty(),
            "an external/std import resolves to nothing, no edge: {records:?}"
        );
    }

    #[test]
    fn cross_file_extern_import_never_binds_coincident_local_module() {
        // Codex P2 on PR #399: `use std::fmt::Display; impl Display for Foo` is an
        // EXTERN-prelude import (bare first segment `std`), so it names external
        // crate std — NEVER a local module. When the repo COINCIDENTALLY defines
        // `mod std { mod fmt { trait Display {} } }` (qualified `std::fmt::Display`),
        // the pre-fix import-aware resolver ran the captured extern path through
        // the in-repo scope walk and mis-bound `Foo` to the local trait — a WRONG
        // IMPLEMENTS edge, breaking PR #389's no-wrong-edge invariant. No edge may
        // target the local `std::fmt::Display`.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/std_shadow.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in(
                        "local-std-fmt-Display",
                        "std::fmt::Display",
                        &["std", "fmt"],
                        "trait",
                        "lib",
                    )],
                    ..FileFacts::default()
                },
            ),
            (
                "src/m.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("m-Foo", "m::Foo", &["m"], "struct", "lib")],
                    pending_impls: vec![PendingImplFact {
                        source_id: "impl-Foo".to_owned(),
                        trait_path: "Display".to_owned(),
                        crate_root: "lib".to_owned(),
                        module_names: vec!["m".to_owned()],
                        shadowed_by_use: true,
                    }],
                    use_trait_imports: vec![use_import(&["m"], "Display", "std::fmt::Display")],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let records = cross_file_implements_records("repo", &facts);
        assert!(
            implements_pairs(&records).is_empty(),
            "an extern-prelude import must never bind a coincident local \
             module's same-path trait: {records:?}"
        );
    }

    #[test]
    fn cross_file_bare_crate_root_local_import_resolves_to_local_trait() {
        // Recall-regression guard (issue #393; Codex "crate-root local imports"
        // on PR #399): `use a::T; impl T for Foo` in src/m.rs where `a` is a
        // LOCAL crate-root module (`mod a { pub trait T {} }`) is valid Rust
        // 2018 and resolves to the local `a::T`. The round-1 fix gated
        // import-aware in-repo resolution to `crate::`/`self::`/`super::`-rooted
        // paths only, which dropped this bare root-local import and lost the
        // IMPLEMENTS edge. The first path segment `a` is NOT an extern-prelude
        // crate name, so the import must resolve to the local `a::T`.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("root-T", "T", &[], "trait", "lib")],
                    ..FileFacts::default()
                },
            ),
            (
                "src/a.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("a-T", "a::T", &["a"], "trait", "lib")],
                    ..FileFacts::default()
                },
            ),
            (
                "src/m.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("m-Foo", "m::Foo", &["m"], "struct", "lib")],
                    pending_impls: vec![PendingImplFact {
                        source_id: "impl-Foo".to_owned(),
                        trait_path: "T".to_owned(),
                        crate_root: "lib".to_owned(),
                        module_names: vec!["m".to_owned()],
                        shadowed_by_use: true,
                    }],
                    use_trait_imports: vec![use_import(&["m"], "T", "a::T")],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let records = cross_file_implements_records("repo", &facts);
        assert_eq!(
            implements_pairs(&records),
            vec![("impl-Foo".to_owned(), "a-T".to_owned())],
            "a bare crate-root-local `use a::T;` binds the local a::T, not the \
             root T: {records:?}"
        );
    }

    #[test]
    fn cross_file_bare_core_alloc_import_never_binds_coincident_local_module() {
        // The extern-prelude exclusion is not std-only: `core` and `alloc` are
        // extern-prelude crate names too. A `use core::fmt::Debug;` /
        // `use alloc::vec::Vec;` import names the external crate even when the
        // repo coincidentally defines a same-path local module — no edge.
        for extern_path in ["core::fmt::Debug", "alloc::vec::Vec"] {
            let (root_seg, _) = extern_path.split_once("::").unwrap();
            let qualified = extern_path.to_owned();
            let module: Vec<&str> = extern_path.split("::").take(2).collect();
            let facts: BTreeMap<String, FileFacts> = [
                (
                    "src/extern_shadow.rs".to_owned(),
                    FileFacts {
                        impl_targets: vec![impl_target_in(
                            "local-extern-target",
                            &qualified,
                            &module,
                            "trait",
                            "lib",
                        )],
                        ..FileFacts::default()
                    },
                ),
                (
                    "src/m.rs".to_owned(),
                    FileFacts {
                        impl_targets: vec![impl_target_in(
                            "m-Foo",
                            "m::Foo",
                            &["m"],
                            "struct",
                            "lib",
                        )],
                        pending_impls: vec![PendingImplFact {
                            source_id: "impl-Foo".to_owned(),
                            trait_path: "Target".to_owned(),
                            crate_root: "lib".to_owned(),
                            module_names: vec!["m".to_owned()],
                            shadowed_by_use: true,
                        }],
                        use_trait_imports: vec![use_import(&["m"], "Target", &qualified)],
                        ..FileFacts::default()
                    },
                ),
            ]
            .into_iter()
            .collect();
            let records = cross_file_implements_records("repo", &facts);
            assert!(
                implements_pairs(&records).is_empty(),
                "an extern-prelude `{root_seg}` import must never bind a \
                 coincident local module: {records:?}"
            );
        }
    }
}
