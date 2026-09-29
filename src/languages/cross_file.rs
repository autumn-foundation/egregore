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

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::crate_attribution::CrateAttributionIndex;
use crate::ir::{
    CallResolution, EdgeLabel, GraphRecord, NodeKind, SourceSpan, SymbolRole, stable_id,
};
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
    /// normalized impl owner for methods OR the enclosing trait name for
    /// trait methods, then the simple name).
    pub match_segments: Vec<String>,
    /// Symbol kind: `function`, `method`, or `test`.
    pub symbol_kind: String,
    /// True when this callable is a trait method — default-bodied or
    /// signature-only — declared inside a `trait` body (issue #390). Trait
    /// methods keep `symbol_kind == "function"` (PR #384's additive record-ID
    /// identity guarantee), so this marker is what distinguishes them from a
    /// free function: it admits them to the `Method`/`Path` candidate pools
    /// while EXCLUDING them from the bare free-function pool (`read()` can
    /// never invoke a trait method). Defaults to `false`.
    #[serde(default)]
    pub is_trait_method: bool,
    /// Repo-relative path of the defining file.
    pub repo_relative_path: String,
}

/// A block-local `fn` definition (issue #422).
///
/// Declared inside a function or closure BODY, hence lexically unreachable
/// from other scopes. It is NOT pooled into the flat module-level
/// [`DefinitionFact`] index (issue #413 round 3 closed that wrong-edge
/// vector); it is carried separately so the resolver can recall it ONLY for
/// calls in its own lexical scope.
///
/// The scope gate is [`BlockLocalDefinitionFact::enclosing_scope_id`]: this
/// def is a candidate solely for a bare (`Direct`) call whose `caller_id`
/// equals the enclosing scope (calls in the enclosing function/method body,
/// including closures and nested blocks, which inherit the enclosing caller
/// id) or equals the def's own id (a recursive call inside its own body). A
/// bare call from any OTHER scope never sees it — the #413 guard.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlockLocalDefinitionFact {
    /// Stable record ID of the Symbol node.
    pub id: String,
    /// Qualified display name (module-qualified, e.g. `alpha::helper`).
    pub qualified_name: String,
    /// Unqualified name (last path segment).
    pub simple_name: String,
    /// Symbol kind: `function` or `test` (a block-local `fn` is never a
    /// `method` — issue #413 corrected that attribution).
    pub symbol_kind: String,
    /// Stable record ID of the lexically-enclosing function/method/test
    /// symbol whose body contains this definition.
    pub enclosing_scope_id: String,
    /// Repo-relative path of the defining file.
    pub repo_relative_path: String,
}

/// A `macro_rules!` definition exported by the Rust extractor for repo-wide
/// macro-invocation resolution (issue #148).
///
/// The macro's body is NEVER carried: the trust boundary is parsed source
/// only, and macro bodies are not expanded.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct MacroDefinitionFact {
    /// Stable record ID of the macro Symbol node.
    pub id: String,
    /// Unqualified macro name, as written after `macro_rules!`.
    pub simple_name: String,
    /// Module-qualified display name (e.g. `helpers::greet`).
    pub qualified_name: String,
    /// Repo-relative path of the defining file.
    pub repo_relative_path: String,
}

/// A `macro_invocation` site whose resolution is deferred to the repo-wide
/// macro pass (issue #148).
///
/// Per-file extraction records the site but emits nothing for it: the
/// repo-wide [`cross_file_macro_records`] pass mints either a resolved
/// `CALLS` edge to the unique repo-defined macro or the pre-existing
/// `unsupported macro invocation` Diagnostic.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct MacroInvocationFact {
    /// Stable record ID of the invoking scope: the lexically-enclosing
    /// function symbol when the site sits inside one, else the enclosing
    /// module or the file itself.
    pub caller_id: String,
    /// Display name of the invoking scope.
    pub caller_name: String,
    /// Invocation as written, e.g. `println!` or `crate::helpers::greet!`.
    pub invocation_display: String,
    /// Simple macro name: the last `::` segment with the trailing `!`
    /// stripped.
    pub simple_name: String,
    /// Span of the invocation site.
    pub span: SourceSpan,
    /// Walk-order ordinal per invocation display, so a site that stays
    /// unresolved mints the same Diagnostic ID the pre-#148 per-file
    /// extractor minted (external-macro diagnostics stay byte-identical).
    pub diagnostic_disambiguator: u64,
    /// Repo-relative path of the invoking file.
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

/// How a path-qualified call names its leading crate scope (issue #440).
///
/// Preserved from the call's written form because [`normalize_call_path`] strips
/// the leading `crate`/`self`/`super` (and rewrites `Self`), which would
/// otherwise erase whether the path was crate-absolute or named another crate —
/// the signal the resolution pass needs to confine a qualified call to the right
/// workspace crate root instead of matching same-named symbols repo-wide.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallPathRoot {
    /// Not a path-qualified call (a bare/method call), or a legacy fact: the
    /// pool is matched repo-wide (the pre-#440 behavior). Default so a cache
    /// written before #440 deserializes to the unchanged matching.
    #[default]
    Unqualified,
    /// The path began with `crate`/`self`/`super`: it names the CALLER'S own
    /// crate, so resolution is confined to the caller's crate root.
    CurrentCrate,
    /// The path began with a bare segment (`dep_crate::…`, `mod_b::…`) that may
    /// name a workspace crate; the raw first segment is retained so the
    /// resolution pass can look it up in the workspace crate registry.
    Leading(String),
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
    /// How the call's leading path segment names its crate scope (issue #440).
    /// `#[serde(default)]` so a pre-#440 cache deserializes to `Unqualified`
    /// (repo-wide matching, unchanged).
    #[serde(default)]
    pub path_root: CallPathRoot,
    /// Normalized impl owner for `self`-receiver calls; `None` elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_owner: Option<String>,
    /// Provable nominal receiver type for a non-`self` identifier receiver
    /// (`let x: T = …; x.m()` or `fn f(x: T) { x.m() }`), reduced to a
    /// matchable nominal type path; `None` when the receiver's type is not
    /// syntactically provable (issue #441). Only stamped for `Method` calls
    /// whose receiver is a simple, unshadowed identifier binding. `self`
    /// receivers keep `receiver_owner` unchanged and never set this.
    /// `#[serde(default)]` so a pre-#441 cache deserializes with `None`
    /// (today's unnarrowed method fan-out, unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_type: Option<String>,
    /// Trait whose method is being dispatched (e.g., `Iterator` for
    /// `iter.next()` where the receiver's type implements `Iterator`).
    /// Used to narrow method call candidates to implementations of this
    /// trait. `None` for non-method calls or when the trait cannot be
    /// determined. `#[serde(default)]` so a pre-#267 cache deserializes
    /// with `None` (today's method fan-out, unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_trait: Option<String>,
    /// Source span of the call expression.
    pub span: SourceSpan,
}

/// A syntactic struct-literal construction site `Type { … }` found inside a
/// recorded symbol body (issue #443).
///
/// The constructing symbol is the enclosing fn/method; the constructed type is
/// the (normalized) type path the literal names. Resolution to the type's
/// definition Symbol — minting a `CONSTRUCTS` edge — happens in the repo-wide
/// [`cross_file_construct_records`] pass, mirroring the CALLS resolution model.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConstructSiteFact {
    /// Stable record ID of the constructing Symbol node (the enclosing fn/method).
    pub constructor_id: String,
    /// Qualified name of the constructing symbol.
    pub constructor_name: String,
    /// Constructed type as written in source (generic arguments dropped, e.g.
    /// `crate_a::Deal`, `Self`, `Shape::Circle`).
    pub type_display: String,
    /// Normalized constructed-type path segments (`crate`/`self`/`super`
    /// stripped, leading `Self` rewritten to the impl owner). The last segment
    /// is the simple type name (or, for an enum-struct variant literal, the
    /// variant name — the resolver retries after dropping it).
    pub type_segments: Vec<String>,
    /// How the type path's leading segment names its crate scope (issue #440),
    /// classified exactly as a path-qualified call is. `#[serde(default)]` so a
    /// pre-#443 cache deserializes to `Unqualified` (repo-wide matching).
    #[serde(default)]
    pub path_root: CallPathRoot,
    /// `true` when the literal is the E0063-breakable EXHAUSTIVE form — it has
    /// NO `..base` functional-record-update tail, so adding a required field to
    /// the struct breaks this site. `false` when the literal carries `..base`
    /// (a `base_field_initializer`), which absorbs new fields and does not break.
    pub is_exhaustive: bool,
    /// Source span of the struct-literal expression.
    pub span: SourceSpan,
}

/// One route-registration reference found inside a `routes![…]` macro
/// invocation (issue #445).
///
/// The registering symbol is the enclosing fn/method that owns the macro call;
/// the handler reference is a bare identifier the macro registers. Resolution to
/// the handler's definition Symbol — minting a `REGISTERS_ROUTE` edge — happens
/// in the repo-wide [`cross_file_route_records`] pass, mirroring the CALLS/
/// CONSTRUCTS resolution model (unique resolution only).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteRegistrationFact {
    /// Stable record ID of the registering Symbol node (the enclosing fn/method).
    pub owner_id: String,
    /// Qualified name of the registering symbol.
    pub owner_name: String,
    /// Handler reference as written in the macro (e.g. `list_contacts`).
    pub handler_display: String,
    /// Normalized handler-name path segments. In this slice each reference is a
    /// bare single-segment identifier (the simple handler name).
    pub handler_segments: Vec<String>,
    /// How the handler reference's leading segment names its crate scope; always
    /// `Unqualified` in this slice. `#[serde(default)]` so a pre-#445 cache
    /// deserializes to `Unqualified`.
    #[serde(default)]
    pub path_root: CallPathRoot,
    /// Source span of the handler identifier token.
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
    /// The declaration's full conditional-compilation gate chain
    /// (issue #190): the declaring file's `#![cfg(...)]` inner attributes,
    /// then enclosing gated items/modules outermost-first, then the `mod x;`
    /// item's own `#[cfg(...)]` / `#[cfg_attr(...)]` predicates. The
    /// repo-wide [`apply_out_of_line_cfg_gates`] pass prepends this chain to
    /// every record of the target file, so the module file's symbols carry
    /// the same gate the declaration does. Empty when the declaration is
    /// ungated — an ungated declaration contributes no inherited gates
    /// (mirroring the dual-use production-precedence rule: a file also
    /// loaded ungated compiles without the gate).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cfg_gates: Vec<String>,
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

/// A `impl Trait for Type` relation captured for the repo-wide
/// IMPLEMENTS-gated self-dispatch call-resolution join (issue #414).
///
/// Unlike [`PendingImplFact`] (recorded ONLY when local trait resolution
/// fails), this fact is emitted for EVERY trait impl — same-file and
/// cross-file — so the resolution pass has a comprehensive "type `S`
/// implements trait `T`" fact set. A `self.read()` call inside `impl S` binds
/// the default `T::read` ONLY when this index proves `S` implements a trait
/// whose crate-root-relative qualified name matches `T::read`'s owner segments
/// (`match_segments[..len-1]`). Inherent impls (`impl S {}`) and blanket/
/// negative impls mint no relation.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImplTraitRelationFact {
    /// Normalized simple LEAF name of the implementing Self type (`S` in
    /// `impl T for S`) — the map key and same-simple-name ambiguity guard.
    pub impl_type: String,
    /// Normalized implementing-type PATH as written, BEFORE the final leaf
    /// reduction (`std::string::String`, `String`, `crate::foo::S`), issue #414
    /// / Codex P2 on #420. Resolved against the repo-wide `ImplTargetIndex` so a
    /// relation whose Self does not resolve to a UNIQUE LOCAL type def (external
    /// like `std::string::String`, ambiguous, or unresolved) is dropped and
    /// never pollutes a same-leaf local type's implemented-trait set. Serde
    /// default (empty) reads back cleanly from a pre-field cache.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub impl_type_path: String,
    /// Normalized trait path as written on the impl header (same normalization
    /// `pending_impls` uses).
    pub trait_path: String,
    /// Crate root the impl belongs to (issue #394 partitioning), derived from
    /// the impl file's repo-relative path by [`crate_root_id`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub crate_root: String,
    /// The impl's enclosing module path (crate-root-relative).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_names: Vec<String>,
}

/// Cross-file resolution facts exported by one file's extraction.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileFacts {
    /// Callable definitions in the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub definitions: Vec<DefinitionFact>,
    /// Block-local `fn` definitions in the file (issue #422). Lexically
    /// scoped: candidates only for calls in their own enclosing scope, never
    /// pooled into [`FileFacts::definitions`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub block_local_definitions: Vec<BlockLocalDefinitionFact>,
    /// Call sites found inside recorded symbol bodies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_sites: Vec<CallSiteFact>,
    /// Struct-literal construction sites found inside recorded symbol bodies
    /// (issue #443).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub construct_sites: Vec<ConstructSiteFact>,
    /// Route-registration references found inside `routes![…]` macro
    /// invocations in recorded symbol bodies (issue #445).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_registration_sites: Vec<RouteRegistrationFact>,
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
    /// Comprehensive `impl Trait for Type` relations, for the IMPLEMENTS-gated
    /// self-dispatch call-resolution join (issue #414).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impl_trait_relations: Vec<ImplTraitRelationFact>,
    /// `macro_rules!` definitions in the file (issue #148), for the
    /// repo-wide macro-invocation resolution pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macro_definitions: Vec<MacroDefinitionFact>,
    /// `macro_invocation` sites in the file (issue #148), deferred to the
    /// repo-wide macro-invocation resolution pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macro_invocations: Vec<MacroInvocationFact>,
    /// Normalized `#[cfg(...)]` / `#[cfg_attr(...)]` predicates from `#![…]`
    /// inner attributes at the file top level (issue #190). Not a cross-file
    /// resolution fact — it rides `FileFacts` as the single channel from the
    /// per-file extractor back to the scan funnel, which stamps it on the
    /// `File` node. Skipped when empty so the incremental cache format is
    /// unchanged for ungated files; deliberately NOT part of `is_empty` (a
    /// file can carry gates without exporting resolution facts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_cfg_gates: Vec<String>,
}

impl FileFacts {
    /// Returns `true` when the file exported no cross-file resolution facts of
    /// any kind.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.definitions.is_empty()
            && self.block_local_definitions.is_empty()
            && self.call_sites.is_empty()
            && self.construct_sites.is_empty()
            && self.route_registration_sites.is_empty()
            && self.out_of_line_mods.is_empty()
            && self.impl_targets.is_empty()
            && self.pending_impls.is_empty()
            && self.use_trait_imports.is_empty()
            && self.impl_trait_relations.is_empty()
            && self.macro_definitions.is_empty()
            && self.macro_invocations.is_empty()
    }
}

// ── Import-path segment parsing ────────────────────────────────────────────
//
// Shared with the `eg query who-imports` lane (`query::who_imports`): the
// scan-time IMPORTS-target pass below and the query lane reduce an Import
// node's raw `name` to module-path segments with one implementation. These
// live in `languages::cross_file` — not in `query` — because the scan
// pipeline must not depend on the query layer.

/// Strips an optional leading Rust visibility + `use` keyword prefix from an
/// Import node's raw `name`, anchored at the very start.
///
/// The Rust extractor's `import_name` only trims a leading BARE `use`, so a
/// public re-export keeps its visibility on the Import node `name`:
/// `pub use crate::internal::Widget;` mints the literal name
/// `pub use crate::internal::Widget` (issue #449 Codex finding). Left as-is, the
/// first segment split on `::` becomes `pub use crate`, so every `crate::…`
/// re-export site is missed. This helper drops the keyword prefix before the
/// split: an optional `pub` visibility token (including a `pub(crate)` /
/// `pub(super)` / `pub(self)` / `pub(in path)` restriction) followed by the
/// `use` keyword, or a bare leading `use `. Only the anchored keyword prefix is
/// consumed — `pub` and `use` are reserved words and can never be module
/// segments, and a segment that merely starts with the substring `use` (e.g.
/// `used`) is not stripped — so this never over-strips a real path.
#[must_use]
pub fn strip_use_prefix(name: &str) -> &str {
    let trimmed = name.trim_start();
    // Optionally consume a leading `pub` visibility token, including a
    // `pub(...)` restriction. A bare `pub` counts only when followed by
    // whitespace or a `(` — otherwise it is part of a longer token and left be.
    let after_vis = trimmed
        .strip_prefix("pub")
        .map_or(trimmed, |rest| match rest.chars().next() {
            // Skip the balanced `(...)` restriction (visibility restrictions do
            // not nest, so the first `)` closes it).
            Some('(') => rest.find(')').map_or(rest, |idx| &rest[idx + 1..]),
            Some(c) if c.is_whitespace() => rest,
            _ => trimmed,
        })
        .trim_start();
    // Strip only when the `use` keyword is actually present (and is a whole
    // keyword, not the prefix of a longer identifier); otherwise the name is
    // already a bare path — return it unchanged.
    match after_vis.strip_prefix("use") {
        Some(rest) if rest.chars().next().is_none_or(char::is_whitespace) => rest.trim_start(),
        _ => name,
    }
}

/// Reduces an Import node's raw `name` path text to its module-path segment
/// list.
///
/// A group import `a::b::{C, D}` reduces to the common module prefix `a::b`; a
/// glob `a::b::*` reduces to `a::b`; a trailing ` as <alias>` rename is
/// stripped. A leading `pub`/visibility + `use` (or bare `use`) keyword prefix
/// left on a re-export node's `name` by the extractor is stripped first (see
/// [`strip_use_prefix`]). Empty segments (from a trailing `::`) and `*` are
/// dropped.
#[must_use]
pub fn parse_import_segments(name: &str) -> Vec<String> {
    // Drop any leading `[pub[(...)]] use` keyword prefix a re-export node kept.
    let name = strip_use_prefix(name);
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

/// Returns `true` for the file a crate-root group's imports resolve against:
/// the single file the group's module tree hangs off. Prefix-aware, so
/// workspace members (`crates/foo/src/lib.rs`) classify exactly like
/// single-crate layouts.
fn is_crate_entry_file(path: &str) -> bool {
    let (_, remainder) = split_crate_prefix(path);
    let segments: Vec<&str> = remainder.iter().map(String::as_str).collect();
    let is_rs_file = |name: &str| {
        std::path::Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
    };
    match segments.as_slice() {
        ["build.rs"] | ["src", "lib.rs" | "main.rs"] => true,
        ["src", "bin", rest @ ..] | ["examples" | "tests" | "benches", rest @ ..] => match rest {
            [file] => is_rs_file(file),
            [_, "main.rs"] => true,
            _ => false,
        },
        _ => false,
    }
}

/// What an import-target `IMPORTS` edge points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportTargetKind {
    /// An inline `mod name { … }`: the edge targets its `Module` node.
    Module,
    /// An out-of-line `mod name;` (or the crate root itself): the edge targets
    /// the module body's `File` node.
    File,
}

/// One resolved import target: the edge target plus its display handle.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedImportTarget {
    target_id: String,
    /// Crate-root-relative module path (`a::b`) for a module target, the
    /// repo-relative file path for a file target.
    display: String,
}

/// Deterministic resolution index for the issue-#444 import-target pass.
struct ImportTargetIndex {
    repository_id: String,
    /// Every scanned repo-relative path, so out-of-line module targets resolve
    /// only to files the scan actually indexed.
    known_paths: std::collections::BTreeSet<String>,
    /// Inline `Module` nodes keyed by `(file, file-relative qualified name)`.
    inline_modules: BTreeMap<(String, String), String>,
    /// `(file, qualified name)` pairs claimed by two Module nodes: never
    /// resolved (fail closed).
    ambiguous_modules: std::collections::BTreeSet<(String, String)>,
    /// Crate-root group id → its single entry file. Groups with zero or
    /// several entry files (the `src/lib.rs` + `src/main.rs` pooling bound,
    /// issue #394) are absent: their imports resolve nothing.
    entries: BTreeMap<String, String>,
    /// Crate name → owning group id, for absolute `<crate_name>::…` imports.
    /// A name claimed by two groups is dropped (ambiguous).
    name_to_group: BTreeMap<String, String>,
}

impl ImportTargetIndex {
    /// Claims `name` for `group`, dropping it as ambiguous when another group
    /// already claimed it.
    fn claim_name(
        name_to_group: &mut BTreeMap<String, String>,
        ambiguous: &mut std::collections::BTreeSet<String>,
        name: String,
        group: &str,
    ) {
        if ambiguous.contains(&name) {
            return;
        }
        match name_to_group.get(&name) {
            None => {
                name_to_group.insert(name, group.to_owned());
            }
            Some(existing) if existing == group => {}
            Some(_) => {
                name_to_group.remove(&name);
                ambiguous.insert(name);
            }
        }
    }

    fn build(
        repository_id: &str,
        records: &[GraphRecord],
        facts_by_file: &BTreeMap<String, FileFacts>,
        attribution: &CrateAttributionIndex,
    ) -> Self {
        let mut known_paths: std::collections::BTreeSet<String> =
            facts_by_file.keys().cloned().collect();
        let mut inline_modules: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut ambiguous_modules: std::collections::BTreeSet<(String, String)> =
            std::collections::BTreeSet::new();
        for record in records {
            if let GraphRecord::Node {
                id,
                kind,
                repo_relative_path: Some(path),
                name,
                language,
                ..
            } = record
            {
                known_paths.insert(path.clone());
                if *kind == NodeKind::Module
                    && language.as_deref() == Some("rust")
                    && let Some(module_name) = name
                {
                    let key = (path.clone(), module_name.clone());
                    if inline_modules.insert(key.clone(), id.clone()).is_some() {
                        ambiguous_modules.insert(key);
                    }
                }
            }
        }

        // Crate-root groups over Rust files; the entry file anchors each
        // group's module walk.
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for path in &known_paths {
            let is_rs = std::path::Path::new(path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"));
            if !is_rs {
                continue;
            }
            groups
                .entry(crate_root_id(path))
                .or_default()
                .push(path.clone());
        }
        let mut entries: BTreeMap<String, String> = BTreeMap::new();
        for (group_id, files) in &groups {
            let mut entry_files = files.iter().filter(|p| is_crate_entry_file(p));
            if let (Some(entry), None) = (entry_files.next(), entry_files.next()) {
                entries.insert(group_id.clone(), entry.clone());
            }
        }

        // Crate-name → group claims.
        let mut name_to_group: BTreeMap<String, String> = BTreeMap::new();
        let mut ambiguous_names: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for (group_id, entry) in &entries {
            let base = group_id.rsplit("::").next().unwrap_or(group_id.as_str());
            let aux_target = base
                .strip_prefix("bin:")
                .or_else(|| base.strip_prefix("example:"))
                .or_else(|| base.strip_prefix("test:"))
                .or_else(|| base.strip_prefix("bench:"));
            if let Some(target) = aux_target {
                // An auxiliary target compiles as its OWN crate named for the
                // target (`examples/demo.rs` → the `demo` crate).
                Self::claim_name(
                    &mut name_to_group,
                    &mut ambiguous_names,
                    target.replace('-', "_"),
                    group_id,
                );
                continue;
            }
            if base == "build" {
                // `build.rs` is its own crate, but an absolute `<pkg>::…`
                // import written there names the LIBRARY crate, not the build
                // script — claiming the package name here would resolve wrong.
                continue;
            }
            // Primary crate: the manifest-stamped owning package name (raw and
            // cargo-normalized, so `use foo_bar::…` reaches package `foo-bar`)
            // plus the path-derived workspace-directory name for graphs whose
            // records predate attribution.
            if let Some((package, _manifest)) =
                attribution.attribution_for(entry).owning_package_for(entry)
            {
                Self::claim_name(
                    &mut name_to_group,
                    &mut ambiguous_names,
                    package.to_owned(),
                    group_id,
                );
                Self::claim_name(
                    &mut name_to_group,
                    &mut ambiguous_names,
                    package.replace('-', "_"),
                    group_id,
                );
            }
            if let Some(derived) = crate_name_of(entry) {
                Self::claim_name(&mut name_to_group, &mut ambiguous_names, derived, group_id);
            }
        }

        Self {
            repository_id: repository_id.to_owned(),
            known_paths,
            inline_modules,
            ambiguous_modules,
            entries,
            name_to_group,
        }
    }

    /// The stable `File` node id the scan mints for `path`
    /// (`lib.rs` `scan_source_text_records`).
    fn file_node_id(&self, path: &str) -> String {
        stable_id(&["node", "file", self.repository_id.as_str(), path])
    }

    /// Resolves one Rust import to its imported Module/File target, or `None`
    /// when any step is ungrounded. Fail-closed throughout: no edge is better
    /// than a wrong edge.
    fn resolve_import(
        &self,
        importer_path: &str,
        raw_name: &str,
        facts_by_file: &BTreeMap<String, FileFacts>,
    ) -> Option<ResolvedImportTarget> {
        let segments = parse_import_segments(raw_name);
        let first = segments.first()?;
        // `self::` / `super::` are module-relative: the graph carries no anchor
        // for the importing file's own module path at scan time, so these
        // stay unresolved (documented gap, matching the who-imports lane).
        if first == "self" || first == "super" {
            return None;
        }
        // `crate::…` resolves within the importing file's own crate-root
        // group — no crate name needed. An absolute `<name>::…` resolves to
        // the single group claiming the name; external crates (`std`,
        // `serde`, …) and ambiguous names resolve nothing.
        let group_id = if first == "crate" {
            crate_root_id(importer_path)
        } else {
            self.name_to_group.get(first)?.clone()
        };
        let entry = self.entries.get(&group_id)?;

        // Walk the module segments from the entry file. `module_path` is the
        // crate-root-relative path of the module currently descended into;
        // `file_module_path` is the crate-root-relative path of the module
        // whose body the current file holds (a prefix of `module_path`).
        let mut file = entry.clone();
        let mut module_path: Vec<String> = Vec::new();
        let mut file_module_path: Vec<String> = Vec::new();
        let mut visited_files: Vec<String> = vec![file.clone()];
        // The deepest resolvable module: `None` until the first module
        // segment resolves; a trailing symbol leaf (`Baz` in `crate::a::Baz`)
        // keeps the enclosing module as the target.
        let mut target: Option<(ImportTargetKind, String)> = None;
        for seg in &segments[1..] {
            let rel_start = file_module_path.len();
            if module_path.len() < rel_start {
                return None;
            }
            let mut child_rel: Vec<String> = module_path[rel_start..].to_vec();
            child_rel.push(seg.clone());
            let child_name = child_rel.join("::");
            let key = (file.clone(), child_name);
            let inline_id = if self.ambiguous_modules.contains(&key) {
                None
            } else {
                self.inline_modules.get(&key)
            };
            let out_of_line: Vec<&OutOfLineModFact> =
                facts_by_file.get(&file).map_or(Vec::new(), |facts| {
                    facts
                        .out_of_line_mods
                        .iter()
                        .filter(|fact| {
                            fact.name == *seg
                                && fact.inline_module_path == module_path[rel_start..].to_vec()
                        })
                        .collect()
                });
            match (inline_id, out_of_line.as_slice()) {
                (Some(id), []) => {
                    // A purely inline module: no out-of-line declaration
                    // competes for the name.
                    module_path.push(seg.clone());
                    target = Some((ImportTargetKind::Module, id.clone()));
                }
                (_, [fact]) => {
                    // Out-of-line `mod name;` wins over the declaration's own
                    // Module node (the extractor mints both): the imported
                    // items live in the body file, so the edge targets it.
                    let next = resolve_out_of_line_target(&file, fact, &self.known_paths)?;
                    // A `#[path]` cycle would loop the walk: fail closed.
                    if visited_files.contains(&next) {
                        return None;
                    }
                    visited_files.push(next.clone());
                    module_path.push(seg.clone());
                    file.clone_from(&next);
                    file_module_path.clone_from(&module_path);
                    target = Some((ImportTargetKind::File, self.file_node_id(&next)));
                }
                // Unresolvable, or contradictory (two competing declarations):
                // stop at the deepest resolved module.
                _ => break,
            }
        }

        let (target_id, display) = match target {
            Some((ImportTargetKind::Module, id)) => (id, module_path.join("::")),
            Some((ImportTargetKind::File, id)) => (id, file.clone()),
            None => {
                // The import named the crate root itself (`use mycrate;`):
                // target the entry file. Anything else with no resolved
                // module segment resolves nothing.
                if segments.len() == 1 {
                    (self.file_node_id(entry), entry.clone())
                } else {
                    return None;
                }
            }
        };
        Some(ResolvedImportTarget { target_id, display })
    }
}

/// Mints `IMPORTS` edges from each importing file to the imported Module/File
/// target (issue #444).
///
/// Every `File —IMPORTS→ Import` edge the extractors mint says "this file
/// declares this `use`"; this pass adds the target-side edge
/// `File —IMPORTS→ Module|File` for each Rust import it can resolve to an
/// in-repo module, so "which files import module X" is answerable by
/// traversing inbound `IMPORTS` edges of the module's node — no `jq` over the
/// raw JSONL.
///
/// Resolution is fail-closed: an import mints an edge only when every step is
/// grounded in facts the graph already carries (inline `Module` nodes,
/// resolved out-of-line `mod` declarations, crate-root partitioning, owning
/// package attribution). Documented bounds — no edge rather than a wrong
/// edge — are listed on [`ImportTargetIndex::resolve_import`]'s semantics
/// above: Rust only, absolute paths only (`crate::…` or `<crate_name>::…`),
/// deepest resolvable module wins, external/unresolvable/ambiguous paths
/// mint nothing, and groups without exactly one entry file resolve nothing.
///
/// Output is deterministic: edges are deduplicated by stable edge id
/// (`edge/IMPORTS/source/target`) and returned sorted by id, so repeated scans
/// of an unchanged tree are byte-identical.
#[must_use]
pub fn cross_file_import_target_edges(
    repository_id: &str,
    records: &[GraphRecord],
    facts_by_file: &BTreeMap<String, FileFacts>,
    attribution: &CrateAttributionIndex,
) -> Vec<GraphRecord> {
    let index = ImportTargetIndex::build(repository_id, records, facts_by_file, attribution);
    // Edge id → record, deduplicating the repeat-import case (`use
    // crate::a::X; use crate::a::Y;` in one file mints one edge).
    let mut edges: BTreeMap<String, GraphRecord> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Import,
            name: Some(raw_name),
            repo_relative_path: Some(importer_path),
            language,
            ..
        } = record
        else {
            continue;
        };
        if language.as_deref() != Some("rust") {
            continue;
        }
        let Some(resolved) = index.resolve_import(importer_path, raw_name, facts_by_file) else {
            continue;
        };
        let source = index.file_node_id(importer_path);
        let summary = format!(
            "{importer_path} imports {} (Rust import {raw_name})",
            resolved.display
        );
        let edge = GraphRecord::edge(
            EdgeLabel::Imports,
            source,
            resolved.target_id,
            Some("1.0".to_owned()),
            summary,
        );
        edges.insert(edge.id().to_owned(), edge);
    }
    edges.into_values().collect()
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
    let known_paths = known_file_paths(records, facts_by_file);
    let test_files = test_only_module_files(facts_by_file, &known_paths);
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

/// Stamps the test-vs-production role on `File` AND `Symbol` records for
/// out-of-line `#[cfg(test)]`-gated modules (issue #238).
///
/// The per-file extractor stamps each `File` record's role from its path
/// alone and each `Symbol` record's role from its lexical signals alone, so a
/// `#[cfg(test)] mod helpers;` declaration (whose body lives in
/// `src/helpers.rs`) would leave that file — and every symbol it defines —
/// `Production` even though the module only compiles under `cfg(test)`.
/// Issue #238's signal (b) explicitly covers out-of-line modules, so this
/// deterministic repo-wide pass upgrades those `File` records AND the
/// `Symbol` records they contain to `Test`, sharing issue #223's test-only
/// module resolution — including production-precedence for dual-use files,
/// so a module file also loaded by an ungated declaration (and its symbols)
/// keeps `Production`. `role` is never an identity input, so record IDs are
/// unchanged; records already `Test` (via path or lexical signals) are
/// untouched.
pub fn apply_out_of_line_test_roles(
    records: &mut [GraphRecord],
    facts_by_file: &BTreeMap<String, FileFacts>,
) {
    let known_paths = known_file_paths(records, facts_by_file);
    let test_files = test_only_module_files(facts_by_file, &known_paths);
    if test_files.is_empty() {
        return;
    }

    for record in records.iter_mut() {
        if let GraphRecord::Node {
            kind: NodeKind::File | NodeKind::Symbol,
            repo_relative_path: Some(path),
            role,
            ..
        } = record
            && test_files.contains(path.as_str())
        {
            *role = Some(SymbolRole::Test);
        }
    }
}

/// Propagates conditional-compilation gates across out-of-line module
/// declarations (issue #190).
///
/// A `#[cfg(feature = "x")] mod foo;` declaration gates the whole target
/// file, which is extracted with no view
/// of the gating attribute. The repo-wide pass prepends each declaration's
/// gate chain to every `File` / `Symbol` / `Module` record of the resolved
/// target file, outermost gate first.
///
/// Composition rule (deterministic, documented in `docs/cli/query.md`):
/// - A target file inherits the *effective* chain of each declaration loading
///   it: the declaring file's own inherited gates, then the declaration's
///   extracted chain (declaring file's `#![cfg]` inner attributes, enclosing
///   gated items outermost-first, the `mod x;` item's own predicates).
/// - Transitivity falls out of the fixpoint: when `a.rs` declares
///   `#[cfg(x)] mod b;` and `b.rs` declares `#[cfg(y)] mod c;`, `c`'s records
///   carry `[x, y]` — `b`'s inherited `[x]` prefixes `b`'s declaration chain
///   `[y]`.
/// - Dual-use declarations: if ANY declaration loading the target is
///   effectively ungated (empty chain), the target inherits nothing — the
///   file compiles without the gate, mirroring the test-role pass's
///   production-precedence rule. Otherwise the target inherits the union of
///   all effective chains, deduplicated, declaring files in sorted order and
///   declarations in fact order.
/// - A record's own gates are never dropped or reordered: inherited gates are
///   prepended, then exact-duplicate predicates collapse (first occurrence
///   wins), so re-running the pass over already-stamped records is a no-op.
///
/// Recomputed over the whole assembled graph every scan — never cached — so a
/// gating change in a parent file re-gates an unchanged module file's cached
/// records correctly. The stamping is idempotent, so incremental reassembly
/// cannot double-apply it.
pub fn apply_out_of_line_cfg_gates(
    records: &mut [GraphRecord],
    facts_by_file: &BTreeMap<String, FileFacts>,
) {
    let known_paths = known_file_paths(records, facts_by_file);
    // Resolve every out-of-line declaration once into (target, declaring
    // file, extracted chain) triples.
    let mut declarations: Vec<(String, String, Vec<String>)> = Vec::new();
    for (file, facts) in facts_by_file {
        for fact in &facts.out_of_line_mods {
            if let Some(target) = resolve_out_of_line_target(file, fact, &known_paths) {
                declarations.push((target, file.clone(), fact.cfg_gates.clone()));
            }
        }
    }
    if declarations.is_empty() {
        return;
    }

    // Fixpoint over declaring files' inherited gates: a declaring file's own
    // inherited gates prefix every chain it contributes. Monotone growth over
    // a finite predicate universe — terminates. `BTreeMap` iteration keeps
    // the union order deterministic.
    let mut inherited: BTreeMap<String, Vec<String>> = BTreeMap::new();
    loop {
        let mut changed = false;
        // Per-target effective chains under the current `inherited` map.
        let mut target_chains: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
        for (target, declaring_file, chain) in &declarations {
            let mut effective = inherited.get(declaring_file).cloned().unwrap_or_default();
            effective.extend(chain.iter().cloned());
            target_chains
                .entry(target.clone())
                .or_default()
                .push(effective);
        }
        for (target, chains) in &target_chains {
            // Ungated wins: any effectively-ungated declaration means the
            // target compiles without a gate.
            if chains.iter().any(Vec::is_empty) {
                if inherited.remove(target).is_some() {
                    changed = true;
                }
                continue;
            }
            let mut union: Vec<String> = Vec::new();
            for chain in chains {
                for gate in chain {
                    if !union.contains(gate) {
                        union.push(gate.clone());
                    }
                }
            }
            if inherited.get(target) != Some(&union) {
                inherited.insert(target.clone(), union);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if inherited.is_empty() {
        return;
    }

    for record in records.iter_mut() {
        let GraphRecord::Node {
            kind: NodeKind::File | NodeKind::Symbol | NodeKind::Module,
            repo_relative_path: Some(path),
            ..
        } = record
        else {
            continue;
        };
        let Some(prefix) = inherited.get(path.as_str()) else {
            continue;
        };
        if prefix.is_empty() {
            continue;
        }
        let existing = record.cfg().cloned().unwrap_or_default();
        let mut merged = prefix.clone();
        for gate in existing {
            if !merged.contains(&gate) {
                merged.push(gate);
            }
        }
        // `with_cfg` consumes the record; clone, stamp, and write back.
        // Every record reaching this point has a non-empty merged chain.
        let stamped = record.clone().with_cfg(merged);
        *record = stamped;
    }
}

/// Every repo-relative file path the repo-wide out-of-line passes can resolve
/// a module declaration against: fact keys plus every file-backed record
/// path, so a module file that exported no facts still resolves.
fn known_file_paths(
    records: &[GraphRecord],
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> BTreeSet<String> {
    let mut known_paths: BTreeSet<String> = facts_by_file.keys().cloned().collect();
    for record in records {
        if let GraphRecord::Node {
            repo_relative_path: Some(path),
            ..
        } = record
        {
            known_paths.insert(path.clone());
        }
    }
    known_paths
}

/// Resolves out-of-line module declarations to the set of files reachable
/// ONLY through test-gated declarations (issue #223's rule, shared with the
/// issue #238 `File`-role pass).
///
/// Production takes precedence for dual-use files: a module file that a
/// non-test declaration also loads still compiles into the production build,
/// so it is excluded from the returned set. The production-reachable set is
/// seeded by declaring files that are never themselves loaded as out-of-line
/// modules (e.g. crate roots) plus conventional crate roots (which always
/// compile into a production build), then propagated through ungated
/// declarations to a fixpoint; the test set seeds from every test-gated
/// declaration's target and expands through all declarations of test-only
/// files — never rewriting or expanding through anything
/// production-reachable. Deterministic: `BTreeSet` iteration throughout.
fn test_only_module_files(
    facts_by_file: &BTreeMap<String, FileFacts>,
    known_paths: &BTreeSet<String>,
) -> BTreeSet<String> {
    // Resolve every out-of-line declaration once into (from, to, gated)
    // module-load edges.
    let mut edges: Vec<(String, String, bool)> = Vec::new();
    for (file, facts) in facts_by_file {
        for fact in &facts.out_of_line_mods {
            if let Some(target) = resolve_out_of_line_target(file, fact, known_paths) {
                edges.push((file.clone(), target, fact.test_gated));
            }
        }
    }
    if edges.is_empty() {
        return BTreeSet::new();
    }

    let targets: BTreeSet<&str> = edges.iter().map(|(_, to, _)| to.as_str()).collect();
    let mut production: BTreeSet<String> = edges
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
    let mut test_files: BTreeSet<String> = BTreeSet::new();
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
    test_files
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
    let (prefix, remainder) = split_crate_prefix(repo_relative_path);
    let base = base_crate_root_id(&remainder);
    if prefix.is_empty() {
        base
    } else {
        // Prefix-qualify so distinct workspace crates (`crates/a/src/lib.rs`,
        // `crates/b/src/lib.rs`) never pool their same-named root definitions
        // into one `lib` partition (issue #440). A single-crate `src/...`
        // layout has an empty prefix and is byte-identical to the pre-#440 id.
        format!("{prefix}::{base}")
    }
}

/// The crate-root identifier for a crate-relative remainder (the second element
/// of [`split_crate_prefix`]). This is the pre-#440 single-crate classification,
/// operating on the remainder so it composes with any workspace-directory prefix.
fn base_crate_root_id(remainder: &[String]) -> String {
    let segments: Vec<&str> = remainder.iter().map(String::as_str).collect();
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

/// Splits a repo-relative Rust file path into its workspace-crate directory
/// prefix and the crate-relative remainder (issue #440).
///
/// A cargo workspace places each member crate in its own directory
/// (`crates/foo/src/lib.rs`, `foo/src/mod_b.rs`), so the pre-#440 assumption
/// that every source path begins at `src/`/`tests/`/… held only for a
/// single-crate repo. This locates the crate's target-root marker — the FIRST
/// `src`/`tests`/`examples`/`benches` segment, or a `build.rs` — and returns
/// everything before it as the crate directory prefix and the marker-onward
/// suffix as the crate-relative remainder. A single-crate `src/lib.rs` yields
/// (`""`, `["src", "lib.rs"]`) — an empty prefix, so every downstream id and
/// module path is byte-identical to the pre-#440 behavior. A path with no
/// marker (a non-`src` stray file) yields an empty prefix and the whole path,
/// matching the pre-#440 `lib`/empty-module-path fallback.
///
/// The FIRST marker is chosen deliberately: it keeps a module literally named
/// `src` (`crate_a/src/src/foo.rs`) attributed to the real crate `crate_a`
/// rather than mis-rooting at the inner `src`. The residual bound is a crate
/// directory literally named after a marker (`benches/src/lib.rs`), which is
/// not a conventional cargo layout.
#[must_use]
pub(crate) fn split_crate_prefix(repo_relative_path: &str) -> (String, Vec<String>) {
    let segments = crate::languages::common::path_segments(repo_relative_path);
    let is_marker = |s: &str| matches!(s, "src" | "tests" | "examples" | "benches");
    let boundary = segments
        .iter()
        .position(|s| is_marker(s))
        .or_else(|| segments.iter().rposition(|s| s == "build.rs"));
    match boundary {
        Some(0) | None => (String::new(), segments),
        Some(i) => (segments[..i].join("/"), segments[i..].to_vec()),
    }
}

/// The inferred crate name for a repo-relative path: the last component of its
/// workspace-crate directory prefix, normalized cargo-style (`-` → `_`), or
/// `None` for a single-crate (`src/…`-rooted) path with no directory prefix
/// (issue #440). Used to recognize a cross-crate qualified call's leading
/// segment (`dep_crate::…`) as naming a workspace member.
#[must_use]
pub(crate) fn crate_name_of(repo_relative_path: &str) -> Option<String> {
    let (prefix, _) = split_crate_prefix(repo_relative_path);
    if prefix.is_empty() {
        return None;
    }
    prefix.rsplit('/').next().map(|name| name.replace('-', "_"))
}

/// The auxiliary-target ENTRY crate root a repo-relative path denotes when the
/// file is one cargo compiles as its OWN crate: a top-level integration test
/// (`tests/<name>.rs` or the directory form `tests/<name>/main.rs`), example
/// (`examples/<name>.rs` / `examples/<name>/main.rs`), or benchmark
/// (`benches/<name>.rs` / `benches/<name>/main.rs`). Returns `None` for any
/// other file, including a nested helper module such as `tests/common/mod.rs`.
///
/// A crate-root file owns its CONTAINING directory for `mod` resolution (unlike
/// an ordinary `foo.rs` module file, which owns the sibling `foo/` directory),
/// which is why these files anchor the [`reassign_aux_helper_crate_roots`]
/// inclusion walk.
fn aux_entry_crate_root(path: &str) -> Option<String> {
    let segments: Vec<&str> = path.split('/').collect();
    let is_rs = |name: &str| {
        std::path::Path::new(name)
            .extension()
            .is_some_and(|e| e == "rs")
    };
    let kind = match segments.first().copied() {
        Some("tests") => "test",
        Some("examples") => "example",
        Some("benches") => "bench",
        _ => return None,
    };
    match segments.as_slice() {
        [_, file] if is_rs(file) => Some(format!("{kind}:{}", file.trim_end_matches(".rs"))),
        [_, name, "main.rs"] => Some(format!("{kind}:{name}")),
        _ => None,
    }
}

/// Resolves a plain top-level `mod <name>;` declaration in `declaring_file` to a
/// scanned repo-relative file, using entry-crate-root-aware base selection: an
/// aux ENTRY crate root (`is_entry_root`) owns its CONTAINING directory, while a
/// nested helper module file (`foo.rs`) owns the sibling `foo/` directory (a
/// `mod.rs` / `main.rs` file already resolves to its containing directory via
/// [`module_dir_segments`], so the flag only matters for a single-file entry
/// like `tests/it.rs`).
///
/// Conservative by construction (preserving the no-wrong-edge invariant): only a
/// plain top-level declaration participates. A `#[path]` override, a declaration
/// nested in an inline module, or an enclosing `#[path]` rebase yields `None` —
/// the helper keeps its path-based crate root (a MISSING edge, never a wrong
/// one).
fn resolve_aux_helper_mod(
    declaring_file: &str,
    is_entry_root: bool,
    fact: &OutOfLineModFact,
    known_paths: &std::collections::BTreeSet<String>,
) -> Option<String> {
    if fact.under_inline_path_override
        || fact.path_override.is_some()
        || !fact.inline_module_path.is_empty()
    {
        return None;
    }
    let base = if is_entry_root {
        parent_dir_segments(declaring_file)
    } else {
        module_dir_segments(declaring_file)
    };
    let file_candidate = join_segments(&base, &format!("{}.rs", fact.name))?;
    if known_paths.contains(&file_candidate) {
        return Some(file_candidate);
    }
    let dir_candidate = join_segments(&base, &format!("{}/mod.rs", fact.name))?;
    known_paths
        .contains(&dir_candidate)
        .then_some(dir_candidate)
}

/// Walks each aux ENTRY crate root's ([`aux_entry_crate_root`]) transitive plain
/// `mod <name>;` declarations ([`resolve_aux_helper_mod`], nested helpers
/// included) and returns, for every reached helper file, the SORTED list of
/// distinct entry crate roots that include it.
///
/// The seed of each walk is the entry crate root itself (owns its CONTAINING
/// directory); every reached helper is an ordinary module file. `visited` is
/// per-entry, so inclusion cycles terminate. A `BTreeSet` accumulates the
/// includer roots, so each helper's list is sorted and duplicate-free —
/// deterministic and byte-identical across runs.
fn aux_helper_includer_roots(
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> BTreeMap<String, Vec<String>> {
    let known_paths: std::collections::BTreeSet<String> = facts_by_file.keys().cloned().collect();
    // helper repo-relative path -> the distinct entry crate roots that
    // transitively include it via plain `mod` declarations.
    let mut reached: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for entry_path in facts_by_file.keys() {
        let Some(entry_root) = aux_entry_crate_root(entry_path) else {
            continue;
        };
        // Walk this entry's mod-inclusion subtree. The seed is the crate root
        // (owns its containing directory); every reached helper is an ordinary
        // module file. `visited` is per-entry, so cycles terminate.
        let mut visited: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        visited.insert(entry_path.clone());
        let mut stack: Vec<(String, bool)> = vec![(entry_path.clone(), true)];
        while let Some((file, is_entry)) = stack.pop() {
            let Some(facts) = facts_by_file.get(&file) else {
                continue;
            };
            for m in &facts.out_of_line_mods {
                let Some(child) = resolve_aux_helper_mod(&file, is_entry, m, &known_paths) else {
                    continue;
                };
                reached
                    .entry(child.clone())
                    .or_default()
                    .insert(entry_root.clone());
                if visited.insert(child.clone()) {
                    stack.push((child, false));
                }
            }
        }
    }
    reached
        .into_iter()
        .map(|(helper, roots)| (helper, roots.into_iter().collect()))
        .collect()
}

/// Computes a crate-root REASSIGNMENT map for auxiliary-target (test / example /
/// bench) HELPER module files, closing the recall gap where a shared helper like
/// `tests/common/mod.rs` — path-classified into its OWN synthetic crate root
/// `test:common` by [`crate_root_id`] — actually belongs to the entry crate that
/// `mod`-includes it (`test:it` for `tests/it.rs`). Issue #394 restricts a
/// pending impl's candidate traits to its own crate root, so without this remap
/// an `impl crate::T for Foo` in the helper cannot resolve a trait `T` defined in
/// the entry file — a MISSING `IMPLEMENTS` edge (Codex round-2/3, PR #399: "test
/// / example helper modules are stamped their own root").
///
/// Path alone cannot decide the owning crate; the `mod` inclusion graph
/// ([`aux_helper_includer_roots`]) is consulted. A helper reachable from EXACTLY
/// ONE entry crate is remapped to that entry's crate root; a helper reachable
/// from ZERO entry crates keeps its path-based crate root (conservative — a
/// standalone helper stays unresolved rather than binding one interpretation,
/// preserving the no-wrong-edge invariant). A helper shared by 2+ distinct entry
/// crates is NOT remapped here either (no single owner to pick); issue #401
/// recovers its recall instead via [`multi_includer_aux_helper_roots`] +
/// [`duplicate_multi_includer_helpers`]. A helper that is ITSELF an aux entry
/// crate root (`tests/common.rs`, which cargo also compiles as its own test
/// target) counts as belonging to its own crate and is never remapped.
///
/// Only test/example/bench helper files are reassigned; `lib`/`bin`/`build`
/// assignment is untouched. The returned map is helper repo-relative path ->
/// reassigned crate root, applied by [`cross_file_implements_records`] as an
/// in-pass fact remap — no serialized fact shape changes, so
/// `CACHE_SCHEMA_VERSION` is unaffected.
///
/// Documented residual bound: only the `crate_root` partition key is remapped,
/// not a helper symbol's crate-root-relative `qualified_name`. A DEEPLY nested
/// helper that defines a root-level symbol colliding by simple name with the
/// entry crate's own root symbol can therefore become same-name-ambiguous and
/// stay unresolved (a conservative MISSING edge), never a wrong edge. The
/// direct, canonical single-`mod` helper case (the reported gap) resolves.
fn reassign_aux_helper_crate_roots(
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> BTreeMap<String, String> {
    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    for (helper, roots) in aux_helper_includer_roots(facts_by_file) {
        // Conservative multi/zero-includer bound: only a single-includer helper
        // is remapped. A helper shared by 2+ entry crates is duplicated per
        // includer root instead (issue #401); it never lands in this map.
        let [new_root] = roots.as_slice() else {
            continue;
        };
        // A file cargo compiles as its OWN aux target belongs to its own crate;
        // never steal it into the including entry (it lives in 2+ crates).
        if aux_entry_crate_root(&helper).is_some() {
            continue;
        }
        if crate_root_id(&helper) != *new_root {
            remap.insert(helper, new_root.clone());
        }
    }
    remap
}

/// Computes the multi-includer helper map for auxiliary-target (test / example /
/// bench) HELPER module files (issue #401): every helper file transitively
/// `mod`-included by 2+ DISTINCT entry crate roots, mapped to the SORTED list of
/// those entry roots.
///
/// Cargo compiles each integration-test / example / bench target as its own
/// crate, so a helper shared by `tests/a.rs` and `tests/b.rs` is REALLY compiled
/// once per including target — but path-based [`crate_root_id`] stamps it one
/// synthetic root (`test:common`), and [`reassign_aux_helper_crate_roots`]
/// conservatively leaves multi-includer helpers un-remapped (no single owner to
/// pick). [`cross_file_implements_records`] therefore DUPLICATES such a helper's
/// facts once per including crate root
/// ([`duplicate_multi_includer_helpers`]): each duplicate resolves strictly
/// within its own crate root (#394 isolation), so the duplication recovers the
/// lost `IMPLEMENTS` recall with no wrong-edge risk — one edge per real
/// compilation.
///
/// A helper that is ITSELF an aux entry crate root (`tests/common.rs`, which
/// cargo also compiles as its own test target) belongs to its own crate and is
/// never duplicated into its includers.
#[must_use]
fn multi_includer_aux_helper_roots(
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> BTreeMap<String, Vec<String>> {
    aux_helper_includer_roots(facts_by_file)
        .into_iter()
        .filter(|(helper, roots)| roots.len() >= 2 && aux_entry_crate_root(helper).is_none())
        .collect()
}

/// Duplicates a multi-includer aux helper's facts once per including entry
/// crate root (issue #401). For each helper in `multi`, one copy of its
/// [`FileFacts::impl_targets`] / [`FileFacts::pending_impls`] is appended per
/// includer root with `crate_root` rewritten to that root — mirroring the real
/// compilations cargo performs (the helper is compiled once per including test
/// / example / bench target). The helper's original path-stamped copies are
/// kept, so the phantom partition resolves exactly as before (status quo, never
/// worse). Only the `crate_root` partition key changes; qualified names, module
/// paths, and imports are untouched — a pure re-partition, deterministic and
/// byte-identical across runs.
#[must_use]
fn duplicate_multi_includer_helpers(
    facts_by_file: &BTreeMap<String, FileFacts>,
    multi: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, FileFacts> {
    let mut out = facts_by_file.clone();
    for (path, roots) in multi {
        let Some(facts) = out.get_mut(path) else {
            continue;
        };
        // Build the per-root copies from the ORIGINAL vectors first: appending
        // while iterating the same vector would trip the borrow checker (and
        // duplicating the duplicates would be wrong).
        let mut extra_targets: Vec<ImplTargetFact> = Vec::new();
        let mut extra_pending: Vec<PendingImplFact> = Vec::new();
        for root in roots {
            for target in &facts.impl_targets {
                let mut duplicate = target.clone();
                duplicate.crate_root.clone_from(root);
                extra_targets.push(duplicate);
            }
            for pending in &facts.pending_impls {
                let mut duplicate = pending.clone();
                duplicate.crate_root.clone_from(root);
                extra_pending.push(duplicate);
            }
        }
        facts.impl_targets.extend(extra_targets);
        facts.pending_impls.extend(extra_pending);
    }
    out
}

/// Applies a [`reassign_aux_helper_crate_roots`] remap to a CLONE of the facts,
/// rewriting the `crate_root` on every `ImplTargetFact`, `PendingImplFact`, and
/// `ImplTraitRelationFact` of each remapped helper file. Only the `crate_root`
/// partition key changes; qualified names, module paths, and imports are
/// untouched, so this is a pure re-partition, deterministic and byte-identical
/// across runs.
fn apply_crate_root_remap(
    facts_by_file: &BTreeMap<String, FileFacts>,
    remap: &BTreeMap<String, String>,
) -> BTreeMap<String, FileFacts> {
    let mut out = facts_by_file.clone();
    for (path, new_root) in remap {
        if let Some(facts) = out.get_mut(path) {
            for target in &mut facts.impl_targets {
                target.crate_root.clone_from(new_root);
            }
            for pending in &mut facts.pending_impls {
                pending.crate_root.clone_from(new_root);
            }
            // The IMPLEMENTS-gated self-dispatch join (issue #414) keys
            // `impl_trait_relations` by `crate_root`, so a helper's relations
            // move with the rest of its facts: otherwise the CALLS pass would
            // look them up under the entry root and miss (issue #475).
            for relation in &mut facts.impl_trait_relations {
                relation.crate_root.clone_from(new_root);
            }
        }
    }
    out
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

/// Per-body set of simple callee names with a trait-dispatch call site
/// (issue #267). The per-file textual pass suppresses its own CALLS edge for
/// these names (the repo-wide pass owns dispatch pairs now, same-file
/// included), so the cross-file pass must emit the same-file pairs it would
/// otherwise skip — for the dispatch sites AND for any static site in the same
/// body that the textual pass no longer covers. Rebuilt from facts alone so
/// history replay and the incremental cache agree with the extractor's
/// suppression.
fn dispatch_suppressed_names(facts: &FileFacts) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut suppressed: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for call in &facts.call_sites {
        if call.dispatch_trait.is_some()
            && let Some(simple_name) = call.callee_segments.last()
        {
            suppressed
                .entry(call.caller_id.as_str())
                .or_default()
                .insert(simple_name.as_str());
        }
    }
    suppressed
}

/// Emits the deduplicated cross-file `CALLS` edge records, attaching
/// resolved call-site spans only to provably single-target pairs
/// (issue #462; the #233 fabrication-guard discipline).
fn emit_call_edge_records(
    records: &mut Vec<GraphRecord>,
    edges: BTreeMap<(String, String), (CallResolution, String, Vec<SourceSpan>)>,
) {
    for ((source, target), (resolution, summary, spans)) in edges {
        let confidence = match resolution {
            CallResolution::Resolved => Some("1.0".to_owned()),
            CallResolution::Ambiguous
            | CallResolution::Unresolved
            | CallResolution::UnresolvedDispatch => None,
        };
        let mut record = GraphRecord::edge(EdgeLabel::Calls, source, target, confidence, summary)
            .with_resolution(resolution);
        // Only a provably single-target pair keeps its call-site spans; an
        // ambiguous pair drops them even when a resolved site contributed
        // (issue #462, the #233 fabrication-guard discipline).
        if resolution == CallResolution::Resolved && !spans.is_empty() {
            record = record.with_call_site_spans(spans);
        }
        records.push(record);
    }
}

/// Emits the unresolved-call and unresolved-dispatch `Diagnostic` marker nodes
/// plus their caller edges (issues #233, #267).
fn emit_unresolved_records(
    repository_id: &str,
    records: &mut Vec<GraphRecord>,
    diagnostics: &BTreeMap<(String, String), SourceSpan>,
    dispatch_diagnostics: &BTreeMap<(String, String, String), SourceSpan>,
    diagnostic_edges: BTreeMap<(String, String, String), String>,
    dispatch_diagnostic_edges: BTreeMap<(String, String, String, String), String>,
) {
    for ((path, display), span) in diagnostics {
        records.push(unresolved_call_diagnostic(
            repository_id,
            path,
            display,
            *span,
        ));
    }
    for ((path, trait_path, method), span) in dispatch_diagnostics {
        records.push(unresolved_dispatch_diagnostic(
            repository_id,
            path,
            trait_path,
            method,
            *span,
        ));
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
    for ((path, trait_path, method, caller_id), caller_name) in dispatch_diagnostic_edges {
        let target = unresolved_dispatch_diagnostic_id(repository_id, &path, &trait_path, &method);
        let display = format!("{trait_path}::{method}");
        records.push(
            GraphRecord::edge(
                EdgeLabel::Calls,
                caller_id,
                target,
                None,
                format!("{caller_name} calls {display} (cross-file, unresolved_dispatch)"),
            )
            .with_resolution(CallResolution::UnresolvedDispatch),
        );
    }
}

/// Computes the cross-file call records for one scanned tree.
///
/// Returned records are `Diagnostic` nodes for unresolved calls followed by
/// `CALLS` edges, in deterministic order. It also appends the `CONSTRUCTS`
/// struct-literal edges (issue #443) via [`cross_file_construct_records`], so
/// every driver that emits cross-file CALLS gets CONSTRUCTS with no extra wiring.
///
/// Auxiliary-target helper modules (`tests/common/mod.rs`) are reassigned to
/// the entry crate that `mod`-includes them (issue #475) before resolution, on
/// both the index side and the caller side, exactly as the IMPLEMENTS and
/// CONSTRUCTS passes do.
#[must_use]
pub fn cross_file_call_records(
    repository_id: &str,
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> Vec<GraphRecord> {
    // Reassign auxiliary-target helper modules (`tests/common/mod.rs`) to the
    // entry crate that `mod`-includes them, exactly as
    // [`cross_file_construct_records`] does (issue #475): a `crate::…` call in
    // such a helper resolves against the including ENTRY crate in Rust, but
    // path-based [`crate_root_id`] stamps the helper its OWN synthetic root
    // (`test:common`), which confines the call to the wrong partition and drops
    // its CALLS edge. The remap rewrites both the index side (helper
    // definitions partition under the entry root via the remap-aware
    // [`DefinitionIndex`]) and the caller side (each call site's
    // `caller_crate_root`, looked up below). When nothing needs remapping the
    // borrowed facts are used directly, keeping output byte-identical.
    let remap = reassign_aux_helper_crate_roots(facts_by_file);
    let remapped;
    let facts_by_file: &BTreeMap<String, FileFacts> = if remap.is_empty() {
        facts_by_file
    } else {
        remapped = apply_crate_root_remap(facts_by_file, &remap);
        &remapped
    };
    let index = DefinitionIndex::build(facts_by_file, remap.clone());

    // (source, target) -> (strongest resolution, summary, resolved call-site
    // spans), deduplicating repeated call sites between the same pair. Spans
    // are retained only from `Resolved` call sites and attached only when the
    // pair's final resolution is `Resolved` (issue #462; the #233
    // fabrication-guard discipline).
    let mut edges: BTreeMap<(String, String), (CallResolution, String, Vec<SourceSpan>)> =
        BTreeMap::new();
    // (file, callee display) -> first-seen span, for diagnostic nodes.
    let mut diagnostics: BTreeMap<(String, String), SourceSpan> = BTreeMap::new();
    // (file, callee display, caller ID) -> caller name, for unresolved edges.
    let mut diagnostic_edges: BTreeMap<(String, String, String), String> = BTreeMap::new();
    // (file, dispatch trait path, method) -> first-seen span, for the typed
    // issue #267 dispatch markers.
    let mut dispatch_diagnostics: BTreeMap<(String, String, String), SourceSpan> = BTreeMap::new();
    // (file, dispatch trait path, method, caller ID) -> caller name, for the
    // typed dispatch-marker edges.
    let mut dispatch_diagnostic_edges: BTreeMap<(String, String, String, String), String> =
        BTreeMap::new();

    for (path, facts) in facts_by_file {
        let caller_crate_root = remap
            .get(path)
            .cloned()
            .unwrap_or_else(|| crate_root_id(path));
        let dispatch_suppressed = dispatch_suppressed_names(facts);
        for call in &facts.call_sites {
            let Some(simple_name) = call.callee_segments.last() else {
                continue;
            };
            // Lexically-scoped block-local recall (issue #422): an in-scope
            // block-local def shadows the flat pool for this call — a bare
            // call in the enclosing body binds the block-local, never a
            // same-named module-level def, and a call from any other scope
            // never sees the block-local at all (the #413 guard, enforced by
            // `block_local_candidates`' scope gate).
            let block_locals = index.block_local_candidates(call, simple_name);
            if record_block_local_call(call, &block_locals, &mut edges) {
                continue;
            }
            let candidates = index.candidates(call, simple_name, &caller_crate_root);
            let is_dispatch = call.dispatch_trait.is_some();
            // Same-file pairs are emitted by the per-file textual pass for
            // ordinary names; for dispatch-suppressed names that pass stays
            // silent, so this pass emits them — dispatch sites always, and
            // static sites in a dispatch-suppressed body too.
            let emit_same_file = is_dispatch
                || dispatch_suppressed
                    .get(call.caller_id.as_str())
                    .is_some_and(|names| names.contains(simple_name.as_str()));
            match candidates.len() {
                0 => {
                    if let Some(trait_path) = &call.dispatch_trait {
                        record_unresolved_dispatch(
                            path,
                            call,
                            trait_path,
                            &mut dispatch_diagnostics,
                            &mut dispatch_diagnostic_edges,
                        );
                    } else {
                        record_unresolved(
                            path,
                            call,
                            simple_name,
                            &mut diagnostics,
                            &mut diagnostic_edges,
                        );
                    }
                }
                1 => {
                    record_candidate_edge(
                        path,
                        call,
                        candidates[0],
                        CallResolution::Resolved,
                        1,
                        emit_same_file,
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
                            emit_same_file,
                            &mut edges,
                        );
                    }
                }
            }
        }
    }

    let mut records = Vec::new();
    emit_call_edge_records(&mut records, edges);
    emit_unresolved_records(
        repository_id,
        &mut records,
        &diagnostics,
        &dispatch_diagnostics,
        diagnostic_edges,
        dispatch_diagnostic_edges,
    );
    records.extend(cross_file_construct_records(repository_id, facts_by_file));
    records.extend(cross_file_route_records(repository_id, facts_by_file));
    records
}

/// Repo-wide macro-invocation resolution (issue #148).
///
/// Every `macro_invocation` site the Rust extractor recorded resolves against
/// the repo-wide pool of `macro_rules!` definitions, matched on the
/// invocation's simple name:
///
/// - exactly one repo-defined macro → a `CALLS` edge from the invoking scope
///   to the macro's Symbol node, stamped `resolved`, carrying the invocation
///   spans — INSTEAD OF an `unsupported macro invocation` Diagnostic;
/// - zero definitions (external macros such as `println!`, dependency
///   macros) → the pre-existing Diagnostic, with the same stable ID scheme
///   the per-file extractor used, so external-macro diagnostics stay
///   byte-identical;
/// - two or more definitions → the name is ambiguous and must never silently
///   bind one definition (the issue #134 direction); the site keeps its
///   Diagnostic.
///
/// Purely syntactic and filesystem-local: simple-name matching over parsed
/// source only — no macro-body expansion, no network, no embeddings, no
/// agent-authored observations (the trust-separation boundary).
///
/// Deterministic: definitions index in sorted path order, invocation sites
/// iterate in (path, source) order, and repeated invocations between one
/// (caller, macro) pair collapse to a single edge.
#[must_use]
pub fn cross_file_macro_records(
    repository_id: &str,
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> Vec<GraphRecord> {
    let mut index: BTreeMap<&str, Vec<&MacroDefinitionFact>> = BTreeMap::new();
    for facts in facts_by_file.values() {
        for definition in &facts.macro_definitions {
            index
                .entry(definition.simple_name.as_str())
                .or_default()
                .push(definition);
        }
    }
    // (caller, macro) -> (caller name, simple name, invocation spans):
    // repeated invocations between one pair collapse to a single edge, with
    // every site's span retained (the issue #462 fabrication-guard rule:
    // only provably single-target pairs keep spans).
    let mut edges: BTreeMap<(String, String), (String, String, Vec<SourceSpan>)> = BTreeMap::new();
    let mut diagnostics: Vec<GraphRecord> = Vec::new();
    for (path, facts) in facts_by_file {
        for invocation in &facts.macro_invocations {
            let candidates: &[&MacroDefinitionFact] = index
                .get(invocation.simple_name.as_str())
                .map_or(&[], Vec::as_slice);
            if candidates.len() == 1 {
                let definition = candidates[0];
                let entry = edges
                    .entry((invocation.caller_id.clone(), definition.id.clone()))
                    .or_insert_with(|| {
                        (
                            invocation.caller_name.clone(),
                            invocation.simple_name.clone(),
                            Vec::new(),
                        )
                    });
                if !entry.2.contains(&invocation.span) {
                    entry.2.push(invocation.span);
                }
                continue;
            }
            // Unresolvable here: either no repo-defined macro carries this
            // name (external / dependency macro) or several do (ambiguous).
            // Keep the pre-existing Diagnostic with its legacy stable ID.
            let id = stable_id(&[
                "node",
                "diagnostic",
                repository_id,
                path,
                &invocation.invocation_display,
                &invocation.diagnostic_disambiguator.to_string(),
            ]);
            diagnostics.push(GraphRecord::syntax_node(
                id,
                NodeKind::Diagnostic,
                path.clone(),
                invocation.span,
                invocation.invocation_display.clone(),
                "rust",
                format!(
                    "unsupported macro invocation {}",
                    invocation.invocation_display
                ),
            ));
        }
    }
    let mut records: Vec<GraphRecord> = Vec::with_capacity(edges.len() + diagnostics.len());
    for ((source, target), (caller_name, simple_name, spans)) in edges {
        records.push(
            GraphRecord::edge(
                EdgeLabel::Calls,
                source,
                target,
                Some("1.0".to_owned()),
                format!("{caller_name} invokes macro {simple_name} (resolved)"),
            )
            .with_resolution(CallResolution::Resolved)
            .with_call_site_spans(spans),
        );
    }
    records.extend(diagnostics);
    records
}

/// Computes the cross-file route-registration `REGISTERS_ROUTE` records for one
/// scanned tree (issue #445).
///
/// Each [`RouteRegistrationFact`] names a handler reference registered by a
/// `routes![…]` macro; this pass resolves that reference to a UNIQUE handler
/// definition Symbol via the repo-wide [`DefinitionIndex`] (the same callable
/// index CALLS uses), so it inherits CALLS's free-function pool selection and
/// crate-root confinement. A single-segment reference resolves as a `Direct`
/// call, a path-qualified one as a `Path` call.
///
/// Only a UNIQUE resolution mints an edge; an ambiguous (2+ candidate) or
/// unresolved/external reference mints NOTHING (no edge, no diagnostic) —
/// ambiguity never silently picks one, mirroring the CONSTRUCTS pass
/// (no-wrong-edge doctrine). The edge runs registration-owner → handler with
/// resolution `Resolved` and confidence `"1.0"`. Sites are collapsed per
/// `(owner, handler)` pair. Deterministic and byte-stable.
#[must_use]
pub fn cross_file_route_records(
    _repository_id: &str,
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> Vec<GraphRecord> {
    // Reassign auxiliary-target helper modules (`tests/common/mod.rs`) to the
    // entry crate that `mod`-includes them, exactly as
    // [`cross_file_construct_records`] does, so a `crate::…`-scoped registration
    // in such a helper resolves against the including entry crate. The remap
    // also partitions a helper's own handler definitions under the entry root
    // on the index side (issue #475), via the remap-aware [`DefinitionIndex`].
    let remap = reassign_aux_helper_crate_roots(facts_by_file);
    let remapped;
    let facts_by_file: &BTreeMap<String, FileFacts> = if remap.is_empty() {
        facts_by_file
    } else {
        remapped = apply_crate_root_remap(facts_by_file, &remap);
        &remapped
    };
    let index = DefinitionIndex::build(facts_by_file, remap.clone());

    // (owner_id, handler_symbol_id) -> summary, collapsing repeated
    // registrations between the same pair.
    let mut edges: BTreeMap<(String, String), String> = BTreeMap::new();
    for (path, facts) in facts_by_file {
        let caller_crate_root = remap
            .get(path)
            .cloned()
            .unwrap_or_else(|| crate_root_id(path));
        for site in &facts.route_registration_sites {
            let Some(simple_name) = site.handler_segments.last() else {
                continue;
            };
            // Synthesize a call fact so the shared DefinitionIndex resolver
            // applies the same free-function pool selection and crate-root
            // confinement calls use. A single-segment reference is `Direct`; a
            // path-qualified one is a `Path`.
            let call_kind = if site.handler_segments.len() <= 1 {
                CallKind::Direct
            } else {
                CallKind::Path
            };
            let synthetic = CallSiteFact {
                caller_id: site.owner_id.clone(),
                caller_name: site.owner_name.clone(),
                callee_display: site.handler_display.clone(),
                callee_segments: site.handler_segments.clone(),
                call_kind,
                path_root: site.path_root.clone(),
                receiver_owner: None,
                receiver_type: None,
                dispatch_trait: None,
                span: site.span,
            };
            let candidates = index.candidates(&synthetic, simple_name, &caller_crate_root);
            // Unique resolution ONLY (no-wrong-edge): 0 or >=2 candidates mint
            // nothing — no edge, no diagnostic.
            if candidates.len() != 1 {
                continue;
            }
            let target = candidates[0];
            // A registration owner and its handler are distinct Symbols, so a
            // self-edge is unreachable; guard anyway.
            if target.id == site.owner_id {
                continue;
            }
            edges
                .entry((site.owner_id.clone(), target.id.clone()))
                .or_insert_with(|| {
                    format!(
                        "{} registers route {}",
                        site.owner_name, site.handler_display
                    )
                });
        }
    }

    edges
        .into_iter()
        .map(|((source, target), summary)| {
            GraphRecord::edge(
                EdgeLabel::RegistersRoute,
                source,
                target,
                Some("1.0".to_owned()),
                summary,
            )
            .with_resolution(CallResolution::Resolved)
        })
        .collect()
}

/// Computes the cross-file struct-literal `CONSTRUCTS` records for one scanned
/// tree (issue #443).
///
/// Each [`ConstructSiteFact`] names a type path (`Type { … }`); this pass
/// resolves that path to a UNIQUE constructible type definition Symbol
/// (`struct`/`enum`) via the repo-wide [`ImplTargetIndex`], applying the same
/// crate-root confinement the Path CALLS arm uses (issue #440): a
/// `crate`/`self`/`super` head confines to the caller's crate root, a bare head
/// naming a workspace crate confines to that crate's library root with the crate
/// segment dropped, and an unqualified head matches repo-wide. An enum-struct
/// variant literal (`Shape::Circle { … }`) whose full path does not resolve is
/// retried after dropping the trailing variant segment, binding the enum.
///
/// Only a UNIQUE resolution mints an edge; an ambiguous (2+ candidate) or
/// unresolved/external type mints NOTHING (no edge, no diagnostic) — ambiguity
/// never silently picks one, mirroring the CALLS/IMPLEMENTS passes. Sites are
/// collapsed per `(constructor, type)` pair; the edge's `is_exhaustive` marker
/// is the OR of the collapsed sites' markers (true when ANY collapsed site is
/// the E0063-breakable non-`..base` form). Deterministic and byte-stable.
#[must_use]
pub fn cross_file_construct_records(
    _repository_id: &str,
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> Vec<GraphRecord> {
    // Reassign auxiliary-target helper modules (`tests/common/mod.rs`) to the
    // entry crate that `mod`-includes them, exactly as
    // [`cross_file_implements_records`] does (issue #394; Codex round on
    // PR #467). A `crate::Type { … }` literal in such a helper resolves against
    // the including ENTRY crate in Rust, but path-based [`crate_root_id`] stamps
    // the helper its OWN synthetic root (`test:common`), which would confine the
    // literal to the wrong partition and drop its CONSTRUCTS edge. The remap
    // rewrites both the index side (a helper's own `ImplTargetFact`s, via
    // [`apply_crate_root_remap`]) and the caller side (the construct site's
    // `caller_crate_root`, looked up below). When nothing needs remapping the
    // borrowed facts are used directly, keeping output byte-identical.
    let remap = reassign_aux_helper_crate_roots(facts_by_file);
    let remapped;
    let facts_by_file: &BTreeMap<String, FileFacts> = if remap.is_empty() {
        facts_by_file
    } else {
        remapped = apply_crate_root_remap(facts_by_file, &remap);
        &remapped
    };
    let index = ImplTargetIndex::build(facts_by_file);
    let crate_name_roots = build_crate_name_roots(facts_by_file);

    // (constructor_id, type_symbol_id) -> (summary, OR-of-site-exhaustiveness),
    // collapsing repeated construction sites between the same pair.
    let mut edges: BTreeMap<(String, String), (String, bool)> = BTreeMap::new();
    for (path, facts) in facts_by_file {
        let caller_crate_root = remap
            .get(path)
            .cloned()
            .unwrap_or_else(|| crate_root_id(path));
        for site in &facts.construct_sites {
            let Some(target) = index.resolve_construct(
                &site.type_segments,
                &site.path_root,
                &caller_crate_root,
                &crate_name_roots,
            ) else {
                continue;
            };
            // A distinct fn/method Symbol and a distinct type Symbol never share
            // a record ID, so a self-edge is unreachable; guard anyway.
            if target.id == site.constructor_id {
                continue;
            }
            let key = (site.constructor_id.clone(), target.id.clone());
            let entry = edges.entry(key).or_insert_with(|| {
                (
                    format!("{} constructs {}", site.constructor_name, site.type_display),
                    false,
                )
            });
            entry.1 = entry.1 || site.is_exhaustive;
        }
    }

    edges
        .into_iter()
        .map(|((source, target), (summary, is_exhaustive))| {
            GraphRecord::edge(
                EdgeLabel::Constructs,
                source,
                target,
                Some("1.0".to_owned()),
                summary,
            )
            .with_construct_exhaustive(is_exhaustive)
        })
        .collect()
}

/// Builds the workspace crate-name → library-crate-root registry (issue #440)
/// shared by the CALLS and CONSTRUCTS resolution passes: every crate directory
/// the scanned file set reveals contributes its inferred name → library root
/// binding; a name shared by two crate directories is marked ambiguous (`None`)
/// so a qualified reference to it is never confined to a wrong root.
fn build_crate_name_roots(
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> BTreeMap<String, Option<String>> {
    let mut crate_name_roots: BTreeMap<String, Option<String>> = BTreeMap::new();
    for path in facts_by_file.keys() {
        let (prefix, _) = split_crate_prefix(path);
        if prefix.is_empty() {
            continue;
        }
        let Some(name) = crate_name_of(path) else {
            continue;
        };
        let lib_root = format!("{prefix}::lib");
        match crate_name_roots.get(&name) {
            None => {
                crate_name_roots.insert(name, Some(lib_root));
            }
            Some(Some(existing)) if *existing != lib_root => {
                crate_name_roots.insert(name, None);
            }
            _ => {}
        }
    }
    crate_name_roots
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
    // Reassign auxiliary-target helper modules (`tests/common/mod.rs`) to the
    // entry crate that `mod`-includes them (issue #394 recall gap; Codex round
    // 2/3, PR #399). Path-based [`crate_root_id`] cannot see the `mod` inclusion
    // graph, so this in-pass remap runs before the crate-root-partitioned index
    // is built. When nothing needs remapping the borrowed facts are used
    // directly, keeping output byte-identical.
    let remap = reassign_aux_helper_crate_roots(facts_by_file);
    let remapped;
    let facts_by_file: &BTreeMap<String, FileFacts> = if remap.is_empty() {
        facts_by_file
    } else {
        remapped = apply_crate_root_remap(facts_by_file, &remap);
        &remapped
    };
    // Issue #401: a helper `mod`-included by 2+ entry crates is compiled once
    // per including test/example/bench target, so its facts are duplicated per
    // includer crate root before the index is built. Each duplicate resolves
    // strictly within its own crate root (#394 isolation), recovering the lost
    // `IMPLEMENTS` recall with no wrong-edge risk — one edge per real
    // compilation. When no helper is shared the borrowed facts are used
    // directly, keeping output byte-identical.
    let multi = multi_includer_aux_helper_roots(facts_by_file);
    let duplicated;
    let facts_by_file: &BTreeMap<String, FileFacts> = if multi.is_empty() {
        facts_by_file
    } else {
        duplicated = duplicate_multi_includer_helpers(facts_by_file, &multi);
        &duplicated
    };
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
        // Import-aware resolution (issue #393; Codex round-5 P2 on PR #399): when
        // the impl's trait path is a BARE name that a module-item `use` in the
        // impl's own module scope binds, the name refers to that import, never a
        // coincidental same-named definition the scope walk would reach. The
        // tri-state [`ImportBinding`] drives three distinct outcomes:
        //   * Resolved(path) — exactly one import binds the name and its path
        //     could name an in-repo target (`crate::`/`self::`/`super::`-rooted,
        //     or a bare first segment that is NOT an extern-prelude crate). Resolve
        //     the aliased PATH against the crate-root-partitioned index. An index
        //     miss (e.g. a genuine external `use serde::Serialize;` with no local
        //     `serde::Serialize`) mints no edge — and still does NOT fall through
        //     to the scope walk. Recovering the bare crate-root-local case
        //     (`use a::T;`) was the round-1 recall fix Codex flagged.
        //   * Veto — an import binds the name but it is EXTERNAL (a single
        //     extern-prelude path like `use std::fmt::Display;`) or AMBIGUOUS (2+
        //     distinct cfg-gated paths). The bare name is shadowed by the import;
        //     mint NO edge and DO NOT fall through to the scope walk (which would
        //     otherwise mis-bind a coincidental root-local same-name trait — the
        //     round-4 wrong-edge bug). This restores PR #389's shadow-veto intent.
        //   * NoImport — no import binds the name; fall through to the scope walk
        //     (normal #389 recall for same/parent-module traits).
        if !pending.trait_path.contains("::") {
            match lookup_use_import(use_imports, &pending.module_names, &pending.trait_path) {
                ImportBinding::Resolved(resolved_path) => {
                    return match self
                        .candidates(&pending.crate_root, resolved_path, &pending.module_names)
                        .as_slice()
                    {
                        [only] => Some(only),
                        _ => None,
                    };
                }
                ImportBinding::Veto => return None,
                ImportBinding::NoImport => {}
            }
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

    /// Resolves a struct-literal construction type path (issue #443) to the
    /// UNIQUE constructible type definition Symbol it names, or `None` when
    /// nothing matches or the match is ambiguous. Crate-root confinement mirrors
    /// the Path CALLS arm (issue #440): `CurrentCrate` confines to the caller's
    /// crate root, `Leading(name)` confines to a workspace crate's library root
    /// (dropping the crate segment) or falls back to the caller root, and
    /// `Unqualified` matches repo-wide.
    ///
    /// An enum-struct variant literal (`Shape::Circle { … }`) does not resolve on
    /// its full path (the variant is not a type definition), so a path of length
    /// ≥2 is retried after dropping the trailing variant segment, binding the
    /// enum. Only a unique resolution binds; ambiguity binds nothing.
    fn resolve_construct(
        &self,
        type_segments: &[String],
        path_root: &CallPathRoot,
        caller_crate_root: &str,
        crate_name_roots: &BTreeMap<String, Option<String>>,
    ) -> Option<&'facts ImplTargetFact> {
        let (target_root, segments): (Option<&str>, &[String]) = match path_root {
            CallPathRoot::CurrentCrate => (Some(caller_crate_root), type_segments),
            CallPathRoot::Leading(name) => {
                let normalized = name.replace('-', "_");
                match crate_name_roots.get(&normalized) {
                    Some(Some(root)) => (Some(root.as_str()), &type_segments[1..]),
                    _ => (Some(caller_crate_root), type_segments),
                }
            }
            CallPathRoot::Unqualified => (None, type_segments),
        };
        if segments.is_empty() {
            return None;
        }
        if let Some(target) = self.unique_constructible(target_root, segments) {
            return Some(target);
        }
        // Enum-struct variant fallback: drop the trailing variant segment and
        // retry against the enum type (`Shape::Circle` -> `Shape`).
        if segments.len() >= 2 {
            return self.unique_constructible(target_root, &segments[..segments.len() - 1]);
        }
        None
    }

    /// Returns the UNIQUE constructible (`struct`/`enum`) impl-target whose
    /// crate-root-relative qualified name ends with `segments`, confined to
    /// `target_root` when `Some` (else repo-wide). `None` on zero or ≥2 distinct
    /// matches — ambiguity never picks one.
    fn unique_constructible(
        &self,
        target_root: Option<&str>,
        segments: &[String],
    ) -> Option<&'facts ImplTargetFact> {
        let mut matched: Vec<&ImplTargetFact> = Vec::new();
        for ((root, qualified), facts) in &self.by_qualified {
            if let Some(want) = target_root
                && *root != want
            {
                continue;
            }
            if !qualified_ends_with(qualified, segments) {
                continue;
            }
            for fact in facts {
                if is_constructible_kind(&fact.symbol_kind) {
                    matched.push(fact);
                }
            }
        }
        matched.sort_by(|a, b| a.id.cmp(&b.id));
        matched.dedup_by(|a, b| a.id == b.id);
        match matched.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

/// `true` when a symbol kind names a type a struct literal `Type { … }` can
/// construct (issue #443): a `struct` or an `enum` (its struct variants). Trait
/// and `type_alias` targets are excluded — a trait can never be constructed, and
/// an alias is not the constructed type's own definition Symbol.
fn is_constructible_kind(symbol_kind: &str) -> bool {
    matches!(symbol_kind, "struct" | "enum")
}

/// `true` when `qualified`'s `::`-separated segments end with `suffix`
/// (segment-aware suffix match), so a bare `Deal` matches `Deal` and
/// `m::Deal`, and `m::Deal` matches `a::m::Deal` but never `mm::Deal`.
fn qualified_ends_with(qualified: &str, suffix: &[String]) -> bool {
    let full: Vec<&str> = qualified.split("::").collect();
    if suffix.len() > full.len() {
        return false;
    }
    full[full.len() - suffix.len()..]
        .iter()
        .zip(suffix)
        .all(|(segment, expected)| *segment == expected.as_str())
}

/// Tri-state outcome of asking whether a module-item `use` in the impl's exact
/// module scope binds `simple_name` (issue #393; Codex round-5 P2 on PR #399).
///
/// The three states are what let the caller distinguish "the name is shadowed by
/// an import that resolves elsewhere" from "no import binds the name". Collapsing
/// the first two into `None` (the round-4 bug) let an EXTERNAL/AMBIGUOUS binding
/// fall through to the scope walk, which then mis-bound a coincidental
/// same-simple-name definition (a root-local trait whose bare name is NOT
/// "ambiguous" by the counting predicate) — a WRONG `IMPLEMENTS` edge.
enum ImportBinding<'a> {
    /// Exactly one `use` import binds the name to a path that could name an
    /// in-repo target: an explicitly in-repo-rooted path (`crate`/`self`/`super`)
    /// or a bare path whose first segment is NOT a known extern-prelude crate.
    /// The caller resolves this path against the crate-root-partitioned index (an
    /// index miss still mints no edge — no fall-through to the scope walk).
    Resolved(&'a str),
    /// A `use` import binds the name but it is EXTERNAL (a single extern-prelude
    /// crate path — `std::fmt::Display` and friends) or AMBIGUOUS (2+ distinct
    /// paths, cfg-gated or otherwise). The bare name is shadowed by the import, so
    /// the caller mints NO edge and MUST NOT fall through to the scope walk. This
    /// restores PR #389's shadow-veto intent for external/ambiguous imports.
    Veto,
    /// No `use` import binds the name in the impl's module scope. The caller falls
    /// through to the existing scope walk (normal #389 recall for same/parent-
    /// module traits).
    NoImport,
}

/// Classifies whether a module-item `use` binds `simple_name` in the impl's exact
/// module scope (issue #393) into the tri-state [`ImportBinding`]. Scoping mirrors
/// the import-shadow veto exactly (own module scope only; ancestor/block-local
/// imports are never captured here), so recall recovery cannot reintroduce a
/// wrong edge.
///
/// A single binding whose path names an extern-prelude crate (and is not
/// in-repo-rooted) is [`ImportBinding::Veto`] — the external name shadows any
/// coincidental local definition. Two or more DISTINCT bound paths (a real
/// cfg-gated name collision) are also [`ImportBinding::Veto`] — the collision is
/// unresolvable and never silently picks one, nor falls through to the scope
/// walk. Any other single binding is [`ImportBinding::Resolved`].
fn lookup_use_import<'a>(
    use_imports: &'a [UseImportFact],
    module_names: &[String],
    simple_name: &str,
) -> ImportBinding<'a> {
    let mut found: Option<&str> = None;
    let mut ambiguous = false;
    for import in use_imports {
        if import.module_names == module_names && import.simple_name == simple_name {
            match found {
                None => found = Some(import.resolved_path.as_str()),
                Some(existing) if existing != import.resolved_path => ambiguous = true,
                Some(_) => {}
            }
        }
    }
    match found {
        None => ImportBinding::NoImport,
        Some(_) if ambiguous => ImportBinding::Veto,
        Some(path)
            if !use_path_is_in_repo_rooted(path)
                && use_path_first_segment_is_extern_prelude(path) =>
        {
            ImportBinding::Veto
        }
        Some(path) => ImportBinding::Resolved(path),
    }
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
/// "resolved". A pair labeled `resolved` also keeps the deduplicated spans of
/// its `Resolved` call sites on the edge (`call_site_spans`, issue #462);
/// `ambiguous` pairs keep none, per the #233 fabrication-guard discipline.
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
            call_site_spans,
            ..
        } = record
        else {
            continue;
        };
        if resolution.is_some() {
            continue;
        }
        let Some((status, spans)) = resolutions.get(&(source.clone(), target.clone())) else {
            continue;
        };
        *resolution = Some(*status);
        // A `Resolved` pair keeps its deduplicated call-site spans; an
        // ambiguous pair drops them (issue #462, the #233 fabrication-guard
        // discipline).
        if *status == CallResolution::Resolved && !spans.is_empty() {
            *call_site_spans = Some(spans.clone());
        }
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
///
/// The returned spans are the deduplicated call-site spans of the pair's
/// `Resolved` call sites only (issue #462); ambiguous sites contribute none,
/// per the #233 fabrication-guard discipline.
fn same_file_call_resolutions(
    facts_by_file: &BTreeMap<String, FileFacts>,
) -> BTreeMap<(String, String), (CallResolution, Vec<SourceSpan>)> {
    // Same aux-helper crate-root remap as the cross-file CALLS pass
    // (issue #475): a helper's call sites and definitions partition under the
    // including entry crate, so the repo-wide candidate count behind each
    // same-file label is computed under the true crate root.
    let remap = reassign_aux_helper_crate_roots(facts_by_file);
    let remapped;
    let facts_by_file: &BTreeMap<String, FileFacts> = if remap.is_empty() {
        facts_by_file
    } else {
        remapped = apply_crate_root_remap(facts_by_file, &remap);
        &remapped
    };
    let index = DefinitionIndex::build(facts_by_file, remap.clone());
    let mut resolutions = BTreeMap::new();
    for (path, facts) in facts_by_file {
        let caller_crate_root = remap
            .get(path)
            .cloned()
            .unwrap_or_else(|| crate_root_id(path));
        for call in &facts.call_sites {
            let Some(simple_name) = call.callee_segments.last() else {
                continue;
            };
            // Block-local defs shadow the flat pool here too (issue #422): a
            // call the scope gate admits binds the block-local. (The per-file
            // text pass already suppresses its own edge from the enclosing
            // body to the shadowed same-named module-level def, so no stale
            // pair reaches this map.)
            let block_locals = index.block_local_candidates(call, simple_name);
            if !block_locals.is_empty() {
                let status = match block_locals.len() {
                    1 => CallResolution::Resolved,
                    _ => CallResolution::Ambiguous,
                };
                for definition in block_locals {
                    if definition.repo_relative_path != *path || definition.id == call.caller_id {
                        continue;
                    }
                    let entry = resolutions
                        .entry((call.caller_id.clone(), definition.id.clone()))
                        .or_insert((status, Vec::new()));
                    // Prefer the strongest status when several call sites hit one pair.
                    if status < entry.0 {
                        entry.0 = status;
                    }
                    // Only `Resolved` call sites contribute spans; ambiguous
                    // sites are dropped even when the pair later resolves via
                    // another site (issue #462, the #233 fabrication-guard
                    // discipline).
                    if status == CallResolution::Resolved && !entry.1.contains(&call.span) {
                        entry.1.push(call.span);
                    }
                }
                continue;
            }
            let candidates = index.candidates(call, simple_name, &caller_crate_root);
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
                    .or_insert((status, Vec::new()));
                // Prefer the strongest status when several call sites hit one pair.
                if status < entry.0 {
                    entry.0 = status;
                }
                // Only `Resolved` call sites contribute spans; ambiguous sites
                // are dropped even when the pair later resolves via another
                // site (issue #462, the #233 fabrication-guard discipline).
                if status == CallResolution::Resolved && !entry.1.contains(&call.span) {
                    entry.1.push(call.span);
                }
            }
        }
    }
    resolutions
}

/// Records a lexically-scoped block-local call edge (issue #422), mirroring
/// [`record_candidate_edge`] without its same-file skip.
fn record_block_local_edge(
    call: &CallSiteFact,
    definition: &BlockLocalDefinitionFact,
    resolution: CallResolution,
    candidate_count: usize,
    edges: &mut BTreeMap<(String, String), (CallResolution, String, Vec<SourceSpan>)>,
) {
    // A block-local def lives in the caller's own file by construction (the
    // scope gate only matches caller ids from the enclosing scope), so the
    // per-file reference pass can never emit this pair — no same-file skip is
    // needed to avoid duplicate stable edge IDs. Self-recursion
    // (`definition.id == call.caller_id`) mints no edge, mirroring
    // `record_candidate_edge`'s treatment of module-level self-recursion.
    if definition.id == call.caller_id {
        return;
    }
    let summary = match resolution {
        CallResolution::Resolved => format!(
            "{} calls {} (block-local, resolved)",
            call.caller_name, definition.qualified_name
        ),
        CallResolution::Ambiguous => format!(
            "{} calls {} (block-local, ambiguous: {candidate_count} in-scope candidates)",
            call.caller_name, definition.qualified_name
        ),
        CallResolution::Unresolved => unreachable!("unresolved calls never bind a candidate"),
        CallResolution::UnresolvedDispatch => {
            unreachable!("unresolved dispatch never binds a candidate")
        }
    };
    let key = (call.caller_id.clone(), definition.id.clone());
    let entry = edges
        .entry(key)
        .or_insert_with(|| (resolution, summary.clone(), Vec::new()));
    // Prefer the strongest status when several call sites hit one pair.
    if resolution < entry.0 {
        entry.0 = resolution;
        entry.1 = summary;
    }
    // Only `Resolved` call sites contribute spans; ambiguous sites are
    // dropped even when the pair later resolves via another site (issue #462,
    // the #233 fabrication-guard discipline).
    if resolution == CallResolution::Resolved && !entry.2.contains(&call.span) {
        entry.2.push(call.span);
    }
}

/// Records the lexically-scoped block-local edges for one call site (issue
/// #422), returning `true` when the scope gate admitted at least one
/// block-local candidate. A handled call binds the block-local INSTEAD of
/// the flat pool (shadowing), so the caller must skip flat candidate
/// resolution for it.
fn record_block_local_call(
    call: &CallSiteFact,
    block_locals: &[&BlockLocalDefinitionFact],
    edges: &mut BTreeMap<(String, String), (CallResolution, String, Vec<SourceSpan>)>,
) -> bool {
    if block_locals.is_empty() {
        return false;
    }
    let resolution = if block_locals.len() == 1 {
        CallResolution::Resolved
    } else {
        CallResolution::Ambiguous
    };
    for definition in block_locals {
        record_block_local_edge(call, definition, resolution, block_locals.len(), edges);
    }
    true
}

fn record_candidate_edge(
    caller_path: &str,
    call: &CallSiteFact,
    candidate: &DefinitionFact,
    resolution: CallResolution,
    candidate_count: usize,
    // Emit same-file (caller, candidate) pairs. `false` for ordinary names:
    // the per-file textual pass already covers those and re-emitting would
    // duplicate stable edge IDs. `true` for trait-dispatch call sites (issue
    // #267 — dispatch pairs are repo-wide owned, same-file included) and for
    // static sites in a dispatch-suppressed body (whose textual edge the
    // per-file pass suppressed).
    allow_same_file: bool,
    edges: &mut BTreeMap<(String, String), (CallResolution, String, Vec<SourceSpan>)>,
) {
    // Same-file targets are already covered by the per-file reference pass;
    // emitting them again would duplicate stable edge IDs. A self-call
    // (candidate IS the caller) never emits: it is not a call between symbols.
    if !allow_same_file && candidate.repo_relative_path == caller_path {
        return;
    }
    if candidate.id == call.caller_id {
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
        CallResolution::UnresolvedDispatch => {
            unreachable!("unresolved dispatch never binds a candidate")
        }
    };
    let key = (call.caller_id.clone(), candidate.id.clone());
    let entry = edges
        .entry(key)
        .or_insert_with(|| (resolution, summary.clone(), Vec::new()));
    // Prefer the strongest status when several call sites hit one pair.
    if resolution < entry.0 {
        entry.0 = resolution;
        entry.1 = summary;
    }
    // Only `Resolved` call sites contribute spans; ambiguous sites are
    // dropped even when the pair later resolves via another site (issue #462,
    // the #233 fabrication-guard discipline).
    if resolution == CallResolution::Resolved && !entry.2.contains(&call.span) {
        entry.2.push(call.span);
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

/// Records a trait-dispatch call site (issue #267) whose target set could not
/// be reduced to a concrete in-crate symbol — the dispatch trait is not a
/// unique local trait, or no in-crate type implements it. Unlike
/// [`record_unresolved`], this ALWAYS mints the typed marker: a missing
/// dispatch target is exactly the boundary the issue must enumerate, never a
/// dropped miss. The marker is keyed by (file, trait path as written, method)
/// so repeated sites share one node; each caller's edge cites its own span.
fn record_unresolved_dispatch(
    caller_path: &str,
    call: &CallSiteFact,
    trait_path: &str,
    dispatch_diagnostics: &mut BTreeMap<(String, String, String), SourceSpan>,
    dispatch_diagnostic_edges: &mut BTreeMap<(String, String, String, String), String>,
) {
    let method = call.callee_segments.last().cloned().unwrap_or_default();
    dispatch_diagnostics
        .entry((
            caller_path.to_owned(),
            trait_path.to_owned(),
            method.clone(),
        ))
        .or_insert(call.span);
    dispatch_diagnostic_edges
        .entry((
            caller_path.to_owned(),
            trait_path.to_owned(),
            method,
            call.caller_id.clone(),
        ))
        .or_insert_with(|| call.caller_name.clone());
}

fn unresolved_dispatch_diagnostic_id(
    repository_id: &str,
    path: &str,
    trait_path: &str,
    method: &str,
) -> String {
    stable_id(&[
        "node",
        "diagnostic",
        repository_id,
        path,
        "unresolved-dispatch",
        trait_path,
        method,
    ])
}

/// The typed dispatch-boundary marker (issue #267): a `Diagnostic` node named
/// `unresolved_dispatch: Trait::method` carrying the call-site span, targeted
/// by a `resolution: "unresolved_dispatch"` CALLS edge. The trait path is the
/// as-written form the extractor stamped, so the marker cites exactly what the
/// source said.
fn unresolved_dispatch_diagnostic(
    repository_id: &str,
    path: &str,
    trait_path: &str,
    method: &str,
    span: SourceSpan,
) -> GraphRecord {
    let display = format!("{trait_path}::{method}");
    GraphRecord::syntax_node(
        unresolved_dispatch_diagnostic_id(repository_id, path, trait_path, method),
        NodeKind::Diagnostic,
        path.to_owned(),
        span,
        format!("unresolved_dispatch: {display}"),
        "rust",
        format!("unresolved trait-dispatch target {display} (no in-crate implementor method)"),
    )
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
    /// Simple name -> block-local `fn` definitions (issue #422), sorted by
    /// (path, qualified name, ID) like [`DefinitionIndex::by_simple_name`].
    /// Consulted ONLY through [`DefinitionIndex::block_local_candidates`],
    /// which applies the lexical scope gate — never pooled with the flat
    /// index.
    block_locals_by_name: BTreeMap<&'facts str, Vec<&'facts BlockLocalDefinitionFact>>,
    /// `(crate_root, implementing_type) -> {trait qualified name}`: the set of
    /// trait qualified names each type provably implements, for IMPLEMENTS-gated
    /// self-dispatch (issue #414). Built by resolving every
    /// [`ImplTraitRelationFact`]'s trait path through the repo-wide
    /// [`ImplTargetIndex`] so the recorded trait name matches a trait method's
    /// `match_segments` prefix exactly.
    implemented: BTreeMap<(String, String), BTreeSet<String>>,
    /// Inferred-crate-name -> that crate's LIBRARY crate-root id, for resolving
    /// cross-crate qualified calls (`dep_crate::mod::fn()`) to a workspace
    /// member (issue #440). The name is the crate directory's last component,
    /// cargo-normalized (`-` → `_`). A name that maps to two distinct crate
    /// directories is ambiguous and stored as `None`, so it never confers a
    /// (possibly wrong) cross-crate binding — resolution stays conservative.
    crate_name_roots: BTreeMap<String, Option<String>>,
    /// The repo-wide impl-target index, retained so a receiver-type method
    /// narrowing (issue #441) can prove a `let x: T` / param `x: T` receiver
    /// resolves to a UNIQUE LOCAL NON-TRAIT type before narrowing `x.m()` to
    /// that type's method. The `implemented` map above is derived from this
    /// same index; retaining it lets the `Method` arm reuse it directly.
    impl_index: ImplTargetIndex<'facts>,
    /// Aux-helper crate-root remap (issue #475): helper repo-relative path ->
    /// the including entry crate's root, from
    /// [`reassign_aux_helper_crate_roots`]. [`Self::definition_crate_root`]
    /// consults it so a helper's definitions partition under the entry crate
    /// they truly belong to instead of their synthetic path-derived root.
    /// Empty when no helper needed reassignment, in which case partitioning is
    /// exactly the path-derived [`crate_root_id`] behavior.
    aux_helper_roots: BTreeMap<String, String>,
}

impl<'facts> DefinitionIndex<'facts> {
    fn build(
        facts_by_file: &'facts BTreeMap<String, FileFacts>,
        aux_helper_roots: BTreeMap<String, String>,
    ) -> Self {
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

        // Lexically-scoped block-local definitions (issue #422): indexed by
        // simple name but kept OUT of the flat `by_simple_name` pool, so only
        // the scope-gated `block_local_candidates` can ever surface them.
        let mut block_locals_by_name: BTreeMap<&str, Vec<&BlockLocalDefinitionFact>> =
            BTreeMap::new();
        for facts in facts_by_file.values() {
            for definition in &facts.block_local_definitions {
                block_locals_by_name
                    .entry(definition.simple_name.as_str())
                    .or_default()
                    .push(definition);
            }
        }
        for candidates in block_locals_by_name.values_mut() {
            candidates.sort_by(|a, b| {
                (&a.repo_relative_path, &a.qualified_name, &a.id).cmp(&(
                    &b.repo_relative_path,
                    &b.qualified_name,
                    &b.id,
                ))
            });
            candidates.dedup_by(|a, b| a.id == b.id);
        }

        // Build the workspace crate-name registry (issue #440): every crate
        // directory the scanned file set reveals contributes its inferred name
        // -> library crate-root binding, so a `dep_crate::…` qualified call can
        // be confined to that crate's definitions. A name shared by two crate
        // directories is marked ambiguous (`None`) — never a wrong binding.
        // Shared with the #443 CONSTRUCTS resolution pass via one helper.
        let crate_name_roots = build_crate_name_roots(facts_by_file);

        // Resolve every recorded `impl Trait for Type` relation to the trait's
        // crate-root-relative qualified name via the repo-wide impl-target
        // index (issue #414). The facts carry the aux-helper crate-root remap
        // when the caller applied [`reassign_aux_helper_crate_roots`]
        // (issue #475): a helper's impl relations then join under the including
        // entry crate they truly belong to, and the remap is conservative
        // (single-includer helpers only), so the join still never crosses a
        // true crate boundary. An unresolvable trait path (external/std,
        // ambiguous) contributes nothing, so the gate degrades to unresolved
        // (a MISS, never a WRONG edge).
        let impl_index = ImplTargetIndex::build(facts_by_file);
        let mut implemented: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        for facts in facts_by_file.values() {
            for relation in &facts.impl_trait_relations {
                // Same-simple-name-type guard (issue #414): `impl_type` is a BARE
                // simple name (`normalize_impl_owner`). Two DISTINCT types both
                // named `S` in different modules of one crate root would collapse
                // their implemented-trait sets under this bare key, so a
                // `self.read()` in an `impl a::S` that implements nothing could
                // bind a trait `b::S` implements — a WRONG edge. When the bare
                // `impl_type` is ambiguous among the crate root's impl-target
                // TYPE definitions, the type identity is unprovable by bare name,
                // so skip the relation and stay unresolved (a MISS, never a WRONG
                // edge — the same counting predicate `ImplTargetIndex` uses).
                if impl_index
                    .bare_simple_name_is_ambiguous(&relation.crate_root, &relation.impl_type)
                {
                    continue;
                }
                // External-leaf-collision guard (issue #414, Codex P2 on #420):
                // `impl_type` is only the bare leaf, so a trait implemented for an
                // EXTERNAL type whose leaf matches a local type
                // (`impl T for std::string::String` alongside a local
                // `struct String`) would pollute the local type's set under the
                // shared leaf key. Resolve the implementing type's retained PATH
                // against the same `ImplTargetIndex`: keep the relation ONLY when
                // Self resolves to a UNIQUE LOCAL type def (a non-trait
                // impl-target kind). An external / ambiguous / unresolved / trait
                // Self drops the relation, so only genuinely local implementing
                // types contribute — the bare-leaf receiver key then matches only
                // the local `impl S`'s own `receiver_owner`.
                let type_pending = PendingImplFact {
                    source_id: String::new(),
                    trait_path: relation.impl_type_path.clone(),
                    crate_root: relation.crate_root.clone(),
                    module_names: relation.module_names.clone(),
                    shadowed_by_use: false,
                };
                let Some(type_def) = impl_index.resolve(&type_pending, &facts.use_trait_imports)
                else {
                    continue;
                };
                if type_def.symbol_kind == "trait"
                    || !crate::languages::rust::is_impl_target_kind(&type_def.symbol_kind)
                {
                    continue;
                }
                let pending = PendingImplFact {
                    source_id: String::new(),
                    trait_path: relation.trait_path.clone(),
                    crate_root: relation.crate_root.clone(),
                    module_names: relation.module_names.clone(),
                    shadowed_by_use: false,
                };
                if let Some(target) = impl_index.resolve(&pending, &facts.use_trait_imports) {
                    implemented
                        .entry((relation.crate_root.clone(), relation.impl_type.clone()))
                        .or_default()
                        .insert(target.qualified_name.clone());
                }
            }
        }

        Self {
            by_simple_name,
            block_locals_by_name,
            implemented,
            crate_name_roots,
            impl_index,
            aux_helper_roots,
        }
    }

    /// The crate root a definition partitions under for call-target
    /// confinement (issue #475): the aux-helper remap when the definition's
    /// file was reassigned to its including entry crate, else the
    /// path-derived [`crate_root_id`].
    fn definition_crate_root(&self, definition: &DefinitionFact) -> Cow<'_, str> {
        self.aux_helper_roots
            .get(&definition.repo_relative_path)
            .map_or_else(
                || Cow::Owned(crate_root_id(&definition.repo_relative_path)),
                |root| Cow::Borrowed(root.as_str()),
            )
    }

    /// The set of trait qualified names the type `impl_type` provably implements
    /// within `crate_root` (issue #414), or `None` when no proof exists.
    fn implemented_traits(&self, crate_root: &str, impl_type: &str) -> Option<&BTreeSet<String>> {
        self.implemented
            .get(&(crate_root.to_owned(), impl_type.to_owned()))
    }

    /// Resolves a provable receiver type path (issue #441) to the simple-leaf
    /// name of the UNIQUE LOCAL NON-TRAIT type it names within `crate_root`, or
    /// `None` when the type is not such a unique local non-trait type.
    ///
    /// The receiver type is reduced to its trailing simple segment and matched
    /// against this crate root's impl-target index by that leaf. Resolution
    /// succeeds ONLY when exactly one distinct impl-target qualified name in the
    /// root carries that leaf AND it is a non-trait impl-target kind
    /// (`struct`/`enum`/`type_alias`). This is what keeps the narrowing sound:
    /// an EXTERNAL receiver type (`u32`) matches nothing and resolves to `None`
    /// (fall back to today's fan-out); a TRAIT-typed receiver (`dyn Device`)
    /// resolves to a trait and is refused; a leaf shared by two local types is
    /// ambiguous and refused (prefer a MISSING narrowing over a WRONG one).
    fn resolve_receiver_type_owner(&self, crate_root: &str, receiver_type: &str) -> Option<String> {
        let leaf = receiver_type.rsplit("::").next()?.trim();
        if leaf.is_empty() {
            return None;
        }
        // (qualified name) -> whether that name is a non-trait impl-target type.
        let mut matched: BTreeMap<&str, bool> = BTreeMap::new();
        for ((root, qualified), facts) in &self.impl_index.by_qualified {
            if *root != crate_root {
                continue;
            }
            let def_leaf = qualified.rsplit("::").next().unwrap_or(qualified);
            if def_leaf != leaf {
                continue;
            }
            if !facts
                .iter()
                .any(|fact| is_impl_target_kind(&fact.symbol_kind))
            {
                continue;
            }
            let is_non_trait = facts
                .iter()
                .any(|fact| is_impl_target_kind(&fact.symbol_kind) && fact.symbol_kind != "trait")
                && !facts.iter().any(|fact| fact.symbol_kind == "trait");
            matched.insert(qualified, is_non_trait);
        }
        match matched.len() {
            1 => matched
                .into_values()
                .next()
                .filter(|is_non_trait| *is_non_trait)
                .map(|_| leaf.to_owned()),
            // 0 matches: external type. 2+ matches: leaf ambiguous across the
            // crate root. Either way, no narrowing.
            _ => None,
        }
    }

    /// Resolves a provable trait-dispatch path (issue #267) to the
    /// crate-root-relative qualified name of the UNIQUE LOCAL trait it names
    /// within `crate_root`, or `None` when it is not such a unique local trait.
    ///
    /// The dispatch trait is reduced to its trailing simple segment and matched
    /// against this crate root's impl-target index by that leaf, mirroring
    /// [`resolve_receiver_type_owner`](Self::resolve_receiver_type_owner).
    /// Resolution succeeds ONLY when exactly one distinct impl-target
    /// qualified name in the root carries that leaf AND it is a trait
    /// impl-target kind. An external trait, a non-trait type, or a leaf shared
    /// by two local traits resolves to `None` (the call site mints a typed
    /// `unresolved_dispatch` marker instead of guessing — prefer a MISSING
    /// dispatch target over a WRONG one).
    fn resolve_dispatch_trait(&self, crate_root: &str, trait_path: &str) -> Option<String> {
        let leaf = trait_path.rsplit("::").next()?.trim();
        if leaf.is_empty() {
            return None;
        }
        let mut matched: Option<&str> = None;
        for ((root, qualified), facts) in &self.impl_index.by_qualified {
            if *root != crate_root {
                continue;
            }
            let def_leaf = qualified.rsplit("::").next().unwrap_or(qualified);
            if def_leaf != leaf {
                continue;
            }
            if !facts.iter().any(|fact| fact.symbol_kind == "trait") {
                continue;
            }
            if matched.is_some() {
                // Two distinct local traits share the leaf: ambiguous, refuse.
                return None;
            }
            matched = Some(qualified);
        }
        matched.map(str::to_owned)
    }

    /// Returns the in-crate implementor methods for a trait-dispatch call site
    /// (issue #267): every callable `method` definition in the caller's crate
    /// root whose owner segment is a type provably implementing the dispatch
    /// trait, and whose simple name is `simple_name`.
    ///
    /// The implementor set comes from the [`ImplTraitRelationFact`] reverse
    /// join (`implemented`: `(crate_root, impl_type) -> {trait qualified
    /// names}`), keyed by the RESOLVED trait qualified name — never by the
    /// method name alone, so an unrelated trait (or inherent method) that
    /// merely shares the method name can never enter the set. A trait method
    /// DECLARATION (kind `"function"` + `is_trait_method`) is not a callable
    /// implementor and is excluded; only concrete `impl Trait for T` methods
    /// (kind `"method"`) link.
    ///
    /// Modeling note: an inherent method and a trait-impl method of the SAME
    /// type share the same `match_segments` tail (`[owner, name]`), exactly as
    /// in the existing [`narrow_methods_to_owner`](Self::narrow_methods_to_owner)
    /// static-call path — when a type defines both, the dispatch set admits
    /// both symbols (the multi-target `Ambiguous` labeling records that the
    /// set, not one edge, is the answer). Distinguishing them would need a new
    /// serialized fact; the AC's phantom-edge bar is unrelated traits, which
    /// the implementor join already excludes.
    fn dispatch_candidates(
        &self,
        trait_path: &str,
        simple_name: &str,
        caller_crate_root: &str,
    ) -> Vec<&'facts DefinitionFact> {
        let Some(pool) = self.by_simple_name.get(simple_name) else {
            return Vec::new();
        };
        let Some(trait_qualified) = self.resolve_dispatch_trait(caller_crate_root, trait_path)
        else {
            return Vec::new();
        };
        let mut implementors: BTreeSet<&str> = BTreeSet::new();
        for ((root, impl_type), traits) in &self.implemented {
            if root == caller_crate_root && traits.contains(&trait_qualified) {
                implementors.insert(impl_type.as_str());
            }
        }
        if implementors.is_empty() {
            return Vec::new();
        }
        pool.iter()
            .copied()
            .filter(|definition| {
                definition.symbol_kind == "method"
                    && crate_root_id(&definition.repo_relative_path) == caller_crate_root
                    && definition.match_segments.len() >= 2
                    && implementors.contains(
                        definition.match_segments[definition.match_segments.len() - 2].as_str(),
                    )
            })
            .collect()
    }

    /// Narrows a receiver-call method pool to a proven owner type's own methods
    /// (issue #441 shares this with the #420 `SelfMethod` self-dispatch path so
    /// the two can never desync). Given the owner's simple-leaf name and the
    /// broad method pool, it applies: (1) inherent-method precedence — a method
    /// whose `match_segments` end with `[owner, name]`; then (2) the #414
    /// IMPLEMENTS-gated trait-default fallback — a trait default is bound only
    /// for a trait the owner PROVABLY implements, and two or more implemented
    /// traits declaring the method stay UNRESOLVED (empty), never fanned out.
    /// An owner that names nothing yields an empty pool (a MISS, never a WRONG
    /// edge). Byte-for-byte the former `SelfMethod` `Some(owner)` body.
    fn narrow_methods_to_owner(
        &self,
        owner: &str,
        methods: Vec<&'facts DefinitionFact>,
        simple_name: &str,
        caller_crate_root: &str,
    ) -> Vec<&'facts DefinitionFact> {
        let narrowing = [owner.to_owned(), simple_name.to_owned()];
        let inherent: Vec<&DefinitionFact> = methods
            .iter()
            .copied()
            .filter(|definition| segments_end_with(&definition.match_segments, &narrowing))
            .collect();
        // Inherent methods (and a trait's own `self.other()` inside its body,
        // whose owner IS the trait name) take precedence over trait defaults
        // (Rust dispatch). If the owner segment matches directly, bind it and
        // never reach for a trait default.
        if !inherent.is_empty() {
            return inherent;
        }
        // IMPLEMENTS-gated dispatch (issue #414): the owner has no directly-named
        // method. Bind to a trait default ONLY for a trait the owner PROVABLY
        // implements (IMPLEMENTS index join), matched by crate-root-relative
        // trait qualified name. An unrelated trait is excluded; multiple
        // implemented traits declaring the method stay UNRESOLVED; no proof =>
        // empty (unresolved, no wrong edge).
        let Some(implemented) = self.implemented_traits(caller_crate_root, owner) else {
            return Vec::new();
        };
        let gated: Vec<&DefinitionFact> = methods
            .into_iter()
            .filter(|definition| {
                definition.is_trait_method
                    && self.definition_crate_root(definition) == *caller_crate_root
                    && definition.match_segments.len() >= 2
                    && implemented.contains(
                        &definition.match_segments[..definition.match_segments.len() - 1]
                            .join("::"),
                    )
            })
            .collect();
        if gated.len() >= 2 {
            // Multiple implemented traits declare this method. Rust dispatch
            // depends on which trait is in lexical scope at the CALL SITE, which
            // this pass does not resolve; emitting all would mint a false edge to
            // an out-of-scope trait's default. Stay conservative: UNRESOLVED.
            return Vec::new();
        }
        gated
    }

    /// Returns the in-scope block-local `fn` candidates for a call site
    /// (issue #422), deterministically ordered. A block-local def is a
    /// candidate ONLY for a bare (`Direct`) call whose caller is the
    /// lexically-enclosing scope — `call.caller_id` equals the def's
    /// `enclosing_scope_id` (calls in the enclosing function/method body,
    /// including closures and nested blocks, which inherit the enclosing
    /// caller id) — or the def itself (`call.caller_id == def.id`, a recursive
    /// call inside its own body). Calls from any other scope see nothing: the
    /// #413 no-wrong-edge guard.
    ///
    /// A non-empty result SHADOWS the flat pool: the caller is lexically
    /// inside the def's scope, so the block-local wins over any same-named
    /// module-level def (Rust name resolution), and the flat candidates must
    /// not also claim the call.
    fn block_local_candidates(
        &self,
        call: &CallSiteFact,
        simple_name: &str,
    ) -> Vec<&'facts BlockLocalDefinitionFact> {
        if call.call_kind != CallKind::Direct {
            return Vec::new();
        }
        let Some(pool) = self.block_locals_by_name.get(simple_name) else {
            return Vec::new();
        };
        pool.iter()
            .copied()
            .filter(|definition| {
                call.caller_id == definition.enclosing_scope_id || call.caller_id == definition.id
            })
            .collect()
    }

    /// Returns the in-repo candidates for a call site, deterministically
    /// ordered. Pool selection is syntactic:
    ///
    /// - `Direct` calls can only bind free functions (a bare Rust call can
    ///   never invoke a method OR a trait method — trait methods are excluded
    ///   by the `is_trait_method` marker, issue #390).
    /// - `Method` calls bind inherent methods AND trait methods (`x.read()`
    ///   can dispatch to either); `SelfMethod` (`self.x()`) carries its
    ///   enclosing owner (the impl owner, or the trait name inside a trait
    ///   body — issue #390) and narrows STRICTLY to `[owner, name]`, admitting
    ///   both inherent and trait methods but only the owner's own. It never
    ///   falls back to the unnarrowed pool: a `self.other()` naming nothing on
    ///   the owner stays unresolved rather than fan out to an unrelated type's
    ///   or trait's same-named method.
    /// - `Path` calls bind any callable whose match segments end with the
    ///   normalized call path — including a trait method whose segments now
    ///   carry the enclosing trait name (`Device::read`, issue #390); a
    ///   single-segment path degrades to the free-function pool (and so, like
    ///   `Direct`, never reaches a trait method).
    fn candidates(
        &self,
        call: &CallSiteFact,
        simple_name: &str,
        caller_crate_root: &str,
    ) -> Vec<&'facts DefinitionFact> {
        let Some(pool) = self.by_simple_name.get(simple_name) else {
            return Vec::new();
        };
        match call.call_kind {
            CallKind::Direct => pool
                .iter()
                .copied()
                .filter(|definition| is_free_function(definition))
                .collect(),
            CallKind::Method => {
                // Trait-dispatch resolution (issue #267): a call whose receiver
                // is syntactically PROVABLE as trait-typed (`&dyn Trait` or a
                // `T: Trait` bound, stamped by the extractor) dispatches ONLY
                // to the trait's in-crate implementor methods — never the broad
                // same-name pool. This arm runs first: a dispatch site's
                // candidate set is defined by its trait identity, and the
                // receiver-type narrowing below is for CONCRETE receivers.
                if let Some(trait_path) = &call.dispatch_trait {
                    return self.dispatch_candidates(trait_path, simple_name, caller_crate_root);
                }
                // A receiver call `x.read()` can dispatch to an inherent impl
                // method OR a trait method (issue #390). Trait methods keep kind
                // `"function"`, so widen the pool by the marker.
                let methods: Vec<&DefinitionFact> = pool
                    .iter()
                    .copied()
                    .filter(|definition| {
                        definition.symbol_kind == "method" || definition.is_trait_method
                    })
                    .collect();
                // Provable receiver-type narrowing (issue #441): when the
                // receiver is a simple, unshadowed identifier whose type is
                // syntactically PROVABLE and resolves to a UNIQUE LOCAL NON-TRAIT
                // type, narrow to THAT type's own method exactly as the
                // `SelfMethod` arm narrows to `self`'s owner — one shared
                // implementation so the two can never desync. Any non-provable /
                // external / trait-typed / ambiguous receiver leaves the pool
                // untouched: today's ambiguous fan-out to every same-named method
                // (the pre-#441 behavior stays byte-identical).
                if let Some(receiver_type) = &call.receiver_type
                    && let Some(owner) =
                        self.resolve_receiver_type_owner(caller_crate_root, receiver_type)
                {
                    return self.narrow_methods_to_owner(
                        &owner,
                        methods,
                        simple_name,
                        caller_crate_root,
                    );
                }
                methods
            }
            CallKind::SelfMethod => {
                // A `self.method()` receiver call reaches an inherent impl
                // method OR a trait method: inside a trait body `self` is the
                // trait's `Self`, so a default method calling `self.other()`
                // dispatches to the trait's own method (issue #390). Trait
                // methods keep kind `"function"`, so widen the pool by the
                // marker — symmetric with the `Method` pool.
                let methods: Vec<&DefinitionFact> = pool
                    .iter()
                    .copied()
                    .filter(|definition| {
                        definition.symbol_kind == "method" || definition.is_trait_method
                    })
                    .collect();
                match &call.receiver_owner {
                    // A `self` call always carries its enclosing owner (the impl
                    // owner, or the trait name inside a trait body), so narrow
                    // strictly to `[owner, name]` via the shared narrowing and
                    // return that set even when it is EMPTY. Never fall back to
                    // the unnarrowed pool: a trait `self.other()` naming nothing
                    // on the trait must stay unresolved, never fan out to an
                    // unrelated `U::other` (prefer a MISSING edge over a WRONG
                    // one). Because the trait method `T::read` has
                    // `match_segments = [.., "T", "read"]`, `self.read()` with
                    // owner `T` narrows to `T::read` only; an unrelated `U::read`
                    // (`[.., "U", "read"]`) fails the `[T, read]` suffix.
                    Some(owner) => {
                        self.narrow_methods_to_owner(owner, methods, simple_name, caller_crate_root)
                    }
                    // A `self` call with no resolvable owner cannot be narrowed;
                    // this is unreachable for extractor-produced facts (a `self`
                    // receiver always sits inside an impl or trait), but if it
                    // ever arises it degrades to the broad pool like `Method`
                    // rather than inventing narrowing.
                    None => methods,
                }
            }
            CallKind::Path => {
                if call.callee_segments.len() <= 1 {
                    return pool
                        .iter()
                        .copied()
                        .filter(|definition| is_free_function(definition))
                        .collect();
                }
                // Determine WHICH crate's definitions this qualified path may
                // bind, and the crate-relative segments to suffix-match, from
                // the call's preserved leading scope (issue #440):
                //
                // - `crate`/`self`/`super` head -> the caller's own crate root.
                // - a bare head naming a WORKSPACE crate -> that crate's library
                //   root, with the crate name segment dropped (`dep::mod::fn`
                //   suffix-matches the dep's `mod::fn`).
                // - any other bare head (an in-crate relative path, or an
                //   external crate) -> the caller's own crate root, full
                //   segments; an external crate then has no local module to
                //   match and stays honestly unresolved (no wrong edge).
                // - a legacy `Unqualified` Path fact -> repo-wide match
                //   (unchanged pre-#440 behavior).
                let (target_root, segments): (Option<&str>, &[String]) = match &call.path_root {
                    CallPathRoot::CurrentCrate => {
                        (Some(caller_crate_root), call.callee_segments.as_slice())
                    }
                    CallPathRoot::Leading(name) => {
                        let normalized = name.replace('-', "_");
                        match self.crate_name_roots.get(&normalized) {
                            Some(Some(root)) => (Some(root.as_str()), &call.callee_segments[1..]),
                            _ => (Some(caller_crate_root), call.callee_segments.as_slice()),
                        }
                    }
                    CallPathRoot::Unqualified => (None, call.callee_segments.as_slice()),
                };
                // `target_root == None` is the legacy repo-wide match (no crate
                // confinement); `Some(root)` confines to that crate root.
                pool.iter()
                    .copied()
                    .filter(|definition| {
                        target_root
                            .is_none_or(|root| self.definition_crate_root(definition) == *root)
                            && segments_end_with(&definition.match_segments, segments)
                    })
                    .collect()
            }
        }
    }
}

fn is_free_function(definition: &DefinitionFact) -> bool {
    // Trait methods keep `symbol_kind == "function"` but are NOT free
    // functions: a bare `read()` (`Direct`, or a single-segment `Path`) can
    // never invoke a trait method, so excluding them here closes the
    // corollary false-bind hole (issue #390) — prefer a missing edge over a
    // wrong one.
    (definition.symbol_kind == "function" || definition.symbol_kind == "test")
        && !definition.is_trait_method
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
            start_column: None,
            end_column: None,
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
            is_trait_method: false,
            repo_relative_path: path.to_owned(),
        }
    }

    /// A trait method definition (issue #390): kind `"function"` with
    /// `is_trait_method == true`, and `match_segments` carrying the enclosing
    /// trait name as the owner segment (e.g. `["a", "Device", "read"]`).
    fn trait_method_definition(id: &str, path: &str, segments: &[&str]) -> DefinitionFact {
        DefinitionFact {
            is_trait_method: true,
            ..definition(id, "function", path, segments)
        }
    }

    fn span_at(
        start_byte: usize,
        end_byte: usize,
        start_line: usize,
        end_line: usize,
    ) -> SourceSpan {
        SourceSpan {
            start_byte,
            end_byte,
            start_line,
            end_line,
            start_column: None,
            end_column: None,
        }
    }

    fn call_at(
        caller_id: &str,
        display: &str,
        segments: &[&str],
        kind: CallKind,
        owner: Option<&str>,
        span: SourceSpan,
    ) -> CallSiteFact {
        CallSiteFact {
            caller_id: caller_id.to_owned(),
            caller_name: caller_id.to_owned(),
            callee_display: display.to_owned(),
            callee_segments: segments.iter().map(|s| (*s).to_owned()).collect(),
            call_kind: kind,
            // Explicit-fact tests keep the pre-#440 repo-wide matching; the
            // real extractor sets a specific `path_root` for scoped-path calls.
            path_root: CallPathRoot::Unqualified,
            receiver_owner: owner.map(ToOwned::to_owned),
            receiver_type: None,
            dispatch_trait: None,
            span,
        }
    }

    fn call(
        caller_id: &str,
        display: &str,
        segments: &[&str],
        kind: CallKind,
        owner: Option<&str>,
    ) -> CallSiteFact {
        call_at(caller_id, display, segments, kind, owner, span())
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

    // ── Route-registration resolver ladder (issue #445) ──────────────────────

    fn route_site(owner: &str, handler: &str) -> RouteRegistrationFact {
        RouteRegistrationFact {
            owner_id: owner.to_owned(),
            owner_name: owner.to_owned(),
            handler_display: handler.to_owned(),
            handler_segments: vec![handler.to_owned()],
            path_root: CallPathRoot::Unqualified,
            span: span(),
        }
    }

    fn facts_with_routes(
        entries: &[(&str, Vec<DefinitionFact>, Vec<RouteRegistrationFact>)],
    ) -> BTreeMap<String, FileFacts> {
        entries
            .iter()
            .map(|(path, definitions, routes)| {
                (
                    (*path).to_owned(),
                    FileFacts {
                        definitions: definitions.clone(),
                        route_registration_sites: routes.clone(),
                        ..FileFacts::default()
                    },
                )
            })
            .collect()
    }

    fn registers_route_targets(records: &[GraphRecord]) -> Vec<String> {
        records
            .iter()
            .filter_map(|record| match record {
                GraphRecord::Edge {
                    label: EdgeLabel::RegistersRoute,
                    target,
                    ..
                } => Some(target.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn route_registration_resolves_cross_file_same_crate_and_mints_edge() {
        let facts = facts_with_routes(&[
            (
                "src/handlers.rs",
                vec![definition(
                    "h_list",
                    "function",
                    "src/handlers.rs",
                    &["handlers", "list_contacts"],
                )],
                vec![],
            ),
            (
                "src/app.rs",
                vec![definition(
                    "build",
                    "function",
                    "src/app.rs",
                    &["app", "build"],
                )],
                vec![route_site("build", "list_contacts")],
            ),
        ]);
        let records = cross_file_call_records("repo", &facts);
        assert_eq!(
            registers_route_targets(&records),
            vec!["h_list".to_owned()],
            "a routes! registration must resolve cross-file to the handler and mint one edge"
        );
        // The edge is Resolved, confidence 1.0, and runs owner -> handler.
        let edge = records
            .iter()
            .find(|record| {
                matches!(
                    record,
                    GraphRecord::Edge {
                        label: EdgeLabel::RegistersRoute,
                        ..
                    }
                )
            })
            .expect("REGISTERS_ROUTE edge present");
        match edge {
            GraphRecord::Edge {
                source,
                target,
                confidence,
                ..
            } => {
                assert_eq!(source, "build");
                assert_eq!(target, "h_list");
                assert_eq!(confidence.as_deref(), Some("1.0"));
            }
            _ => unreachable!(),
        }
        assert_eq!(edge.resolution(), Some(CallResolution::Resolved));
    }

    #[test]
    fn ambiguous_route_handler_name_stays_unbound_with_no_edge_or_diagnostic() {
        // Two distinct free functions share the simple name `handler`, so the
        // reference is ambiguous: mint NOTHING (no edge, no diagnostic).
        let facts = facts_with_routes(&[
            (
                "src/a.rs",
                vec![definition(
                    "a_handler",
                    "function",
                    "src/a.rs",
                    &["a", "handler"],
                )],
                vec![],
            ),
            (
                "src/b.rs",
                vec![definition(
                    "b_handler",
                    "function",
                    "src/b.rs",
                    &["b", "handler"],
                )],
                vec![],
            ),
            (
                "src/app.rs",
                vec![definition(
                    "build",
                    "function",
                    "src/app.rs",
                    &["app", "build"],
                )],
                vec![route_site("build", "handler")],
            ),
        ]);
        let records = cross_file_call_records("repo", &facts);
        assert!(
            registers_route_targets(&records).is_empty(),
            "an ambiguous handler name must mint no REGISTERS_ROUTE edge"
        );
        assert!(
            !records.iter().any(|record| matches!(
                record,
                GraphRecord::Node {
                    kind: NodeKind::Diagnostic,
                    ..
                }
            )),
            "an ambiguous registration must mint no Diagnostic (no-wrong-edge)"
        );
    }

    #[test]
    fn unresolvable_route_handler_stays_unresolved_with_no_edge() {
        let facts = facts_with_routes(&[(
            "src/app.rs",
            vec![definition(
                "build",
                "function",
                "src/app.rs",
                &["app", "build"],
            )],
            vec![route_site("build", "does_not_exist")],
        )]);
        let records = cross_file_call_records("repo", &facts);
        assert!(
            registers_route_targets(&records).is_empty(),
            "an unresolvable handler reference must mint no REGISTERS_ROUTE edge"
        );
    }

    #[test]
    fn route_registration_records_are_byte_deterministic() {
        let facts = facts_with_routes(&[
            (
                "src/handlers.rs",
                vec![
                    definition("h_a", "function", "src/handlers.rs", &["handlers", "a"]),
                    definition("h_b", "function", "src/handlers.rs", &["handlers", "b"]),
                ],
                vec![],
            ),
            (
                "src/app.rs",
                vec![definition(
                    "build",
                    "function",
                    "src/app.rs",
                    &["app", "build"],
                )],
                vec![route_site("build", "a"), route_site("build", "b")],
            ),
        ]);
        let first = cross_file_route_records("repo", &facts);
        let second = cross_file_route_records("repo", &facts);
        assert_eq!(first, second, "repeated resolution must be byte-identical");
        assert_eq!(registers_route_targets(&first).len(), 2);
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

    // --- Trait-method call resolution (issue #390) ------------------------

    #[test]
    fn path_call_binds_trait_method_by_owner_segment() {
        // `Device::read()` (`Path`, `["Device","read"]`) name-resolves to a
        // trait method whose match segments carry the trait name.
        let facts = facts(&[(
            "src/a.rs",
            vec![trait_method_definition(
                "t-read",
                "src/a.rs",
                &["a", "Device", "read"],
            )],
            vec![],
        )]);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call(
            "caller",
            "Device::read",
            &["Device", "read"],
            CallKind::Path,
            None,
        );
        let ids: Vec<&str> = index
            .candidates(&call, "read", "lib")
            .iter()
            .map(|definition| definition.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["t-read"],
            "Device::read must bind the trait method"
        );
    }

    #[test]
    fn method_call_pool_includes_trait_methods() {
        // `x.read()` (`Method`) reaches a trait method even though it keeps
        // `symbol_kind == "function"`.
        let facts = facts(&[(
            "src/a.rs",
            vec![trait_method_definition(
                "t-read",
                "src/a.rs",
                &["a", "Device", "read"],
            )],
            vec![],
        )]);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call("caller", "read", &["read"], CallKind::Method, None);
        let ids: Vec<&str> = index
            .candidates(&call, "read", "lib")
            .iter()
            .map(|definition| definition.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["t-read"],
            "x.read() must include the trait method"
        );
    }

    #[test]
    fn bare_direct_call_never_binds_a_trait_method() {
        // The corollary closure (issue #390): a bare `read()` (`Direct`) can
        // never invoke a trait method, so the pool is empty even though the
        // trait method keeps `symbol_kind == "function"`.
        let facts = facts(&[(
            "src/a.rs",
            vec![trait_method_definition(
                "t-read",
                "src/a.rs",
                &["a", "Device", "read"],
            )],
            vec![],
        )]);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call("caller", "read", &["read"], CallKind::Direct, None);
        assert!(
            index.candidates(&call, "read", "lib").is_empty(),
            "a bare read() must not bind a trait method"
        );
    }

    #[test]
    fn single_segment_path_call_never_binds_a_trait_method() {
        // A single-segment `Path` degrades to the free-function pool, which
        // now excludes trait methods (issue #390).
        let facts = facts(&[(
            "src/a.rs",
            vec![trait_method_definition(
                "t-read",
                "src/a.rs",
                &["a", "Device", "read"],
            )],
            vec![],
        )]);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call("caller", "read", &["read"], CallKind::Path, None);
        assert!(
            index.candidates(&call, "read", "lib").is_empty(),
            "a single-segment read() path must not bind a trait method"
        );
    }

    #[test]
    fn path_call_binds_only_the_exact_trait_suffix() {
        // `A::read()` with two traits `A` and `B` each declaring `read` binds
        // ONLY `A::read` — the exact owner suffix, never a fan-out.
        let facts = facts(&[(
            "src/a.rs",
            vec![
                trait_method_definition("a-read", "src/a.rs", &["a", "A", "read"]),
                trait_method_definition("b-read", "src/a.rs", &["a", "B", "read"]),
            ],
            vec![],
        )]);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call("caller", "A::read", &["A", "read"], CallKind::Path, None);
        let ids: Vec<&str> = index
            .candidates(&call, "read", "lib")
            .iter()
            .map(|definition| definition.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a-read"], "A::read must bind only A::read");
    }

    #[test]
    fn path_call_does_not_bind_a_free_function_named_like_the_method() {
        // `Device::read()` must not fall through to a free function `read`
        // that shares the simple name but not the trait owner segment.
        let facts = facts(&[(
            "src/a.rs",
            vec![definition(
                "free-read",
                "function",
                "src/a.rs",
                &["a", "read"],
            )],
            vec![],
        )]);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call(
            "caller",
            "Device::read",
            &["Device", "read"],
            CallKind::Path,
            None,
        );
        assert!(
            index.candidates(&call, "read", "lib").is_empty(),
            "Device::read must not bind an unrelated free function read"
        );
    }

    #[test]
    fn method_call_fans_out_to_all_trait_method_candidates() {
        // Two same-named trait methods → the `Method` pool holds both, so the
        // driver labels ambiguous edges to each, never a silent winner.
        let facts = facts(&[
            (
                "src/a.rs",
                vec![trait_method_definition(
                    "a-read",
                    "src/a.rs",
                    &["a", "Device", "read"],
                )],
                vec![],
            ),
            (
                "src/b.rs",
                vec![trait_method_definition(
                    "b-read",
                    "src/b.rs",
                    &["b", "Sensor", "read"],
                )],
                vec![],
            ),
            (
                "src/c.rs",
                vec![],
                vec![call("caller", "read", &["read"], CallKind::Method, None)],
            ),
        ]);
        let records = cross_file_call_records("repo", &facts);
        let mut ambiguous = edge_targets(&records, CallResolution::Ambiguous);
        ambiguous.sort();
        assert_eq!(
            ambiguous,
            vec!["a-read".to_owned(), "b-read".to_owned()],
            "two trait methods named read must both be labeled ambiguous candidates"
        );
        assert!(edge_targets(&records, CallResolution::Resolved).is_empty());
    }

    #[test]
    fn self_method_pool_includes_trait_methods_narrowed_by_owner() {
        // REGRESSION (issue #390): `self.read()` inside `trait T` carries owner
        // `T`. The SelfMethod pool is widened to admit trait methods, but the
        // owner-narrowing suffix keeps ONLY `T::read` — an unrelated trait
        // `U::read` never enters the candidate set.
        let facts = facts(&[(
            "src/a.rs",
            vec![
                trait_method_definition("t-read", "src/a.rs", &["a", "T", "read"]),
                trait_method_definition("u-read", "src/a.rs", &["a", "U", "read"]),
            ],
            vec![],
        )]);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call("caller", "read", &["read"], CallKind::SelfMethod, Some("T"));
        let ids: Vec<&str> = index
            .candidates(&call, "read", "lib")
            .iter()
            .map(|definition| definition.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["t-read"],
            "self.read() in trait T must bind only T::read, never U::read"
        );
    }

    #[test]
    fn self_method_with_no_owner_match_returns_empty_not_the_broad_pool() {
        // NO-WRONG-EDGE / no fallback (issue #390): `self.read()` with owner
        // `T` where no `T::read` exists must NOT fall back to the unnarrowed
        // pool. An inherent `Q::read` shares the simple name but not the owner,
        // so the candidate set stays empty (prefer a MISSING edge over a WRONG
        // one). This is the fallback-removal guard: pre-fix the branch returned
        // `methods` (here `[q-read]`) when the narrowed set was empty.
        let facts = facts(&[(
            "src/a.rs",
            vec![definition(
                "q-read",
                "method",
                "src/a.rs",
                &["a", "Q", "read"],
            )],
            vec![],
        )]);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call("caller", "read", &["read"], CallKind::SelfMethod, Some("T"));
        assert!(
            index.candidates(&call, "read", "lib").is_empty(),
            "self.read() with owner T and no T::read must stay empty, never fan out to Q::read"
        );
    }

    #[test]
    fn impl_self_method_call_never_admits_a_trait_method() {
        // SYMMETRY GUARD (issue #390): an impl `self.g()` (owner `S`) still
        // narrows to its inherent `S::g`; the SelfMethod trait-method widening
        // cannot leak a same-named trait method `T::g` into an impl self-call,
        // because `T::g`'s segments end `[T, g]`, failing the `[S, g]` suffix.
        let facts = facts(&[(
            "src/a.rs",
            vec![
                definition("s-g", "method", "src/a.rs", &["a", "S", "g"]),
                trait_method_definition("t-g", "src/a.rs", &["a", "T", "g"]),
            ],
            vec![],
        )]);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call("caller", "g", &["g"], CallKind::SelfMethod, Some("S"));
        let ids: Vec<&str> = index
            .candidates(&call, "g", "lib")
            .iter()
            .map(|definition| definition.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["s-g"],
            "impl self.g() must bind only the inherent S::g, never the trait method T::g"
        );
    }

    /// Facts for the IMPLEMENTS-gated self-dispatch join (issue #414): a root
    /// trait `T` and `U` each declaring a `read` default, plus (optionally) an
    /// `impl T for S` relation proving `S` implements `T`.
    fn self_dispatch_facts(with_impl_t_for_s: bool) -> BTreeMap<String, FileFacts> {
        let mut map = BTreeMap::new();
        map.insert(
            "src/lib.rs".to_owned(),
            FileFacts {
                definitions: vec![
                    trait_method_definition("t-read", "src/lib.rs", &["T", "read"]),
                    trait_method_definition("u-read", "src/lib.rs", &["U", "read"]),
                ],
                impl_targets: vec![
                    impl_target_in("T", "T", &[], "trait", "lib"),
                    impl_target_in("U", "U", &[], "trait", "lib"),
                    // The local implementing type must resolve to a UNIQUE LOCAL
                    // type def (issue #414 / Codex P2 on #420), so `S` is a
                    // struct impl-target here.
                    impl_target_in("S", "S", &[], "struct", "lib"),
                ],
                impl_trait_relations: if with_impl_t_for_s {
                    vec![ImplTraitRelationFact {
                        impl_type: "S".to_owned(),
                        impl_type_path: "S".to_owned(),
                        trait_path: "T".to_owned(),
                        crate_root: "lib".to_owned(),
                        module_names: vec![],
                    }]
                } else {
                    vec![]
                },
                ..FileFacts::default()
            },
        );
        map
    }

    #[test]
    fn self_dispatch_admits_only_an_implemented_trait_method() {
        // PRECISION (issue #414): `self.read()` with owner `S`, where `S` has no
        // inherent `read` but PROVABLY implements `T` (whose default declares
        // `read`), binds ONLY `T::read`. The unrelated `U::read` (S does not
        // implement `U`) is excluded even though it shares the simple name.
        let facts = self_dispatch_facts(true);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call("caller", "read", &["read"], CallKind::SelfMethod, Some("S"));
        let ids: Vec<&str> = index
            .candidates(&call, "read", "lib")
            .iter()
            .map(|definition| definition.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["t-read"],
            "self.read() in impl S must bind only the implemented T::read, never U::read"
        );
    }

    #[test]
    fn self_dispatch_without_an_implements_proof_admits_nothing() {
        // NO-WRONG-EDGE (issue #414): the same fixture WITHOUT the `impl T for S`
        // relation leaves `S`'s implemented-trait set empty, so `self.read()`
        // binds nothing — a MISS, never a WRONG edge to `T::read` or `U::read`.
        let facts = self_dispatch_facts(false);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call("caller", "read", &["read"], CallKind::SelfMethod, Some("S"));
        assert!(
            index.candidates(&call, "read", "lib").is_empty(),
            "self.read() with no `impl T for S` proof must admit no trait-default candidate"
        );
    }

    #[test]
    fn self_dispatch_gate_respects_the_caller_crate_root() {
        // CONSERVATISM (issue #414): the implemented-trait set is keyed on the
        // caller's crate root. A caller in a DIFFERENT crate root (`bin:tool`)
        // finds no proof for `S`, so the trait default is not bound — a
        // cross-crate-root trait degrades to unresolved, never a wrong edge.
        let facts = self_dispatch_facts(true);
        let index = DefinitionIndex::build(&facts, BTreeMap::new());
        let call = call("caller", "read", &["read"], CallKind::SelfMethod, Some("S"));
        assert!(
            index.candidates(&call, "read", "bin:tool").is_empty(),
            "the IMPLEMENTS gate must not cross crate roots"
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

    // ── Call-site span retention on edges (issue #462) ───────────────────

    #[test]
    fn resolved_cross_file_edge_retains_deduplicated_call_site_spans() {
        let helper = definition("helper", "function", "src/a.rs", &["a", "helper"]);
        let span_a = span_at(10, 16, 2, 2);
        let span_b = span_at(40, 46, 5, 5);
        let facts = facts(&[
            ("src/a.rs", vec![helper], vec![]),
            (
                "src/b.rs",
                vec![],
                vec![
                    call_at(
                        "caller",
                        "helper",
                        &["helper"],
                        CallKind::Direct,
                        None,
                        span_a,
                    ),
                    call_at(
                        "caller",
                        "a::helper",
                        &["a", "helper"],
                        CallKind::Path,
                        None,
                        span_b,
                    ),
                    // A repeated call site at an already-seen span must not
                    // duplicate the span on the edge.
                    call_at(
                        "caller",
                        "helper",
                        &["helper"],
                        CallKind::Direct,
                        None,
                        span_a,
                    ),
                ],
            ),
        ]);
        let records = cross_file_call_records("repo", &facts);
        assert_eq!(records.len(), 1, "sites must still collapse to one edge");
        let edge = &records[0];
        assert_eq!(edge.resolution(), Some(CallResolution::Resolved));
        assert_eq!(
            edge.call_site_spans(),
            Some([span_a, span_b].as_slice()),
            "a resolved edge must retain every distinct call-site span, in scan order"
        );
    }

    #[test]
    fn ambiguous_cross_file_edge_carries_no_call_site_spans() {
        let facts = facts(&[
            (
                "src/a.rs",
                vec![definition("a-dupe", "function", "src/a.rs", &["a", "dupe"])],
                vec![],
            ),
            (
                "src/b.rs",
                vec![definition("b-dupe", "function", "src/b.rs", &["b", "dupe"])],
                vec![],
            ),
            (
                "src/c.rs",
                vec![],
                vec![call_at(
                    "caller",
                    "dupe",
                    &["dupe"],
                    CallKind::Direct,
                    None,
                    span_at(3, 7, 1, 1),
                )],
            ),
        ]);
        let records = cross_file_call_records("repo", &facts);
        let edges: Vec<&GraphRecord> = records
            .iter()
            .filter(|record| matches!(record, GraphRecord::Edge { .. }))
            .collect();
        assert_eq!(
            edges.len(),
            2,
            "an ambiguous call must fan out to both candidates"
        );
        for edge in edges {
            assert_eq!(edge.resolution(), Some(CallResolution::Ambiguous));
            assert_eq!(
                edge.call_site_spans(),
                None,
                "an ambiguous edge must not retain call-site spans (fabrication guard)"
            );
        }
    }

    #[test]
    fn same_file_resolved_edge_retains_call_site_spans() {
        let span_a = span_at(10, 16, 2, 2);
        let facts = facts(&[(
            "src/a.rs",
            vec![definition(
                "helper",
                "function",
                "src/a.rs",
                &["a", "helper"],
            )],
            vec![call_at(
                "caller",
                "helper",
                &["helper"],
                CallKind::Direct,
                None,
                span_a,
            )],
        )]);
        let mut records = vec![per_file_calls_edge("caller", "helper")];
        label_same_file_call_resolutions(&mut records, &facts);
        assert_eq!(records[0].resolution(), Some(CallResolution::Resolved));
        assert_eq!(
            records[0].call_site_spans(),
            Some([span_a].as_slice()),
            "a resolved same-file edge must retain its call-site span"
        );
    }

    #[test]
    fn same_file_ambiguous_edge_carries_no_call_site_spans() {
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
        assert_eq!(
            records[0].call_site_spans(),
            None,
            "an ambiguous same-file edge must not retain call-site spans (fabrication guard)"
        );
    }

    #[test]
    fn same_file_mixed_pair_keeps_only_resolved_site_spans() {
        // One pair, two call sites: a path-qualified site that resolves
        // uniquely and a bare site that is ambiguous. The pair labels
        // `resolved` (strongest wins) but only the resolved site's span may
        // be retained — the ambiguous site's span is dropped, never promoted.
        let resolved_span = span_at(10, 16, 2, 2);
        let ambiguous_span = span_at(40, 46, 5, 5);
        let facts = facts(&[
            (
                "src/a.rs",
                vec![definition("a-dupe", "function", "src/a.rs", &["a", "dupe"])],
                vec![
                    call_at(
                        "caller",
                        "a::dupe",
                        &["a", "dupe"],
                        CallKind::Path,
                        None,
                        resolved_span,
                    ),
                    call_at(
                        "caller",
                        "dupe",
                        &["dupe"],
                        CallKind::Direct,
                        None,
                        ambiguous_span,
                    ),
                ],
            ),
            (
                "src/b.rs",
                vec![definition("b-dupe", "function", "src/b.rs", &["b", "dupe"])],
                vec![],
            ),
        ]);
        let mut records = vec![per_file_calls_edge("caller", "a-dupe")];
        label_same_file_call_resolutions(&mut records, &facts);
        assert_eq!(records[0].resolution(), Some(CallResolution::Resolved));
        assert_eq!(
            records[0].call_site_spans(),
            Some([resolved_span].as_slice()),
            "only resolved call sites may contribute spans to a resolved edge"
        );
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

    // --- aux-target helper-module crate-root reassignment (issue #394; Codex
    // round 2/3, PR #399) ----------------------------------------------------

    #[test]
    fn aux_entry_crate_root_identifies_only_own_crate_entry_files() {
        // Single-file and directory-form aux ENTRY crate roots.
        assert_eq!(
            aux_entry_crate_root("tests/it.rs").as_deref(),
            Some("test:it")
        );
        assert_eq!(
            aux_entry_crate_root("tests/it/main.rs").as_deref(),
            Some("test:it")
        );
        assert_eq!(
            aux_entry_crate_root("examples/demo.rs").as_deref(),
            Some("example:demo")
        );
        assert_eq!(
            aux_entry_crate_root("benches/perf.rs").as_deref(),
            Some("bench:perf")
        );
        // A nested helper module is NOT an entry crate root.
        assert_eq!(aux_entry_crate_root("tests/common/mod.rs"), None);
        assert_eq!(aux_entry_crate_root("tests/it/helper.rs"), None);
        // Non-aux paths are never entry roots.
        assert_eq!(aux_entry_crate_root("src/lib.rs"), None);
        assert_eq!(aux_entry_crate_root("src/bin/tool.rs"), None);
    }

    /// Builds one file's facts with an out-of-line `mod <name>;` declaration.
    fn mod_only_facts(names: &[&str]) -> FileFacts {
        FileFacts {
            out_of_line_mods: names
                .iter()
                .map(|name| OutOfLineModFact {
                    name: (*name).to_owned(),
                    inline_module_path: Vec::new(),
                    test_gated: false,
                    path_override: None,
                    under_inline_path_override: false,
                    cfg_gates: Vec::new(),
                })
                .collect(),
            ..FileFacts::default()
        }
    }

    #[test]
    fn single_includer_test_helper_is_reassigned_to_entry_crate_root() {
        // `tests/it.rs` (crate root `test:it`) includes `mod common;`, resolving
        // to `tests/common/mod.rs` (a crate root owns its CONTAINING directory).
        let facts: BTreeMap<String, FileFacts> = [
            ("tests/it.rs".to_owned(), mod_only_facts(&["common"])),
            (
                "tests/common/mod.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in(
                        "helper-Foo",
                        "Foo",
                        &[],
                        "struct",
                        "test:common",
                    )],
                    pending_impls: vec![pending_impl_in(
                        "impl-helper-Foo",
                        "crate::T",
                        &[],
                        "test:common",
                    )],
                    impl_trait_relations: vec![ImplTraitRelationFact {
                        impl_type: "Foo".to_owned(),
                        impl_type_path: "Foo".to_owned(),
                        trait_path: "crate::T".to_owned(),
                        crate_root: "test:common".to_owned(),
                        module_names: Vec::new(),
                    }],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let remap = reassign_aux_helper_crate_roots(&facts);
        assert_eq!(
            remap.get("tests/common/mod.rs").map(String::as_str),
            Some("test:it"),
            "a single-includer helper is reassigned to the including entry crate"
        );
        // The remap rewrites crate_root on all three fact kinds.
        let remapped = apply_crate_root_remap(&facts, &remap);
        let helper = &remapped["tests/common/mod.rs"];
        assert_eq!(helper.impl_targets[0].crate_root, "test:it");
        assert_eq!(helper.pending_impls[0].crate_root, "test:it");
        assert_eq!(helper.impl_trait_relations[0].crate_root, "test:it");
    }

    #[test]
    fn definition_crate_root_prefers_aux_helper_remap() {
        // Issue #475: a definition in a remapped helper file partitions under
        // the including entry crate's root, not its synthetic path-derived
        // root; every other definition keeps the path-derived root.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "tests/common/mod.rs".to_owned(),
                FileFacts {
                    definitions: vec![definition(
                        "helper-deep",
                        "function",
                        "tests/common/mod.rs",
                        &["inner", "deep"],
                    )],
                    ..FileFacts::default()
                },
            ),
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    definitions: vec![definition(
                        "lib-root",
                        "function",
                        "src/lib.rs",
                        &["root_fn"],
                    )],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let remap = BTreeMap::from([("tests/common/mod.rs".to_owned(), "test:it".to_owned())]);
        let index = DefinitionIndex::build(&facts, remap);
        let helper_def = &facts["tests/common/mod.rs"].definitions[0];
        assert_eq!(index.definition_crate_root(helper_def), "test:it");
        let lib_def = &facts["src/lib.rs"].definitions[0];
        assert_eq!(index.definition_crate_root(lib_def), "lib");
    }

    #[test]
    fn helper_included_by_two_entry_crates_is_not_reassigned() {
        // Both `tests/a.rs` and `tests/b.rs` include the same helper; it belongs
        // to neither unambiguously, so it keeps its path-based crate root.
        let facts: BTreeMap<String, FileFacts> = [
            ("tests/a.rs".to_owned(), mod_only_facts(&["common"])),
            ("tests/b.rs".to_owned(), mod_only_facts(&["common"])),
            (
                "tests/common/mod.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in(
                        "helper-Foo",
                        "Foo",
                        &[],
                        "struct",
                        "test:common",
                    )],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let remap = reassign_aux_helper_crate_roots(&facts);
        assert!(
            remap.is_empty(),
            "a helper shared by 2+ entry crates is left unresolved: {remap:?}"
        );
    }

    #[test]
    fn multi_includer_helper_roots_lists_each_including_entry_crate() {
        // `tests/common/mod.rs` is `mod`-included by both `tests/a.rs` and
        // `tests/b.rs`: the multi-includer map (issue #401) lists BOTH entry
        // crate roots, sorted, instead of picking one.
        let facts: BTreeMap<String, FileFacts> = [
            ("tests/a.rs".to_owned(), mod_only_facts(&["common"])),
            ("tests/b.rs".to_owned(), mod_only_facts(&["common"])),
            (
                "tests/common/mod.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in(
                        "helper-Foo",
                        "Foo",
                        &[],
                        "struct",
                        "test:common",
                    )],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let multi = multi_includer_aux_helper_roots(&facts);
        assert_eq!(
            multi.get("tests/common/mod.rs"),
            Some(&vec!["test:a".to_owned(), "test:b".to_owned()]),
            "a helper shared by 2+ entry crates lists every includer root: {multi:?}"
        );
    }

    #[test]
    fn own_aux_target_helper_is_excluded_from_multi_includer_map() {
        // `tests/common.rs` is included by both entries, but cargo ALSO compiles
        // it as its own test target `test:common` — it belongs to 3 crates, so
        // it is never duplicated into the including entries (conservative,
        // matching `helper_that_is_its_own_aux_target_is_not_reassigned`).
        let facts: BTreeMap<String, FileFacts> = [
            ("tests/a.rs".to_owned(), mod_only_facts(&["common"])),
            ("tests/b.rs".to_owned(), mod_only_facts(&["common"])),
            (
                "tests/common.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in(
                        "helper-Foo",
                        "Foo",
                        &[],
                        "struct",
                        "test:common",
                    )],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let multi = multi_includer_aux_helper_roots(&facts);
        assert!(
            multi.is_empty(),
            "a file cargo compiles as its own aux target is never duplicated: {multi:?}"
        );
    }

    #[test]
    fn shared_helper_included_by_two_entries_mints_implements_edge_per_including_crate() {
        // `tests/a.rs` and `tests/b.rs` each define a root `trait T` and each
        // `mod common;` the shared helper. The helper's `impl crate::T for Foo`
        // cannot pick one crate — but cargo compiles the helper ONCE PER
        // including test target, so the pass duplicates the helper's facts per
        // includer root (issue #401) and mints one edge per real compilation.
        // Each duplicate resolves strictly within its own crate root (#394
        // isolation), so no wrong edge is possible — only recovered recall.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "tests/a.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("a-T", "T", &[], "trait", "test:a")],
                    out_of_line_mods: mod_only_facts(&["common"]).out_of_line_mods,
                    ..FileFacts::default()
                },
            ),
            (
                "tests/b.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in("b-T", "T", &[], "trait", "test:b")],
                    out_of_line_mods: mod_only_facts(&["common"]).out_of_line_mods,
                    ..FileFacts::default()
                },
            ),
            (
                "tests/common/mod.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in(
                        "helper-Foo",
                        "Foo",
                        &[],
                        "struct",
                        "test:common",
                    )],
                    pending_impls: vec![pending_impl_in(
                        "impl-helper-Foo",
                        "crate::T",
                        &[],
                        "test:common",
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
                ("impl-helper-Foo".to_owned(), "a-T".to_owned()),
                ("impl-helper-Foo".to_owned(), "b-T".to_owned()),
            ],
            "the shared helper's impl must edge-back to EACH including crate's T: {records:?}"
        );
    }

    #[test]
    fn helper_that_is_its_own_aux_target_is_not_reassigned() {
        // `tests/common.rs` is included by `tests/it.rs` but cargo ALSO compiles
        // it as its own test target `test:common`, so it belongs to 2+ crates
        // and is never stolen into the including entry.
        let facts: BTreeMap<String, FileFacts> = [
            ("tests/it.rs".to_owned(), mod_only_facts(&["common"])),
            (
                "tests/common.rs".to_owned(),
                FileFacts {
                    impl_targets: vec![impl_target_in(
                        "helper-Foo",
                        "Foo",
                        &[],
                        "struct",
                        "test:common",
                    )],
                    ..FileFacts::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let remap = reassign_aux_helper_crate_roots(&facts);
        assert!(
            remap.is_empty(),
            "a file cargo compiles as its own aux target is not reassigned: {remap:?}"
        );
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

    // -----------------------------------------------------------------------
    // Qualified-path and cross-crate call resolution over a multi-crate
    // workspace (issue #440). These drive the REAL per-file extractor
    // (`extract_file_source`) so the crate-root / module-path derivation from
    // the repo-relative path is exercised end to end, then resolve the calls
    // with `cross_file_call_records`.
    // -----------------------------------------------------------------------

    /// Extracts every `(repo_relative_path, source)` entry through the real
    /// Rust extractor and collects the per-file facts into the map the
    /// cross-file resolution pass consumes.
    fn workspace_facts(entries: &[(&str, &str)]) -> BTreeMap<String, FileFacts> {
        use crate::fs::SourceFile;
        use crate::ir::Graph;

        entries
            .iter()
            .map(|(path, source)| {
                let file = SourceFile {
                    path: std::path::PathBuf::from(path),
                    repo_relative_path: (*path).to_owned(),
                };
                let mut graph = Graph::default();
                let facts = crate::languages::rust::extract_file_source(
                    &file,
                    source,
                    &format!("file:{path}"),
                    "repo",
                    &mut graph,
                )
                .expect("source should parse");
                ((*path).to_owned(), facts)
            })
            .collect()
    }

    /// The repo-relative paths of definitions reached by a `resolved` CALLS
    /// edge, paired with the callee's simple name, sorted for determinism.
    fn resolved_definition_hits(
        records: &[GraphRecord],
        facts: &BTreeMap<String, FileFacts>,
    ) -> Vec<(String, String)> {
        let by_id: BTreeMap<&str, &DefinitionFact> = facts
            .values()
            .flat_map(|f| f.definitions.iter())
            .map(|d| (d.id.as_str(), d))
            .collect();
        let mut hits: Vec<(String, String)> = records
            .iter()
            .filter(|r| r.resolution() == Some(CallResolution::Resolved))
            .filter_map(|r| match r {
                GraphRecord::Edge { target, .. } => by_id
                    .get(target.as_str())
                    .map(|d| (d.repo_relative_path.clone(), d.simple_name.clone())),
                _ => None,
            })
            .collect();
        hits.sort();
        hits
    }

    /// True when any `unresolved` CALLS edge (a per-file `Diagnostic` stub) was
    /// minted for a callee whose display path contains `needle`.
    fn has_unresolved_stub_for(records: &[GraphRecord], needle: &str) -> bool {
        records.iter().any(|r| match r {
            GraphRecord::Node {
                kind: NodeKind::Diagnostic,
                name,
                ..
            } => name.as_deref().is_some_and(|n| n.contains(needle)),
            _ => false,
        })
    }

    // (a) An intra-crate `crate::mod_b::target()` call binds the definition
    // Symbol, so `transitive-callers` can see the caller.
    #[test]
    fn qualified_crate_path_call_resolves_within_workspace_crate() {
        let facts = workspace_facts(&[
            ("crate_a/src/mod_b.rs", "pub fn target() {}\n"),
            (
                "crate_a/src/mod_a.rs",
                "pub fn caller() { crate::mod_b::target(); }\n",
            ),
            ("crate_a/src/lib.rs", "pub mod mod_a;\npub mod mod_b;\n"),
        ]);
        let records = cross_file_call_records("repo", &facts);
        let hits = resolved_definition_hits(&records, &facts);
        assert!(
            hits.contains(&("crate_a/src/mod_b.rs".to_owned(), "target".to_owned())),
            "crate::mod_b::target() must resolve to mod_b::target, got {hits:?}"
        );
        assert!(
            !has_unresolved_stub_for(&records, "mod_b::target"),
            "the qualified call must not become a Diagnostic stub: {records:?}"
        );
    }

    // (b) A cross-crate `crate_b::mod_c::target()` call resolves when
    // `crate_b` is a workspace crate.
    #[test]
    fn cross_crate_qualified_call_resolves_to_workspace_definition() {
        let facts = workspace_facts(&[
            ("crate_b/src/mod_c.rs", "pub fn target() {}\n"),
            ("crate_b/src/lib.rs", "pub mod mod_c;\n"),
            (
                "crate_a/src/mod_a.rs",
                "pub fn caller() { crate_b::mod_c::target(); }\n",
            ),
            ("crate_a/src/lib.rs", "pub mod mod_a;\n"),
        ]);
        let records = cross_file_call_records("repo", &facts);
        let hits = resolved_definition_hits(&records, &facts);
        assert!(
            hits.contains(&("crate_b/src/mod_c.rs".to_owned(), "target".to_owned())),
            "crate_b::mod_c::target() must resolve across crates, got {hits:?}"
        );
    }

    // (c) A qualified call into a crate that is NOT part of the workspace stays
    // unresolved — no wrong edge (doctrine).
    #[test]
    fn external_crate_qualified_call_stays_unresolved() {
        let facts = workspace_facts(&[
            // A local `mod_c::target` exists, but the call names `ext_dep::…`,
            // which is not a workspace crate, so it must NOT bind here.
            ("crate_a/src/mod_c.rs", "pub fn target() {}\n"),
            (
                "crate_a/src/mod_a.rs",
                "pub fn caller() { ext_dep::mod_c::target(); }\n",
            ),
            ("crate_a/src/lib.rs", "pub mod mod_a;\npub mod mod_c;\n"),
        ]);
        let records = cross_file_call_records("repo", &facts);
        let hits = resolved_definition_hits(&records, &facts);
        assert!(
            !hits.contains(&("crate_a/src/mod_c.rs".to_owned(), "target".to_owned())),
            "an external-crate qualified call must not bind a same-named local \
             definition: {hits:?}"
        );
        assert!(
            has_unresolved_stub_for(&records, "ext_dep"),
            "the external qualified call must stay an honest Diagnostic stub: {records:?}"
        );
    }

    // (d) Two same-named `target` functions in DIFFERENT modules must not
    // cross-resolve: `crate::mod_b::target()` binds only mod_b's.
    #[test]
    fn qualified_call_disambiguates_same_name_across_modules() {
        let facts = workspace_facts(&[
            ("crate_a/src/mod_b.rs", "pub fn target() {}\n"),
            ("crate_a/src/mod_d.rs", "pub fn target() {}\n"),
            (
                "crate_a/src/mod_a.rs",
                "pub fn caller() { crate::mod_b::target(); }\n",
            ),
            (
                "crate_a/src/lib.rs",
                "pub mod mod_a;\npub mod mod_b;\npub mod mod_d;\n",
            ),
        ]);
        let records = cross_file_call_records("repo", &facts);
        let hits = resolved_definition_hits(&records, &facts);
        assert!(
            hits.contains(&("crate_a/src/mod_b.rs".to_owned(), "target".to_owned())),
            "crate::mod_b::target() must resolve to mod_b's target, got {hits:?}"
        );
        assert!(
            !hits.contains(&("crate_a/src/mod_d.rs".to_owned(), "target".to_owned())),
            "crate::mod_b::target() must NOT bind mod_d's same-named target: {hits:?}"
        );
    }

    // ── Issue #444: inbound IMPORTS edges ────────────────────────────────

    /// Scans `(repo-relative path, source)` fixtures through the real per-file
    /// extractor, returning the exact `(records, facts_by_file)` inputs the
    /// scan pipelines feed `cross_file_import_target_edges`.
    fn import_edge_fixture(
        files: &[(&str, &str)],
    ) -> (Vec<GraphRecord>, BTreeMap<String, FileFacts>) {
        let mut records = Vec::new();
        let mut facts_by_file = BTreeMap::new();
        for (path, source) in files {
            let source_file = crate::fs::SourceFile {
                path: std::path::PathBuf::from(path),
                repo_relative_path: (*path).to_owned(),
            };
            let (mut file_records, facts) =
                crate::scan_source_text_records(&source_file, source, "repo")
                    .expect("fixture source must extract");
            records.append(&mut file_records);
            facts_by_file.insert((*path).to_owned(), facts);
        }
        (records, facts_by_file)
    }

    fn unattributed() -> CrateAttributionIndex {
        CrateAttributionIndex::from_facts(Vec::new())
    }

    fn package_attribution(manifest: &str, name: &str) -> CrateAttributionIndex {
        CrateAttributionIndex::from_facts(vec![crate::crate_attribution::ManifestPackageFact::new(
            manifest,
            crate::crate_attribution::ManifestParseOutcome::Package {
                name: name.to_owned(),
            },
        )])
    }

    /// The issue-#444 target edges minted for `importer_path`'s File node, as
    /// `(target kind, target id)` pairs. The extractor's containment-shaped
    /// `File —IMPORTS→ Import` edges are excluded by requiring a Module/File
    /// target — exactly the dependency-edge shape this pass mints.
    fn import_targets(
        importer_path: &str,
        files: &[(&str, &str)],
        attribution: &CrateAttributionIndex,
    ) -> Vec<(NodeKind, String)> {
        let (records, facts_by_file) = import_edge_fixture(files);
        let target_of: BTreeMap<&str, NodeKind> = records
            .iter()
            .filter_map(|record| match record {
                GraphRecord::Node { id, kind, .. } => Some((id.as_str(), *kind)),
                GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => None,
            })
            .collect();
        let importer_file_id = stable_id(&["node", "file", "repo", importer_path]);
        cross_file_import_target_edges("repo", &records, &facts_by_file, attribution)
            .into_iter()
            .filter_map(|record| match record {
                GraphRecord::Edge {
                    label: EdgeLabel::Imports,
                    source,
                    target,
                    ..
                } if source == importer_file_id => {
                    let kind = target_of.get(target.as_str()).copied()?;
                    (kind == NodeKind::Module || kind == NodeKind::File).then_some((kind, target))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn import_edge_targets_inline_module() {
        let files = &[
            ("src/lib.rs", "pub mod helpers {\n    pub fn help() {}\n}\n"),
            ("src/consumer.rs", "use crate::helpers;\n"),
        ];
        let targets = import_targets("src/consumer.rs", files, &unattributed());
        assert_eq!(
            targets,
            vec![(
                NodeKind::Module,
                stable_id(&["node", "module", "repo", "src/lib.rs", "helpers"])
            )],
            "crate::helpers must edge to the inline Module node"
        );
    }

    #[test]
    fn import_declared_inside_inline_module_edges_from_file() {
        // A `use` inside an inline `mod` block still belongs to the file:
        // the target-side IMPORTS edge sources from the File node, not the
        // enclosing Module node (issue #444: "importing files carry an
        // inbound IMPORTS edge").
        let files = &[
            (
                "src/lib.rs",
                "pub mod inner {\n    use crate::other;\n}\npub mod other;\n",
            ),
            ("src/other.rs", "pub fn f() {}\n"),
        ];
        let targets = import_targets("src/lib.rs", files, &unattributed());
        assert_eq!(
            targets,
            vec![(
                NodeKind::File,
                stable_id(&["node", "file", "repo", "src/other.rs"])
            )],
            "use inside inline mod must edge from the File node to the target"
        );
    }

    #[test]
    fn import_edge_targets_out_of_line_file() {
        let files = &[
            ("src/lib.rs", "pub mod b;\n"),
            ("src/b.rs", "pub fn f() {}\n"),
            ("src/consumer.rs", "use crate::b;\n"),
        ];
        let targets = import_targets("src/consumer.rs", files, &unattributed());
        assert_eq!(
            targets,
            vec![(
                NodeKind::File,
                stable_id(&["node", "file", "repo", "src/b.rs"])
            )],
            "crate::b must edge to b.rs's File node"
        );
    }

    #[test]
    fn import_edge_stops_at_deepest_resolvable_module() {
        let files = &[
            ("src/lib.rs", "pub mod b;\n"),
            ("src/b.rs", "pub struct Widget;\n"),
            ("src/consumer.rs", "use crate::b::Widget;\n"),
        ];
        let targets = import_targets("src/consumer.rs", files, &unattributed());
        // `Widget` is a symbol, not a module: the edge stops at the deepest
        // resolvable module — b.rs's File node.
        assert_eq!(
            targets,
            vec![(
                NodeKind::File,
                stable_id(&["node", "file", "repo", "src/b.rs"])
            )],
            "a trailing symbol leaf must not move the edge past its module"
        );
    }

    #[test]
    fn external_and_unresolvable_imports_mint_no_edge() {
        let files = &[
            ("src/lib.rs", "pub fn f() {}\n"),
            (
                "src/consumer.rs",
                "use serde::Serialize;\nuse std::fmt::Debug;\nuse nowhere::missing;\n",
            ),
        ];
        let targets = import_targets("src/consumer.rs", files, &unattributed());
        assert!(
            targets.is_empty(),
            "external/unresolvable imports must mint no edge, got {targets:?}"
        );
    }

    #[test]
    fn relative_imports_mint_no_edge() {
        let files = &[
            ("src/lib.rs", "pub mod b;\n"),
            ("src/b.rs", "pub fn f() {}\n"),
            ("src/consumer.rs", "use self::b;\nuse super::b;\n"),
        ];
        let targets = import_targets("src/consumer.rs", files, &unattributed());
        assert!(
            targets.is_empty(),
            "self::/super:: imports carry no module anchor at scan time: {targets:?}"
        );
    }

    #[test]
    fn workspace_absolute_import_resolves_via_manifest_attribution() {
        let files = &[
            ("crates/foo/src/lib.rs", "pub mod inner;\n"),
            ("crates/foo/src/inner.rs", "pub fn f() {}\n"),
            ("crates/foo/src/bin/tool.rs", "use foo::inner;\n"),
        ];
        let attribution = package_attribution("crates/foo/Cargo.toml", "foo");
        let targets = import_targets("crates/foo/src/bin/tool.rs", files, &attribution);
        assert_eq!(
            targets,
            vec![(
                NodeKind::File,
                stable_id(&["node", "file", "repo", "crates/foo/src/inner.rs"])
            )],
            "use foo::inner must resolve through the manifest-stamped package name"
        );
    }

    #[test]
    fn ambiguous_crate_name_mints_no_edge() {
        // Two workspace members declaring the SAME package name: the name is
        // dropped as ambiguous and absolute imports through it resolve nothing.
        let files = &[
            ("crates/a/src/lib.rs", "pub mod inner;\n"),
            ("crates/a/src/inner.rs", "pub fn f() {}\n"),
            ("crates/b/src/lib.rs", "pub fn g() {}\n"),
            ("crates/b/src/consumer.rs", "use foo::inner;\n"),
        ];
        let mut facts = vec![crate::crate_attribution::ManifestPackageFact::new(
            "crates/a/Cargo.toml",
            crate::crate_attribution::ManifestParseOutcome::Package {
                name: "foo".to_owned(),
            },
        )];
        facts.push(crate::crate_attribution::ManifestPackageFact::new(
            "crates/b/Cargo.toml",
            crate::crate_attribution::ManifestParseOutcome::Package {
                name: "foo".to_owned(),
            },
        ));
        let attribution = CrateAttributionIndex::from_facts(facts);
        let targets = import_targets("crates/b/src/consumer.rs", files, &attribution);
        assert!(
            targets.is_empty(),
            "an ambiguous crate name must resolve nothing: {targets:?}"
        );
    }

    #[test]
    fn pooled_entry_group_mints_no_edge() {
        // `src/lib.rs` + `src/main.rs` pool into one group with two entry
        // files: no single module tree anchors `crate::` imports, so nothing
        // resolves.
        let files = &[
            ("src/lib.rs", "pub mod a;\n"),
            ("src/a.rs", "pub fn f() {}\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("src/consumer.rs", "use crate::a;\n"),
        ];
        let targets = import_targets("src/consumer.rs", files, &unattributed());
        assert!(
            targets.is_empty(),
            "a group with two entry files must resolve nothing: {targets:?}"
        );
    }

    #[test]
    fn repeated_imports_deduplicate_to_one_edge() {
        let files = &[
            (
                "src/lib.rs",
                "pub mod helpers {\n    pub struct Widget;\n}\n",
            ),
            (
                "src/consumer.rs",
                "use crate::helpers;\nuse crate::helpers::Widget;\n",
            ),
        ];
        let targets = import_targets("src/consumer.rs", files, &unattributed());
        assert_eq!(
            targets,
            vec![(
                NodeKind::Module,
                stable_id(&["node", "module", "repo", "src/lib.rs", "helpers"])
            )],
            "two imports of the same module must mint exactly one edge"
        );
    }

    #[test]
    fn import_edges_are_deterministic() {
        let files = &[
            (
                "src/lib.rs",
                "pub mod b;\npub mod helpers {\n    pub fn help() {}\n}\n",
            ),
            ("src/b.rs", "pub mod deep;\n"),
            // Rust 2018 module resolution: `mod deep;` inside `src/b.rs`
            // (module `b`) lives at `src/b/deep.rs`.
            ("src/b/deep.rs", "pub fn f() {}\n"),
            (
                "src/consumer.rs",
                "use crate::b::deep;\nuse crate::helpers;\n",
            ),
        ];
        let attribution = unattributed();
        let (records, facts_by_file) = import_edge_fixture(files);
        let first = cross_file_import_target_edges("repo", &records, &facts_by_file, &attribution);
        let second = cross_file_import_target_edges("repo", &records, &facts_by_file, &attribution);
        assert_eq!(first, second, "the pass must be deterministic");
        // And sorted by edge id.
        let ids: Vec<&str> = first.iter().map(GraphRecord::id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "edges must come out sorted by id");
        assert_eq!(first.len(), 2, "expected two edges, got {first:?}");
    }

    #[test]
    fn import_edge_summaries_are_human_readable() {
        let files = &[
            ("src/lib.rs", "pub mod b;\n"),
            ("src/b.rs", "pub fn f() {}\n"),
            ("src/consumer.rs", "use crate::b;\n"),
        ];
        let (records, facts_by_file) = import_edge_fixture(files);
        let edges =
            cross_file_import_target_edges("repo", &records, &facts_by_file, &unattributed());
        assert_eq!(edges.len(), 1);
        let GraphRecord::Edge { summary, .. } = &edges[0] else {
            panic!("expected an edge record");
        };
        assert!(
            summary.contains("src/consumer.rs") && summary.contains("src/b.rs"),
            "summary must name importer and target, got {summary:?}"
        );
    }

    // ── Trait-dispatch resolution (issue #267) ────────────────────────────

    /// Fixture for trait-dispatch resolution (issue #267): trait `Renderable`
    /// implemented by `Circle` and `Square`, an unrelated inherent method that
    /// merely shares the method name (`Speaker::render`), a trait with no
    /// in-crate implementors (`Orphan`), and four dispatch shapes plus a static
    /// control call.
    fn dispatch_fixture() -> BTreeMap<String, FileFacts> {
        workspace_facts(&[
            ("crate_a/src/lib.rs", "pub mod shapes;\npub mod draw;\n"),
            (
                "crate_a/src/shapes.rs",
                "pub trait Renderable { fn render(&self); }\n                 pub struct Circle;\n                 impl Renderable for Circle { fn render(&self) {} }\n                 pub struct Square;\n                 impl Renderable for Square { fn render(&self) {} }\n                 pub struct Speaker;\n                 impl Speaker { pub fn render(&self) {} }\n                 pub trait Orphan { fn orphan_render(&self); }\n",
            ),
            (
                "crate_a/src/draw.rs",
                "use crate::shapes::{Circle, Orphan, Renderable, Speaker};\n                 pub fn draw_static(c: &Circle) { c.render(); }\n                 pub fn draw_dyn(item: &dyn Renderable) { item.render(); }\n                 pub fn draw_generic<T: Renderable>(t: &T) { t.render(); }\n                 pub fn draw_where<T>(t: T) where T: Renderable { t.render(); }\n                 pub fn draw_orphan(o: &dyn Orphan) { o.orphan_render(); }\n                 pub fn draw_speaker(s: &Speaker) { s.render(); }\n",
            ),
        ])
    }

    /// Repo-relative paths of definitions reached by `ambiguous` CALLS edges,
    /// alongside their resolution — mirrors `resolved_definition_hits`.
    fn ambiguous_definition_hits(
        records: &[GraphRecord],
        facts: &BTreeMap<String, FileFacts>,
    ) -> Vec<(String, String)> {
        let by_id: BTreeMap<&str, &DefinitionFact> = facts
            .values()
            .flat_map(|f| f.definitions.iter())
            .map(|d| (d.id.as_str(), d))
            .collect();
        let mut hits: Vec<(String, String)> = records
            .iter()
            .filter(|r| r.resolution() == Some(CallResolution::Ambiguous))
            .filter_map(|r| match r {
                GraphRecord::Edge { target, .. } => by_id
                    .get(target.as_str())
                    .map(|d| (d.repo_relative_path.clone(), d.simple_name.clone())),
                _ => None,
            })
            .collect();
        hits.sort();
        hits.dedup();
        hits
    }

    fn unresolved_dispatch_markers(records: &[GraphRecord]) -> Vec<String> {
        records
            .iter()
            .filter_map(|r| match r {
                GraphRecord::Node {
                    kind: crate::ir::NodeKind::Diagnostic,
                    name: Some(name),
                    ..
                } if name.starts_with("unresolved_dispatch:") => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn dyn_dispatch_links_every_in_crate_implementor() {
        let facts = dispatch_fixture();
        let records = cross_file_call_records("repo", &facts);
        let hits = ambiguous_definition_hits(&records, &facts);
        assert!(
            hits.contains(&("crate_a/src/shapes.rs".to_owned(), "render".to_owned())),
            "dyn dispatch must reach an implementor's render: {hits:?}"
        );
        // Both implementors must be reachable from the dispatch call site —
        // find the dispatch caller's outgoing ambiguous edges precisely.
        let by_id: BTreeMap<&str, &DefinitionFact> = facts
            .values()
            .flat_map(|f| f.definitions.iter())
            .map(|d| (d.id.as_str(), d))
            .collect();
        let draw_dyn_id = facts["crate_a/src/draw.rs"]
            .definitions
            .iter()
            .find(|d| d.simple_name == "draw_dyn")
            .expect("draw_dyn defined")
            .id
            .clone();
        let targets: Vec<String> = records
            .iter()
            .filter_map(|r| match r {
                GraphRecord::Edge {
                    source,
                    target,
                    label: crate::ir::EdgeLabel::Calls,
                    ..
                } if source == &draw_dyn_id
                    && r.resolution() == Some(CallResolution::Ambiguous) =>
                {
                    by_id
                        .get(target.as_str())
                        .map(|d| format!("{}::{}", d.repo_relative_path, d.simple_name))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            targets.len(),
            2,
            "draw_dyn must link exactly the two implementor methods: {targets:?}"
        );
        assert!(
            targets.iter().all(|t| t == "crate_a/src/shapes.rs::render"),
            "dispatch targets must be the implementor renders: {targets:?}"
        );
    }

    #[test]
    fn generic_bound_dispatch_links_every_in_crate_implementor() {
        let facts = dispatch_fixture();
        let records = cross_file_call_records("repo", &facts);
        let by_id: BTreeMap<&str, &DefinitionFact> = facts
            .values()
            .flat_map(|f| f.definitions.iter())
            .map(|d| (d.id.as_str(), d))
            .collect();
        for caller in ["draw_generic", "draw_where"] {
            let caller_id = facts["crate_a/src/draw.rs"]
                .definitions
                .iter()
                .find(|d| d.simple_name == caller)
                .unwrap_or_else(|| panic!("{caller} defined"))
                .id
                .clone();
            let targets: Vec<&DefinitionFact> = records
                .iter()
                .filter_map(|r| match r {
                    GraphRecord::Edge { source, target, .. }
                        if source == &caller_id
                            && r.resolution() == Some(CallResolution::Ambiguous) =>
                    {
                        by_id.get(target.as_str()).copied()
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                targets.len(),
                2,
                "{caller} must link exactly the two implementor methods"
            );
            assert!(
                targets
                    .iter()
                    .all(|d| d.repo_relative_path == "crate_a/src/shapes.rs"
                        && d.simple_name == "render"),
                "{caller} targets must be the implementor renders"
            );
        }
    }

    #[test]
    fn dispatch_never_links_unrelated_same_named_methods() {
        let facts = dispatch_fixture();
        let records = cross_file_call_records("repo", &facts);
        let by_id: BTreeMap<&str, &DefinitionFact> = facts
            .values()
            .flat_map(|f| f.definitions.iter())
            .map(|d| (d.id.as_str(), d))
            .collect();
        // Speaker::render is an inherent method that merely shares the name;
        // no dispatch edge may target it.
        let speaker_render = facts["crate_a/src/shapes.rs"]
            .definitions
            .iter()
            .find(|d| d.simple_name == "render" && d.qualified_name.contains("Speaker"))
            .map(|d| d.id.clone());
        if let Some(speaker_id) = speaker_render {
            let phantom = records.iter().any(|r| match r {
                GraphRecord::Edge { target, .. } => {
                    target == &speaker_id && r.resolution() == Some(CallResolution::Ambiguous)
                }
                _ => false,
            });
            assert!(
                !phantom,
                "dispatch must never link the unrelated same-named Speaker::render"
            );
        }
        let _ = by_id;
    }

    #[test]
    fn static_call_is_unchanged_by_dispatch() {
        let facts = dispatch_fixture();
        let records = cross_file_call_records("repo", &facts);
        let hits = resolved_definition_hits(&records, &facts);
        assert!(
            hits.contains(&("crate_a/src/shapes.rs".to_owned(), "render".to_owned())),
            "static call must still resolve: {hits:?}"
        );
    }

    #[test]
    fn unimplementable_dispatch_mints_a_typed_marker() {
        let facts = dispatch_fixture();
        let records = cross_file_call_records("repo", &facts);
        let markers = unresolved_dispatch_markers(&records);
        assert_eq!(
            markers.len(),
            1,
            "exactly one unresolved_dispatch marker is expected: {markers:?}"
        );
        assert!(
            markers[0].contains("Orphan") && markers[0].contains("orphan_render"),
            "marker must carry the trait+method handle: {:?}",
            markers[0]
        );
        // The dispatch edge must carry the TYPED resolution, not `unresolved`.
        let typed = records.iter().any(|r| {
            r.resolution() == Some(CallResolution::UnresolvedDispatch)
                && matches!(r, GraphRecord::Edge { .. })
        });
        assert!(typed, "dispatch edge must be unresolved_dispatch-typed");
    }

    // ── Out-of-line test-role propagation (issue #238, RED) ───────────────

    /// One out-of-line `mod <name>;` fact with an explicit gate flag.
    fn gated_mod_fact(name: &str, test_gated: bool) -> OutOfLineModFact {
        OutOfLineModFact {
            name: name.to_owned(),
            inline_module_path: Vec::new(),
            test_gated,
            path_override: None,
            under_inline_path_override: false,
            cfg_gates: Vec::new(),
        }
    }

    /// A `File` node record for `path` with a path-derived initial role,
    /// mirroring `scan_source_text_records`.
    fn file_record(path: &str) -> GraphRecord {
        GraphRecord::node(
            format!("file:{path}"),
            NodeKind::File,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            format!("summary for {path}"),
        )
        .with_role(crate::ir::SymbolRole::Production)
    }

    /// A `Symbol` node record for `name` in `path` with a lexical initial
    /// role, mirroring the per-file extractor.
    fn symbol_record(name: &str, path: &str, role: crate::ir::SymbolRole) -> GraphRecord {
        GraphRecord::node(
            format!("symbol:{path}#{name}"),
            NodeKind::Symbol,
            Some(path.to_owned()),
            None,
            Some(name.to_owned()),
            format!("summary for {name}"),
        )
        .with_role(role)
    }

    fn record_role(records: &[GraphRecord], id: &str) -> Option<crate::ir::SymbolRole> {
        records
            .iter()
            .find(|r| r.id() == id)
            .unwrap_or_else(|| panic!("record `{id}` should exist"))
            .role()
            .copied()
    }

    fn file_role(records: &[GraphRecord], path: &str) -> Option<crate::ir::SymbolRole> {
        record_role(records, &format!("file:{path}"))
    }

    #[test]
    fn out_of_line_test_gated_module_file_is_test() {
        // `#[cfg(test)] mod helpers;` in src/lib.rs → src/helpers.rs is a
        // test-only module file (issue #238 signal b, out-of-line form).
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![gated_mod_fact("helpers", true)],
                    ..FileFacts::default()
                },
            ),
            ("src/helpers.rs".to_owned(), FileFacts::default()),
        ]
        .into_iter()
        .collect();
        let mut records = vec![
            file_record("src/lib.rs"),
            file_record("src/helpers.rs"),
            // Symbols carry only their lexical role before the repo-wide
            // pass: `helper` has no in-file test signal, so it extracts as
            // production even though its module only compiles under cfg(test).
            symbol_record(
                "helper",
                "src/helpers.rs",
                crate::ir::SymbolRole::Production,
            ),
            symbol_record(
                "already_test",
                "src/helpers.rs",
                crate::ir::SymbolRole::Test,
            ),
        ];
        apply_out_of_line_test_roles(&mut records, &facts);
        assert_eq!(
            file_role(&records, "src/helpers.rs"),
            Some(crate::ir::SymbolRole::Test),
            "test-gated out-of-line module file must be test"
        );
        assert_eq!(
            file_role(&records, "src/lib.rs"),
            Some(crate::ir::SymbolRole::Production),
            "the declaring crate root stays production"
        );
        assert_eq!(
            record_role(&records, "symbol:src/helpers.rs#helper"),
            Some(crate::ir::SymbolRole::Test),
            "issue #238 signal (b) covers out-of-line modules: a symbol in a \
             test-only module file is test even with no lexical signal"
        );
        assert_eq!(
            record_role(&records, "symbol:src/helpers.rs#already_test"),
            Some(crate::ir::SymbolRole::Test),
            "an already-test symbol stays test"
        );
    }

    #[test]
    fn out_of_line_dual_use_module_file_stays_production() {
        // Production takes precedence (issue #223's rule, reused for #238):
        // a module file loaded by BOTH a test-gated and an ungated
        // declaration still compiles into the production build.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![gated_mod_fact("shared", false)],
                    ..FileFacts::default()
                },
            ),
            (
                "src/other.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![gated_mod_fact("shared", true)],
                    ..FileFacts::default()
                },
            ),
            ("src/shared.rs".to_owned(), FileFacts::default()),
        ]
        .into_iter()
        .collect();
        let mut records = vec![
            file_record("src/lib.rs"),
            file_record("src/other.rs"),
            file_record("src/shared.rs"),
            symbol_record(
                "shared_fn",
                "src/shared.rs",
                crate::ir::SymbolRole::Production,
            ),
        ];
        apply_out_of_line_test_roles(&mut records, &facts);
        assert_eq!(
            file_role(&records, "src/shared.rs"),
            Some(crate::ir::SymbolRole::Production),
            "dual-use module file must stay production"
        );
        assert_eq!(
            record_role(&records, "symbol:src/shared.rs#shared_fn"),
            Some(crate::ir::SymbolRole::Production),
            "production precedence extends to the dual-use file's symbols"
        );
    }

    #[test]
    fn out_of_line_test_scope_is_transitive_through_test_files() {
        // A test-gated module's own out-of-line submodules are test too,
        // even when their declarations carry no gate of their own.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![gated_mod_fact("helpers", true)],
                    ..FileFacts::default()
                },
            ),
            (
                "src/helpers.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![gated_mod_fact("inner", false)],
                    ..FileFacts::default()
                },
            ),
            ("src/helpers/inner.rs".to_owned(), FileFacts::default()),
        ]
        .into_iter()
        .collect();
        let mut records = vec![
            file_record("src/lib.rs"),
            file_record("src/helpers.rs"),
            file_record("src/helpers/inner.rs"),
        ];
        apply_out_of_line_test_roles(&mut records, &facts);
        assert_eq!(
            file_role(&records, "src/helpers/inner.rs"),
            Some(crate::ir::SymbolRole::Test),
            "transitive submodule of a test-only module must be test"
        );
    }

    // ── Out-of-line cfg-gate propagation (issue #190) ─────────────────────

    /// One out-of-line `mod <name>;` fact with an explicit cfg gate chain.
    fn cfg_gated_mod_fact(name: &str, gates: &[&str]) -> OutOfLineModFact {
        OutOfLineModFact {
            name: name.to_owned(),
            inline_module_path: Vec::new(),
            test_gated: false,
            path_override: None,
            under_inline_path_override: false,
            cfg_gates: gates.iter().map(|gate| (*gate).to_owned()).collect(),
        }
    }

    fn record_cfg(records: &[GraphRecord], id: &str) -> Option<Vec<String>> {
        records
            .iter()
            .find(|record| record.id() == id)
            .unwrap_or_else(|| panic!("record `{id}` should exist"))
            .cfg()
            .cloned()
    }

    fn file_cfg(records: &[GraphRecord], path: &str) -> Option<Vec<String>> {
        record_cfg(records, &format!("file:{path}"))
    }

    #[test]
    fn out_of_line_cfg_gated_module_file_inherits_gates() {
        // `#[cfg(feature = "x")] mod helpers;` in src/lib.rs → the whole
        // target file's records carry the declaration's gate chain.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![cfg_gated_mod_fact("helpers", &[r#"feature = "x""#])],
                    ..FileFacts::default()
                },
            ),
            ("src/helpers.rs".to_owned(), FileFacts::default()),
        ]
        .into_iter()
        .collect();
        let mut records = vec![
            file_record("src/lib.rs"),
            file_record("src/helpers.rs"),
            symbol_record(
                "helper",
                "src/helpers.rs",
                crate::ir::SymbolRole::Production,
            ),
        ];
        apply_out_of_line_cfg_gates(&mut records, &facts);
        assert_eq!(
            file_cfg(&records, "src/helpers.rs"),
            Some(vec![r#"feature = "x""#.to_owned()]),
            "gated out-of-line module file inherits the declaration gate"
        );
        assert_eq!(
            record_cfg(&records, "symbol:src/helpers.rs#helper"),
            Some(vec![r#"feature = "x""#.to_owned()]),
            "the module file's symbols inherit the declaration gate"
        );
        assert_eq!(
            file_cfg(&records, "src/lib.rs"),
            None,
            "the declaring file is never re-gated by its own declaration"
        );
    }

    #[test]
    fn out_of_line_cfg_gates_are_transitive() {
        // `#[cfg(x)] mod b;` in lib.rs and `#[cfg(y)] mod c;` in b.rs →
        // b/c.rs carries [x, y]: the declaring file's inherited gates prefix
        // the declaration's own chain. Paths follow real Rust module
        // resolution: `mod b;` in the crate root is src/b.rs, and `mod c;`
        // in src/b.rs is src/b/c.rs.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![cfg_gated_mod_fact("b", &[r#"feature = "x""#])],
                    ..FileFacts::default()
                },
            ),
            (
                "src/b.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![cfg_gated_mod_fact("c", &[r#"feature = "y""#])],
                    ..FileFacts::default()
                },
            ),
            ("src/b/c.rs".to_owned(), FileFacts::default()),
        ]
        .into_iter()
        .collect();
        let mut records = vec![
            file_record("src/lib.rs"),
            file_record("src/b.rs"),
            file_record("src/b/c.rs"),
        ];
        apply_out_of_line_cfg_gates(&mut records, &facts);
        assert_eq!(
            file_cfg(&records, "src/b.rs"),
            Some(vec![r#"feature = "x""#.to_owned()]),
            "direct target inherits the declaration gate"
        );
        assert_eq!(
            file_cfg(&records, "src/b/c.rs"),
            Some(vec![
                r#"feature = "x""#.to_owned(),
                r#"feature = "y""#.to_owned()
            ]),
            "transitive target accumulates gates outermost-first"
        );
    }

    #[test]
    fn out_of_line_cfg_ungated_declaration_wins() {
        // Dual-use module file: loaded ungated by src/lib.rs and gated by
        // src/other.rs. It compiles without the gate, so it inherits nothing
        // — mirroring the test-role pass's production precedence.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![cfg_gated_mod_fact("shared", &[])],
                    ..FileFacts::default()
                },
            ),
            (
                "src/other.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![cfg_gated_mod_fact("shared", &[r#"feature = "x""#])],
                    ..FileFacts::default()
                },
            ),
            ("src/shared.rs".to_owned(), FileFacts::default()),
        ]
        .into_iter()
        .collect();
        let mut records = vec![
            file_record("src/lib.rs"),
            file_record("src/other.rs"),
            file_record("src/shared.rs"),
            symbol_record(
                "shared_fn",
                "src/shared.rs",
                crate::ir::SymbolRole::Production,
            ),
        ];
        apply_out_of_line_cfg_gates(&mut records, &facts);
        assert_eq!(
            file_cfg(&records, "src/shared.rs"),
            None,
            "a file also loaded ungated inherits no gates"
        );
        assert_eq!(
            record_cfg(&records, "symbol:src/shared.rs#shared_fn"),
            None,
            "ungated precedence extends to the dual-use file's symbols"
        );
    }

    #[test]
    fn out_of_line_cfg_unions_gated_declarations() {
        // Two gated declarations load one file: the target inherits the
        // union, declaring files in sorted order. Both declarations come
        // from crate roots (lib.rs and main.rs) so that `mod shared;`
        // resolves to the same src/shared.rs under real Rust module
        // resolution.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![cfg_gated_mod_fact("shared", &[r#"feature = "x""#])],
                    ..FileFacts::default()
                },
            ),
            (
                "src/main.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![cfg_gated_mod_fact("shared", &[r#"feature = "y""#])],
                    ..FileFacts::default()
                },
            ),
            ("src/shared.rs".to_owned(), FileFacts::default()),
        ]
        .into_iter()
        .collect();
        let mut records = vec![
            file_record("src/lib.rs"),
            file_record("src/main.rs"),
            file_record("src/shared.rs"),
        ];
        apply_out_of_line_cfg_gates(&mut records, &facts);
        assert_eq!(
            file_cfg(&records, "src/shared.rs"),
            Some(vec![
                r#"feature = "x""#.to_owned(),
                r#"feature = "y""#.to_owned()
            ]),
            "all-gated dual use inherits the union in declaring-file order"
        );
    }

    #[test]
    fn out_of_line_cfg_prepends_before_existing_gates() {
        // A target file with its own `#![cfg]` inner attributes keeps them;
        // inherited gates prefix the chain, outermost first.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![cfg_gated_mod_fact("helpers", &[r#"feature = "x""#])],
                    ..FileFacts::default()
                },
            ),
            ("src/helpers.rs".to_owned(), FileFacts::default()),
        ]
        .into_iter()
        .collect();
        let mut records = vec![
            file_record("src/lib.rs"),
            file_record("src/helpers.rs").with_cfg(vec![r#"feature = "file""#.to_owned()]),
        ];
        apply_out_of_line_cfg_gates(&mut records, &facts);
        assert_eq!(
            file_cfg(&records, "src/helpers.rs"),
            Some(vec![
                r#"feature = "x""#.to_owned(),
                r#"feature = "file""#.to_owned()
            ]),
            "inherited gates prepend before the file's own gates"
        );
    }

    #[test]
    fn out_of_line_cfg_pass_is_idempotent() {
        // Re-running the pass over already-stamped records is a no-op, so
        // incremental reassembly cannot double-apply gates.
        let facts: BTreeMap<String, FileFacts> = [
            (
                "src/lib.rs".to_owned(),
                FileFacts {
                    out_of_line_mods: vec![cfg_gated_mod_fact("helpers", &[r#"feature = "x""#])],
                    ..FileFacts::default()
                },
            ),
            ("src/helpers.rs".to_owned(), FileFacts::default()),
        ]
        .into_iter()
        .collect();
        let mut records = vec![
            file_record("src/lib.rs"),
            file_record("src/helpers.rs"),
            symbol_record(
                "helper",
                "src/helpers.rs",
                crate::ir::SymbolRole::Production,
            ),
        ];
        apply_out_of_line_cfg_gates(&mut records, &facts);
        let once = records.clone();
        apply_out_of_line_cfg_gates(&mut records, &facts);
        assert_eq!(
            records, once,
            "a second pass run must not change already-stamped records"
        );
    }

    // ── Issue #148: macro invocation resolution ──────────────────────────

    fn macro_definition_fact(id: &str, name: &str, path: &str) -> MacroDefinitionFact {
        MacroDefinitionFact {
            id: id.to_owned(),
            simple_name: name.to_owned(),
            qualified_name: name.to_owned(),
            repo_relative_path: path.to_owned(),
        }
    }

    fn macro_invocation_fact(
        caller_id: &str,
        display: &str,
        simple_name: &str,
        path: &str,
        disambiguator: u64,
    ) -> MacroInvocationFact {
        MacroInvocationFact {
            caller_id: caller_id.to_owned(),
            caller_name: caller_id.to_owned(),
            invocation_display: display.to_owned(),
            simple_name: simple_name.to_owned(),
            span: span(),
            diagnostic_disambiguator: disambiguator,
            repo_relative_path: path.to_owned(),
        }
    }

    fn macro_facts(
        path: &str,
        definitions: Vec<MacroDefinitionFact>,
        invocations: Vec<MacroInvocationFact>,
    ) -> (String, FileFacts) {
        (
            path.to_owned(),
            FileFacts {
                macro_definitions: definitions,
                macro_invocations: invocations,
                ..FileFacts::default()
            },
        )
    }

    fn calls_edges(records: &[GraphRecord]) -> Vec<&GraphRecord> {
        records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Edge {
                        label: EdgeLabel::Calls,
                        ..
                    }
                )
            })
            .collect()
    }

    fn diagnostic_nodes(records: &[GraphRecord]) -> Vec<&GraphRecord> {
        records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Node {
                        kind: NodeKind::Diagnostic,
                        ..
                    }
                )
            })
            .collect()
    }

    #[test]
    fn macro_unique_resolution_emits_resolved_calls_edge() {
        let facts: BTreeMap<String, FileFacts> = [
            macro_facts(
                "src/lib.rs",
                vec![macro_definition_fact("macro-id", "greet", "src/lib.rs")],
                vec![macro_invocation_fact(
                    "caller-id",
                    "greet!",
                    "greet",
                    "src/lib.rs",
                    0,
                )],
            ),
            macro_facts(
                "src/main.rs",
                vec![],
                vec![macro_invocation_fact(
                    "other-caller",
                    "greet!",
                    "greet",
                    "src/main.rs",
                    0,
                )],
            ),
        ]
        .into_iter()
        .collect();
        let records = cross_file_macro_records("repo-id", &facts);
        let edges = calls_edges(&records);
        assert_eq!(edges.len(), 2, "one CALLS edge per invoking scope");
        for edge in &edges {
            let GraphRecord::Edge {
                source,
                target,
                resolution,
                ..
            } = edge
            else {
                panic!("expected an edge record");
            };
            assert_eq!(target, "macro-id");
            assert!(
                source == "caller-id" || source == "other-caller",
                "unexpected edge source {source}"
            );
            assert_eq!(
                *resolution,
                Some(CallResolution::Resolved),
                "a unique repo-defined macro resolves"
            );
        }
        assert!(
            diagnostic_nodes(&records).is_empty(),
            "a resolved invocation must not mint a diagnostic"
        );
    }

    #[test]
    fn macro_external_invocation_keeps_legacy_diagnostic() {
        let facts: BTreeMap<String, FileFacts> = BTreeMap::from([macro_facts(
            "src/main.rs",
            vec![],
            vec![macro_invocation_fact(
                "caller-id",
                "println!",
                "println",
                "src/main.rs",
                0,
            )],
        )]);
        let records = cross_file_macro_records("repo-id", &facts);
        assert!(
            calls_edges(&records).is_empty(),
            "no edge may bind an external macro name"
        );
        let diagnostics = diagnostic_nodes(&records);
        assert_eq!(diagnostics.len(), 1);
        let GraphRecord::Node {
            id, name, summary, ..
        } = diagnostics[0]
        else {
            panic!("expected a diagnostic node");
        };
        // The pass preserves the pre-#148 per-file diagnostic ID scheme, so
        // external-macro diagnostics stay byte-identical.
        let expected = stable_id(&[
            "node",
            "diagnostic",
            "repo-id",
            "src/main.rs",
            "println!",
            "0",
        ]);
        assert_eq!(id, &expected, "diagnostic ID scheme must not change");
        assert_eq!(name.as_deref(), Some("println!"));
        assert_eq!(summary.as_str(), "unsupported macro invocation println!");
    }

    #[test]
    fn macro_ambiguous_name_keeps_diagnostic_and_no_edge() {
        let facts: BTreeMap<String, FileFacts> = [
            macro_facts(
                "src/a.rs",
                vec![macro_definition_fact("macro-a", "greet", "src/a.rs")],
                vec![],
            ),
            macro_facts(
                "src/b.rs",
                vec![macro_definition_fact("macro-b", "greet", "src/b.rs")],
                vec![macro_invocation_fact(
                    "caller-id",
                    "greet!",
                    "greet",
                    "src/b.rs",
                    0,
                )],
            ),
        ]
        .into_iter()
        .collect();
        let records = cross_file_macro_records("repo-id", &facts);
        assert!(
            calls_edges(&records).is_empty(),
            "an ambiguous macro name must never silently bind one definition"
        );
        assert_eq!(
            diagnostic_nodes(&records).len(),
            1,
            "an ambiguous invocation stays a diagnostic"
        );
    }

    #[test]
    fn macro_resolution_is_deterministic_across_runs() {
        let facts: BTreeMap<String, FileFacts> = [
            macro_facts(
                "src/lib.rs",
                vec![macro_definition_fact("macro-id", "greet", "src/lib.rs")],
                vec![
                    macro_invocation_fact("caller-id", "greet!", "greet", "src/lib.rs", 0),
                    macro_invocation_fact("caller-id", "println!", "println", "src/lib.rs", 0),
                ],
            ),
            macro_facts(
                "src/main.rs",
                vec![],
                vec![macro_invocation_fact(
                    "other-caller",
                    "greet!",
                    "greet",
                    "src/main.rs",
                    0,
                )],
            ),
        ]
        .into_iter()
        .collect();
        let first = cross_file_macro_records("repo-id", &facts);
        for _ in 0..5 {
            let again = cross_file_macro_records("repo-id", &facts);
            assert_eq!(first, again, "the macro pass must be deterministic");
        }
    }
}
